"""The benchmark runner: headless Claude Code as the fixed agent for ALL arms.

Port of adaptive-context-clean/bench/cc_runner.py, adapted to the parsec
topology. Every arm runs the SAME Claude Code scaffold (driven via the Python
Claude Agent SDK, which spawns the real `claude` CLI) against the SAME
upstream; the ONLY thing that varies is the compression layer at the
model-call seam.

TOPOLOGY — the usage gateway is the SINGLE BOTTOM BRIDGE to the upstream
--------------------------------------------------------------------------
The upstream is https://api.anthropic.com by default (env BENCH_UPSTREAM to
retarget); auth is the USER'S OWN key on the box (ANTHROPIC_API_KEY), which
rides the chain untouched — the gateway and the parsec proxy both forward auth
headers verbatim and never store them.

    baseline
        Claude Code ── ANTHROPIC_BASE_URL=gateway ──> gateway ──> upstream
        The control: the model direct (no compression layer above the gateway).

    parsec (LocalProxyArm)
        Claude Code ── ANTHROPIC_BASE_URL=proxy ──> parsec proxy (curates,
            writes the savings ledger; PARSEC_UPSTREAM = gateway)
            ──> gateway ──> upstream
        The arm spawns the proxy per solve (black-box binary); the gateway
        BELOW it captures the real post-compression usage, and the proxy's
        per-run ledger provides the §8.4 counterfactual (parsec_bench.ledger).

The gateway records the PER-REQUEST cache split the SDK's ResultMessage omits,
into a per-run JSONL keyed by the x-ccb-run-id header we set via
ANTHROPIC_CUSTOM_HEADERS (the parsec proxy forwards it upstream — server.rs
forward_auth_headers — so the gateway sees it on the proxy-mediated leg too;
the per-run gateway's default_run_id remains the fallback). The SDK's
ResultMessage.total_cost_usd stays a diagnostic (reported_cost_usd); the
gateway rows drive the cache-aware token KPIs and the price frames.

Heavy imports (claude_agent_sdk; datasets; swebench) are LAZY so --list-arms
and `import parsec_bench.cc_runner` work on a box without them installed.

CUT vs the reference (clearly-marked seams):
  * GCS trace-bus rsync (--bus/gsutil) and any remote-instance orchestration —
    local single-machine runs only. To re-add, see _rsync_traj in the
    reference; the traj dir layout here is unchanged.
  * The shared standalone gateway (CCB_GATEWAY_URL) — per-solve ephemeral
    gateways only. (The proxy now forwards x-ccb-run-id upstream, so a SHARED
    gateway CAN isolate the parsec arm's rows per run; the per-solve topology
    is kept for simplicity, not necessity.)
  * The Vertex bridge (the reference gateway's MODE_VERTEX) — see
    parsec_bench.usage_gateway.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import multiprocessing as mp
import os
import signal
import subprocess
import time
from concurrent.futures import ProcessPoolExecutor, as_completed
from pathlib import Path
from typing import Optional

# arms self-register on import; the registry lives in parsec_bench.arm.
import parsec_bench.arms  # noqa: F401  (import side effect: registers every arm)
from parsec_bench import ledger as ledger_mod
from parsec_bench.arm import Arm, ArmKind, RunContext, available_arms, get_arm
from parsec_bench.grader import SWEBenchGrader
from parsec_bench.pricing import price_run, rates_for
from parsec_bench.schema import RunRecord
from parsec_bench.usage_gateway import DEFAULT_UPSTREAM, RUN_ID_HEADER, UsageGateway


# ── defaults / caps (mirror the reference so rows share budgets) ──────────────
DEFAULT_WORKERS = 4
CALL_CAP = 100                # max agent turns per (task, arm)
WALL_CAP_S = 50 * 60          # hard wall-clock watchdog per solve
# Model id Claude Code sends on the wire and what we record on the RunRecord.
DEFAULT_MODEL = os.environ.get("MODEL", "claude-sonnet-4-6")
# Bottom-bridge upstream every chain ends at (the user's own credits).
BENCH_UPSTREAM = os.environ.get("BENCH_UPSTREAM", DEFAULT_UPSTREAM)

# Native Claude Code tools the agent may use (the fixed scaffold's surface).
DEFAULT_ALLOWED_TOOLS = [
    "Bash", "Read", "Edit", "Write", "Glob", "Grep", "MultiEdit", "TodoWrite",
]


# ── task set loading ──────────────────────────────────────────────────────────
def load_tasks(path: str) -> list[str]:
    """Return the ordered list of instance ids from a task-set JSON file.

    Accepts either {"instances": [...]} or a bare JSON list of instance ids
    (strings or {"instance_id": ...} dicts).
    """
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if isinstance(data, dict):
        return list(data.get("instances", []))
    if isinstance(data, list):
        return [x if isinstance(x, str) else x["instance_id"] for x in data]
    raise ValueError(f"unrecognized task file shape: {type(data)}")


# ── instance fetch ────────────────────────────────────────────────────────────
def _fetch_instance(instance_id: str, dataset: str, split: str) -> dict:
    """Resolve the SWE-bench instance dict (problem_statement + repo info).

    AC_TASK_PROBLEM (optionally @file) is the bring-your-own-repo escape hatch;
    otherwise the HuggingFace datasets loader, falling back to a minimal dict
    when the dataset is unavailable.
    """
    _byo = os.environ.get("AC_TASK_PROBLEM")
    if _byo:
        if _byo.startswith("@"):
            try:
                _byo = open(_byo[1:], encoding="utf-8").read()
            except Exception:
                pass
        return {"instance_id": instance_id, "problem_statement": _byo}
    try:
        from datasets import load_dataset  # lazy
        ds = load_dataset(dataset, split=split)
        for row in ds:
            if row.get("instance_id") == instance_id:
                return dict(row)
    except Exception:
        pass
    return {"instance_id": instance_id}


class _NullGrade:
    """Sentinel grade for bring-your-own-repo runs (AC_NO_GRADE=1): no SWE-bench
    image or test set to grade against; the run captures the working-tree patch
    and leaves evaluation to the user."""
    success = False
    ftp = 0.0
    n_pass_to_pass = 0
    n_pass_to_pass_passed = 0
    error = ""


# ── arm -> SDK config mapping ─────────────────────────────────────────────────
class _ArmConfig:
    """The resolved per-arm SDK wiring for one (instance, arm) solve.

    client_base_url : the ANTHROPIC_BASE_URL Claude Code points at. None until
                      the worker resolves it (gateway URL for baseline; the
                      arm-started proxy URL for a LocalProxyArm; the fixed URL
                      for a pre-provisioned ProxyArm).
    """

    def __init__(self) -> None:
        self.client_base_url: Optional[str] = None
        self.proxy_base_url: str = ""      # recorded for chain logging
        self.allowed_tools: list[str] = list(DEFAULT_ALLOWED_TOOLS)
        self.disallowed_tools: list[str] = []
        self.setup_arm: Optional[Arm] = None
        # optional harness-level hook wiring (see _build_harness_hooks)
        self.system_prompt_append: Optional[str] = None
        self.sdk_hooks: Optional[dict] = None


def build_arm_config(arm: Arm) -> _ArmConfig:
    """Map an arm onto its Claude Code SDK wiring.

    A pre-provisioned ProxyArm's URL is known now; baseline and LocalProxyArm
    resolve at run time (the gateway / the spawned proxy).
    """
    cfg = _ArmConfig()
    cfg.setup_arm = arm
    if arm.kind == ArmKind.PROXY:
        base = arm.model_base_url()  # type: ignore[attr-defined]
        if base:
            cfg.client_base_url = base
            cfg.proxy_base_url = base
    return cfg


# ── OPTIONAL harness-level hooks (step0 / pre-tool / stop) ────────────────────
def _build_harness_hooks(arm: Arm, arm_cfg: _ArmConfig, instance: dict, repo_dir: str) -> None:
    """Translate an arm's OPTIONAL harness-level hooks into SDK wiring.

    (1) step0_injection -> appended to Claude Code's system prompt (preset
        {"type":"preset","preset":"claude_code","append": text}).
    (2) pre_tool_hook -> a PreToolUse hook; a returned tool_input becomes the
        SDK's updatedInput; the call is always ALLOWED (routes, never blocks).
    (3) stop_decision -> a Stop hook; finalize=False blocks the stop with the
        arm's directive. When the SDK reports the stop is already a forced
        continuation (stop_hook_active) we always allow it — a buggy arm can't
        pin the agent forever.

    Arms that override nothing leave arm_cfg untouched. Hook callbacks are
    async (the SDK requires awaitables) around the arm's SYNC methods.
    """
    if not arm.has_harness_hooks():
        return

    try:
        step0 = arm.step0_injection(instance, repo_dir)
    except Exception as e:  # noqa: BLE001 — an arm hook must never crash the solve
        print(f"  WARN: {arm.name}.step0_injection raised: "
              f"{type(e).__name__}: {str(e)[:160]}", flush=True)
        step0 = None
    if step0:
        arm_cfg.system_prompt_append = str(step0)

    try:
        from claude_agent_sdk import HookMatcher  # lazy
    except Exception:
        return  # no SDK on this box — step0 (which needs no SDK type) is kept

    hooks: dict = {}

    if _arm_overrides(arm, "pre_tool_hook"):
        async def _pre_tool(input_data, tool_use_id, context):  # noqa: ANN001
            tool_name = input_data.get("tool_name", "")
            tool_input = input_data.get("tool_input", {}) or {}
            try:
                res = arm.pre_tool_hook(tool_name, dict(tool_input))
            except Exception as e:  # noqa: BLE001
                print(f"  WARN: {arm.name}.pre_tool_hook raised: "
                      f"{type(e).__name__}: {str(e)[:160]}", flush=True)
                res = None
            spec: dict = {"hookEventName": "PreToolUse", "permissionDecision": "allow"}
            if isinstance(res, dict) and res.get("tool_input") is not None:
                spec["updatedInput"] = res["tool_input"]
            return {"hookSpecificOutput": spec}

        hooks["PreToolUse"] = [HookMatcher(hooks=[_pre_tool])]

    if _arm_overrides(arm, "stop_decision"):
        async def _stop(input_data, tool_use_id, context):  # noqa: ANN001
            if input_data.get("stop_hook_active"):
                return {}
            state = {
                "stop_hook_active": bool(input_data.get("stop_hook_active")),
                "session_id": input_data.get("session_id"),
                "cwd": input_data.get("cwd"),
                "transcript_path": input_data.get("transcript_path"),
            }
            try:
                dec = arm.stop_decision(state)
            except Exception as e:  # noqa: BLE001
                print(f"  WARN: {arm.name}.stop_decision raised: "
                      f"{type(e).__name__}: {str(e)[:160]}", flush=True)
                dec = None
            if dec is not None and not getattr(dec, "finalize", True):
                out: dict = {"decision": "block"}
                directive = getattr(dec, "directive", None)
                if directive:
                    out["reason"] = str(directive)
                return out
            return {}

        hooks["Stop"] = [HookMatcher(hooks=[_stop])]

    arm_cfg.sdk_hooks = hooks or None


def _arm_overrides(arm: Arm, name: str) -> bool:
    """Whether ``arm`` overrides the optional hook ``name`` away from the no-op."""
    return getattr(type(arm), name) is not getattr(Arm, name)


# ── the prompt the agent is given ─────────────────────────────────────────────
def _build_task_prompt(instance: dict, instance_id: str, repo_dir: str) -> str:
    """The single user prompt that kicks off the headless solve."""
    problem = (instance.get("problem_statement") or "").strip()
    if not problem:
        problem = f"Resolve the failing tests for instance {instance_id}."
    return (
        f"You are working in the repository checked out at {repo_dir}.\n\n"
        f"Resolve the following issue by editing the code in this repository. "
        f"Make the failing tests pass without breaking existing tests. Do not "
        f"write a patch file — edit the source directly; the harness will capture "
        f"your changes from the working tree.\n\n"
        f"--- ISSUE ---\n{problem}\n"
    )


# ── SDK run (async) — drive Claude Code to completion, collect the result ─────
async def _run_sdk(
    *,
    prompt: str,
    cwd: str,
    model: str,
    arm_cfg: _ArmConfig,
    client_base_url: str,
    run_id: str,
    call_cap: int,
    env_overrides: dict,
) -> dict:
    """Run one headless Claude Code solve via the SDK and collect raw signals.

    ``client_base_url`` is the ANTHROPIC_BASE_URL Claude Code points at: the
    gateway directly (baseline) or the arm's proxy. Either way the chain
    bottoms out at the gateway. Auth stays the user's own (ANTHROPIC_API_KEY
    inherited from the environment; never rewritten here).
    """
    from claude_agent_sdk import ClaudeAgentOptions, query  # lazy

    env = dict(os.environ)
    env["ANTHROPIC_BASE_URL"] = client_base_url
    # Claude Code forwards ANTHROPIC_CUSTOM_HEADERS onto every model request:
    # the gateway tags usage rows by it, and the parsec proxy prefixes its
    # conv_id with it (ledger row attribution).
    env["ANTHROPIC_CUSTOM_HEADERS"] = f"{RUN_ID_HEADER}: {run_id}"
    env["CCB_RUN_ID"] = run_id
    env.setdefault("ANTHROPIC_MODEL", model)
    # Per-instance test environment: prepare_repos builds an isolated venv at
    # <repo_root>/.venvs/<iid>, a SIBLING of the worktree. Point the agent's
    # PATH/VIRTUAL_ENV at it so python/pytest resolve to the task's own
    # toolchain (without this the agent loops on a broken host pytest).
    _cwd_n = os.path.normpath(cwd)
    _venv_dir = os.path.join(os.path.dirname(_cwd_n), ".venvs", os.path.basename(_cwd_n))
    _venv_bin = os.path.join(_venv_dir, "bin")
    if os.path.isdir(_venv_bin):
        env["PATH"] = _venv_bin + os.pathsep + env.get("PATH", "")
        env["VIRTUAL_ENV"] = _venv_dir
        env.pop("PYTHONHOME", None)
        env.pop("PYTHONPATH", None)
    env.update(env_overrides or {})

    sdk_hooks = arm_cfg.sdk_hooks
    sys_append = arm_cfg.system_prompt_append
    # Cache isolation: tag the system block with this run's id so each run's
    # cached prefix is byte-unique — the per-org content-addressed prompt cache
    # cannot be shared across runs (that would flatter every arm's read rate).
    sys_append = (sys_append or "") + ("\n<!-- ccbench-cache-iso:%s -->" % run_id)
    system_prompt = {"type": "preset", "preset": "claude_code", "append": sys_append}
    options = ClaudeAgentOptions(
        allowed_tools=arm_cfg.allowed_tools,
        disallowed_tools=arm_cfg.disallowed_tools,
        max_turns=call_cap,
        cwd=cwd,
        model=model,
        hooks=sdk_hooks,
        system_prompt=system_prompt,
        env=env,
        # headless: never block on a permission prompt — the agent runs
        # unattended in a per-task working copy.
        permission_mode="bypassPermissions",
        strict_mcp_config=True,
        include_hook_events=bool(sdk_hooks),
    )

    messages: list[dict] = []
    result_msg = None
    # Hold the async generator EXPLICITLY so we can always finalize it: a bare
    # `async for` that raises mid-stream leaves the generator suspended and its
    # `claude` subprocess ORPHANED (still running, still billing). aclose()
    # terminates the subprocess on EVERY exit path.
    agen = query(prompt=prompt, options=options)
    try:
        async for msg in agen:
            messages.append(_message_to_jsonable(msg))
            if type(msg).__name__ == "ResultMessage":
                result_msg = msg
    finally:
        aclose = getattr(agen, "aclose", None)
        if aclose is not None:
            try:
                await aclose()
            except Exception:  # noqa: BLE001 — best-effort reap; never mask the real error
                pass

    return _collect_result(result_msg, messages)


def _message_to_jsonable(msg) -> dict:
    """Best-effort JSON-able dump of one SDK message (for the trajectory file).
    Never raises — a message we can't introspect is dumped as its repr."""
    out: dict = {"type": type(msg).__name__}
    try:
        for k, v in vars(msg).items():
            if k.startswith("_"):
                continue
            out[k] = _jsonable(v)
    except TypeError:
        out["repr"] = repr(msg)[:2000]
    return out


