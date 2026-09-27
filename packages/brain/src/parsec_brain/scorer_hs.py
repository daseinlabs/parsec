"""brain-api/v3 trace scorer for the HS curator (hidden-states encoder over the v13d graph).

Mirrors gnn-train hs/encgnn_train.py `assemble_trace` (HET path, nf=None) + the tau_report eval
forward (`encpool.build_tokens` -> `_embed_nodes_tok` -> `score_decided` -> sigmoid), sourced from the
v3 payload the way v1graph.build_graph mirrors the v1 dev path:

  node text/cmd/head      -> own-chunk node texts [text[:2000], cmd, head[:240]] (Qwen tokens)
                             + the bge content vector (kNN rel-2 edges, dupcos, empty-text fallback)
  node.struct (27)        -> node_struct_with_type(21) + rereq(6), client-computed (engine rereq.rs)
  read_columns (50)       -> computed HERE (vendored_hs.read_columns) from cmd/lo/hi/tokens/kind/step
                             and an extension stand-in file; appended after the 27 -> 77
  markers (2)             -> [kind == reasoning, head present]
  +3 zero cols            -> nf=None block-parity (AC_NEIGHBORS=0)
  steps / hetero hubs     -> vendored_hs attach_steps / attach_hetero; command hubs carry the real
                             cmd text, head hubs head[:240], file hubs no text, harness hub sys_text
  rel-4 supersession      -> the client's edges_supersession, spliced exactly as v1graph does
  decided_struct (104)    -> client row; the brain fills read columns 22-71 and dupcos 102-103

Scores are RAW sigmoid(score_decided) — the served-probability scale the per-kind taus
(hs_taus) are derived on. Non-mask rows return 1.0 (never cut), as on v1/v2.

Not reproduced (flagged, not silent): the training-time giant-trace windowing (WIN_BUDGET 130000);
the GATE/tool/rule/candidate nodes, which training appends LAST with inbound-only edges and which
therefore never change an own-chunk embedding.
"""
from __future__ import annotations

from . import _flags  # noqa: F401  edge-shape pins (AC_HETGRAPH/HUBPROPS/FILECHAIN/KNN_XFILE, STRICTEDGE/PRUNE unset)
from . import _flags_hs  # noqa: F401  HS construction pins (AC_PERX/AC_PERX_K/AC_HEAD)

import os
import time

import numpy as np
import torch

from ._log import get_logger
from .embed_cache import EmbedCache, embed_cache_max
from .vendored.embedding import EmbeddingClient
from .vendored_hs.pyg_model import (attach_hetero, attach_rules, attach_steps, attach_task,
                                    attach_tools, edges)
from .vendored_hs.read_columns import READCOLS_EXTS, READCOLS_WIDTH, read_columns
from .vendored_hs.rereq import rereq_features_base

log = get_logger("scorer_hs")

EMBED_DIM = 1024                  # bge-large content width == encoder hidden width (emb_dim)
NODE_STRUCT_IN = 27               # v3 wire: node_struct_with_type(21) + rereq(6)
READ_COLS = slice(22, 22 + READCOLS_WIDTH)   # decided cols 22-71
_OTHER_EXT_FILE = "x.parsec-other-ext"       # an extension outside READCOLS_EXTS -> the "other" column

_EXT_SET = frozenset(READCOLS_EXTS)


def ext_file(ext: str | None) -> str | None:
    """Stand-in basename whose extension class equals `ext` under read_columns' own parser.
    "none"/""/None -> no file (column `none`), a known extension -> x.<ext>, anything else -> the
    `other` column. Only read_columns ever sees this name — never the graph."""
    if not ext or ext == "none":
        return None
    if ext in _EXT_SET:
        return "x." + ext
    return _OTHER_EXT_FILE


class _GraphChunk:
    """Duck-typed chunk for the vendored graph builders (edges/attach_steps/attach_hetero).
    file = the opaque file_id (identity by equality, as v1graph); cmd/head = the REAL strings, so
    command/head hub identity AND hub text match training; text="" keeps edges()' span-Jaccard
    rel-4 block inert (its line-overlap residue is replaced by the client pairs)."""
    __slots__ = ("step", "kind", "file", "lo", "hi", "cmd", "head", "text")

    def __init__(self, n):
        self.step = n.step
        self.kind = n.kind
        self.file = n.file_id or None
        self.lo = n.lo
        self.hi = n.hi
        self.cmd = n.cmd or ""
        self.head = n.head or ""
        self.text = ""


class _ReadChunk:
    """Duck-typed chunk for read_columns: real cmd (tool/offset/limit parsing + (step, cmd)
    sibling groups), the extension stand-in file, and the client's token weight."""
    __slots__ = ("step", "kind", "file", "lo", "hi", "cmd", "tokens")

    def __init__(self, n):
        self.step = n.step
        self.kind = n.kind
        self.file = ext_file(n.ext)
        self.lo = n.lo
        self.hi = n.hi
        self.cmd = n.cmd or ""
        self.tokens = int(n.tokens)


