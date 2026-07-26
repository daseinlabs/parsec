"""TraceScorer — ContextCurator's SCORING half, exactly (reference: curator.py).

The reference ContextCurator mixes the decision engine (birth schedule, registries, insist
valve, render) with the scorer (_trace_scores: graph build + GNN forward + tau). The decision
engine is engine::freeze, ported client-side; THIS class is only the scorer:

  parse()               — curate()'s parse block (L893-966): messages -> step-sorted chunks +
                          task_text/recent_cmds/sys_text, the exact trainer chunk pipeline.
  _embed()              — the two-layer embed cache (curator L281-286): self.cache keyed by the
                          exact truncated text over EmbeddingClient's sha1 cache.
  _trace_scores()       — the trace-level GNN forward (L387-494), het path only (the ckpt is
                          hetgraph): _het_node_graph + _het_readout + score_decided, raw
                          sigmoid scores, calibrated global tau. Neighbors default OFF
                          (nf=None, +3 zero struct cols — valid: trained under 20% block
                          dropout); PARSEC_HOODS_PKL mounts the hoods artifact and attaches
                          the top-x=2 cross-trace blocks (assemble_trace order, no serve-time
                          dropout). Doom head scored only when the request carries gf (the
                          client-computed loop_feats 4-vector) — exposed as self.last_doom.
  score_request_tools() — the tool-schema head (L798-843) through the vendored
                          build_tool_spec + assemble_trace + collate_traces, nf=None,
                          RandomState(0); (None, None, None) = fail-open.
  score_request_rules() — the rule head (L845-890): candidate rules injected as
                          spec['rule_nodes'] at the fire step (nearest-earlier step-node clamp
                          in the vendored assemble_trace), AC_RULE=on scoped to assemble only;
                          {} = fail-open (ungated fire is the baseline).
  score_request_gate()  — the gate/brief head: attach_brief + brief_stats through the same
                          vendored assemble path (AC_GATE=on scoped to assemble only);
                          None = fail-open (serve the brief ungated).

NO decision schedule, NO registries, NO curate() render — the client owns those.
"""
from __future__ import annotations

from . import _flags  # noqa: F401  parity pins before any vendored import

import hashlib
import os
import time
from collections import OrderedDict
from dataclasses import dataclass, field

import numpy as np
import torch

from ._log import count_fail_open, get_logger
from .bundle import Bundle
from .vendored.chunking import chunk_assistant, chunk_observation, reasoning_chunk
from .vendored.embedding import EmbeddingClient
from .vendored.pyg_model import (attach_blocks, attach_hetero, attach_steps,
                                 attach_task, edges)
from .vendored.torch_curator import (decided_extra_feats, decided_rerank_feats,
                                     node_struct_with_type, struct_features,
                                     struct_type_features)

_EMBED_DIM = 1024          # bge-large / hash backend width; xe is the 3x tri-embedding
_NSCACHE_CONVS = 64        # per-conversation node-struct caches kept (LRU; byte-identical reuse)

log = get_logger("scorer")


def _text(m) -> str:
    c = m.get("content", "")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return " ".join(p.get("text", "") for p in c if isinstance(p, dict))
    return ""


def chunk_checksum(chunks) -> str:
    """The cross-language chunker-parity guard: sha256 over the (step, kind, tokens) triples of
    the parsed chunk array. The Rust client computes the same over ITS parse; a mismatch means
    the two chunkers diverged and scores would land on the wrong rows -> refuse (409)."""
    payload = "\n".join(f"{c.step}:{c.kind}:{c.tokens}" for c in chunks)
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


@dataclass
class ParsedTrace:
    """curate()'s parse products (curator L893-966 + het extras L924-932)."""
    chunks: list = field(default_factory=list)      # step-sorted (obs before reasoning per step)
    owner: list = field(default_factory=list)       # message index per chunk, permuted in lockstep
    cur_step: int = -1
    first_obs: int | None = None
    task_text: str = ""
    recent_cmds: str = ""
    sys_text: str = ""
    het_steps: list = field(default_factory=list)   # (cmd, obs_text) per step, for rerank feats
    T: int = 0