def _jsonable(v):
    """Coerce a value (incl. content blocks) into something json.dumps handles."""
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if isinstance(v, (list, tuple)):
        return [_jsonable(x) for x in v]
    if isinstance(v, dict):
        return {str(k): _jsonable(x) for k, x in v.items()}
    if hasattr(v, "__dict__"):
        d = {"_type": type(v).__name__}
        for k, x in vars(v).items():
            if not k.startswith("_"):
                d[k] = _jsonable(x)
        return d
    return str(v)


def _collect_result(result_msg, messages: list[dict]) -> dict:
    """Pull raw run signals off the ResultMessage + message stream."""
    usage = {}
    reported_cost = 0.0
    num_turns = 0
    is_error = False
    subtype = ""
    session_id = ""
    if result_msg is not None:
        usage = getattr(result_msg, "usage", None) or {}
        reported_cost = float(getattr(result_msg, "total_cost_usd", 0.0) or 0.0)
        num_turns = int(getattr(result_msg, "num_turns", 0) or 0)
        is_error = bool(getattr(result_msg, "is_error", False))
        subtype = str(getattr(result_msg, "subtype", "") or "")
        session_id = str(getattr(result_msg, "session_id", "") or "")

    asst = [m for m in messages if m.get("type") == "AssistantMessage"]
    tool_calls = 0
    for m in asst:
        for blk in (m.get("content") or []):
            if isinstance(blk, dict) and blk.get("_type") == "ToolUseBlock":
                tool_calls += 1
    steps = num_turns or len(asst)

    return {
        "reported_cost_usd": reported_cost,
        "sdk_usage": usage,
        "num_turns": num_turns,
        "steps": steps,
        "tool_calls": tool_calls,
        "is_error": is_error,
        "subtype": subtype,
        "session_id": session_id,
        "messages": messages,
    }


