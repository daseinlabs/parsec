"""Cross-trace neighborhoods — the serving port of the reference ArtifactFetcher
(adaptive-context-clean optimizer/neighbors.py), keyed on TASK VECTORS.

The reference fetcher keys on task TEXT (it owns an embed_fn) and excludes the
current instance id from the anchor search. At serve in the learner brain:

  - the DEV path embeds task_text through the request's embed backend and the
    V1 path carries the client-embedded task vector — so THIS class takes the
    1024-d task vector directly and never sees text;
  - there is NO self-iid exclusion: the current conversation is never in the
    artifact (anchors are past runs baked by build_hoods.py), so the
    reference's `a != iid` anti-leak filter has nothing to exclude at serve.

Artifact format (exact reference keys): pickle dict
  {"task_embs": {iid: 1024-d vec}, "blocks": {iid: (bemb (M,1024) with row 0
   the trace anchor, extras (M,3)=[is_nbr, needed, run_success], src, dst,
   typ)}, "cost": {iid: cumulative billed input tokens}}   ("cost" optional)

Env dials:
  PARSEC_HOODS_PKL    — artifact path; unset = neighbors OFF (nf=None, exactly
                        today's serving). Set-but-missing/corrupt = the caller
                        (bundle) REFUSES TO START — the reference's
                        train/serve-skew guard. No gsutil auto-fetch.
  PARSEC_NEIGHBORS    — runaway-baseline cost top-k (default 16; reference
                        AC_NEIGHBORS)
  PARSEC_NEIGHBORS_X  — GNN block top-x (default 2 — the TRAINED value; do
                        not change: reference `ArtifactFetcher.x`)
"""
from __future__ import annotations

import pickle
import statistics
from pathlib import Path

import numpy as np

from ._log import get_logger

_EMBED_DIM = 1024
log = get_logger("neighbors")


class HoodsError(RuntimeError):
    """Artifact missing/corrupt while PARSEC_HOODS_PKL is set — refuse to serve
    (silent divergence between training and serving neighborhoods is the bug
    class this kills; reference neighbors.py L49-51)."""


def _unit(v: np.ndarray) -> np.ndarray:
    return v / (np.linalg.norm(v) + 1e-8)                # reference normalization, verbatim