def parse_internal(messages: list[dict], read_lines: int) -> ParsedTrace:
    """Mirror of curate()'s parse block. `messages` is the internal flat view (to_internal):
    assistant messages carry extra.actions with bash-twin `command`s; the FIRST user/tool
    message is the task statement and is never chunked."""
    last_cmd, step, first_obs = "", 0, None
    obs_items, reason_items, asst_items = [], [], []
    for i, m in enumerate(messages):
        role = m.get("role")
        if role == "assistant":
            acts = m.get("extra", {}).get("actions", [])
            last_cmd = " ; ".join(a.get("command") or a.get("query") or "" for a in acts)
            rc = reasoning_chunk(m, step)
            if rc is not None:
                reason_items.append((i, rc))
            c0 = m.get("content")
            ok = isinstance(c0, str) or (isinstance(c0, list) and all(
                isinstance(p, dict) and p.get("type", "text") == "text" for p in c0))
            if ok:
                at = c0 if isinstance(c0, str) else " ".join(p.get("text", "") for p in c0)
                if at.strip():
                    asst_items.append((i, at, step))
        elif role in ("user", "tool"):
            if first_obs is None:
                first_obs = i
            else:
                obs_items.append((i, last_cmd, _text(m), step))
            step += 1
    cur_step = step - 1
    sys_text = next((_text(m) for m in messages
                     if m.get("role") == "system" and _text(m).strip()), "")[:2000]
    het_steps = [("", "")] * (cur_step + 1)
    for (_mi, _cmd, _txt, _st) in obs_items:
        if 0 <= _st <= cur_step:
            het_steps[_st] = (_cmd, _txt)
    chunks, owner = [], []
    for (mi, cmd, txt, st) in obs_items:
        for c in chunk_observation(cmd, txt, st, read_lines=read_lines):
            chunks.append(c); owner.append(mi)
    for (mi, at, st) in asst_items:               # obs-then-asst within a step: trainer parity
        if st <= cur_step:
            for c in chunk_assistant(at, st):
                chunks.append(c); owner.append(mi)
    for (mi, rc) in reason_items:
        chunks.append(rc); owner.append(mi)
    # NODE-ORDER PARITY with the trainer: stable sort by step, reasoning after obs within a
    # step — the temporal edge chain (edges links i->i+1) must match training topology.
    order = sorted(range(len(chunks)),
                   key=lambda i: (chunks[i].step, 1 if chunks[i].kind == "reasoning" else 0))
    chunks = [chunks[i] for i in order]
    owner = [owner[i] for i in order]
    task_text = _text(messages[first_obs])[:2000] if first_obs is not None else ""
    recent_cmds = " ".join(cmd for (_mi, cmd, _txt, st) in obs_items
                           if st >= cur_step - 1)[:4000]   # trainer parity: commands, not texts
    return ParsedTrace(chunks=chunks, owner=owner, cur_step=cur_step, first_obs=first_obs,
                       task_text=task_text, recent_cmds=recent_cmds, sys_text=sys_text,
                       het_steps=het_steps, T=cur_step + 1)


