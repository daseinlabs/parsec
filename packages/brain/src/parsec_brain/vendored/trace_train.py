"""TRACE-LEVEL assembly + training — ONE GRAPH PER RUN + CAUSAL EDGE MASK (Option 1, reverted).

VENDORED TRIM (parsec-brain serving): assemble_trace + collate_traces (+ the _flatten_readouts
helper assemble_trace's return references) survive — the graph assembly the tool/rule serve heads
run through. The DDP training loop, hot-path collate_flat and eval metrics are deleted; the kept
function bodies are byte-identical to the rulehead drop that trained curator_v4_prod.pt.

The data unit is ONE graph per run (scripts.trace_graph): all of a run's chunks (sorted by step) are
nodes, the task node + the k=16 neighbour blocks attach ONCE per run, and the graph is message-passed
ONCE. The edges are DIRECTED EARLIER->LATER (pyg_model.edges / attach_task / attach_blocks), so the
single forward pass is CAUSAL — node t's embedding depends only on nodes whose step <= t (RGCNConv
aggregates a target from its sources; no edge ever points backward). A readout at step t just SELECTS
which GLOBAL node rows are scored (the decided newborns) and pooled (alive_at(t)); it does NOT build
its own graph.

assemble_trace : trace spec -> ONE tensor graph (chunks + task + k=16 blocks) + readouts (GLOBAL idx)
collate_traces : merge the batch's run-graphs disjointly; offset edges + readout node-indices
"""
from __future__ import annotations

import os

import numpy as np
import torch

from .pyg_model import (attach_blocks, attach_hetero, attach_steps,
                        attach_task, edges)