def node_read_columns(nodes) -> np.ndarray:
    return read_columns([_ReadChunk(n) for n in nodes]).astype(np.float32)


def fill_dupcos(content: np.ndarray, steps, mask, dstruct: np.ndarray) -> np.ndarray:
    """Decided cols 102-103 (the last two): max cosine to an EARLIER-step live chunk + the recency
    of that match. assemble_trace's AC_DUPCOS block over r["alive"] == the live node set; same
    1e-8 epsilon and first-max argmax as app._v2_fill_dupcos."""
    en = content / (np.linalg.norm(content, axis=1, keepdims=True) + 1e-8)
    stp = np.asarray(steps)
    out = dstruct.copy()
    for k, j in enumerate(mask):
        ej = np.where(stp < stp[j])[0]
        if len(ej):
            sims = en[ej] @ en[j]
            m = int(np.argmax(sims))
            out[k, -2] = float(sims[m])
            out[k, -1] = float(1.0 / (1.0 + stp[j] - stp[ej[m]]))
        else:
            out[k, -2] = 0.0
            out[k, -1] = 0.0
    return out


def build_graph(nodes, content: np.ndarray, task_text: str, task_emb, sys_text: str, sys_emb,
                edges_supersession, rc: np.ndarray):
    """(xe fp32, xs fp32, ei, et, node_texts, n_own, step_rows). `content` = bge(text[:2000]) per
    node, `rc` = node_read_columns(nodes); step_rows maps step value -> its step-node row."""
    chunks = [_GraphChunk(n) for n in nodes]
    n_own = len(chunks)
    zed = np.zeros(EMBED_DIM, np.float32)
    emb = np.asarray(content, np.float32).reshape(n_own, EMBED_DIM)
    zeros = np.zeros_like(emb)
    # xe = [content | action | head]; only the content slot is ever read (base_vec for text-less
    # rows), so the action/head slots stay zero rather than embedding cmd/head with bge for nothing.
    xe = np.hstack([emb, zeros, zeros]).astype(np.float32)
    ntexts = [[(n.text or "")[:2000], (n.cmd or ""), ((n.head or "")[:240])] for n in nodes]
    node_struct = np.hstack([np.asarray([n.struct for n in nodes], np.float32).reshape(n_own, -1),
                             rc]).astype(np.float32)
    markers = np.stack([
        np.asarray([1.0 if c.kind == "reasoning" else 0.0 for c in chunks], np.float32),
        np.asarray([1.0 if c.head else 0.0 for c in chunks], np.float32),
    ], axis=1)
    xs = np.hstack([node_struct, markers]).astype(np.float32)
    ei, et = edges(chunks, emb, causal=True)
    if et.numel():
        keep = et != 4                     # client pairs replace rel-4 wholesale (v1graph)
        ei, et = ei[:, keep], et[keep]
    pairs = sorted((int(a), int(b)) for (a, b) in edges_supersession)
    if pairs:
        add = torch.tensor([[a for a, _ in pairs], [b for _, b in pairs]], dtype=torch.long)
        ei = torch.cat([ei, add], dim=1) if ei.numel() else add
        et = torch.cat([et, torch.full((len(pairs),), 4, dtype=torch.long)])
    ntype = ["observation"] * n_own
    temb = np.hstack([np.asarray(task_emb, np.float32).reshape(-1), zed, zed]).astype(np.float32)
    xe, xs, ei, et = attach_task(xe, xs, ei, et, temb, causal=True)
    ntype.append("task")
    ntexts.append([task_text or ""])
    xs = np.hstack([xs, np.zeros((xs.shape[0], 3), np.float32)])        # nf=None width parity
    _n0 = xe.shape[0]
    xe, xs, ei, et = attach_steps(xe, xs, ei, et, chunks, n_own)
    ntype += ["step"] * (xe.shape[0] - _n0)
    ntexts += [[] for _ in range(xe.shape[0] - _n0)]
    step_rows = {t: _n0 + k for k, t in enumerate(sorted({c.step for c in chunks}))}
    if os.environ.get("AC_HUBPROPS", "on") != "off":
        raise RuntimeError("AC_HUBPROPS != off: the HS run trained with file-hub props OFF")
    sysv = np.asarray(sys_emb, np.float32).reshape(-1) if sys_emb is not None else zed
    xe, xs, ei, et = attach_hetero(xe, xs, ei, et, chunks, n_own, zeros, zeros, {}, {}, ntype,
                                   sys_emb=sysv, ntexts=ntexts, sys_text=(sys_text or ""))
    if len(ntexts) != xe.shape[0]:
        raise RuntimeError(f"node_texts {len(ntexts)} != graph rows {xe.shape[0]}")
    return xe, xs, ei, et, ntexts, n_own, step_rows