class RunInfraError(Exception):
    """Raised on an infrastructure failure (SDK/model/network) — retried once.

    Carries ``usage`` — the gateway rows the failed solve DID generate — so the
    failure is priced from real tokens instead of recorded as $0.
    """
    def __init__(self, msg: str, usage: Optional[list] = None):
        super().__init__(msg)
        self.usage = usage or []


# ── patch capture (git diff of the working tree the agent edited) ─────────────
def _capture_patch(repo_dir: str) -> str:
    """The unified diff the agent produced: ``git add -A && git diff --cached``.
    Empty string on no repo / no change / any git failure — never raises."""
    try:
        subprocess.run(["git", "-C", repo_dir, "add", "-A"],
                       capture_output=True, timeout=120)
        out = subprocess.run(["git", "-C", repo_dir, "diff", "--cached"],
                             capture_output=True, text=True, timeout=120)
        return out.stdout or ""
    except Exception:
        return ""


# ── orphan reaper: backstop for a leaked `claude` CLI subprocess ──────────────
def _reap_orphan_claude() -> int:
    """SIGKILL any `claude` CLI still descending from THIS worker, on teardown.

    Kill by EXPLICIT PID after walking /proc ancestry — never a pattern match.
    Best-effort, Linux-only (returns 0 where there is no procfs, e.g. macOS
    dev boxes), never raises.
    """
    me = os.getpid()
    try:
        pids = [int(p) for p in os.listdir("/proc") if p.isdigit()]
    except Exception:
        return 0
    ppid_of: dict[int, int] = {}
    comm_of: dict[int, str] = {}
    for pid in pids:
        try:
            stat = open(f"/proc/{pid}/stat").read()
            rp = stat.rindex(")")               # comm is in parens, may hold spaces
            comm_of[pid] = stat[stat.index("(") + 1:rp]
            ppid_of[pid] = int(stat[rp + 2:].split()[1])
        except Exception:
            continue
    killed = 0
    for pid, comm in comm_of.items():
        if comm != "claude":
            continue
        cur, hops = ppid_of.get(pid, 0), 0      # walk ancestry up to me (bounded)
        while cur and hops < 64:
            if cur == me:
                try:
                    os.kill(pid, signal.SIGKILL)
                    killed += 1
                except Exception:
                    pass
                break
            cur = ppid_of.get(cur, 0)
            hops += 1
    return killed


