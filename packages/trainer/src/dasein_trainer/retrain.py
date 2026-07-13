"""Identity retrain pipeline — the trainer STUB for the inference-first v0.

The full retrain-loop SHAPE runs locally end to end (collect -> train ->
eval gate -> promote), but the train stage is an IDENTITY: it re-emits the
base GNN byte-for-byte, loudly labeled `trained: false`. That closes the
product loop (served traffic -> corpus -> trainer -> promoted bundle -> brain
reload) without a GPU, and pins the seams a real trainer drops into:

- collect(): counts what the capture seams have actually persisted (ledger
  rows today; tcv2 contracts once telemetry lands). A real trainer consumes
  these; the stub only reports them.
- train_identity(): the placeholder for the trace_graph/trace_train lineage
  (~17 min on 4xA100 per PRODUCTION_SERVING_HANDOFF.md). Returns the input.
- eval_gate(): REAL, not stubbed — the promoted candidate must pass the same
  self-validation the brain enforces at startup (dasein_brain.bundle) and
  must score a fixed conversation; the stub additionally requires exact
  score equality with the base (identity => anything else is corruption).
  A real trainer swaps the equality check for cov@70/AUC thresholds against
  a held split (the temporal firewall lives here).
- promote(): versioned, immutable bundle dir + manifest (checkpoint_id =
  sha256 of the .pt), which `DASEIN_CKPT` can point the brain at directly.

Nothing here may become a runtime dependency of serving (DIRECTION.md §7);
the brain never imports the trainer — the bundle directory is the interface.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from ._log import get_logger

_GRID = 1_000_000

log = get_logger("retrain")

# Internal-view conversation the eval gate scores (parse_internal shape:
# system entry, task user turn, assistant with bash-twin actions, observation).
_EVAL_MESSAGES: list[dict] = [
    {"role": "system", "content": "You are a coding agent. Be terse."},
    {"role": "user", "content": "Fix the failing quantize test in grid.py"},
    {
        "role": "assistant",
        "content": "Reading the module.",
        "extra": {"actions": [{"tool": "Read", "name": "Read", "command": "cat grid.py"}]},
    },
    {
        "role": "user",
        "content": "\n".join(
            f"{i}: q{i} = round(x{i} * SCALE_{i})  # grid line {i}" for i in range(1, 31)
        ),
    },
    {
        "role": "assistant",
        "content": "The rounding mode is wrong on line 12.",
        "extra": {"actions": [{"tool": "Grep", "name": "Grep", "command": "grep 'round' grid.py"}]},
    },
    {"role": "user", "content": "12: q12 = round(x12 * SCALE_12)  # grid line 12"},
]


@dataclass
class RetrainResult:
    bundle_dir: Path
    manifest: dict


def collect(corpus_dir: Path | None) -> dict:
    """Inventory what the capture seams have persisted. Tolerant of absence —
    the telemetry pipeline is deliberately unbuilt (§6 consent UX first)."""
    report = {"dir": str(corpus_dir) if corpus_dir else None, "ledger_rows": 0, "tcv2_rows": 0}
    if corpus_dir is None or not corpus_dir.exists():
        log.info("collect: no corpus dir (%s) — 0 rows", report["dir"])
        return report
    n_files = 0
    for p in sorted(corpus_dir.rglob("*.jsonl")):
        n_files += 1
        for line in p.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            v = str(row.get("contract_version", ""))
            if v.startswith("savings-ledger/"):
                report["ledger_rows"] += 1
            elif v.startswith("tcv2"):
                report["tcv2_rows"] += 1
    log.info("collect: dir=%s files_scanned=%d ledger_rows=%d tcv2_rows=%d",
             corpus_dir, n_files, report["ledger_rows"], report["tcv2_rows"])
    return report


def train_identity(base_ckpt: Path, work_dir: Path) -> Path:
    """The stub: emit the SAME GNN. No optimizer step ever runs here."""
    out = work_dir / "candidate.pt"
    work_dir.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(base_ckpt, out)
    log.info("train_identity: IDENTITY TRAINER — returning the base GNN unchanged "
             "(base=%s sha8=%s out=%s); no training was performed",
             base_ckpt.name, _sha256(base_ckpt)[:8], out)
    return out


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for blk in iter(lambda: f.read(1 << 20), b""):
            h.update(blk)
    return h.hexdigest()


def _score(ckpt: Path) -> dict:
    """Load through the brain's own self-validating loader and score the
    fixed eval conversation with deterministic hash embeddings."""
    os.environ.setdefault("DASEIN_EMBED_BACKEND", "hash")
    from dasein_brain.bundle import load_bundle
    from dasein_brain.scorer import TraceScorer

    bundle = load_bundle(ckpt_path=str(ckpt))
    scorer = TraceScorer(bundle)
    parsed = scorer.parse(_EVAL_MESSAGES)
    live_gi = list(range(len(parsed.chunks)))
    sc, tau, _ = scorer.score_trace(parsed, live_gi, mask=live_gi, conv_id="trainer-eval")
    return {
        "checkpoint_id": bundle.checkpoint_id,
        "scores_q": [int(round(float(s) * _GRID)) for s in sc],
        "tau_q": bundle.tau_q,
        "n_chunks": len(parsed.chunks),
    }


def eval_gate(candidate: Path, base: Path) -> dict:
    """The promotion gate. Bundle self-validation + a scoring pass are REAL;
    the identity stub's acceptance criterion is exact score equality with the
    base — a real trainer replaces that line with held-split thresholds
    (cov@70 / within-run AUC, eval-exclusion list enforced)."""
    cand = _score(candidate)
    ref = _score(base)
    identical = cand["scores_q"] == ref["scores_q"] and cand["tau_q"] == ref["tau_q"]
    max_delta = max((abs(a - b) for a, b in zip(cand["scores_q"], ref["scores_q"])), default=0)
    log.info("eval_gate: n_scores_compared=%d max_abs_delta_q=%d tau_q_equal=%s verdict=%s",
             len(cand["scores_q"]), max_delta, cand["tau_q"] == ref["tau_q"],
             "PASS" if identical else "FAIL")
    if not identical:
        raise RuntimeError(
            "eval gate FAILED: identity candidate does not reproduce base scores "
            f"(candidate {cand['checkpoint_id'][:12]} vs base {ref['checkpoint_id'][:12]})"
        )
    return {
        "passed": True,
        "criterion": "identity: exact score equality with base",
        "n_chunks_scored": cand["n_chunks"],
        "scores_q": cand["scores_q"],
        "tau_q": cand["tau_q"],
        "identical_to_base": identical,
    }


def promote(
    candidate: Path,
    out_dir: Path,
    version: str,
    base_ckpt: Path,
    corpus_report: dict,
    eval_report: dict,
    rules_path: Path | None = None,
    changeprone_path: Path | None = None,
) -> RetrainResult:
    """Write the versioned, immutable bundle dir the brain serves from
    (`DASEIN_CKPT=<bundle>/curator.pt`). Refuses to overwrite a version."""
    bundle_dir = out_dir / version
    if bundle_dir.exists():
        raise FileExistsError(f"bundle version already promoted: {bundle_dir}")
    bundle_dir.mkdir(parents=True)
    ckpt_out = bundle_dir / "curator.pt"
    shutil.copyfile(candidate, ckpt_out)

    models = Path(__file__).resolve().parents[3] / "brain" / "models"
    rules_src = rules_path or models / "rules.json"
    cp_src = changeprone_path or models / "changeprone.pkl"
    shutil.copyfile(rules_src, bundle_dir / "rules.json")
    if cp_src.exists():
        shutil.copyfile(cp_src, bundle_dir / "changeprone.pkl")

    manifest = {
        "schema": "trainer-bundle/v0",
        "version": version,
        "created": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "trained": False,
        "provenance": "identity-stub (dasein_trainer.retrain — no training performed)",
        "checkpoint_id": _sha256(ckpt_out),
        "base_checkpoint_id": _sha256(base_ckpt),
        "corpus": corpus_report,
        "eval": eval_report,
        "artifacts": {
            "ckpt": "curator.pt",
            "rules": "rules.json",
            "changeprone": "changeprone.pkl" if cp_src.exists() else None,
        },
    }
    (bundle_dir / "bundle.json").write_text(json.dumps(manifest, indent=1) + "\n")
    log.info("promote: version=%s bundle_dir=%s manifest=%s checkpoint_id=%s… trained=false "
             "artifacts=%s", version, bundle_dir, bundle_dir / "bundle.json",
             manifest["checkpoint_id"][:12],
             [v for v in manifest["artifacts"].values() if v])
    return RetrainResult(bundle_dir=bundle_dir, manifest=manifest)


def retrain(
    base_ckpt: Path,
    out_dir: Path,
    corpus_dir: Path | None = None,
    version: str | None = None,
) -> RetrainResult:
    def _stage(name, fn, *args, **kw):
        log.info("stage %s START", name)
        t0 = time.perf_counter()
        out = fn(*args, **kw)
        log.info("stage %s DONE wall_ms=%.1f", name, (time.perf_counter() - t0) * 1000.0)
        return out

    t_run0 = time.perf_counter()
    log.info("retrain START base_ckpt=%s out_dir=%s corpus=%s", base_ckpt, out_dir, corpus_dir)
    corpus_report = _stage("collect", collect, corpus_dir)
    work = out_dir / "_work"
    candidate = _stage("train_identity", train_identity, base_ckpt, work)
    eval_report = _stage("eval_gate", eval_gate, candidate, base_ckpt)
    version = version or f"identity-{_sha256(base_ckpt)[:12]}"
    result = _stage("promote", promote, candidate, out_dir, version, base_ckpt,
                    corpus_report, eval_report)
    shutil.rmtree(work, ignore_errors=True)
    log.info("retrain DONE version=%s bundle_dir=%s total_ms=%.1f",
             version, result.bundle_dir, (time.perf_counter() - t_run0) * 1000.0)
    return result


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "--base-ckpt",
        default=os.path.expanduser("~/.dasein/brain/curator_v4_prod.pt"),
        help="the GNN to 'retrain' (identity: also the output)",
    )
    ap.add_argument("--out", required=True, help="bundle output root")
    ap.add_argument("--corpus", default=None, help="captured-rows dir (inventory only in v0)")
    ap.add_argument("--version", default=None)
    args = ap.parse_args()
    result = retrain(
        Path(args.base_ckpt).expanduser(),
        Path(args.out),
        Path(args.corpus) if args.corpus else None,
        args.version,
    )
    print(json.dumps(result.manifest["eval"], indent=1))


if __name__ == "__main__":
    main()