class V2NodeView:
    """A brain-api/v2 node presented as a v3 node: struct = the client's 21 cols + the 6 rereq
    cols computed here (vendored rereq_features_base over the live set; cut cols 1/5 = 0 because
    v2 carries no cut history; file identity = the opaque file_id), ext = "none" (v2 sends no
    extension). An APPROXIMATION of the v3 input, used only for v2 requests to the HS brain."""
    __slots__ = ("text", "cmd", "head", "struct", "step", "kind", "tokens", "file_id", "lo",
                 "hi", "ext")

    def __init__(self, n, rereq_row):
        self.text, self.cmd, self.head = n.text, n.cmd, n.head
        self.struct = list(n.struct) + [float(x) for x in rereq_row]
        self.step, self.kind, self.tokens = n.step, n.kind, n.tokens
        self.file_id, self.lo, self.hi = n.file_id, n.lo, n.hi
        self.ext = "none"


class _RereqChunk:
    __slots__ = ("step", "file", "lo", "hi", "cmd")

    def __init__(self, n):
        self.step, self.file, self.lo, self.hi, self.cmd = (n.step, n.file_id or None, n.lo, n.hi,
                                                            n.cmd or "")


def adapt_v2_nodes(nodes) -> list[V2NodeView]:
    base, _ = rereq_features_base([_RereqChunk(n) for n in nodes])
    return [V2NodeView(n, base[i]) for i, n in enumerate(nodes)]