# ── the per-(instance, arm) solve ─────────────────────────────────────────────
def run_agent(
    arm: Arm,
    instance_id: str,
    *,
    model: str,
    dataset: str,
    split: str,
    out_dir: str,
    run_id: str,
    call_cap: int = CALL_CAP,
    wall_cap_s: int = WALL_CAP_S,
    repo_root: Optional[str] = None,
    traj_path: Optional[str] = None,
) -> dict:
    """Drive one headless Claude Code solve for (instance, arm); return raw signals.

    Builds the arm->SDK config, starts a per-solve usage gateway (the bottom
    bridge to BENCH_UPSTREAM), asks the arm to start its proxy (LocalProxyArm),
    runs the SDK to completion against the task repo, captures the patch via
    git, and returns raw signals: the gateway usage rows plus the proxy-ledger
    savings totals (§8.4). No grading here — the caller grades the patch.
    """
    try:
        arm.setup()
    except Exception as e:  # noqa: BLE001
        raise RunInfraError(f"arm.setup() failed for '{arm.name}': "
                            f"{type(e).__name__}: {str(e)[:200]}") from e

    arm_cfg = build_arm_config(arm)
    repo_dir = _resolve_repo_dir(instance_id, repo_root, out_dir,
                                 arm_name=getattr(arm, "name", None))

    # CLEAN START per (instance, arm): reset the worktree to base BEFORE the
    # agent runs, so runs/arms never accumulate edits on a shared tree.
    if (not os.environ.get("AC_TASK_REPO")) and (   # BYO: never reset the user's tree
            os.path.isdir(os.path.join(repo_dir, ".git"))
            or os.path.isfile(os.path.join(repo_dir, ".git"))):
        try:
            subprocess.run(["git", "-C", repo_dir, "reset", "--hard", "HEAD"],
                           capture_output=True, text=True, timeout=120)
            subprocess.run(["git", "-C", repo_dir, "clean", "-fd"],
                           capture_output=True, text=True, timeout=120)
        except Exception as _e:  # noqa: BLE001
            print(f"  [warn] worktree reset failed for {instance_id}: {_e}", flush=True)

    instance = _fetch_instance(instance_id, dataset, split)
    prompt = _build_task_prompt(instance, instance_id, repo_dir)
    _build_harness_hooks(arm, arm_cfg, instance, repo_dir)

    # Per-solve gateway: the bottom bridge. default_run_id tags even the rows
    # arriving via the parsec proxy (which strips the run-id header upstream).
    run_dir = str(Path(out_dir) / "runs_scratch" / run_id)
    Path(run_dir).mkdir(parents=True, exist_ok=True)
    gateway = UsageGateway(BENCH_UPSTREAM, log_dir=str(Path(out_dir) / "usage"),
                           default_run_id=run_id, timeout_s=float(wall_cap_s)).start()
    gateway_usage_path = gateway.usage_path(run_id)

    # Where Claude Code points: the arm's proxy or the gateway directly.
    arm_base: Optional[str] = None
    try:
        arm_base = arm.start_run(RunContext(
            upstream_base_url=gateway.base_url, run_dir=run_dir, run_id=run_id))
    except Exception as e:  # noqa: BLE001
        gateway.stop()
        raise RunInfraError(f"arm.start_run() failed for '{arm.name}': "
                            f"{type(e).__name__}: {str(e)[:200]}") from e
    client_base_url = arm_base or arm_cfg.client_base_url or gateway.base_url
    if arm_base or arm_cfg.proxy_base_url:
        print(f"  chain [{arm.name}]: ClaudeCode -> "
              f"{arm_base or arm_cfg.proxy_base_url} (arm proxy) -> "
              f"{gateway.base_url} (gateway) -> {BENCH_UPSTREAM}", flush=True)
    else:
        print(f"  chain [{arm.name}]: ClaudeCode -> {gateway.base_url} (gateway) "
              f"-> {BENCH_UPSTREAM}", flush=True)

    # accumulation guard: read ONLY the rows this attempt appends.
    _usage_start = len(_read_usage_rows(gateway_usage_path))
    t0 = time.time()
    exit_status = "incomplete"
    raw: dict = {}
    try:
        raw = asyncio.run(
            _run_with_wall_cap(
                prompt=prompt, cwd=repo_dir, model=model, arm_cfg=arm_cfg,
                client_base_url=client_base_url, run_id=run_id, call_cap=call_cap,
                wall_cap_s=wall_cap_s,
                env_overrides={},
            )
        )
        exit_status = raw.get("subtype") or ("error" if raw.get("is_error") else "success")
    except _WallCapExceeded:
        exit_status = "wall_cap"
        raw = raw or {}
    except Exception as e:  # noqa: BLE001 — surface infra faults to the worker (retried once)
        _msg = str(e).lower()
        # HITTING THE CAP IS THE FINDING, NOT AN INFRA FAULT: grade it as a
        # (limit-death) terminal outcome, never retry.
        if ("maximum number of turns" in _msg or "max_turns" in _msg or "max turns" in _msg
                or "maximum budget" in _msg or "max_budget" in _msg or "error_max_budget" in _msg):
            exit_status = "error_max_budget_usd" if "budget" in _msg else "error_max_turns"
            raw = raw or {}
        else:
            try:
                _failed_usage = _read_usage_rows(gateway_usage_path)[_usage_start:]
            except Exception:
                _failed_usage = []
            arm.end_run()
            try:
                arm.teardown()
            except Exception:
                pass
            gateway.stop()
            raise RunInfraError(f"{type(e).__name__}: {str(e)[:300]}",
                                usage=_failed_usage) from e
    finally:
        arm.end_run()
        gateway.stop()
        try:
            arm.teardown()
        except Exception:
            pass
        # backstop: SIGKILL any `claude` child that survived aclose.
        n = _reap_orphan_claude()
        if n:
            print(f"  reaped {n} orphan claude proc(s) for {instance_id} "
                  f"[{arm.name}]", flush=True)

    wall_s = round(time.time() - t0, 1)
    patch = _capture_patch(repo_dir)
    # LEAK HARDENING: reset the checkout immediately after capturing the patch
    # so a sibling arm's run can never diff-and-copy this run's edits.
    if os.environ.get("CCB_NO_RESET_AFTER") != "1" and not os.environ.get("AC_TASK_REPO"):
        try:
            subprocess.run(["git", "reset", "--hard", "-q", "HEAD"], cwd=repo_dir,
                           capture_output=True, timeout=60)
            subprocess.run(["git", "clean", "-fdq"], cwd=repo_dir,
                           capture_output=True, timeout=60)
        except Exception:
            pass
    submitted = bool(patch.strip())

    # the authoritative per-CALL usage series: the gateway JSONL rows.
    usage = _read_usage_rows(gateway_usage_path)[_usage_start:]

    # §8.4 savings: the arm proxy's ledger (parsec arm), summarized with null
    # probes excluded. None for arms without a ledger (baseline).
    ledger_totals = None
    lp = arm.ledger_path()
    if lp:
        try:
            ledger_totals = ledger_mod.summarize_path(lp, run_id="")
        except Exception as e:  # noqa: BLE001
            print(f"  WARN: ledger read failed for {instance_id} [{arm.name}]: "
                  f"{type(e).__name__}: {str(e)[:160]}", flush=True)

    # write the native SDK trajectory dump (best-effort; never crash a paid run).
    if traj_path:
        try:
            _tr_msgs = raw.get("messages", [])
            _tr_src = "sdk_stream"
            if not _tr_msgs:
                # cli_transcript_fallback: the SDK stream can die on oversized
                # messages while the CLI child completes — recover its own
                # session transcript so the run stays auditable.
                _sid = str(raw.get("session_id") or "")
                if _sid:
                    import glob as _glob
                    _cand = _glob.glob(os.path.expanduser(
                        "~/.claude/projects/*/%s.jsonl" % _sid))
                    if _cand:
                        try:
                            _tr_msgs = [json.loads(_l) for _l in
                                        open(_cand[0], encoding="utf-8", errors="replace")
                                        if _l.strip()]
                            _tr_src = "cli_transcript_fallback:%s" % _cand[0]
                        except Exception:
                            _tr_msgs = []
            Path(traj_path).write_text(json.dumps({
                "instance": instance_id, "arm": arm.name, "model": model,
                "run_id": run_id, "exit_status": exit_status,
                "num_turns": raw.get("num_turns", 0),
                "traj_source": _tr_src,
                "messages": _tr_msgs,
            }), encoding="utf-8")
        except Exception as e:  # noqa: BLE001
            print(f"  WARN: trajectory write failed for {instance_id} [{arm.name}]: "
                  f"{type(e).__name__}: {str(e)[:160]}", flush=True)

    # token rollups from the gateway series.
    in_tok = sum(u.get("prompt_tokens", 0) for u in usage)
    out_tok = sum(u.get("completion_tokens", 0) for u in usage)
    max_prompt = max((u.get("prompt_tokens", 0) for u in usage), default=0)
    lats = [u["latency_s"] for u in usage if u.get("latency_s") is not None]
    mean_lat = round(sum(lats) / len(lats), 3) if lats else 0.0
    calls = raw.get("num_turns", 0) or len(usage)
    steps = raw.get("steps", 0) or calls

    el = (exit_status or "").lower()
    hit_cap = exit_status == "wall_cap" or "max_turns" in el or "max_budget" in el or calls >= call_cap
    limit_death = hit_cap and not submitted

    return {
        "instance": instance_id,
        "arm": arm.name,
        "patch": patch,
        "calls": calls,
        "exit_status": exit_status,
        "input_tokens": in_tok,
        "output_tokens": out_tok,
        "usage": usage,
        "ledger_totals": ledger_totals,
        "wall_s": wall_s,
        "submitted": submitted,
        "limit_death": limit_death,
        "steps": steps,
        "tool_calls": raw.get("tool_calls", 0),
        "time_to_submit_s": wall_s if submitted else 0.0,
        "mean_call_latency_s": mean_lat,
        "max_prompt_tokens": max_prompt,
        "retries": 0,
        "degraded": bool(ledger_totals and (ledger_totals.fail_opens
                                            or ledger_totals.scorer_fail_opens)),
        "reported_cost_usd": float(raw.get("reported_cost_usd", 0.0) or 0.0),
    }


