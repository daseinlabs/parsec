"""brain-api/v1 graph builder + scorers — the dev scorer's graph, sourced from the PAYLOAD.

The v1 contract (contracts/schemas/brain-api.schema.json) carries vectors + structural
features + opaque ids; NO raw text is representable, so nothing here may embed, chunk or
read text. The trained topology is the law (specs graph-construction.md §1-2): this module
mirrors TraceScorer._het_node_graph / trace_train.assemble_trace EXACTLY, substituting
payload data for everything the dev path derives from text:

  chunk text        -> node.emb_text  (client-embedded, 1024)
  command text      -> node.emb_cmd + node.cmd_id (opaque id: hub identity by equality)
  observation head  -> node.emb_head + node.head_id (presence gates the head hub + marker)
  file basename     -> node.file_id (opaque id: rel-0 chain / kNN cross-file / file hub)
  node_struct(21)   -> node.struct (client-computed node_struct_with_type row)
  task/sys text     -> task_emb / sys_emb
  rel-4 supersession-> edges_supersession (client-computed — the ONE text-dependent relation)
  49-col readout    -> decided_struct rows (client-computed where the text lives)

REUSE RULE: everything that does not need text runs through the VENDORED functions
(pyg_model.edges/attach_task/attach_steps/attach_hetero/attach_tools) over duck-typed
_V1Chunk stand-ins — verified line-by-line to touch only .step (ordering/equality),
.kind (marker), .file/.cmd/.head (truthiness + equality; opaque ids substitute exactly),
.lo/.hi (integer ordering). The single vendored text dependency is edges()'s rel-4
span-Jaccard block; the stand-ins expose text="" so that block is inert and its residual
line-overlap emissions are dropped and replaced by the payload pairs (see build_graph).
"""
from __future__ import annotations

from . import _flags  # noqa: F401  parity pins before any vendored import

import os
import time

import numpy as np
import torch

from ._log import count_fail_open, get_logger
from .vendored.pyg_model import (attach_blocks, attach_hetero, attach_rules,
                                 attach_steps, attach_task, attach_tools, edges)

EMBED_DIM = 1024      # bge-large content-embedding width; xe = 3x tri-embedding

log = get_logger("v1graph")


class _V1Chunk:
    """Duck-typed stand-in for chunking.Chunk as consumed by the vendored graph builders.

    text="" is deliberate: the ONLY vendored consumer of .text on this path is edges()'s
    rel-4 supersession block (attribution.spans), whose output is discarded — v1 takes
    rel-4 from the payload verbatim. Nothing on the v1 path can see raw text."""
    __slots__ = ("step", "kind", "file", "lo", "hi", "cmd", "head", "text")

    def __init__(self, step, kind, file_id, lo, hi, cmd_id, head_id):
        self.step = step
        self.kind = kind
        self.file = file_id or None      # None/empty -> fileless: no rel-0 chain, no file hub
        self.lo = lo
        self.hi = hi
        self.cmd = cmd_id or ""          # empty -> no command hub (dev: c.cmd truthiness)
        self.head = head_id or ""        # empty -> no head hub / marker 0 (dev: c.head)
        self.text = ""


