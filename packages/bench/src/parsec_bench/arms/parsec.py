"""parsec — the local `parsec proxy` binary with the brain scorer (LocalProxyArm).

Adaptation of adaptive-context-clean/arms/parsec.py: the reference arm pointed
at a HOSTED service and shelled out to a harness-runner CLI for the v9 scout /
adjudicator hooks. This repo's product is the packages/proxy binary itself
(freeze + tool prune + savings ledger, scored by packages/brain), so the arm:

  * spawns `parsec proxy` per solve (start_run), with PARSEC_UPSTREAM pointed
    at the run's usage gateway — the chain is
        Claude Code -> parsec proxy -> gateway -> api.anthropic.com
    so the gateway still observes the REAL post-compression usage;
  * activates the scorer via PARSEC_BRAIN_URL (+ PARSEC_BRAIN_DEV_RAW=1, the
    v0 dev raw-text opt-in — our-machines-only posture, docs/brain-serving-v0.md);
  * isolates the savings ledger per solve by giving the proxy its own HOME
    under the run dir; ledger_path() hands it to the runner for the §8.4
    counterfactual accounting (parsec_bench.ledger).

FAITHFULNESS GATE (mirrors the reference ready() posture): without a
reachable brain the proxy silently degrades to passthrough-shaped curation —
the run would be LABELED "parsec" while measuring nothing. ready() therefore
requires PARSEC_BRAIN_URL set AND its /health answering, plus a resolvable
proxy binary; a missing piece SKIPs with a precise, actionable reason.

BENCHMARK CHECKPOINT WARNING: the brain this arm points at must serve
`curator_v4.pt` (the eval-excluded twin), NEVER `curator_v4_prod.pt` — the
prod checkpoint trained on the eval tasks and any benchmark number from it is
contaminated (rulehead PRODUCTION_SERVING_HANDOFF.md). ready() cannot see the
server's checkpoint file name, so it cross-checks /v1/bundle's checkpoint id
against PARSEC_BENCH_CKPT_SHA256 when the operator provides it, and always
RECORDS the id so contamination is auditable after the fact.

Env:
  PARSEC_BRAIN_URL          — the brain scoring service (required).
  PARSEC_BIN                — path to the parsec binary (else target/release,
                              target/debug, else cargo build).
  PARSEC_BENCH_CKPT_SHA256  — optional expected checkpoint sha256; ready()
                              fails on a mismatch (contamination guard).
  PARSEC_BRAIN_KEY / PARSEC_BRAIN_TIMEOUT_MS / PARSEC_TARGET_COV /
  PARSEC_TOOL_PRUNE / PARSEC_TOOL_CUT — forwarded to the proxy verbatim.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Optional

from parsec_bench.arm import LocalProxyArm, RunContext, register
from parsec_bench.proxy_bin import ProxyProcess, resolve_proxy_bin


def _get_json(url: str, timeout_s: float = 5.0) -> Optional[dict]:
    try:
        with urllib.request.urlopen(url, timeout=timeout_s) as r:
            obj = json.loads(r.read().decode("utf-8", "replace"))
            return obj if isinstance(obj, dict) else None
    except Exception:
        return None


@register("parsec")
class ParsecArm(LocalProxyArm):
    name = "parsec"
    needs = ["PARSEC_BRAIN_URL"]

    def __init__(self) -> None:
        self._proc: Optional[ProxyProcess] = None
        self._last_ledger: Optional[str] = None
        self.checkpoint_id: str = ""

    # ── readiness gate ────────────────────────────────────────────────────────
    def ready(self) -> tuple[bool, str]:
        ok, reason = super().ready()
        if not ok:
            return ok, reason
        try:
            bin_path = resolve_proxy_bin(build=False)
        except FileNotFoundError as e:
            return False, str(e)
        brain = os.environ.get("PARSEC_BRAIN_URL", "").rstrip("/")
        health = _get_json(brain + "/health")
        if health is None:
            return False, (
                f"brain at {brain} did not answer /health — without it the proxy "
                f"fail-opens to passthrough and the run would be mislabeled "
                f"'parsec'. Start packages/brain (or kubectl port-forward "
                f"svc/dasein-brain) and retry.")
        bundle = _get_json(brain + "/v1/bundle") or {}
        self.checkpoint_id = str(bundle.get("checkpoint_id") or "")
        want = os.environ.get("PARSEC_BENCH_CKPT_SHA256", "").strip().lower()
        if want and self.checkpoint_id and self.checkpoint_id.lower() != want:
            return False, (
                f"brain checkpoint_id {self.checkpoint_id[:12]}… != expected "
                f"PARSEC_BENCH_CKPT_SHA256 {want[:12]}… — refusing: a benchmark "
                f"against the wrong bundle (e.g. curator_v4_prod.pt, which "
                f"trained on the eval tasks) is contaminated. Serve the "
                f"eval-excluded curator_v4.pt for benchmarks.")
        return True, (f"ok (bin={bin_path}, brain={brain}, "
                      f"ckpt={self.checkpoint_id[:12] or 'unknown'}…)")

    # ── per-solve lifecycle ───────────────────────────────────────────────────
    def start_run(self, ctx: RunContext) -> Optional[str]:
        home = os.path.join(ctx.run_dir, "proxy_home")
        self._proc = ProxyProcess(
            upstream=ctx.upstream_base_url,
            home_dir=home,
            brain_url=os.environ.get("PARSEC_BRAIN_URL"),
        ).start()
        self._last_ledger = self._proc.ledger_path
        return self._proc.base_url

    def end_run(self) -> None:
        proc, self._proc = self._proc, None
        if proc is not None:
            proc.stop()

    def ledger_path(self) -> Optional[str]:
        # survives end_run so the runner can read the ledger after teardown
        return self._last_ledger