class ArtifactFetcher:
    """THE one neighborhood path: the versioned hoods artifact, vector-keyed."""

    def __init__(self, path: str | Path, x: int = 2, k: int = 16):
        self.x = int(x)                                  # GNN anchor count — trained value
        self.k = int(k)                                  # runaway cost sample top-k
        p = Path(path).expanduser()
        if not p.is_file():
            raise HoodsError(
                f"hoods artifact MISSING at {p} (PARSEC_HOODS_PKL is set) — refusing to "
                f"serve with ad-hoc neighborhoods (train/serve skew guard)")
        try:
            art = pickle.loads(p.read_bytes())
            task_embs = art["task_embs"]
            blocks = art["blocks"]
            cost = art.get("cost", {})
        except HoodsError:
            raise
        except Exception as e:
            raise HoodsError(f"hoods artifact unreadable at {p}: {type(e).__name__}: {e}")
        self.task_embs: dict[str, np.ndarray] = {}
        for iid, v in task_embs.items():
            vec = np.asarray(v, dtype=np.float32)
            if vec.shape != (_EMBED_DIM,):
                raise HoodsError(f"hoods anchor {iid!r}: task_emb shape {vec.shape} != "
                                 f"({_EMBED_DIM},)")
            self.task_embs[iid] = vec
        self._blocks: dict = {}
        n_block_nodes = 0
        # block-internal edge relations the artifact may carry: temporal(1) + membership(5).
        # attach_blocks (vendored pyg_model.py L48) splices the tuple's src/dst/typ VERBATIM
        # into the serving graph — rels 5 (anchor->task) and 6 (CF bridge) are appended by
        # attach_blocks itself, never read from the tuple — so a bad value would sail past
        # this loud loader and crash/corrupt inside EVERY trace forward instead (500 per
        # request until redeploy). Validate value RANGES at load, not just lengths.
        _BLOCK_RELS = {1, 5}
        for iid, b in blocks.items():
            try:
                bemb, extras, src, dst, typ = b
            except Exception:
                raise HoodsError(f"hoods block {iid!r}: not a (bemb, extras, src, dst, typ) tuple")
            bemb = np.asarray(bemb, dtype=np.float32)
            extras = np.asarray(extras, dtype=np.float32)
            if bemb.ndim != 2 or bemb.shape[1] != _EMBED_DIM:
                raise HoodsError(f"hoods block {iid!r}: bemb shape {bemb.shape} != (M, {_EMBED_DIM})")
            if extras.shape != (bemb.shape[0], 3):
                raise HoodsError(f"hoods block {iid!r}: extras shape {extras.shape} != "
                                 f"({bemb.shape[0]}, 3)")
            if not (len(src) == len(dst) == len(typ)):
                raise HoodsError(f"hoods block {iid!r}: ragged edge arrays")
            try:
                src_a = np.asarray(src, dtype=np.int64).reshape(-1)
                dst_a = np.asarray(dst, dtype=np.int64).reshape(-1)
                typ_a = np.asarray(typ, dtype=np.int64).reshape(-1)
            except (TypeError, ValueError) as e:
                raise HoodsError(f"hoods block {iid!r}: non-integer edge arrays ({e})")
            M = int(bemb.shape[0])
            if src_a.size:                               # empty edge arrays are legal
                lo = int(min(src_a.min(), dst_a.min()))
                hi = int(max(src_a.max(), dst_a.max()))
                if lo < 0 or hi >= M:
                    raise HoodsError(
                        f"hoods block {iid!r}: edge node index out of range "
                        f"(min={lo}, max={hi}, block has {M} nodes) — would crash "
                        f"attach_blocks on every trace forward")
                bad_rels = sorted(set(typ_a.tolist()) - _BLOCK_RELS)
                if bad_rels:
                    raise HoodsError(
                        f"hoods block {iid!r}: invalid edge relation(s) {bad_rels} — "
                        f"block-internal edges are temporal(1)/membership(5) only "
                        f"(anchor->task rel 5 and CF-bridge rel 6 are added at attach "
                        f"time, never baked into the artifact)")
            self._blocks[iid] = (bemb, extras, src, dst, typ)
            n_block_nodes += int(bemb.shape[0])
        self.cost: dict = {k_: float(v) for k_, v in cost.items()}
        log.info("hoods loaded path=%s anchors=%d blocks=%d block_nodes=%d costs=%d x=%d k=%d",
                 p, len(self.task_embs), len(self._blocks), n_block_nodes, len(self.cost),
                 self.x, self.k)

    # ---- anchor search (reference neighbor_iids, sans self-iid exclusion) ----
    def neighbor_iids(self, task_vec, k: int | None = None) -> list[str]:
        q = _unit(np.asarray(task_vec, dtype=np.float32))
        if not np.any(q):
            return []                                    # zero vector = no task statement
        scored = [(float(q @ _unit(v)), a) for a, v in self.task_embs.items()]
        # reference tie-break verbatim: sort (score, iid) tuples descending
        return [a for _s, a in sorted(scored, reverse=True)[:(k or self.x)]]

    def blocks(self, task_vec) -> list[tuple]:
        out = []
        for a in self.neighbor_iids(task_vec):
            if a in self._blocks:
                out.append(self._blocks[a])
            else:                                        # loud, never silent partiality
                log.warning("anchor %s has NO block in the artifact", a)
        return out

    def neighbor_costs(self, task_vec, k: int | None = None) -> list[float]:
        """Realized cost of the top-k nearest anchors that carry it. Own top-k
        (16) — decoupled from the GNN's trained x=2 (reference semantics)."""
        if not self.cost:
            return []
        top = self.neighbor_iids(task_vec, k=(k or self.k))
        return [float(self.cost[a]) for a in top if a in self.cost]

    def cost_median(self, task_vec) -> float | None:
        """MEDIAN of >=4 neighbor costs, else None (runaway stays inert —
        reference curator._neighbor_cost_median semantics)."""
        costs = self.neighbor_costs(task_vec)
        return float(statistics.median(costs)) if len(costs) >= 4 else None
