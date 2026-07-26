"""REPLAY mode — the poor man's savings measurement (no Anthropic credits).

Replays a RECORDED conversation's request bodies, turn by turn, through a
locally-started arm proxy backed by the in-process mock upstream, and reports
what the proxy DID to each turn:

  * forwarded-size delta   — bytes of the ORIGINAL body vs the bytes the proxy
                             actually forwarded (from the mock's recording);
  * tool-prune delta       — tool schemas on the original request vs forwarded;
  * counterfactual-vs-served estimate — per the proxy's own savings ledger:
    the count_tokens probe on the ORIGINAL body vs the billed full input on the
    FORWARDED body. Against the mock upstream both sit on one chars/4 scale, so
    their difference is a consistent ESTIMATE of per-turn savings.

MEASUREMENT HONESTY (§8.4): this is a machinery measurement, clearly labeled
an estimate — the mock's count_tokens is chars/4, not a real tokenizer. The
WIRING is exactly the honest one (probe original, bill forwarded, null probes
excluded from savings); point the same proxy at api.anthropic.com and the
identical ledger math yields real-token savings.

Fixture shape: {"turns": [<full /v1/messages body>, ...]} (each body carries
model/system/messages/tools...; turn k is a prefix-extension of turn k-1, the
packages/proxy/parity/fixtures/golden_conversation.json shape). A bare JSON
list of bodies is also accepted.

Arms:
  baseline — the parsec binary WITHOUT a brain: passthrough-shaped curation
             (wire freeze + ledger machinery live, no cuts). Deltas ≈ 0.
  parsec   — the same binary with PARSEC_BRAIN_URL (+ dev-raw opt-in): the
             real-scorer path trims observations and prunes tools.

CLI:
  python -m parsec_bench.replay --fixture <recorded.json> [--brain-url URL]
"""

from __future__ import annotations

import argparse
import json
import tempfile
import urllib.request
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Optional

from parsec_bench import ledger as ledger_mod
from parsec_bench.mock_upstream import MockUpstream
from parsec_bench.proxy_bin import ProxyProcess, repo_root

DEFAULT_FIXTURE = str(repo_root() / "packages" / "proxy" / "parity" / "fixtures"
                      / "golden_conversation.json")


def load_fixture(path: str | Path) -> list[dict]:
    """The recorded turns: {"turns": [bodies...]} or a bare list of bodies."""
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if isinstance(data, dict):
        turns = data.get("turns")
    else:
        turns = data
    if not isinstance(turns, list) or not turns:
        raise ValueError(f"no turns in fixture {path} (want {{'turns': [bodies...]}})")
    bad = [i for i, t in enumerate(turns) if not isinstance(t, dict) or "messages" not in t]
    if bad:
        raise ValueError(f"fixture turns {bad} are not /v1/messages bodies")
    return turns


