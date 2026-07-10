"""The Arm adapter contract + registry.

Port of adaptive-context-clean/bench/arm.py, adapted to the dasein topology.

An "arm" is one compression strategy. Every arm runs the SAME fixed agent
scaffold (headless Claude Code) against the SAME upstream; the only thing that
varies is HOW the prompt is compressed before it reaches the model. Patterns:

  baseline      — no-op control: Claude Code points straight at the run's
                  usage gateway (which forwards verbatim to the upstream).

  LocalProxyArm — the runner asks the arm to START a local proxy per solve
                  (`start_run`), pointed at the run's gateway as its upstream;
                  Claude Code's ANTHROPIC_BASE_URL is the proxy. The arm owns
                  the process lifecycle and exposes where its savings ledger
                  landed (`ledger_path`). This is the dasein arm's shape: the
                  proxy is driven as a BLACK BOX binary (bench -> proxy
                  dependency direction; bench never links proxy internals).

  ProxyArm      — a pre-provisioned Anthropic-speaking endpoint at a FIXED URL
                  (the reference's hosted-vendor pattern). Claude Code points
                  at it; its upstream must be provisioned to the gateway.

CUT vs the reference (clearly-marked seam): ToolArm/ToolAttach (MCP tool
servers, Claude Code plugin loading — the reference's woz arm). Nothing in
this repo needs them; see adaptive-context-clean/bench/arm.py to reintroduce.

The optional harness-level hooks (step0_injection / pre_tool_hook /
stop_decision) are kept: they are how an arm participates in the agent loop
without editing the runner's core.

Concrete arm classes live in `dasein_bench.arms` (one module per arm); they
register via @register. This module is pure stdlib and has NO knowledge of any
specific arm beyond the built-in baseline control.
"""

from __future__ import annotations

import abc
import os
from dataclasses import dataclass
from enum import Enum
from typing import Callable, Optional


# ── message type (the chat-message shape the scaffold passes) ────────────────
Message = dict
Messages = list[Message]


# ── stop-decision return type (the loop-stop hook contract) ──────────────────
@dataclass
class StopDecision:
    """An arm's verdict when the agent tries to end its turn loop.

      finalize=True   — let the loop END (the agent's work is accepted).
      finalize=False  — KEEP the loop going; the runner's Stop hook BLOCKS the
                        stop and feeds ``directive`` back to the agent.

    Returning ``None`` from ``stop_decision`` (the default) means the arm
    abstains and the loop stops normally.
    """

    finalize: bool
    directive: Optional[str] = None


# A PreToolUse rewrite/observation result: {"tool_input": {...}} to REWRITE the
# call's input, or None to leave the call untouched (pure observation).
PreToolResult = Optional[dict]


class ArmKind(str, Enum):
    """Which adapter pattern an arm implements (selects the runner's wiring)."""

    BASELINE = "baseline"          # no-op: endpoint passes through unchanged
    PROXY = "proxy"                # a pre-provisioned proxy at a fixed URL
    LOCAL_PROXY = "local_proxy"    # the arm spawns a local proxy per solve


@dataclass
class RunContext:
    """What the runner hands a LocalProxyArm when a solve starts.

    upstream_base_url : the run's gateway URL — the proxy MUST forward here so
                        the gateway (the bottom bridge) observes the real
                        post-compression usage.
    run_dir           : per-solve scratch dir the arm may own (e.g. the proxy's
                        HOME, so its ledger is isolated per run).
    run_id            : the per-(instance, arm) run id; Claude Code forwards it
                        as the x-ccb-run-id header on every request.
    """

    upstream_base_url: str
    run_dir: str
    run_id: str