class TraceScorer:
    def __init__(self, bundle: Bundle):
        self.bundle = bundle
        self.model = bundle.model
        self.calib_tau = bundle.calib_tau
        self.read_struct = bundle.read_struct
        self.trace_chunk_lines = bundle.chunk_lines
        self._changeprone = bundle.changeprone
        backend = os.environ.get("PARSEC_EMBED_BACKEND", "dasein")
        cfg = {"models": {"embedder": {"dim": _EMBED_DIM, "backend": backend}}}
        self.embedder = EmbeddingClient(cfg=cfg, backend=backend)
        self.cache: dict[str, list[float]] = {}    # exact-text layer over the client's sha1 layer
        # per-CONVERSATION node-struct caches (curator._nstruct_cache is per-run): rows for steps
        # < cur_step are constant once their drops commit, so reuse across requests of one conv is
        # byte-identical; keys must not cross conversations (same _nskey, different causal prefix).
        self._nscaches: OrderedDict[str, dict] = OrderedDict()
        self._embed_ms = 0.0
        self._forward_ms = 0.0
        # cross-trace neighborhoods (nf): None = OFF (v0 default, +3 zero block-parity cols);
        # an ArtifactFetcher when PARSEC_HOODS_PKL is mounted. TRACE path only — the tool/rule/
        # gate heads assemble with nf=None even in the reference proxy.
        self.nf = bundle.hoods
        # per-call telemetry mirrors of the reference curator (read by app.py right after the
        # scoring call, under the app lock; pure functions of the request, not serving state):
        self.last_doom: float | None = None       # sigmoid(doom head) when gf rode the request
        self.last_nbr_blocks: int | None = None   # blocks attached this call (None = nf off)

    # ---- embeddings (curator L281-286) ----
    def _embed(self, texts):
        miss = [t for t in texts if t not in self.cache]
        if miss:
            t0 = time.perf_counter()
            vecs = self.embedder.embed(miss, as_query=False)
            self._embed_ms += (time.perf_counter() - t0) * 1000.0
            for t, v in zip(miss, vecs):
                self.cache[t] = v
        return [self.cache[t] for t in texts]

    # ---- parse (curate L893-966) ----
    def parse(self, messages: list[dict]) -> ParsedTrace:
        return parse_internal(messages, self.trace_chunk_lines)

    # ---- node-struct cache (curator._nskey + _node_struct_cached L292-355, sans rowrestrict) ----
    @staticmethod
    def _nskey(c) -> tuple:
        return (c.step, c.kind, c.file, c.lo, c.hi, hash(c.text or ""))

    def _nscache_for(self, conv_id: str) -> dict:
        if conv_id not in self._nscaches:
            self._nscaches[conv_id] = {}
            while len(self._nscaches) > _NSCACHE_CONVS:
                self._nscaches.popitem(last=False)
        self._nscaches.move_to_end(conv_id)
        return self._nscaches[conv_id]

    def _node_struct_cached(self, chunks, cache: dict | None) -> np.ndarray:
        """BIT-IDENTICAL, cached node_struct_with_type: a step-t row is cacheable only once
        t < cur_step (its drops are committed); the birth step always recomputes fresh."""
        if os.environ.get("AC_NSCACHE", "on") == "off" or cache is None or not chunks:
            return node_struct_with_type(chunks)
        keys = [self._nskey(c) for c in chunks]
        cur_step = max(c.step for c in chunks)
        out: list = [None] * len(chunks)
        for t in sorted({c.step for c in chunks}):
            idxs = [i for i in range(len(chunks)) if chunks[i].step == t]
            if t < cur_step and all(keys[i] in cache for i in idxs):
                for i in idxs:                            # STABLE (drops committed) + cached
                    out[i] = cache[keys[i]]
                continue
            prefix_idx = [i for i in range(len(chunks)) if chunks[i].step <= t]
            prefix = [chunks[i] for i in prefix_idx]      # current ALIVE prefix
            rows = np.hstack([struct_features(prefix, ""),
                              struct_type_features(prefix)]).astype(np.float32)
            prow = {id(prefix[r]): r for r in range(len(prefix))}
            for i in idxs:
                out[i] = rows[prow[id(chunks[i])]]
                if t < cur_step:                          # stable now -> safe to cache
                    cache[keys[i]] = out[i]
        return np.stack(out).astype(np.float32)

    # ---- trace-level GNN forward (curator._trace_scores L387-494, het path) ----
    def _trace_scores(self, chunks, task_text: str, cur_step: int, is_admission: float,
                      recent: str = "", mask_js: list[int] | None = None, sys_text: str = "",
                      het_steps: list | None = None, T: int = 0, nscache: dict | None = None,
                      gf: list[float] | None = None):
        n_own = len(chunks)
        zed = np.zeros(np.asarray(self._embed([chunks[0].text[:2000]])[0]).shape[0],
                       dtype=np.float32)
        emb = np.asarray(self._embed([c.text[:2000] for c in chunks]), dtype=np.float32)
        cmds = [getattr(c, "cmd", "") or "" for c in chunks]
        heads = [(getattr(c, "head", "") or "")[:240] for c in chunks]
        uniq = [t for t in dict.fromkeys(cmds + heads) if t]
        if uniq:
            self._embed(uniq)
        ace = np.asarray([self.cache[t] if t else zed for t in cmds], dtype=np.float32)
        hde = np.asarray([self.cache[t] if t else zed for t in heads], dtype=np.float32)
        cdim = emb.shape[1]
        node_struct = self._node_struct_cached(chunks, nscache)
        markers = np.stack([
            np.asarray([1.0 if c.kind == "reasoning" else 0.0 for c in chunks], np.float32),
            np.asarray([1.0 if (getattr(c, "head", "") or "") else 0.0 for c in chunks],
                       np.float32),
        ], axis=1)
        task_emb = (np.asarray(self._embed([task_text])[0], dtype=np.float32)
                    if task_text else zed)
        temb = np.hstack([task_emb, zed, zed]).astype(np.float32)
        decided = list(mask_js) if mask_js else list(range(n_own))
        age = 0.0 if is_admission >= 0.5 else 1.0
        xe, xs, ei, et = self._het_node_graph(chunks, emb, ace, hde, node_struct, markers,
                                              temb, task_text, cdim, zed, sys_text)
        dstruct = self._het_readout(chunks, decided, recent, task_text, age, emb, cur_step,
                                    het_steps or [], T)
        t0 = time.perf_counter()
        with torch.no_grad():
            h = self.model._embed_nodes(torch.from_numpy(xe), torch.from_numpy(xs), ei, et)
            didx = torch.tensor(decided, dtype=torch.long)
            u_dec = self.model.score_decided(h, didx, torch.from_numpy(dstruct))
            # DOOM head — rides the SAME forward (zero extra message passes). Scored only when
            # the request carried gf (the client-computed loop_feats trajectory) and the ckpt
            # has a doom head of that width. Pooled over the OWN live chunks only (reference
            # curator L479-484: aidx = arange(n_own) — never task/block/step/hub rows).
            if gf is not None and self.bundle.doom_gf and len(gf) == self.bundle.doom_gf:
                pooled = h[:n_own].mean(dim=0, keepdim=True)
                gft = torch.tensor(list(gf), dtype=torch.float32)
                self.last_doom = float(torch.sigmoid(
                    self.model.doom_head(self.model._doom_in(pooled, gft)).reshape(())))
        self._forward_ms += (time.perf_counter() - t0) * 1000.0
        # the calibrated global tau for AC_TARGET_COV OVERRIDES the budget head on every call of
        # a calibrated ckpt (curator L469-478); the bundle guarantees the table, so the free-
        # running budget-head tau (provenance only in the reference) is not computed here.
        tau = self.calib_tau
        # RAW sigmoid on the calibrated-tau path — calib_tau was fit on raw sigmoid(score_decided).
        di = np.asarray(decided, dtype=np.int64)
        probs = torch.sigmoid(u_dec).numpy()
        sc = np.full(n_own, 1.0, dtype=np.float32)        # non-decided default HIGH -> never cut
        sc[di] = probs
        return sc, tau

    def _het_node_graph(self, chunks, emb, ace, hde, node_struct, markers, temb, task_text,
                        cdim, zed, sys_text):
        """The TYPED hetgraph the tcv1 ckpt trains on (curator L496-543, BYTE-MIRROR of
        trace_train.assemble_trace HET). nf=None in v0 -> +3 zero block-parity struct cols."""
        n_own = len(chunks)
        xe = np.hstack([emb, ace, hde]).astype(np.float32)   # FULL tri-embedding
        xs = np.hstack([node_struct, markers]).astype(np.float32)
        ei, et = edges(chunks, emb, causal=True)
        ntype = ["observation"] * n_own
        xe, xs, ei, et = attach_task(xe, xs, ei, et, temb, causal=True)
        ntype.append("task")
        # NEIGHBOR BLOCKS between attach_task and attach_steps — assemble_trace's order is the
        # law (trace_train.py L61-78). NO dropout at serve (the rng branch is training-only);
        # the anchor query is the task content embedding this request already computed. nf=None
        # (or no task statement) keeps the +3 zero block-parity cols — bit-identical to v0.
        self.last_nbr_blocks = None
        if self.nf is not None and task_text:
            blocks = self.nf.blocks(temb[:cdim])         # cosine top-x over the artifact anchors
            _b0 = xe.shape[0]
            xe, xs, ei, et = attach_blocks(xe, xs, ei, et, blocks, list(range(n_own)),
                                           cdim, causal=True)
            ntype += ["observation"] * (xe.shape[0] - _b0)
            self.last_nbr_blocks = len(blocks)
            if not blocks:                               # measured, never silent (reference
                count_fail_open(log, "hoods live but ZERO neighbor blocks attached")   # _degrade)
            else:
                log.debug("neighbor blocks attached n_blocks=%d n_nodes=%d",
                          len(blocks), xe.shape[0] - _b0)
        else:
            xs = np.hstack([xs, np.zeros((xs.shape[0], 3), np.float32)])   # nf=None width parity
        _n0 = xe.shape[0]
        xe, xs, ei, et = attach_steps(xe, xs, ei, et, chunks, n_own)
        ntype += ["step"] * (xe.shape[0] - _n0)
        cen_by_file, cp_by_file = {}, {}
        # AC_HUBPROPS is pinned "off" (_flags): the file hubs carry ZERO props for this ckpt —
        # real props shift every score high -> under-evict (curator L521-527). Gate mirrored.
        if os.environ.get("AC_HUBPROPS", "on") != "off":
            reads = [c for c in chunks if c.kind in ("read", "grep") and c.file]
            if reads:
                from .vendored.symbol_graph import decided_centrality_feats
                cen = decided_centrality_feats(reads, chunks, task_text)
                for k, c in enumerate(reads):
                    cen_by_file.setdefault(c.file, np.asarray(cen[k], np.float32))
            if self._changeprone:
                _cc, _ct = self._changeprone
                for c in chunks:
                    if c.file:
                        cp_by_file[c.file] = _cc.get(c.file, 0) / _ct
        sys_emb = (np.asarray(self._embed([sys_text])[0], np.float32)
                   if sys_text else np.zeros(cdim, np.float32))
        xe, xs, ei, et = attach_hetero(xe, xs, ei, et, chunks, n_own, ace, hde,
                                       cen_by_file, cp_by_file, ntype, sys_emb=sys_emb)
        return xe, xs, ei, et

    def _het_readout(self, chunks, decided_idx, recent, task_text, age, emb, cur_step,
                     het_steps, T):
        """49-col readout struct (curator._het_readout L563-636): struct_features + age + extra
        + readmit + issuematch + rerank + centrality + changeprone + scout + dupcos, in
        trace_graph._readout's exact order."""
        from .vendored.symbol_graph import decided_centrality_feats
        from .vendored.trace_graph import (_DEFCLASS_RE, _chunk_file, _issue_idents,
                                           _tb_frame_files, scout_feats, scout_rec_for)
        decided = [chunks[j] for j in decided_idx]
        alive = chunks
        ds = struct_features(alive, recent)[np.asarray(decided_idx, np.int64)]
        ds = np.hstack([ds, np.full((len(ds), 1), float(age), np.float32)])
        ds = np.hstack([ds, decided_extra_feats(decided, alive, task_text)]).astype(np.float32)
        t = int(cur_step)
        T = int(T or (max((c.step for c in chunks), default=0) + 1))
        rm = np.zeros((len(decided), 4), np.float32)                              # AC_READMIT
        for k, c in enumerate(decided):
            rm[k, 0] = float(t); rm[k, 1] = float(t) / max(T - 1, 1)
            if c.file:
                nres = sum(1 for a in alive if a.step < c.step and a.file == c.file)
                rm[k, 2] = 1.0 if nres > 0 else 0.0
                rm[k, 3] = float(nres)
        ds = np.hstack([ds, rm]).astype(np.float32)
        if os.environ.get("AC_ISSUEMATCH") == "on":      # 5 cols — pinned on for this ckpt
            iss = _issue_idents(task_text)
            tb_files = _tb_frame_files(task_text)
            niss = max(len(iss), 1)
            im = np.zeros((len(decided), 5), np.float32)
            if iss:
                for k, c in enumerate(decided):
                    ctxt = c.text or ""
                    nmatch = sum(1 for w in iss if w in ctxt)
                    im[k, 0] = 1.0 if nmatch else 0.0
                    im[k, 1] = np.log1p(nmatch)
                    im[k, 2] = 1.0 if (set(_DEFCLASS_RE.findall(ctxt)) & iss) else 0.0
                    im[k, 3] = nmatch / niss
                    cf = _chunk_file(c)
                    im[k, 4] = 1.0 if (cf and cf in tb_files) else 0.0
            ds = np.hstack([ds, im]).astype(np.float32)
        ds = np.hstack([ds, decided_rerank_feats(decided, alive, het_steps or [], t)
                        ]).astype(np.float32)                                     # AC_RERANK
        ds = np.hstack([ds, decided_centrality_feats(decided, alive, task_text)
                        ]).astype(np.float32)                                     # AC_CENTRALITY
        # PARSEC-PATCH: ALWAYS emit the changeprone col (zeros when the sidecar is absent) — the
        # reference appends it only when the pickle loaded, and a 48-wide struct crashes the
        # read_head (384x433) matmul. Zeros are the trained missing-value convention.
        if self._changeprone:                                                     # AC_CHANGEPRONE
            _cc, _ct = self._changeprone
            col = np.array([[(_cc.get(c.file, 0) / _ct) if c.file else 0.0]
                            for c in decided], np.float32)
        else:
            col = np.zeros((len(decided), 1), np.float32)
        ds = np.hstack([ds, col]).astype(np.float32)
        # PARSEC-PATCH: scout block likewise always present — AC_SCOUTFEAT is pinned truthy ("on"
        # = no pickle path) so scout_feats returns zeros when no record; the env gate stays for
        # reference parity but the width no longer depends on it.
        if os.environ.get("AC_SCOUTFEAT"):                                        # 4 cols
            ds = np.hstack([ds, scout_feats(decided, scout_rec_for(task_text))]).astype(np.float32)
        else:
            ds = np.hstack([ds, np.zeros((len(decided), 4), np.float32)]).astype(np.float32)
        En = emb / (np.linalg.norm(emb, axis=1, keepdims=True) + 1e-8)            # AC_DUPCOS
        stp = np.asarray([c.step for c in chunks])
        f = np.zeros((len(decided), 2), np.float32)
        for k, j in enumerate(decided_idx):
            ej = np.where(stp < stp[j])[0]
            if len(ej):
                sims = En[ej] @ En[j]
                m = int(np.argmax(sims))
                f[k, 0] = float(sims[m]); f[k, 1] = float(1.0 / (1.0 + stp[j] - stp[ej[m]]))
        ds = np.hstack([ds, f]).astype(np.float32)
        if ds.shape[1] != self.read_struct:
            raise RuntimeError(f"readout struct width {ds.shape[1]} != ckpt read_struct "
                               f"{self.read_struct} — feature-flag drift")
        return ds

    # ---- request-level entry points ----
    def score_trace(self, parsed: ParsedTrace, live_gi: list[int], mask: list[int],
                    conv_id: str = "", gf: list[float] | None = None):
        """(scores over live rows, tau, timings_ms). mask rows carry real scores; the rest stay
        at the never-cut 1.0 default. When `gf` (the client-computed loop_feats 4-vector) rides
        in and the ckpt has a doom head, `self.last_doom` carries sigmoid(doom) after the call
        (same forward — the reference's curator.last_doom seam); None otherwise."""
        lc = [parsed.chunks[g] for g in live_gi]
        self._embed_ms = 0.0
        self._forward_ms = 0.0
        self.last_doom = None
        sc, tau = self._trace_scores(lc, parsed.task_text, parsed.cur_step, is_admission=1.0,
                                     recent=parsed.recent_cmds, mask_js=list(mask),
                                     sys_text=parsed.sys_text, het_steps=parsed.het_steps,
                                     T=parsed.T, nscache=self._nscache_for(conv_id), gf=gf)
        return sc, tau, {"embed": round(self._embed_ms, 3), "forward": round(self._forward_ms, 3)}

    def score_request_tools(self, messages: list[dict], tools: list[dict]):
        """TOOL-SCHEMA head at serve time (curator L798-843): (scores, tokens, names) aligned to
        the deduped roster, or (None, None, None) when ineligible/failed — the caller serves the
        full roster (fail-open, never break a tool call)."""
        if not (hasattr(self.model, "score_tools") and tools):
            return None, None, None
        try:
            from .vendored.trace_graph import build_tool_spec
            from .vendored.trace_train import assemble_trace, collate_traces
            spec = build_tool_spec(messages, tools, "serve")
            if spec is None or not spec.get("tool_nodes"):
                return None, None, None
            # embed every text assemble_trace will look up (chunk content/cmd/head + task + sys +
            # tool schemas); a missing cache entry silently becomes a zero vector = a wrong score.
            texts = []
            for c in spec["chunks"]:
                texts += [c.text[:2000], getattr(c, "cmd", "") or "",
                          (getattr(c, "head", "") or "")[:240]]
            texts.append(spec["task_text"])
            texts.append((spec.get("sys_text", "") or "")[:2000])
            texts += [tn["text"] for tn in spec["tool_nodes"]]
            uniq = [t for t in dict.fromkeys(texts) if t]
            if uniq:
                self._embed(uniq)
            dim = len(next(iter(self.cache.values())))
            d = assemble_trace(spec, self.cache, dim, None, np.random.RandomState(0))
            ti = d["tool_idx"]
            if not len(ti):
                return None, None, None
            xe, xs, ei, et, _ = collate_traces([d], "cpu")
            with torch.no_grad():
                h = self.model._embed_nodes(xe, xs, ei, et)
                sg = torch.sigmoid(self.model.score_tools(
                    h, torch.tensor(np.asarray(ti)))).cpu().numpy().astype(np.float32)
            names = [tn.get("name", "") for tn in spec["tool_nodes"]]
            return sg, np.asarray(d["tool_tok"], np.float32), names
        except Exception as e:
            # WARNING carries the exception TYPE only (an exception message can embed raw
            # request text — the data-plane rule applies to logs); full detail at DEBUG.
            log.debug("tool scoring exception detail", exc_info=True)
            count_fail_open(log, f"tool scoring failed ({type(e).__name__}) -> full tools served")
            return None, None, None

    def score_request_rules(self, messages: list[dict], tools: list[dict],
                            candidate_rules: list[dict], step: int) -> dict[str, float]:
        """RULE head at serve time (curator L845-890): fire-probability at `step` for each
        candidate rule = [{eid, text}], through the SAME vendored assemble path as the tool head
        so served score == trained score (within-run AUC 0.814). The rules ride in as
        spec['rule_nodes'] at the given fire step; a step with no step node (pure-reasoning turn,
        or a client step numbering past the chunk pipeline's) clamps to the NEAREST EARLIER step
        node inside the vendored assemble_trace. Returns {eid: raw sigmoid prob}; {} =
        ineligible/failed — the caller fails open (ungated fire is the baseline)."""
        if not (hasattr(self.model, "score_rules") and candidate_rules):
            return {}
        try:
            from .vendored.trace_graph import build_tool_spec
            from .vendored.trace_train import assemble_trace, collate_traces
            spec = build_tool_spec(messages, tools or [], "serve")
            if spec is None:
                return {}
            spec["rule_nodes"] = [{"eid": r["eid"], "text": r["text"], "step": int(step),
                                   "label": -1.0} for r in candidate_rules]
            texts = []
            for c in spec["chunks"]:
                texts += [c.text[:2000], getattr(c, "cmd", "") or "",
                          (getattr(c, "head", "") or "")[:240]]
            texts.append(spec["task_text"])
            texts.append((spec.get("sys_text", "") or "")[:2000])
            texts += [r["text"] for r in candidate_rules]     # untruncated: assemble's exact key
            uniq = [t for t in dict.fromkeys(texts) if t]
            if uniq:
                self._embed(uniq)
            dim = len(next(iter(self.cache.values())))
            # AC_RULE=on ONLY around assemble_trace: _flags pops any stray value at import (rule
            # nodes are a graph-shape change no other path may inherit); the scoped toggle is
            # what attaches spec['rule_nodes'], restored even on assemble failure.
            os.environ["AC_RULE"] = "on"
            try:
                d = assemble_trace(spec, self.cache, dim, None, np.random.RandomState(0))
            finally:
                os.environ.pop("AC_RULE", None)
            ri, rs = d["rule_idx"], d["rule_step"]
            if not len(ri):
                return {}
            xe, xs, ei, et, _ = collate_traces([d], "cpu")
            with torch.no_grad():
                h = self.model._embed_nodes(xe, xs, ei, et)
                sc = torch.sigmoid(self.model.score_rules(
                    h, torch.tensor(np.asarray(ri)),
                    torch.tensor(np.asarray(rs)))).cpu().numpy().astype(np.float32)
            return {candidate_rules[k]["eid"]: float(sc[k])
                    for k in range(min(len(sc), len(candidate_rules)))}
        except Exception as e:
            log.debug("rule scoring exception detail", exc_info=True)
            count_fail_open(log, f"rule scoring failed ({type(e).__name__}) -> fail-open")
            return {}

    def score_request_gate(self, messages: list[dict], tools: list[dict], brief_text: str,
                           gate_stats6: list[float] | None = None) -> float | None:
        """GATE/BRIEF head at serve time: serve-the-brief probability through the vendored
        AC_GATE assemble path (attach_brief + brief_stats — the scout-record scalars zero-fill at
        serve, the trained missing-value convention; brief-text scalars stay real). gate_stats6
        overrides the 6 conditioning scalars when the caller has real ones. Returns the raw
        sigmoid score, or None = ineligible/failed — the caller fails open (serve the brief
        ungated, the pre-gate baseline)."""
        if not (hasattr(self.model, "score_gate") and (brief_text or "").strip()):
            return None
        try:
            from .vendored.trace_graph import build_tool_spec
            from .vendored.trace_train import assemble_trace, collate_traces
            spec = build_tool_spec(messages, tools or [], "serve")
            if spec is None:
                return None
            spec["brief"] = brief_text
            texts = []
            for c in spec["chunks"]:
                texts += [c.text[:2000], getattr(c, "cmd", "") or "",
                          (getattr(c, "head", "") or "")[:240]]
            texts.append(spec["task_text"])
            texts.append((spec.get("sys_text", "") or "")[:2000])
            texts.append(brief_text[:2000])                   # assemble embeds brief[:2000]
            uniq = [t for t in dict.fromkeys(texts) if t]
            if uniq:
                self._embed(uniq)
            dim = len(next(iter(self.cache.values())))
            # AC_GATE=on scoped to assemble_trace, same pattern as AC_RULE above.
            os.environ["AC_GATE"] = "on"
            try:
                d = assemble_trace(spec, self.cache, dim, None, np.random.RandomState(0))
            finally:
                os.environ.pop("AC_GATE", None)
            gi = int(d["gate_idx"])
            if gi < 0:
                return None
            gstats = (np.asarray(gate_stats6, np.float32).reshape(1, 6)
                      if gate_stats6 is not None
                      else np.asarray(d["gate_stats"], np.float32).reshape(1, 6))
            xe, xs, ei, et, _ = collate_traces([d], "cpu")
            with torch.no_grad():
                h = self.model._embed_nodes(xe, xs, ei, et)
                sg = torch.sigmoid(self.model.score_gate(
                    h, torch.tensor([gi], dtype=torch.long), torch.from_numpy(gstats)))
            return float(sg.reshape(-1)[0])
        except Exception as e:
            log.debug("gate scoring exception detail", exc_info=True)
            count_fail_open(log, f"gate scoring failed ({type(e).__name__}) -> fail-open")
            return None