def _resolve_repo_dir(instance_id: str, repo_root: Optional[str], out_dir: str,
                      arm_name: Optional[str] = None) -> str:
    """The host path the agent's cwd points at for this instance's repo.

    Precedence: AC_TASK_REPO (bring-your-own-repo) > --repo-root/<iid> >
    AC_REPO_ROOT/<iid> > a stub dir under out_dir (patch will be empty,
    surfacing the mount gap rather than crashing).

    PER-ARM PARALLELISM: a per-arm worktree ``<root>/<iid>__<arm>``
    (provisioned by ``prepare_repos --arms``) wins over the shared
    ``<root>/<iid>`` so multiple arms can run the same task concurrently.
    """
    _byo = os.environ.get("AC_TASK_REPO")
    if _byo:
        return os.path.abspath(_byo)
    root = repo_root or os.environ.get("AC_REPO_ROOT")
    if root:
        if arm_name:
            per_arm = Path(root) / ("%s__%s" % (instance_id, arm_name))
            if per_arm.exists():
                return str(per_arm)
        return str(Path(root) / instance_id)
    d = Path(out_dir) / "repos" / instance_id
    d.mkdir(parents=True, exist_ok=True)
    return str(d)


def _read_usage_rows(path: Path) -> list[dict]:
    """Read the gateway's per-run usage JSONL into a CallUsage list (call order)."""
    rows: list[dict] = []
    try:
        if path.exists():
            for line in path.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    rows.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    except Exception:
        pass
    return rows