def assemble_trace(spec, cache, dim, nf, rng_nbr):
    """spec (build_trace_graph) -> ONE run graph: own chunks are nodes 0..N-1 (so the readouts' GLOBAL
    indices need no remap), task = N, the k=16 neighbour blocks follow (attached ONCE per run). node
    features = content|action|obs tri-embedding + trace-level node_struct (over ALL chunks) + 2 role
    markers; the per-step decision-relative struct rides in each readout's head. The edges are CAUSAL
    (forward-only), so the single message pass is causal and the readouts simply select node rows.
    This is byte-for-byte the graph curator._trace_scores builds over the live chunk set (modulo the
    future chunks the prefix doesn't yet have — which, being causal sinks, never affect any node<=t)."""
    chunks = spec["chunks"]
    zed = np.zeros(dim, dtype=np.float32)
    emb = np.asarray([cache.get(c.text[:2000], zed) for c in chunks], dtype=np.float32)
    ace = np.asarray([cache.get(getattr(c, "cmd", "") or "", zed) for c in chunks], dtype=np.float32)
    hde = np.asarray([cache.get((getattr(c, "head", "") or "")[:240], zed) for c in chunks],
                     dtype=np.float32)
    HET = os.environ.get("AC_HETGRAPH") == "on"
    STEP = HET or os.environ.get("AC_STEPGRAPH") == "on"
    ei, et = edges(chunks, emb, causal=True)             # CAUSAL content-sim + temporal among own
    # HET = bag + typed hubs, ADDITIVE. The old het stripped the action/head embedding off the obs node
    # (xe content-only) AND dropped the direct same-file/temporal obs<->obs edges, betting message-passing
    # through hubs would restore them — with 3 layers it didn't, so het was info-starved and LOST to the
    # bag baseline it strictly contains. Keep the full tri-embedding + all in-window edges; attach_steps/
    # attach_hetero then ADD the spine + typed hubs on top. So het can only add signal, never remove it.
    xe = np.hstack([emb, ace, hde]).astype(np.float32)   # (N, 3*dim) content|action|obs — both paths
    # node features = trace-level struct (so message-passing sees it) + role markers; the per-step
    # dynamic struct rides in the readout head instead.
    markers = np.stack([np.asarray([1.0 if c.kind == "reasoning" else 0.0 for c in chunks], np.float32),
                        np.asarray([1.0 if (getattr(c, "head", "") or "") else 0.0 for c in chunks],
                                   np.float32)], axis=1)
    xs = np.hstack([spec["node_struct"], markers]).astype(np.float32)
    ntype = ["observation"] * len(chunks)                # explicit node TYPE per row (hetero stamps it)
    temb = np.hstack([cache.get(spec["task_text"], zed), zed, zed]).astype(np.float32)
    xe, xs, ei, et = attach_task(xe, xs, ei, et, temb, causal=True)   # +task node N, task->chunk only
    ntype.append("task")
    n_own = len(chunks)
    # k nearest TRACES attached ONCE per run (not per decision). Dropout per §ablation (block-less
    # validity). The CF bridges span block->ALL own chunks (range(n_own)); a readout selects which
    # decided rows it scores, but the graph (and its bridges) is identical for every readout.
    blocks = nf.blocks(spec["task_text"], spec["iid"]) if nf is not None else []
    r = rng_nbr.rand()
    if r < 0.20:
        blocks = []
    elif r < 0.35 and len(blocks) > 1:
        blocks = blocks[:max(1, len(blocks) // 2)]
    if nf is not None:
        _n0 = xe.shape[0]
        xe, xs, ei, et = attach_blocks(xe, xs, ei, et, blocks, list(range(n_own)), dim, causal=True)
        ntype += ["observation"] * (xe.shape[0] - _n0)   # neighbour-block nodes (cross-trace obs)
    else:                                                # block-less width parity (+3 struct cols)
        xs = np.hstack([xs, np.zeros((xs.shape[0], 3), dtype=np.float32)])
    # AC_DUPCOS=on: CONTINUOUS embedding-duplication signal (finer than a lexical >=0.6 flag). Per decided
    # chunk: max cosine to EARLIER-ALIVE chunks (redundancy with content already in context) + recency of
    # that nearest match. Causal (earlier-alive only) + serve-safe; computed here because the content
    # embeddings (emb) live in assemble, not the graph builder (which is why the old dup feats are lexical).
    if os.environ.get("AC_DUPCOS") == "on":
        En = emb / (np.linalg.norm(emb, axis=1, keepdims=True) + 1e-8)   # normalized content emb (N,dim)
        stp = np.asarray([c.step for c in chunks])
        for r in spec["readouts"]:
            al = np.asarray(r["alive"], dtype=np.int64)
            f = np.zeros((len(r["decided"]), 2), dtype=np.float32)
            for k, idx in enumerate(r["decided"]):
                ej = al[stp[al] < stp[idx]]                              # earlier-alive (causal)
                if len(ej):
                    sims = En[ej] @ En[idx]
                    m = int(np.argmax(sims))
                    f[k, 0] = float(sims[m]); f[k, 1] = float(1.0 / (1.0 + stp[idx] - stp[ej[m]]))
            r["decided_struct"] = np.hstack(
                [np.asarray(r["decided_struct"], dtype=np.float32), f]).astype(np.float32)
    # PROPER TRACE GRAPH. AC_STEPGRAPH/AC_HETGRAPH: append the step-node spine (positioned) LAST so the
    # PE cols sit at the end; chunk indices 0..n_own-1 (readouts address them) are unchanged.
    _rule_sidx = {}
    if STEP:
        _n0 = xe.shape[0]
        xe, xs, ei, et = attach_steps(xe, xs, ei, et, chunks, n_own)
        ntype += ["step"] * (xe.shape[0] - _n0)
        _rule_sidx = {t: _n0 + k for k, t in enumerate(sorted({c.step for c in chunks}))}
    if HET:                                              # command/head/file typed nodes + node-type stamp;
        from .symbol_graph import decided_centrality_feats                             # file-level props ->
        cen_by_file = {}; cp_by_file = {}                                              # the FILE node, once
        # AC_HUBPROPS=off: do NOT carry centrality/change-proneness on the file hubs. Use when those props
        # are fed as READOUT features (AC_CENTRALITY/AC_CHANGEPRONE) instead — phase8b showed having them on
        # BOTH hub + readout double-counts and HURTS (63.8 vs 67.2). Hubs still exist structurally (props=0).
        if os.environ.get("AC_HUBPROPS", "on") != "off":
            reads = [c for c in chunks if c.kind in ("read", "grep") and c.file]
            cen = decided_centrality_feats(reads, chunks, spec["task_text"]) if reads else np.zeros((0, 5), np.float32)
            for k, c in enumerate(reads):
                cen_by_file.setdefault(c.file, np.asarray(cen[k], np.float32))
            try:
                import pickle as _pk
                _d = _pk.load(open("/home/nicks/changeprone.pkl", "rb"))
                _cc = _d["cnt"] if isinstance(_d, dict) else _d[0]
                _ct = max((_d.get("total", 1) if isinstance(_d, dict) else _d[1]), 1)
                for c in chunks:
                    if c.file:
                        cp_by_file[c.file] = _cc.get(c.file, 0) / _ct
            except Exception:
                pass
        sys_emb = cache.get((spec.get("sys_text", "") or "")[:2000], zed)   # harness fingerprint embedding
        xe, xs, ei, et = attach_hetero(xe, xs, ei, et, chunks, n_own, ace, hde,
                                       cen_by_file, cp_by_file, ntype, sys_emb=sys_emb)
    # TOOL-SCHEMA NODES (the Claude-Code curation target): append the static step-0 roster LAST so the
    # observation node indices the readouts address stay put. Conditioned on the task only (causal, no
    # future-call leak); scored by the dedicated tool head. tool_idx are graph-local rows (collate offsets).
    tool_nodes = spec.get("tool_nodes") or []
    tool_idx, tool_y, tool_tok = [], [], []
    if tool_nodes:
        from .pyg_model import attach_tools
        tembs = np.asarray([cache.get(tn["text"], zed) for tn in tool_nodes], dtype=np.float32)
        xe, xs, ei, et, tool_idx = attach_tools(xe, xs, ei, et, tembs, dim, task_idx=n_own)
        tool_y = [float(tn["used"]) for tn in tool_nodes]
        tool_tok = [float(tn["tokens"]) for tn in tool_nodes]
    # GATE / BRIEF node (AC_GATE=on): ONE node carrying the brief embedding (content slot),
    # task->brief conditioning edge (attach_brief). Appended LAST so readout indices stay put.
    # gate_y = the labeler's per-run helped/hurt 0/1; -1.0 = unlabeled (loss skips the graph).
    gate_idx, gate_y = -1, -1.0
    gate_stats = np.zeros(6, np.float32)
    if os.environ.get("AC_GATE") == "on":
        from .pyg_model import attach_brief
        from .trace_graph import brief_stats, _scout_map
        bt = (spec.get("brief") or "")[:2000]
        bemb = np.hstack([cache.get(bt, zed) if bt else zed, zed, zed]).astype(np.float32)
        xe, xs, ei, et, gate_idx = attach_brief(xe, xs, ei, et, bemb, task_idx=n_own)
        gy = spec.get("gate_y")
        gate_y = float(gy) if gy is not None else -1.0
        gate_stats = brief_stats(bt, spec.get("task_text") or "", _scout_map().get(spec.get("iid")))
    # RULE NODES (AC_RULE=on): one node per (rule, fire-step) from spec["rule_nodes"] =
    # [{eid,text,step,label}], edged from its fire STEP node (per-step, cut-at-birth). Scored by
    # rule_head. rule_idx are graph-local rows (collate offsets); rule_y the per-step 0/1 labels.
    rule_idx, rule_y, rule_edged, rule_step = [], [], 0, []
    rns = spec.get("rule_nodes") or []
    if os.environ.get("AC_RULE") == "on" and rns and _rule_sidx:
        from .pyg_model import attach_rules
        rembs = np.asarray([cache.get(rn["text"], zed) for rn in rns], dtype=np.float32)
        # a fire step with no chunk (pure-reasoning turn) has no step node -> clamp to the NEAREST
        # EARLIER step node (causal: the governor at step t saw context up to t). Keeps per-step
        # conditioning ~exact and gets every fire an edge (align ~100%).
        _sk = sorted(_rule_sidx)
        _exact = sum(1 for rn in rns if int(rn["step"]) in _rule_sidx)

        def _nearest(t):
            le = [s for s in _sk if s <= t]
            return _rule_sidx[le[-1]] if le else _rule_sidx[_sk[0]]
        step_rows = [_nearest(int(rn["step"])) for rn in rns]
        rule_edged = int(_exact)
        xe, xs, ei, et, rule_idx = attach_rules(xe, xs, ei, et, rembs, dim, step_rows)
        rule_y = [float(rn["label"]) for rn in rns]
        rule_step = step_rows                    # fire-STEP node row per rule node (for score_rules)
    return dict(xe=torch.from_numpy(xe.astype(np.float32)).half(), xs=torch.from_numpy(xs),
                ei=ei, et=et, n_own=n_own, readouts=spec["readouts"],
                tool_idx=np.asarray(tool_idx, np.int64), tool_y=np.asarray(tool_y, np.float32),
                tool_tok=np.asarray(tool_tok, np.float32),
                gate_idx=int(gate_idx), gate_y=float(gate_y), gate_stats=gate_stats,
                rule_idx=np.asarray(rule_idx, np.int64), rule_y=np.asarray(rule_y, np.float32),
                rule_step=np.asarray(rule_step, np.int64), rule_edged=int(rule_edged),
                flat=_flatten_readouts(spec["readouts"]))   # precompute hot-path arrays ONCE here


def collate_traces(batch, dev):
    """Merge K run graphs into one; offset edges + each readout's GLOBAL node-indices by the trace base.
    No edges cross traces, so one message pass scores every run's decisions causally and independently."""
    if not batch:
        z = torch.zeros((2, 0), dtype=torch.long, device=dev)
        return (torch.zeros((0, 0), device=dev), torch.zeros((0, 0), device=dev), z,
                torch.zeros((0,), dtype=torch.long, device=dev), [])
    xe = torch.cat([d["xe"] for d in batch]).to(dev).float()
    xs = torch.cat([d["xs"] for d in batch]).to(dev)
    eis, ets, base, ros = [], [], 0, []
    for d in batch:
        n = d["xe"].shape[0]
        eis.append(d["ei"] + base); ets.append(d["et"])
        for r in d["readouts"]:
            ros.append(dict(decided=[base + i for i in r["decided"]],
                            alive=[base + i for i in r["alive"]],
                            y=r["y"], struct=r["decided_struct"], is_adm=r["is_adm"],
                            tok=r.get("decided_tok", [1.0] * len(r["decided"])),
                            rnorm=d.get("rnorm", 0.0),   # trace reward -> value baseline / advantage
                            doom=r["doom"], gf=r["gf"], rem=r["rem"]))
        base += n
    ei = torch.cat(eis, dim=1).to(dev) if eis else torch.zeros((2, 0), dtype=torch.long, device=dev)
    et = torch.cat(ets).to(dev) if ets else torch.zeros((0,), dtype=torch.long, device=dev)
    return xe, xs, ei, et, ros


def _flatten_readouts(readouts):
    """Precompute per-GRAPH flat readout arrays ONCE (at assemble), so the training hot path does NO
    per-atom Python loop and NO per-step tensor build. Indices are graph-LOCAL (collate_flat offsets
    them by the node/readout base). One-time cost at data load — never in the step loop."""
    didx, ridx_d, dy, dtok, dstruct, aidx, ridx_a = [], [], [], [], [], [], []
    doom, gf, rem, isadm = [], [], [], []
    for k, r in enumerate(readouts):
        nd = len(r["decided"])
        didx += r["decided"]; ridx_d += [k] * nd; dy += list(r["y"])
        dtok += list(r.get("decided_tok", [1.0] * nd)); dstruct.append(r["decided_struct"])
        aidx += r["alive"]; ridx_a += [k] * len(r["alive"])
        doom.append(r["doom"]); gf.append(r["gf"]); rem.append(r["rem"]); isadm.append(r["is_adm"])
    return dict(
        didx=np.asarray(didx, np.int64), ridx_d=np.asarray(ridx_d, np.int64),
        dy=np.asarray(dy, np.float32), dtok=np.asarray(dtok, np.float32),
        dstruct=(np.concatenate(dstruct).astype(np.float32) if dstruct else np.zeros((0, 1), np.float32)),
        aidx=np.asarray(aidx, np.int64), ridx_a=np.asarray(ridx_a, np.int64),
        doom=np.asarray(doom, np.float32), gf=np.asarray(gf, np.float32),
        rem=np.asarray(rem, np.float32), isadm=np.asarray(isadm, np.float32), R=len(readouts))