def build_graph(nodes, task_emb, sys_emb, edges_supersession, blocks=None):
    """(xe fp32, xs, ei, et, n_own, step_rows) — BYTE-MIRROR of TraceScorer._het_node_graph
    (which is itself the byte-mirror of trace_train.assemble_trace's HET path), from payload
    data. step_rows maps step value -> its step-node row (assemble_trace's _rule_sidx twin,
    consumed by score_rules' nearest-earlier clamp).

    `nodes` are the live chunk set in decision order (pydantic V1Node objects or anything
    with the same attributes); vectors arrive as JSON doubles and are cast to fp32 exactly
    as the dev path casts the embedder output. `blocks` (hoods artifact tuples) attach
    between attach_task and attach_steps — assemble_trace's order — when neighbors are live;
    None keeps the +3 zero block-parity cols (bit-identical to v0)."""
    chunks = [_V1Chunk(n.step, n.kind, n.file_id, n.lo, n.hi, n.cmd_id, n.head_id)
              for n in nodes]
    n_own = len(chunks)
    emb = np.asarray([n.emb_text for n in nodes], dtype=np.float32)
    ace = np.asarray([n.emb_cmd for n in nodes], dtype=np.float32)
    hde = np.asarray([n.emb_head for n in nodes], dtype=np.float32)
    node_struct = np.asarray([n.struct for n in nodes], dtype=np.float32)
    markers = np.stack([
        np.asarray([1.0 if c.kind == "reasoning" else 0.0 for c in chunks], np.float32),
        np.asarray([1.0 if c.head else 0.0 for c in chunks], np.float32),
    ], axis=1)
    xe = np.hstack([emb, ace, hde]).astype(np.float32)   # FULL tri-embedding
    xs = np.hstack([node_struct, markers]).astype(np.float32)
    # rels 0/1/2 through the VENDORED edges() — exact chain/argsort/tie semantics by
    # construction (numpy argsort over the causal-past candidate list; equal-step ties break
    # by chunk index via _fwd's a<=b). Its rel-4 block is the one place it wants raw text
    # (span Jaccard); with text="" it still emits the same-file LINE-OVERLAP subset, which is
    # DISCARDED below and replaced by the client's pairs verbatim. rel-4 is emitted LAST by
    # the vendored loop (i asc, then j asc = lexicographic (src, dst)), so sorting the payload
    # pairs the same way keeps the total edge ORDER byte-identical to the dev path (scatter
    # summation order == edge order; parity is bit-exact, not just set-equal).
    ei, et = edges(chunks, emb, causal=True, skip_supersession=True)
    if et.numel():
        keep = et != 4        # defensive no-op now: rel-4 was never built
        ei, et = ei[:, keep], et[keep]
    pairs = sorted((int(a), int(b)) for (a, b) in edges_supersession)
    if pairs:
        add = torch.tensor([[a for a, _ in pairs], [b for _, b in pairs]], dtype=torch.long)
        ei = torch.cat([ei, add], dim=1) if ei.numel() else add
        et = torch.cat([et, torch.full((len(pairs),), 4, dtype=torch.long)])
    ntype = ["observation"] * n_own
    zed = np.zeros(EMBED_DIM, dtype=np.float32)
    temb = np.hstack([np.asarray(task_emb, np.float32), zed, zed]).astype(np.float32)
    xe, xs, ei, et = attach_task(xe, xs, ei, et, temb, causal=True)
    ntype.append("task")
    if blocks is not None:
        _b0 = xe.shape[0]
        xe, xs, ei, et = attach_blocks(xe, xs, ei, et, blocks, list(range(n_own)),
                                       EMBED_DIM, causal=True)
        ntype += ["observation"] * (xe.shape[0] - _b0)
    else:
        xs = np.hstack([xs, np.zeros((xs.shape[0], 3), np.float32)])   # nf=None width parity
    _n0 = xe.shape[0]
    xe, xs, ei, et = attach_steps(xe, xs, ei, et, chunks, n_own)
    ntype += ["step"] * (xe.shape[0] - _n0)
    step_rows = {t: _n0 + k for k, t in enumerate(sorted({c.step for c in chunks}))}
    # AC_HUBPROPS must be "off" for this ckpt (real props shift every score high -> the
    # documented under-evict parity bug) AND the props are text-derived (symbol-graph over
    # chunk text) — unrepresentable in the v1 payload. _flags pins "off"; refuse loudly if
    # someone un-pins it rather than silently serving a different graph than the dev path.
    if os.environ.get("AC_HUBPROPS", "on") != "off":
        raise RuntimeError("AC_HUBPROPS != off: file-hub props need raw text and are OFF "
                           "for this checkpoint — the v1 path cannot honor that flag")
    sysv = np.asarray(sys_emb, np.float32) if sys_emb is not None else zed
    xe, xs, ei, et = attach_hetero(xe, xs, ei, et, chunks, n_own, ace, hde,
                                   {}, {}, ntype, sys_emb=sysv)
    return xe, xs, ei, et, n_own, step_rows


def _trace_blocks(bundle, task_emb):
    """Hoods blocks for a v1 trace request: the anchor query is the TASK NODE's payload vector
    (raw task text is unrepresentable on v1 — the client embedded it locally). None when hoods
    are off or the task vector is all-zeros (no task statement — reference gate)."""
    if bundle.hoods is None:
        return None
    tv = np.asarray(task_emb, dtype=np.float32)
    if not np.any(tv):
        return None
    blocks = bundle.hoods.blocks(tv)
    if not blocks:                                       # measured, never silent
        count_fail_open(log, "hoods live but ZERO neighbor blocks attached (v1)")
    else:
        log.debug("v1 neighbor blocks attached n_blocks=%d", len(blocks))
    return blocks


def score_trace(bundle, nodes, task_emb, sys_emb, mask, decided_struct, edges_supersession,
                gf=None):
    """(scores over nodes, tau, timings_ms, doom) — the v1 twin of TraceScorer.score_trace:
    same forward, same never-cut 1.0 default on non-mask rows, same constant calibrated tau.
    decided_struct rides in from the client (49 cols, computed where the text lives). doom =
    sigmoid(doom head) when `gf` rode the request and the ckpt has a doom head; else None."""
    n_own = len(nodes)
    blocks = _trace_blocks(bundle, task_emb)
    xe, xs, ei, et, _, _ = build_graph(nodes, task_emb, sys_emb, edges_supersession,
                                       blocks=blocks)
    dstruct = np.asarray(decided_struct, dtype=np.float32).reshape(len(mask), -1)
    doom = None
    t0 = time.perf_counter()
    with torch.inference_mode():
        h = bundle.model._embed_nodes(torch.from_numpy(xe), torch.from_numpy(xs), ei, et)
        didx = torch.tensor(list(mask), dtype=torch.long)
        u_dec = bundle.model.score_decided(h, didx, torch.from_numpy(dstruct))
        if gf is not None and bundle.doom_gf and len(gf) == bundle.doom_gf:
            # pooled over the OWN chunk rows only (0..n_own-1) — never task/block/step/hub
            # rows (reference curator L479-484); rides the same forward.
            pooled = h[:n_own].mean(dim=0, keepdim=True)
            gft = torch.tensor(list(gf), dtype=torch.float32)
            doom = float(torch.sigmoid(
                bundle.model.doom_head(bundle.model._doom_in(pooled, gft)).reshape(())))
    forward_ms = (time.perf_counter() - t0) * 1000.0
    probs = torch.sigmoid(u_dec).numpy()     # RAW sigmoid — calib_tau was fit on this scale
    sc = np.full(n_own, 1.0, dtype=np.float32)          # non-mask rows never cut
    sc[np.asarray(list(mask), dtype=np.int64)] = probs
    # embed=0.0 is structural on v1: the client embeds; the server never can.
    return sc, bundle.calib_tau, {"embed": 0.0, "forward": round(forward_ms, 3)}, doom