# ── wall-clock cap around the async SDK run ──────────────────────────────────
class _WallCapExceeded(Exception):
    """The solve blew its wall-clock budget — aborted (productive-death)."""


async def _run_with_wall_cap(*, wall_cap_s: int, **kw) -> dict:
    """Run the SDK solve under a wall-clock deadline (asyncio.wait_for)."""
    try:
        return await asyncio.wait_for(_run_sdk(**kw), timeout=float(wall_cap_s))
    except asyncio.TimeoutError as e:
        raise _WallCapExceeded(f"wall cap {wall_cap_s}s exceeded") from e


# ── worker: run + grade + price one (instance, arm) ──────────────────────────
def _worker(job: tuple) -> dict:
    """Process-pool task: solve, grade, price one (instance, arm). Never raises.

    Returns a RunRecord.to_json() dict; on infra failure a stub with
    infra_failed=True (excluded from metrics, retried once by the driver).
    """
    (instance_id, arm_name, model, dataset, split, call_cap, wall_cap_s,
     grade_timeout_s, out_dir, run_id, repo_root) = job
    t0 = time.time()
    traj_dir = Path(out_dir) / "traj"
    traj_dir.mkdir(parents=True, exist_ok=True)
    tag = f"{instance_id}_{run_id}_{arm_name}"
    traj_path = str(traj_dir / f"{tag}.traj.json")
    gw_run_id = tag
    try:
        arm = get_arm(arm_name)
        raw = run_agent(
            arm, instance_id, model=model, dataset=dataset, split=split,
            out_dir=out_dir, run_id=gw_run_id, call_cap=call_cap,
            wall_cap_s=wall_cap_s, repo_root=repo_root, traj_path=traj_path,
        )
        if os.environ.get("AC_NO_GRADE") == "1":
            g = _NullGrade()
        else:
            grader = SWEBenchGrader(dataset=dataset, split=split, timeout_s=grade_timeout_s,
                                    cache_level=os.environ.get("CCB_GRADE_CACHE", "instance"))
            g = grader.grade(instance_id, raw["patch"])

        rates = rates_for(model)
        # ONE headline $ frame for every arm: the price table over the gateway
        # rows (the bottom-bridge truth). SDK-reported spend is a diagnostic.
        cb = price_run(raw["usage"], rates)
        lt = raw.get("ledger_totals")

        rec = RunRecord(
            instance=instance_id,
            arm=arm_name,
            success=bool(g.success),
            ftp=float(g.ftp),
            input_tokens=raw["input_tokens"],
            output_tokens=raw["output_tokens"],
            cache_write_tok=cb.cache_write_tok,
            cache_read_tok=cb.cache_read_tok,
            calls=raw["calls"],
            wall_s=raw["wall_s"],
            cost_usd=round(cb.total_usd, 6),
            requests=len(raw["usage"] or []),
            haiku_requests=sum(1 for _u in (raw["usage"] or [])
                               if "haiku" in str(_u.get("model") or "")),
            patch=raw["patch"],
            pass_to_pass_ok=(g.n_pass_to_pass_passed >= g.n_pass_to_pass),
            limit_death=bool(raw["limit_death"]),
            steps=raw["steps"],
            tool_calls=raw["tool_calls"],
            time_to_submit_s=raw["time_to_submit_s"],
            mean_call_latency_s=raw["mean_call_latency_s"],
            max_prompt_tokens=raw["max_prompt_tokens"],
            uncached_input_tokens=cb.uncached_input_tok,
            cache_hit_rate=round(cb.cache_hit_rate, 4),
            cost_usd_list=round(cb.list_usd, 6),
            reported_cost_usd=round(float(raw.get("reported_cost_usd", 0.0) or 0.0), 6),
            cache_write_usd=round(cb.write_usd, 6),
            cache_read_usd=round(cb.read_usd, 6),
            output_usd=round(cb.output_usd, 6),
            # §8.4 savings from the proxy ledger (probed rows only; null probes
            # excluded, never estimated). None/0 for ledger-less arms.
            counterfactual_input_tokens=(lt.counterfactual_input_tokens if lt else None),
            tokens_saved=(lt.tokens_saved if lt else None),
            probed_requests=(lt.probed_requests if lt else 0),
            null_probe_requests=(lt.null_probe_requests if lt else 0),
            proxy_fail_opens=(lt.fail_opens if lt else 0),
            scorer_fail_opens=(lt.scorer_fail_opens if lt else 0),
            checkpoint_id=(lt.checkpoint_ids[0] if lt and lt.checkpoint_ids else ""),
            retries=raw["retries"],
            degraded=bool(raw["degraded"]),
            model=model,
            exit_status=raw["exit_status"],
            usage=raw["usage"],
            infra_failed=False,
            error=("grade: " + g.error) if g.error else "",
        )
        run_record = rec.to_json()

        # Per-trace OUTCOME sidecar next to the SDK traj dump (ab_curator
        # schema: the trainer attaches a reward without re-grading).
        try:
            (traj_dir / f"{tag}.outcome.json").write_text(json.dumps(dict(
                success=bool(g.success), ftp=float(g.ftp),
                in_tok=raw["input_tokens"], out_tok=raw["output_tokens"],
                steps=raw["calls"], exit=raw["exit_status"])), encoding="utf-8")
        except Exception as e:  # noqa: BLE001
            print(f"  WARN: outcome sidecar write failed for {instance_id} [{arm_name}]: "
                  f"{type(e).__name__}: {str(e)[:160]}", flush=True)

        return run_record
    except RunInfraError as e:
        return _infra_stub(instance_id, arm_name, model, str(e), t0, getattr(e, "usage", None))
    except Exception as e:  # noqa: BLE001 — worker must never crash the pool
        return _infra_stub(instance_id, arm_name, model,
                           f"{type(e).__name__}: {str(e)[:200]}", t0)


