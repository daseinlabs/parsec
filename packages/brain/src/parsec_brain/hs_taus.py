"""Per-kind keep thresholds for the HS curator (brain-api/v3 `tau_q_by_kind`).

The HS serving rule is `keep = fought OR score >= tau[kind]` with one tau per chunk kind in
{read, other, grep, reasoning}; `asst` has no tau (never cut). The checkpoint's own
`tau_calib_kind` is on the raw-logit scale and is identification only — the handoff forbids serving
it. The served taus are derived on SERVED probabilities (sigmoid of score_decided, the scale
scorer_hs returns) and stored beside the checkpoint as JSON:

    {"coverage": 0.9, "tau": {"read": 0.4262, "other": 0.1769, "grep": 0.1935, "reasoning": 0.213},
     "checkpoint_md5": "...", "source": "...", "rows": {...}}

Derivation = gnn-train hs/encgnn_battery.py `tau_for_needed_cov`, applied within each kind: sort the
kind's held rows by score descending and take the score at which the token-weighted needed coverage
first reaches `coverage`.

    python -m parsec_brain.hs_taus derive <scores.npz> --coverage 0.90 --out <ckpt>.taus.json

The npz holds the held rows (`y`, `tok`, `fm`, `kinds`) and a score column (`sc_none` by default).
Scores must come from the scorer being shipped: the battery's `sc_none` is the training-side
serving-faithful scorer, so re-run with the brain's own scores before shipping (handoff §5.3).
"""
from __future__ import annotations

import argparse
import json
import os
from dataclasses import dataclass

import numpy as np

KINDS = ("read", "other", "grep", "reasoning")
GRID = 1_000_000


class TauError(RuntimeError):
    """Missing or malformed per-kind tau table — the HS brain must not serve without one."""


@dataclass(frozen=True)
class KindTaus:
    tau: dict[str, float]          # served-probability scale
    coverage: float
    path: str

    @property
    def tau_q(self) -> dict[str, int]:
        return {k: int(round(self.tau[k] * GRID)) for k in KINDS}


def tau_for_needed_cov(sc: np.ndarray, y: np.ndarray, tok: np.ndarray, target: float) -> float:
    """Verbatim logic of gnn-train hs/encgnn_battery.py tau_for_needed_cov (TOK/Y passed in)."""
    o = np.argsort(-sc); need = tok * (y > 0.5); tot_need = need.sum()
    cum = np.cumsum(need[o]) / max(tot_need, 1e-9)
    j = np.searchsorted(cum, target)
    j = min(j, len(o) - 1)
    return float(sc[o][j])


def derive(sc, y, tok, kinds, coverage: float) -> dict[str, float]:
    sc, y, tok, kinds = (np.asarray(sc, np.float64), np.asarray(y, np.float64),
                         np.asarray(tok, np.float64), np.asarray(kinds))
    out = {}
    for k in KINDS:
        m = kinds == k
        if not m.any():
            raise TauError(f"no held rows of kind {k!r}")
        out[k] = tau_for_needed_cov(sc[m], y[m], tok[m], coverage)
    return out


def report(sc, y, tok, fm, kinds, tau: dict[str, float], force_fought: bool = True) -> dict:
    """Token-weighted cut / needed-cov / fought-ret per kind and overall. force_fought=True is the
    served rule (fought rows always kept); False is the score-only view the handoff tables use."""
    sc, y, tok, fm, kinds = (np.asarray(sc, np.float64), np.asarray(y, np.float64),
                             np.asarray(tok, np.float64), np.asarray(fm, bool), np.asarray(kinds))
    thr = np.array([tau.get(k, -np.inf) for k in kinds])          # asst -> never cut
    keep = (sc >= thr) | (fm if force_fought else False)

    def stats(m):
        need = m & (y > 0.5)
        return dict(cut=float(tok[m & ~keep].sum() / max(tok[m].sum(), 1e-9)),
                    needed_cov=float(tok[need & keep].sum() / max(tok[need].sum(), 1e-9)),
                    fought_ret=float(tok[m & fm & keep].sum() / max(tok[m & fm].sum(), 1e-9)),
                    rows=int(m.sum()))
    rep = {k: stats(kinds == k) for k in KINDS}
    rep["overall"] = stats(np.ones(len(sc), bool))
    return rep


def default_path(ckpt_path: str) -> str:
    return ckpt_path + ".taus.json"


def load(ckpt_path: str) -> KindTaus:
    """PARSEC_HS_TAUS, else `<ckpt>.taus.json`. Refuses anything but four finite taus in (0,1)."""
    path = os.environ.get("PARSEC_HS_TAUS") or default_path(ckpt_path)
    try:
        with open(path) as f:
            d = json.load(f)
    except FileNotFoundError:
        raise TauError(f"per-kind tau table not found: {path} (derive it with "
                       f"`python -m parsec_brain.hs_taus derive`, or set PARSEC_HS_TAUS)") from None
    except (OSError, ValueError) as e:
        raise TauError(f"per-kind tau table unreadable: {path}: {e}") from None
    tau = d.get("tau") if isinstance(d, dict) else None
    if not isinstance(tau, dict) or set(tau) != set(KINDS):
        raise TauError(f"{path}: 'tau' must have exactly the kinds {list(KINDS)}")
    try:
        tau = {k: float(tau[k]) for k in KINDS}
        cov = float(d["coverage"])
    except (KeyError, TypeError, ValueError) as e:
        raise TauError(f"{path}: malformed ({e})") from None
    if not all(0.0 < v < 1.0 for v in tau.values()) or not 0.0 < cov <= 1.0:
        raise TauError(f"{path}: taus must be in (0,1) and coverage in (0,1]")
    return KindTaus(tau=tau, coverage=cov, path=path)


def _main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="python -m parsec_brain.hs_taus")
    sub = ap.add_subparsers(dest="cmd", required=True)
    d = sub.add_parser("derive", help="derive per-kind taus from held-row scores")
    d.add_argument("npz")
    d.add_argument("--coverage", type=float, default=0.90)
    d.add_argument("--score-key", default="sc_none")
    d.add_argument("--checkpoint-md5", default="")
    d.add_argument("--out", required=True)
    a = ap.parse_args(argv)
    z = np.load(a.npz, allow_pickle=True)
    y = z["y"] if "y" in z.files else z["Y"]
    sc, tok, fm, kinds = z[a.score_key], z["tok"], z["fm"], z["kinds"]
    tau = derive(sc, y, tok, kinds, a.coverage)
    doc = {"coverage": a.coverage, "tau": tau, "checkpoint_md5": a.checkpoint_md5,
           "source": f"{os.path.basename(a.npz)}:{a.score_key}",
           "report_served": report(sc, y, tok, fm, kinds, tau, force_fought=True),
           "report_score_only": report(sc, y, tok, fm, kinds, tau, force_fought=False)}
    with open(a.out, "w") as f:
        json.dump(doc, f, indent=2, sort_keys=True)
    print(json.dumps(doc, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