class Arm(abc.ABC):
    """Base adapter every arm subclasses.

    Identity / capability surface every arm MUST expose:
      name  : str        — registry key (e.g. "baseline", "dasein")
      kind  : ArmKind    — which adapter pattern (selects runner wiring)
      needs : list[str]  — env var names this arm requires to run
    """

    name: str = "arm"
    kind: ArmKind = ArmKind.BASELINE
    needs: list[str] = []

    def ready(self) -> tuple[bool, str]:
        """Whether this arm can run now. Returns (ok, reason).

        Default check: every env var in `needs` is present and non-empty.
        Subclasses override for richer checks (e.g. ping the brain).
        """
        missing = [k for k in self.needs if not os.environ.get(k)]
        if missing:
            return False, f"missing env: {', '.join(missing)}"
        return True, "ok"

    def setup(self) -> None:
        """Optional one-time prep before a batch of runs. No-op by default."""

    def teardown(self) -> None:
        """Optional cleanup after a batch of runs. No-op by default."""

    # ── per-solve lifecycle (LocalProxyArm pattern; no-ops otherwise) ─────────
    def start_run(self, ctx: RunContext) -> Optional[str]:
        """Called when a solve starts. Return the base URL Claude Code should
        point ANTHROPIC_BASE_URL at, or None for gateway-direct (baseline)."""
        return None

    def end_run(self) -> None:
        """Called when the solve ends (success or failure). Tear down whatever
        start_run started. Must never raise."""

    def ledger_path(self) -> Optional[str]:
        """Where THIS solve's proxy savings ledger landed (the §8.4 source),
        or None for arms with no proxy ledger (baseline)."""
        return None

    # ── OPTIONAL harness-level hooks (default no-ops) ─────────────────────────
    # These let an arm declare behaviour the Claude Agent SDK supports at the
    # harness level — a turn-0 system-prompt injection, a PreToolUse rewrite,
    # and a loop-stop decision — WITHOUT editing the runner's core.

    def step0_injection(self, instance: dict, repo_dir: str) -> Optional[str]:
        """Text appended to Claude Code's system prompt for this solve (a
        cold-retrieval brief). None (default) adds nothing."""
        return None

    def pre_tool_hook(self, tool_name: str, tool_input: dict) -> PreToolResult:
        """Observe or REWRITE a tool call before it runs. Return
        {"tool_input": {...}} to rewrite, None to leave untouched. The runner
        always ALLOWS the (possibly rewritten) call — routes, never blocks."""
        return None

    def stop_decision(self, transcript_state: dict) -> Optional[StopDecision]:
        """Consulted when the agent tries to stop. None (default) abstains.
        The runner guards against infinite continuation (stop_hook_active)."""
        return None

    def has_harness_hooks(self) -> bool:
        """True iff at least one optional hook is overridden away from the
        base no-op. Arms never need to override this."""
        return any(
            getattr(type(self), name) is not getattr(Arm, name)
            for name in ("step0_injection", "pre_tool_hook", "stop_decision")
        )


# ── pre-provisioned proxy at a fixed URL (reference hosted-vendor pattern) ────
class ProxyArm(Arm):
    """Routes the model call through a pre-provisioned Anthropic endpoint.

    The runner points Claude Code's ANTHROPIC_BASE_URL at model_base_url().
    The endpoint's UPSTREAM must be provisioned to the run gateway so the
    bottom bridge still observes the real usage.
    """

    kind = ArmKind.PROXY

    @abc.abstractmethod
    def model_base_url(self) -> str:
        """The Anthropic-speaking base URL the runner points Claude Code at."""
        raise NotImplementedError


# ── local proxy spawned per solve (the dasein pattern) ────────────────────────
class LocalProxyArm(Arm):
    """The arm spawns and owns a LOCAL proxy process per solve.

    Subclasses implement start_run/end_run/ledger_path; the runner treats the
    proxy as a black box reachable at the returned base URL.
    """

    kind = ArmKind.LOCAL_PROXY


# ── no-op baseline (the control) ─────────────────────────────────────────────
class BaselineArm(Arm):
    """The control arm: no compression. Claude Code points straight at the
    run's gateway. Always ready (needs nothing)."""

    name = "baseline"
    kind = ArmKind.BASELINE
    needs: list[str] = []


# ── registry ──────────────────────────────────────────────────────────────
_REGISTRY: dict[str, Callable[[], Arm]] = {}


def register(name: str) -> Callable[[Callable[[], Arm]], Callable[[], Arm]]:
    """Class/factory decorator: register an Arm factory under `name`."""

    def deco(factory: Callable[[], Arm]) -> Callable[[], Arm]:
        key = name.lower()
        if key in _REGISTRY:
            raise ValueError(f"arm already registered: {name}")
        _REGISTRY[key] = factory
        return factory

    return deco


def get_arm(name: str) -> Arm:
    """Instantiate the registered arm by name. Raises KeyError if unknown."""
    key = name.lower()
    if key not in _REGISTRY:
        raise KeyError(f"unknown arm '{name}'. registered: {available_arms()}")
    return _REGISTRY[key]()


def available_arms() -> list[str]:
    """Sorted list of registered arm names."""
    return sorted(_REGISTRY)


# register the built-in control
register("baseline")(BaselineArm)