def _infra_stub(instance_id: str, arm_name: str, model: str, err: str, t0: float,
                usage: Optional[list] = None) -> dict:
    """Price the failure from whatever the gateway actually logged (a wedged
    solve still spent real tokens); flagged infra_failed so it's excluded from
    success metrics without hiding the spend."""
    usage = usage or []
    it = sum((u.get("prompt_tokens", 0) or 0) for u in usage)
    ot = sum((u.get("completion_tokens", 0) or 0) for u in usage)
    cr = sum((u.get("cache_read_input_tokens", 0) or 0) for u in usage)
    cw = sum((u.get("cache_creation_input_tokens", 0) or 0) for u in usage)
    return RunRecord(
        instance=instance_id, arm=arm_name, success=False, ftp=0.0,
        input_tokens=it, output_tokens=ot, cache_write_tok=cw, cache_read_tok=cr,
        calls=len(usage), wall_s=round(time.time() - t0, 1), cost_usd=0.0,
        model=model, exit_status="infra_failed", infra_failed=True, error=err,
    ).to_json()


# ── resume ledger ──────────────────────────────────────────────────────────────
def _load_done(ledger: Path) -> set[tuple[str, str]]:
    """The set of (instance, arm) pairs already completed (non-infra)."""
    done: set[tuple[str, str]] = set()
    if not ledger.exists():
        return done
    for line in ledger.read_text(encoding="utf-8").splitlines():
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            continue  # truncated tail from a prior crash — skip
        if not r.get("infra_failed"):
            done.add((r["instance"], r["arm"]))
    return done


def _resolve_run_id(tasks: str, arms: list[str], run_id: Optional[str]) -> str:
    """A STABLE run id keying the trace tags (deterministic for resume)."""
    if run_id:
        return run_id.strip()
    stem = Path(tasks).stem
    arms_sorted = sorted(a.lower() for a in arms)
    key = f"{stem}|{','.join(arms_sorted)}"
    h = hashlib.sha1(key.encode("utf-8")).hexdigest()[:8]
    arms_slug = "-".join(arms_sorted)[:48]
    return f"{stem}__{arms_slug}__{h}"