class HSTraceScorer:
    """One HS bundle + the bge embedder (edges/dupcos) + a device. Not thread-safe: the app
    serializes calls under its lock, as for TraceScorer."""

    def __init__(self, bundle, embedder=None):
        self.bundle = bundle
        self.model = bundle.model
        dev = os.environ.get("PARSEC_HS_DEVICE") or ("cuda" if torch.cuda.is_available() else "cpu")
        self.device = torch.device(dev)
        self.model.to(self.device)
        self.model.eval()
        if embedder is None:
            backend = os.environ.get("PARSEC_EMBED_BACKEND", "local")
            cfg = {"models": {"embedder": {"dim": EMBED_DIM, "backend": backend}}}
            embedder = EmbeddingClient(cfg=cfg, backend=backend, max_entries=embed_cache_max())
        self.embedder = embedder
        self.cache = EmbedCache(max_entries=embed_cache_max())
        log.info("HS scorer ready device=%s node_struct=%d read_struct=%d", self.device,
                 bundle.node_struct, bundle.read_struct)

    def embed(self, texts: list[str]) -> list:
        """bge vectors with the v2 convention: "" -> zero vector, never embedded."""
        nonempty = [t for t in texts if t]
        if nonempty:
            miss = self.cache.touch_batch(nonempty)
            if miss:
                vecs = self.embedder.embed(miss, as_query=False)
                self.cache.insert_batch(zip(miss, vecs), set(nonempty))
        zed = np.zeros(EMBED_DIM, np.float32)
        return [np.asarray(self.cache[t], np.float32) if t else zed for t in texts]

    def _graph(self, nodes, task_text, sys_text, edges_supersession):
        t0 = time.perf_counter()
        content = np.asarray(self.embed([(n.text or "")[:2000] for n in nodes]), np.float32)
        task_emb = self.embed([task_text or ""])[0]
        sys_emb = self.embed([sys_text or ""])[0]
        embed_ms = (time.perf_counter() - t0) * 1000.0
        rc = node_read_columns(nodes)
        g = build_graph(nodes, content.reshape(len(nodes), EMBED_DIM), task_text, task_emb,
                        sys_text or "", sys_emb, edges_supersession, rc)
        xs = g[1]
        if xs.shape[1] != self.bundle.node_struct:
            raise RuntimeError(f"node row width {xs.shape[1]} != ckpt node_struct "
                               f"{self.bundle.node_struct} — feature-flag drift")
        return content, rc, g, embed_ms

    def _embed_nodes(self, xe, xs, ei, et, ntexts):
        dev = self.device
        base_vec = torch.from_numpy(np.ascontiguousarray(xe[:, :EMBED_DIM])).to(dev).half()
        feat, nof = self.model.encpool.build_tokens(ntexts, base_vec)
        return self.model._embed_nodes_tok(feat.float(), nof, xe.shape[0],
                                           torch.from_numpy(xs).to(dev).float(),
                                           ei.to(dev), et.to(dev))

    def score_trace(self, nodes, task_text: str, sys_text: str | None, mask, decided_struct,
                    edges_supersession, gf=None):
        """(scores over nodes, timings_ms, doom). decided_struct rows arrive 104 wide with the
        read columns and dupcos zeroed; they are filled here before the head."""
        n_own = len(nodes)
        content, rc, (xe, xs, ei, et, ntexts, _, _), embed_ms = self._graph(
            nodes, task_text, sys_text, edges_supersession)
        mask = [int(j) for j in mask]
        ds = np.asarray(decided_struct, np.float32).reshape(len(mask), -1)
        if ds.shape[1] != self.bundle.read_struct:
            raise RuntimeError(f"decided row width {ds.shape[1]} != ckpt read_struct "
                               f"{self.bundle.read_struct}")
        if mask:
            ds[:, READ_COLS] = rc[mask]
            ds = fill_dupcos(content, [n.step for n in nodes], mask, ds)
        dev = self.device
        doom = None
        t1 = time.perf_counter()
        with torch.inference_mode():
            h = self._embed_nodes(xe, xs, ei, et, ntexts)
            probs = np.zeros(0, np.float32)
            if mask:
                didx = torch.tensor(mask, dtype=torch.long, device=dev)
                u = self.model.score_decided(h, didx, torch.from_numpy(ds).to(dev))
                probs = torch.sigmoid(u).float().cpu().numpy().reshape(-1)
            if gf is not None and self.bundle.doom_gf and len(gf) == self.bundle.doom_gf:
                pooled = h[:n_own].mean(dim=0, keepdim=True)
                gft = torch.tensor(list(gf), dtype=torch.float32, device=dev)
                doom = float(torch.sigmoid(
                    self.model.doom_head(self.model._doom_in(pooled, gft)).reshape(())))
        forward_ms = (time.perf_counter() - t1) * 1000.0
        sc = np.full(n_own, 1.0, dtype=np.float32)          # non-mask rows never cut
        if mask:
            sc[np.asarray(mask, dtype=np.int64)] = probs
        return sc, {"embed": round(embed_ms, 3), "forward": round(forward_ms, 3)}, doom

    def score_tools(self, nodes, task_text: str, sys_text: str | None, tool_texts: list[str]):
        """(scores aligned with tool_texts, forward_ms). assemble_trace's TOOL-SCHEMA NODES:
        attach_tools (task -> schema, rel 3, causal sinks) with each schema's text as its node
        text; no rel-4 pairs (the tools request carries none; they target obs rows only)."""
        _, _, (xe, xs, ei, et, ntexts, n_own, _), _ = self._graph(nodes, task_text, sys_text, [])
        tembs = np.zeros((len(tool_texts), EMBED_DIM), np.float32)   # content slot unread: text set
        xe, xs, ei, et, tool_idx = attach_tools(xe, xs, ei, et, tembs, EMBED_DIM, task_idx=n_own)
        ntexts = ntexts + [[t] for t in tool_texts]
        t0 = time.perf_counter()
        with torch.inference_mode():
            h = self._embed_nodes(xe, xs, ei, et, ntexts)
            sg = torch.sigmoid(self.model.score_tools(
                h, torch.tensor(np.asarray(tool_idx, np.int64), device=self.device)))
        return (sg.float().cpu().numpy().reshape(-1).astype(np.float32),
                round((time.perf_counter() - t0) * 1000.0, 3))

    def score_rules(self, nodes, task_text: str, sys_text: str | None, edges_supersession,
                    fire_step: int, rule_texts: list[str]):
        """(scores aligned with rule_texts, forward_ms). assemble_trace's RULE NODES: one node
        per rule edged from its fire STEP node (nearest-earlier clamp), scored by
        rule_head([h[rule] | h[step]]); the rule text is the node's encoded text."""
        _, _, (xe, xs, ei, et, ntexts, _, sidx), _ = self._graph(
            nodes, task_text, sys_text, edges_supersession)
        sk = sorted(sidx)
        le = [s for s in sk if s <= int(fire_step)]
        step_row = sidx[le[-1]] if le else sidx[sk[0]]
        step_rows = [step_row] * len(rule_texts)
        rembs = np.zeros((len(rule_texts), EMBED_DIM), np.float32)
        xe, xs, ei, et, rule_idx = attach_rules(xe, xs, ei, et, rembs, EMBED_DIM, step_rows)
        ntexts = ntexts + [[t] for t in rule_texts]
        t0 = time.perf_counter()
        with torch.inference_mode():
            h = self._embed_nodes(xe, xs, ei, et, ntexts)
            sc = torch.sigmoid(self.model.score_rules(
                h, torch.tensor(np.asarray(rule_idx, np.int64), device=self.device),
                torch.tensor(np.asarray(step_rows, np.int64), device=self.device)))
        return (sc.float().cpu().numpy().reshape(-1).astype(np.float32),
                round((time.perf_counter() - t0) * 1000.0, 3))