def _compact(body: dict) -> bytes:
    """Serialize a body the way it goes on the wire (compact, UTF-8)."""
    return json.dumps(body, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


@dataclass
class TurnReport:
    """What the proxy did to one replayed turn."""

    turn: int
    original_bytes: int
    forwarded_bytes: int
    forwarded_delta_bytes: int            # original − forwarded (positive = trimmed)
    original_est_tokens: int              # chars/4 of the original body
    forwarded_est_tokens: int             # chars/4 of the forwarded body
    tools_original: int
    tools_forwarded: int
    tools_pruned: int                     # original − forwarded
    counterfactual_input_tokens: Optional[int]   # ledger probe (None = probe failed)
    served_input_tokens: int              # billed full input off the ledger row
    est_tokens_saved: Optional[int]       # counterfactual − served; None on null probe
    fail_open: bool


@dataclass
class ReplayReport:
    """The whole replayed conversation."""

    fixture: str
    arm: str                              # "parsec" (brain wired) or "baseline"
    turns: list[TurnReport]
    # totals
    original_bytes: int = 0
    forwarded_bytes: int = 0
    tools_pruned_total: int = 0
    # §8.4: savings summed over PROBED turns only; null probes counted, excluded
    probed_turns: int = 0
    null_probe_turns: int = 0
    counterfactual_input_tokens: int = 0
    served_input_probed: int = 0
    est_tokens_saved: int = 0
    fail_opens: int = 0
    scorer_fail_opens: int = 0
    checkpoint_id: str = ""

    def to_json(self) -> dict:
        return asdict(self)


def _post(url: str, body: dict, timeout_s: float) -> None:
    raw = _compact(body)
    req = urllib.request.Request(
        url + "/v1/messages", data=raw, method="POST",
        headers={"content-type": "application/json", "x-api-key": "replay"})
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        resp.read()


def replay(fixture: str | Path, *, brain_url: Optional[str] = None,
           proxy_bin: Optional[str] = None, work_dir: Optional[str] = None,
           timeout_s: float = 120.0) -> ReplayReport:
    """Replay a recorded conversation through a fresh arm proxy + mock upstream.

    Starts everything on ephemeral ports, POSTs each turn body in order, then
    correlates (original body, forwarded body, ledger row) per turn — the
    proxy makes exactly one upstream /v1/messages call and writes exactly one
    ledger row per turn, in order.
    """
    turns = load_fixture(fixture)
    own_tmp = None
    if work_dir is None:
        own_tmp = tempfile.TemporaryDirectory(prefix="parsec_replay_")
        work_dir = own_tmp.name
    try:
        with MockUpstream() as mock:
            with ProxyProcess(upstream=mock.base_url, home_dir=work_dir,
                              brain_url=brain_url, bin_path=proxy_bin) as proxy:
                for body in turns:
                    _post(proxy.base_url, body, timeout_s)
                ledger_path = proxy.ledger_path
            forwarded = mock.messages()
        rows = ledger_mod.read_ledger(ledger_path)
    finally:
        if own_tmp is not None:
            own_tmp.cleanup()

    if len(forwarded) != len(turns):
        raise RuntimeError(f"replay saw {len(forwarded)} forwarded requests for "
                           f"{len(turns)} turns — proxy did not forward 1:1")
    if len(rows) != len(turns):
        raise RuntimeError(f"replay saw {len(rows)} ledger rows for "
                           f"{len(turns)} turns")

    rep = ReplayReport(fixture=str(fixture),
                       arm="parsec" if brain_url else "baseline", turns=[])
    for i, body in enumerate(turns):
        orig = _compact(body)
        fwd = forwarded[i].raw
        try:
            fwd_tools = forwarded[i].body().get("tools") or []
        except Exception:
            fwd_tools = []
        row = rows[i]
        cf = row.get("counterfactual_input_tokens")
        cf = int(cf) if cf is not None else None
        served = ledger_mod.served_input_tokens(row)
        tr = TurnReport(
            turn=i,
            original_bytes=len(orig),
            forwarded_bytes=len(fwd),
            forwarded_delta_bytes=len(orig) - len(fwd),
            original_est_tokens=len(orig) // 4,
            forwarded_est_tokens=len(fwd) // 4,
            tools_original=len(body.get("tools") or []),
            tools_forwarded=len(fwd_tools),
            tools_pruned=len(body.get("tools") or []) - len(fwd_tools),
            counterfactual_input_tokens=cf,
            served_input_tokens=served,
            est_tokens_saved=(cf - served) if cf is not None else None,
            fail_open=bool(row.get("fail_open")),
        )
        rep.turns.append(tr)
        rep.original_bytes += tr.original_bytes
        rep.forwarded_bytes += tr.forwarded_bytes
        rep.tools_pruned_total += tr.tools_pruned
        if cf is None:
            rep.null_probe_turns += 1
        else:
            rep.probed_turns += 1
            rep.counterfactual_input_tokens += cf
            rep.served_input_probed += served
            rep.est_tokens_saved += cf - served
        if tr.fail_open:
            rep.fail_opens += 1

    totals = ledger_mod.summarize(rows)
    rep.scorer_fail_opens = totals.scorer_fail_opens
    rep.checkpoint_id = totals.checkpoint_ids[0] if totals.checkpoint_ids else ""
    return rep


def _print_report(rep: ReplayReport) -> None:
    print(f"replay [{rep.arm}] {rep.fixture}")
    print(f"{'turn':>4} {'orig_B':>8} {'fwd_B':>8} {'ΔB':>7} "
          f"{'tools':>7} {'counterf':>9} {'served':>8} {'est_saved':>9}")
    for t in rep.turns:
        cf = t.counterfactual_input_tokens
        sv = t.est_tokens_saved
        print(f"{t.turn:>4} {t.original_bytes:>8} {t.forwarded_bytes:>8} "
              f"{t.forwarded_delta_bytes:>7} "
              f"{t.tools_original:>3}→{t.tools_forwarded:<3} "
              f"{cf if cf is not None else 'null':>9} {t.served_input_tokens:>8} "
              f"{sv if sv is not None else '—':>9}"
              f"{'  FAIL-OPEN' if t.fail_open else ''}")
    pct = (100.0 * rep.est_tokens_saved / rep.counterfactual_input_tokens
           if rep.counterfactual_input_tokens else 0.0)
    print(f"totals: bytes {rep.original_bytes}→{rep.forwarded_bytes}, "
          f"tools pruned {rep.tools_pruned_total}, "
          f"est saved {rep.est_tokens_saved} tok over {rep.probed_turns} probed "
          f"turn(s) ({pct:.1f}% of counterfactual; {rep.null_probe_turns} null "
          f"probe(s) EXCLUDED — §8.4)")
    if rep.arm == "baseline":
        print("note: baseline arm = passthrough machinery; deltas ≈ 0 by design")
    print("note: mock-upstream scale (chars/4) — an ESTIMATE, not billed tokens")


def main(argv: Optional[list] = None) -> int:
    ap = argparse.ArgumentParser(
        prog="parsec_bench.replay",
        description="Replay a recorded conversation through the arm proxy + "
                    "mock upstream; report per-turn savings estimates.")
    ap.add_argument("--fixture", default=DEFAULT_FIXTURE,
                    help=f"recorded conversation JSON (default {DEFAULT_FIXTURE})")
    ap.add_argument("--brain-url", default="",
                    help="brain URL -> replays the PARSEC arm (real-scorer "
                         "path); empty -> baseline (passthrough machinery)")
    ap.add_argument("--proxy-bin", default="",
                    help="parsec binary (default: $PARSEC_BIN / target/{release,debug})")
    ap.add_argument("--json", default="", help="also write the full report JSON here")
    a = ap.parse_args(argv)

    rep = replay(a.fixture, brain_url=a.brain_url or None,
                 proxy_bin=a.proxy_bin or None)
    _print_report(rep)
    if a.json:
        Path(a.json).write_text(json.dumps(rep.to_json(), indent=1), encoding="utf-8")
        print(f"report -> {a.json}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