# SEAM (cut): the reference rsync'd the traj dir to a GCS bus here
# (_rsync_traj / --bus / --sync-every). Local single-machine runs keep
# everything under --out; re-add from adaptive-context-clean/bench/cc_runner.py
# if a durable trace store is needed.


# ── arm readiness listing ─────────────────────────────────────────────────────
def list_arms() -> None:
    print("registered arms (env readiness):")
    for name in available_arms():
        arm = get_arm(name)
        ok, reason = arm.ready()
        flag = "READY" if ok else "SKIP "
        print(f"  [{flag}] {name:10s} kind={arm.kind.value:11s} {reason}")


# ── driver ─────────────────────────────────────────────────────────────────────
def main() -> None:
    ap = argparse.ArgumentParser(description="parsec-bench Claude Code runner")
    ap.add_argument("--tasks", default="tasks.json", help="task-set JSON path")
    ap.add_argument("--arms", default="baseline",
                    help="comma-separated arm names (default: baseline)")
    ap.add_argument("--workers", type=int, default=DEFAULT_WORKERS)
    ap.add_argument("--limit", type=int, default=0,
                    help="cap the number of instances (0 = all); smoke uses 1")
    ap.add_argument("--out", default="runs", help="output dir for the ledger + per-run JSON")
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--dataset", default=_GraderDefault("dataset"))
    ap.add_argument("--split", default=_GraderDefault("split"))
    ap.add_argument("--call-cap", type=int, default=CALL_CAP)
    ap.add_argument("--wall-cap-s", type=int, default=WALL_CAP_S)
    ap.add_argument("--grade-timeout-s", type=int, default=1800)
    ap.add_argument("--repo-root", default="",
                    help="host path whose <iid> subdirs hold each task repo the agent "
                         "edits (else AC_REPO_ROOT, else a stub dir under --out)")
    ap.add_argument("--run-id", default="",
                    help="stable run id keying the trace tag (default: a deterministic slug)")
    ap.add_argument("--list-arms", action="store_true", help="list arms + readiness and exit")
    a = ap.parse_args()

    if a.list_arms:
        list_arms()
        return

    arm_names = [x.strip() for x in a.arms.split(",") if x.strip()]
    ready_arms: list[str] = []
    for name in arm_names:
        try:
            arm = get_arm(name)
        except KeyError as e:
            print(f"  skip unknown arm: {e}")
            continue
        ok, reason = arm.ready()
        if ok:
            ready_arms.append(name)
        else:
            print(f"  skip arm '{name}': {reason}")
    if not ready_arms:
        print("no ready arms — nothing to run.")
        return

    instances = load_tasks(a.tasks)
    if a.limit:
        instances = instances[:a.limit]

    out_dir = Path(a.out)
    out_dir.mkdir(parents=True, exist_ok=True)
    runs_dir = out_dir / "runs"
    runs_dir.mkdir(exist_ok=True)
    traj_dir = out_dir / "traj"
    traj_dir.mkdir(exist_ok=True)
    ledger = out_dir / "ledger.jsonl"

    done = _load_done(ledger)
    print(f"resume: {len(done)} completed (instance, arm) pairs in {ledger}")

    run_id = _resolve_run_id(a.tasks, ready_arms, a.run_id)
    repo_root = (a.repo_root or "").strip()
    jobs = [
        (iid, arm, a.model, a.dataset, a.split, a.call_cap, a.wall_cap_s,
         a.grade_timeout_s, str(out_dir), run_id, repo_root)
        for iid in instances
        for arm in ready_arms
        if (iid, arm) not in done
    ]
    print(f"scheduling {len(jobs)} runs over {len(instances)} instances x "
          f"{len(ready_arms)} arms ({a.workers} workers)")
    if not jobs:
        print("nothing to do (all pairs already in the ledger).")
        return

    def log(msg: str) -> None:
        print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)

    retried: set[tuple[str, str]] = set()
    with ProcessPoolExecutor(max_workers=a.workers,
                             mp_context=mp.get_context("spawn")) as ex:
        futs = {ex.submit(_worker, j): j for j in jobs}
        while futs:
            for fut in as_completed(list(futs)):
                j = futs.pop(fut)
                iid, arm_name = j[0], j[1]
                try:
                    row = fut.result()
                except Exception as e:  # executor-level failure
                    row = _infra_stub(iid, arm_name, j[2],
                                      f"executor: {type(e).__name__}: {str(e)[:200]}",
                                      time.time())
                if row.get("infra_failed") and (iid, arm_name) not in retried:
                    retried.add((iid, arm_name))
                    log(f"  RETRY {iid} [{arm_name}] after infra failure: {row.get('error')}")
                    futs[ex.submit(_worker, j)] = j
                    continue
                (runs_dir / f"{iid}__{arm_name}.json").write_text(
                    json.dumps(row), encoding="utf-8")
                with ledger.open("a", encoding="utf-8") as f:
                    f.write(json.dumps(row) + "\n")
                if not row.get("infra_failed"):
                    done.add((iid, arm_name))
                    saved = row.get("tokens_saved")
                    log(f"  [{arm_name}] {iid}: success={row['success']} "
                        f"in={row['input_tokens']:,} calls={row['calls']} "
                        f"cost=${row['cost_usd']:.4f}"
                        + (f" saved={saved:,}tok" if saved is not None else "")
                        + f" ({row['exit_status']})")
                else:
                    log(f"  [{arm_name}] {iid}: infra_failed {row.get('error', '')[:80]}")
    log("BENCH_RUN_DONE")


def _GraderDefault(field: str) -> str:
    """Defer to the grader module's env-driven defaults for dataset/split."""
    from parsec_bench import grader as _g
    return _g.DEFAULT_DATASET if field == "dataset" else _g.DEFAULT_SPLIT


if __name__ == "__main__":
    main()