def score_tools(bundle, nodes, task_emb, sys_emb, tool_embs):
    """Tool-schema scores from payload embeddings — the v1 twin of score_request_tools'
    graph (build_tool_spec + assemble_trace), reusing the vendored attach_tools: schema
    nodes are task-conditioned causal SINKS on rel 3 (task -> schema only, never a
    call -> schema edge). Two deliberate deltas vs assemble_trace, both provably outside
    the tool rows' receptive field (and pinned by the cross-path parity test):
      - no rel-4 supersession edges among the observation nodes (the v1 tools request
        carries no edges_supersession: those edges target obs rows only, and tool rows
        receive messages exclusively from the task node);
      - xe is round-tripped through fp16 EXPLICITLY below, because assemble_trace stores
        xe .half() and collate_traces casts back .float() — skipping the round-trip would
        move the low mantissa bits and break dev==v1 bit-parity."""
    xe, xs, ei, et, n_own, _ = build_graph(nodes, task_emb, sys_emb, [])
    tembs = np.asarray(tool_embs, dtype=np.float32)
    xe, xs, ei, et, tool_idx = attach_tools(xe, xs, ei, et, tembs, EMBED_DIM, task_idx=n_own)
    t0 = time.perf_counter()
    with torch.inference_mode():
        xet = torch.from_numpy(xe).half().float()       # assemble_trace fp16 parity
        h = bundle.model._embed_nodes(xet, torch.from_numpy(xs), ei, et)
        sg = torch.sigmoid(bundle.model.score_tools(
            h, torch.tensor(np.asarray(tool_idx, dtype=np.int64))))
    forward_ms = (time.perf_counter() - t0) * 1000.0
    return sg.numpy().astype(np.float32), round(forward_ms, 3)


def score_rules(bundle, nodes, task_emb, sys_emb, edges_supersession, fire_step, rule_embs):
    """Rule-head scores from a v1 payload — the v1 twin of score_request_rules' graph
    (build_tool_spec + assemble_trace with AC_RULE=on), reusing the vendored attach_rules:
    one rule node per candidate, edged from its fire-STEP node (nearest-EARLIER clamp, the
    vendored trainer clamp verbatim), scored by rule_head([h[rule] | h[step]]).

    `rule_embs` are the SERVER-embedded roster texts (rules.json lives server-side; nothing
    of the user's is needed). `nodes` is the tool-spec chunk pipeline view — the same
    featurization the v1 tools request carries. Two deliberate deltas vs the dev path, both
    provably outside the rule rows' receptive field (rule nodes receive ONLY from their fire
    step node; tool/gate nodes are causal SINKS that never send a message):
      - no tool schema nodes are attached (the v1 rules request carries no tools);
      - xe rides the same fp16 round-trip as the dev assemble path.
    Returns (scores fp32 aligned with rule_embs, forward_ms)."""
    xe, xs, ei, et, _n_own, sidx = build_graph(nodes, task_emb, sys_emb, edges_supersession)
    # a fire step with no step node (pure-reasoning turn / numbering past the pipeline) clamps
    # to the NEAREST EARLIER step node — vendored assemble_trace's _nearest, verbatim.
    sk = sorted(sidx)
    le = [s for s in sk if s <= int(fire_step)]
    step_row = sidx[le[-1]] if le else sidx[sk[0]]
    step_rows = [step_row] * len(rule_embs)
    rembs = np.asarray(rule_embs, dtype=np.float32)
    xe, xs, ei, et, rule_idx = attach_rules(xe, xs, ei, et, rembs, EMBED_DIM, step_rows)
    t0 = time.perf_counter()
    with torch.inference_mode():
        xet = torch.from_numpy(xe).half().float()       # assemble_trace fp16 parity
        h = bundle.model._embed_nodes(xet, torch.from_numpy(xs), ei, et)
        sc = torch.sigmoid(bundle.model.score_rules(
            h, torch.tensor(np.asarray(rule_idx, dtype=np.int64)),
            torch.tensor(np.asarray(step_rows, dtype=np.int64))))
    forward_ms = (time.perf_counter() - t0) * 1000.0
    return sc.numpy().astype(np.float32), round(forward_ms, 3)
