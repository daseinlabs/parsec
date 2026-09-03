"""The production curator GNN (PyTorch Geometric R-GCN) + graph construction, importable by both
the trainer and the live curator. Inductive: scores chunks on a brand-new trace's window."""
from __future__ import annotations

import math
import os

import numpy as np
import torch
import torch.nn as nn
from torch_geometric.nn import GATv2Conv, RGCNConv, TransformerConv

N_REL = 7   # 0 same-file, 1 temporal, 2 k-NN sim, 3 task<->chunk, 4 supersession,
#             5 cross-trace membership/anchor, 6 CF-bridge (neighbor chunk -> decided chunk).
#             Audit fix: bridges previously OVERLOADED relation 2 (in-window kNN), forcing one
#             RGCN weight matrix to serve two different semantics.


def attach_blocks(emb: np.ndarray, struct: np.ndarray, ei, et, blocks: list, decided_js: list,
                  content_dim: int, bridge_rel: int = 6, causal: bool = False):
    """§5.3 AS DESIGNED: the X nearest tasks' ENTIRE traces enter the decision graph. Each block
    = (bemb (M,1024) with node 0 the trace anchor, extras (M,3)=[is_nbr, needed, run_success],
    src, dst, typ) carrying its own membership (5) and temporal (1) edges. Bridges: trace anchor
    -> task node (5); each block chunk's 2 nearest decided chunks get similarity edges (2).
    Adds 3 struct cols to ALL nodes; existing nodes get zeros. Anti-leakage upstream (the anchor
    search can never return the current instance).

    Block edges already point INTO the current graph (anchor->task, block-chunk->decided) and the
    block-internal edges connect block nodes only, so attach_blocks is ALREADY causal-safe (a block
    is a step=-1 historical source; no chunk->block edge is ever emitted). The `causal` flag is
    accepted for signature parity with edges()/attach_task() but changes nothing here."""
    n0 = emb.shape[0]
    task_idx = n0 - 1
    struct = np.hstack([struct, np.zeros((n0, 3), dtype=np.float32)])
    embs, structs = [emb], [struct]
    srcs, dsts, typs = [], [], []
    base = n0
    cur = emb[:, :content_dim]
    for (bemb, extras, bs, bd, bt) in blocks:
        M = bemb.shape[0]
        be = np.zeros((M, emb.shape[1]), dtype=np.float32)
        be[:, :content_dim] = bemb[:, :content_dim]
        bstr = np.zeros((M, struct.shape[1]), dtype=np.float32)
        bstr[:, -3:] = extras
        embs.append(be); structs.append(bstr)
        # block-INTERNAL edges: both source AND dest are block nodes (step=-1 each) -> causal-OK,
        # keep all of them unchanged.
        srcs += list(base + np.asarray(bs)); dsts += list(base + np.asarray(bd)); typs += list(bt)
        srcs.append(base); dsts.append(task_idx); typs.append(5)        # anchor (src) -> task ONLY
        if decided_js and M > 1:
            bn = bemb[1:, :content_dim]
            bn = bn / (np.linalg.norm(bn, axis=1, keepdims=True) + 1e-8)
            for j in decided_js:
                v = cur[j] / (np.linalg.norm(cur[j]) + 1e-8)
                for t_ in np.argsort(-(bn @ v))[:2]:                     # CF bridge: block (src) -> decided
                    srcs.append(base + 1 + int(t_)); dsts.append(int(j)); typs.append(bridge_rel)
        base += M
    emb = np.vstack(embs)
    struct = np.vstack(structs)
    if srcs:
        addt = torch.tensor([np.asarray(srcs, dtype=np.int64),
                             np.asarray(dsts, dtype=np.int64)], dtype=torch.long)
        ei = torch.cat([ei, addt], dim=1) if ei.numel() else addt
        et = torch.cat([et, torch.tensor(np.asarray(typs, dtype=np.int64), dtype=torch.long)])
    return emb, struct, ei, et


def attach_task(emb: np.ndarray, struct: np.ndarray, ei, et, task_emb, causal: bool = False):
    """Append a TASK node (problem-statement embedding) connected to every chunk (relation 3), so
    message-passing conditions each chunk's keep/cut score on the GOAL. The task node is the last
    index; callers score only the chunk nodes (u[:n]). The §6 graph = task + trajectory + candidates.

    causal=False (default, per-decision path / w326-era ckpts): task <-> chunk BOTH directions, as
    those models trained. causal=True (trace-level one-graph-per-run path): the task node is a pure
    SOURCE (step=-1, always earlier) so only task -> chunk edges are emitted; the reverse
    (chunk -> task) is dropped so the task hub can never ABSORB a future chunk and leak it back into
    an earlier chunk -> the single forward pass stays causal."""
    n = emb.shape[0]
    emb2 = np.vstack([emb, np.asarray(task_emb, dtype=np.float32)[None, :]])
    struct2 = np.vstack([struct, np.zeros((1, struct.shape[1]), dtype=np.float32)])
    src, dst, typ = [], [], []
    for i in range(n):
        if causal:                                       # task (src) -> chunk ONLY (forward)
            src.append(n); dst.append(i); typ.append(3)
        else:                                            # task <-> chunk, both directions (legacy)
            src += [n, i]; dst += [i, n]; typ += [3, 3]
    add = torch.tensor([src, dst], dtype=torch.long)
    ei2 = torch.cat([ei, add], dim=1) if ei.numel() else add
    et2 = torch.cat([et, torch.tensor(typ, dtype=torch.long)])
    return emb2, struct2, ei2, et2


def attach_tools(emb: np.ndarray, struct: np.ndarray, ei, et, tool_embs, content_dim: int,
                 task_idx: int | None = None, harness_idx: int | None = None, rel: int = 3):
    """Append the TOOL-SCHEMA roster as nodes — the Claude-Code curation TARGET (the ~26k of tool
    definitions in the step-0 prefix, ~80% droppable). Each schema node carries ONLY its serialized-
    schema embedding (content slot); it is scored at BIRTH by the dedicated tool head as keep/cut.

    CUT-AT-BIRTH / NO LEAK: the roster is static at step 0, so the keep/cut decision must be made
    WITHOUT seeing the future tool CALLS (the `used` outcome the head predicts). We therefore add only
    INBOUND conditioning edges task->schema (and harness->schema when present) — pure earlier-step
    sources — and NEVER a call->schema edge (that would pipe the future `used` label straight into the
    node). Reuses relation 3 (task<->chunk conditioning) so no new relation / nrel bump is needed; the
    tool head alone distinguishes these rows. Returns (emb, struct, ei, et, tool_idx) where tool_idx
    are the appended schema nodes' global row indices (what the head scores)."""
    M = 0 if tool_embs is None else len(tool_embs)
    if M == 0:
        return emb, struct, ei, et, []
    n0 = emb.shape[0]
    te = np.zeros((M, emb.shape[1]), dtype=np.float32)
    te[:, :content_dim] = np.asarray(tool_embs, dtype=np.float32)[:, :content_dim]
    emb2 = np.vstack([emb, te])
    struct2 = np.vstack([struct, np.zeros((M, struct.shape[1]), dtype=np.float32)])
    tool_idx = list(range(n0, n0 + M))
    src, dst, typ = [], [], []
    for k in tool_idx:
        if task_idx is not None:
            src.append(task_idx); dst.append(k); typ.append(rel)        # task -> schema (goal conditions)
        if harness_idx is not None:
            src.append(harness_idx); dst.append(k); typ.append(rel)     # harness -> schema (surface)
    if src:
        add = torch.tensor([src, dst], dtype=torch.long)
        ei = torch.cat([ei, add], dim=1) if ei.numel() else add
        et = torch.cat([et, torch.tensor(typ, dtype=torch.long)])
    return emb2, struct2, ei, et, tool_idx


def attach_rules(emb: np.ndarray, struct: np.ndarray, ei, et, rule_embs, content_dim: int,
                 step_rows, rel: int = 3):
    """Append RULE nodes — one per (rule, fire-step). The governor injects rules PER STEP (always-on
    every step; cosine at matched steps), so the head decides per step. Each rule node carries ONLY
    its rule-text embedding (content slot) and takes ONE inbound edge from the STEP node it fired at
    (`step_rows[k]`), so message-passing conditions it on THAT step's context — per-step, cut-at-birth
    (the step node aggregates only its own+earlier context; no future-outcome leak). Scored by
    rule_head. Returns (emb, struct, ei, et, rule_idx) with rule_idx the appended rows the head scores."""
    M = 0 if rule_embs is None else len(rule_embs)
    if M == 0:
        return emb, struct, ei, et, []
    n0 = emb.shape[0]
    re_ = np.zeros((M, emb.shape[1]), dtype=np.float32)
    re_[:, :content_dim] = np.asarray(rule_embs, dtype=np.float32)[:, :content_dim]
    emb2 = np.vstack([emb, re_])
    struct2 = np.vstack([struct, np.zeros((M, struct.shape[1]), dtype=np.float32)])
    rule_idx = list(range(n0, n0 + M))
    src, dst, typ = [], [], []
    for k, si in zip(rule_idx, step_rows):
        if si is not None and si >= 0:
            src.append(int(si)); dst.append(k); typ.append(rel)     # fire-step -> rule (per-step cond)
    if src:
        add = torch.tensor([src, dst], dtype=torch.long)
        ei = torch.cat([ei, add], dim=1) if ei.numel() else add
        et = torch.cat([et, torch.tensor(typ, dtype=torch.long)])
    return emb2, struct2, ei, et, rule_idx


def attach_brief(emb, struct, ei, et, brief_emb, task_idx=None, rel=3):
    """Append ONE BRIEF node — the codescout brief the gate head decides to serve or drop. Carries the
    brief-text embedding (tri-embedding row, content slot filled by the caller); inbound task->brief
    conditioning edge only (same cut-at-birth/no-future-leak shape as attach_tools). Scored by
    gate_head once per run at step 0. Returns (emb, struct, ei, et, brief_idx)."""
    n0 = emb.shape[0]
    emb2 = np.vstack([emb, np.asarray(brief_emb, dtype=np.float32)[None, :]])
    struct2 = np.vstack([struct, np.zeros((1, struct.shape[1]), dtype=np.float32)])
    if task_idx is not None:
        add = torch.tensor([[task_idx], [n0]], dtype=torch.long)
        ei = torch.cat([ei, add], dim=1) if ei.numel() else add
        et = torch.cat([et, torch.tensor([rel], dtype=torch.long)])
    return emb2, struct2, ei, et, n0


_PE_DIM = 8   # sinusoidal positional-encoding width for the step spine


def _pe(step_arr, maxstep):
    """Sinusoidal absolute-position encoding of a step index + one normalized-position column."""
    a = np.asarray(step_arr, dtype=np.float32)
    out = np.zeros((len(a), _PE_DIM + 1), dtype=np.float32)
    div = np.exp(np.arange(0, _PE_DIM, 2, dtype=np.float32) * (-np.log(10000.0) / _PE_DIM))
    p = a[:, None]
    out[:, 0:_PE_DIM:2] = np.sin(p * div)
    out[:, 1:_PE_DIM:2] = np.cos(p * div)
    out[:, _PE_DIM] = a / max(float(maxstep), 1.0)
    return out


def attach_steps(emb: np.ndarray, struct: np.ndarray, ei, et, chunks, n_own: int,
                 step_adv: int = 7, step_att: int = 8):
    """Turn the bag-of-context-nodes into a real TRACE graph: add a STEP-NODE SPINE — one node per
    step, step_t -> step_{t+1} (relation step_adv) — that carries ABSOLUTE position, and attach every
    observation chunk to the step that produced it both ways (relation step_att). The flat chunk chain
    never encoded *when* a chunk happened; now each chunk is anchored to a positioned step, so two
    identical reads at different steps attach to different step nodes and are no longer indistinguishable.
    Also stamps each EXISTING chunk row with its own step's PE (rows >= n_own = task/blocks get 0).
    emb/struct are the post-attach_task/blocks arrays; this is the LAST graph op so PE cols sit at the end."""
    n_exist = emb.shape[0]
    maxstep = max((c.step for c in chunks), default=1)
    ex_step = np.array([chunks[i].step if i < n_own else 0 for i in range(n_exist)], dtype=np.float32)
    ex_pe = _pe(ex_step, maxstep)
    for i in range(n_exist):
        if i >= n_own:                                    # task / neighbour-block nodes have no step
            ex_pe[i] = 0.0
    struct_w = np.hstack([struct, np.zeros((n_exist, 1), np.float32), ex_pe]).astype(np.float32)
    steps_present = sorted({c.step for c in chunks})
    sidx = {t: n_exist + k for k, t in enumerate(steps_present)}
    ns = len(steps_present)
    emb2 = np.vstack([emb, np.zeros((ns, emb.shape[1]), dtype=np.float32)])  # step nodes: no content
    base_w = struct.shape[1]
    step_struct = np.hstack([np.zeros((ns, base_w), np.float32), np.ones((ns, 1), np.float32),
                             _pe(steps_present, maxstep)]).astype(np.float32)
    struct2 = np.vstack([struct_w, step_struct])
    src, dst, typ = [], [], []
    for k in range(ns - 1):                               # spine: step_t -> step_{t+1} (forward)
        src.append(sidx[steps_present[k]]); dst.append(sidx[steps_present[k + 1]]); typ.append(step_adv)
    for i in range(n_own):                                # chunk <-> its step (same step, causal-safe)
        si = sidx[chunks[i].step]
        src += [si, i]; dst += [i, si]; typ += [step_att, step_att]
    add = torch.tensor([src, dst], dtype=torch.long)
    ei2 = torch.cat([ei, add], dim=1) if ei.numel() else add
    et2 = torch.cat([et, torch.tensor(typ, dtype=torch.long)])
    return emb2, struct2, ei2, et2


# node TYPES (explicit, first-class) — one-hot stamped into struct so the homogeneous RGCN sees them.
NODE_TYPES = ["observation", "step", "command", "head", "file", "task", "rule", "harness"]
# heterogeneous relations (in addition to the in-window content rels 0-4 / task 3 / blocks 5,6):
R_STEP_ADV, R_STEP_ATT = 7, 8        # step_t->step_{t+1} ; chunk<->its step
R_CMD_STEP, R_CMD_OBS = 9, 10        # command->step ; command->its observations
R_HEAD_OBS = 11                      # head->its observation
R_FILE_OBS = 12                      # file-hub->each read/edit of that file
R_HARNESS = 13                       # harness/system-prompt hub -> every step (conditions all decisions)
N_REL_HET = 14


def attach_hetero(emb, struct, ei, et, chunks, n_own, ace, hde, cen_by_file, cp_by_file, ntype_existing,
                  sys_emb=None):
    """Build the REAL heterogeneous trace graph by NORMALIZING what the bag-of-chunks denormalized:
      - OBSERVATION rows (0..n_own-1) keep ONLY their content embedding (caller has zeroed the
        command/head slots of emb); they get an explicit 'observation' type stamp.
      - COMMAND node per (step, cmd): carries the action embedding `ace` once; edges command->step
        and command->each observation it produced (no more copying the command vector onto every chunk).
      - HEAD node per (step, cmd): carries the header/returncode embedding `hde`; edge head->observation.
      - FILE node per distinct file: carries centrality + change-proneness ONCE (file-level props that
        were duplicated on every read); edge file->each observation of that file (replaces flat same-file).
      - explicit node-TYPE one-hot on every node (was only implicit kind columns).
    Called LAST (after attach_task/blocks/steps). Observation indices 0..n_own-1 are preserved so the
    readouts still address them. Returns widened (emb, struct, ei, et)."""
    n_exist = emb.shape[0]
    dim = ace.shape[1]
    nt = len(NODE_TYPES); tindex = {t: k for k, t in enumerate(NODE_TYPES)}

    def onehot(type_name, n=1):
        v = np.zeros((n, nt), np.float32); v[:, tindex[type_name]] = 1.0; return v
    # type stamp for existing rows from the caller-provided type list (observation/task/step/...).
    tcols = np.zeros((n_exist, nt), np.float32)
    for i, tn in enumerate(ntype_existing):
        tcols[i, tindex.get(tn, tindex["observation"])] = 1.0
    cen_w = len(next(iter(cen_by_file.values()))) if cen_by_file else 5
    fileprop_w = cen_w + 1                               # centrality dims + changeprone
    struct = np.hstack([struct, tcols, np.zeros((n_exist, fileprop_w), np.float32)]).astype(np.float32)

    new_emb, new_struct, src, dst, typ = [], [], [], [], []
    nxt = n_exist
    maxstep = max((c.step for c in chunks), default=1); pe_w = _PE_DIM + 1

    def add_node(e_slots, type_name, prop=None, pe_step=None):
        nonlocal nxt
        row = np.zeros(struct.shape[1], np.float32)
        row[-(nt + fileprop_w):-fileprop_w] = onehot(type_name)[0]
        if prop is not None:
            row[-fileprop_w:] = prop
        if pe_step is not None:                          # command/head carry their OWN step's position
            lo = struct.shape[1] - fileprop_w - nt - pe_w
            if lo >= 0:
                row[lo:lo + pe_w] = _pe([pe_step], maxstep)[0]
        new_struct.append(row); new_emb.append(e_slots)
        idx = nxt; nxt += 1; return idx

    z = np.zeros(dim, np.float32)
    # COMMAND + HEAD nodes per (step, cmd)
    seen_cmd = {}
    for i in range(n_own):
        key = (chunks[i].step, chunks[i].cmd or "")
        if chunks[i].cmd and key not in seen_cmd:
            ci = add_node(np.hstack([z, ace[i], z]), "command", pe_step=chunks[i].step)  # [_, action, _]
            seen_cmd[key] = ci
            if chunks[i].head:
                hi = add_node(np.hstack([z, z, hde[i]]), "head", pe_step=chunks[i].step)  # [_, _, head]
                src.append(hi); dst.append(ci); typ.append(R_HEAD_OBS)
        ci = seen_cmd.get(key)
        if ci is not None:
            src += [ci]; dst += [i]; typ += [R_CMD_OBS]             # command -> observation
    # FILE hub nodes
    seen_file = {}
    for i in range(n_own):
        f = chunks[i].file
        if not f:
            continue
        if f not in seen_file:
            prop = np.concatenate([np.asarray(cen_by_file.get(f, np.zeros(cen_w or 1)), np.float32),
                                   [float(cp_by_file.get(f, 0.0))]]).astype(np.float32)
            seen_file[f] = add_node(np.hstack([z, z, z]), "file", prop=prop)
        src += [seen_file[f]]; dst += [i]; typ += [R_FILE_OBS]      # file -> observation
    # HARNESS / SYSTEM-PROMPT hub: ONE node carrying the system-prompt embedding, edged to EVERY step
    # node so it conditions every decision via the spine. First-class + always present (constant on a
    # single-harness corpus = zero signal, but the graph is READY: the instant multi-harness traces land,
    # sys_emb varies and the model conditions curation on the harness — no structural change needed).
    se = np.asarray(sys_emb, np.float32).reshape(-1) if sys_emb is not None else z
    if se.shape[0] != dim:
        se = z
    step_idx = [k for k, tn in enumerate(ntype_existing) if tn == "step"]
    hub = add_node(np.hstack([se, z, z]), "harness")             # [sys_emb, _, _]
    for si in step_idx:
        src.append(hub); dst.append(si); typ.append(R_HARNESS)   # harness -> step (conditions the spine)
    if not step_idx:                                             # no spine -> fall back to task/all obs
        tnodes = [k for k, tn in enumerate(ntype_existing) if tn == "task"] or list(range(n_own))
        for ti in tnodes:
            src.append(hub); dst.append(ti); typ.append(R_HARNESS)
    if new_emb:
        emb = np.vstack([emb, np.asarray(new_emb, np.float32)])
        struct = np.vstack([struct, np.asarray(new_struct, np.float32)])
    if src:
        add = torch.tensor([src, dst], dtype=torch.long)
        ei = torch.cat([ei, add], dim=1) if ei.numel() else add
        et = torch.cat([et, torch.tensor(typ, dtype=torch.long)])
    return emb, struct, ei, et


def edges(chunks, emb: np.ndarray, causal: bool = False, skip_supersession: bool = False):
    """Build the in-window relation edges (same-file 0, temporal 1, k-NN sim 2, supersession 4).

    causal=False (default, per-decision path / w326-era ckpts): the legacy SYMMETRIC graph those
    models trained on — same-file/temporal/k-NN emit BOTH directions, supersession emits NEW->OLD.

    causal=True (trace-level one-graph-per-run path): every edge is directed EARLIER-or-equal step ->
    LATER step, so RGCNConv (which aggregates a TARGET from its SOURCES) makes node t's embedding
    depend only on nodes whose step <= t. Each relation keeps only the forward copy; supersession is
    redirected OLD->NEW so it too is forward. Equal-step ties break by chunk index (the stable
    step-sorted order) so each undirected pair yields exactly one forward edge, never a 2-cycle.
    THIS is what makes one message pass over the whole run == the live prefix pass (train==serve)."""
    from .attribution import spans
    n = len(chunks); src, dst, typ = [], [], []
    stp = [c.step for c in chunks]

    _fast = os.environ.get("AC_EDGES_FAST", "on") != "off"   # vectorized rel-0/2 twins — BIT-IDENTICAL
    #   selection (same candidate sets, same (lo, index)/argsort tie-breaks; pinned by
    #   tests/test_edges_fast.py). The pure-Python scans were O(n^2) with a fat constant and
    #   dominated serve CPU at n~2000 (perf research 2026-09-02 §2.2). "off" restores the loops.
    _strict = os.environ.get("AC_STRICTEDGE") == "on"    # drop SAME-step edges (parallel siblings issued
    #                                                      together: a sibling's result didn't exist at the
    #                                                      others' decision time -> no concurrent edge)

    _prune = os.environ.get("AC_PRUNE") == "on"          # AC_PRUNE: delete the DENSE all-to-all
    #   homogenizers (same-file + kNN-sim) that connect every chunk of a file to every other and smooth
    #   them to ~one vector (measured 0.964). Keep the structured edges (temporal/supersession/hub/spine/
    #   task) and ALL layers. File-level signal rides in per-chunk struct feats + the file hub, not these.

    def _fwd(a, b):                                      # a -> b allowed iff a is strictly-earlier (or =)
        return (stp[a] < stp[b]) if _strict else (stp[a] < stp[b] or (stp[a] == stp[b] and a <= b))

    _chain = causal and os.environ.get("AC_FILECHAIN", "on") != "off"   # within-file LINE-NEIGHBOR chain
    if not _prune:
        if _chain:
            # CAUSAL same-file LINE-NEIGHBOR CHAIN -- replaces the all-to-all same-file CLIQUE that
            # mean-aggregated every chunk of a file to ~one vector (measured same-file cos 0.964,
            # needed-vs-dead 0.941; sim meanCos 1.0 / posCorr 0 / needGap 0 -> chunks INTERCHANGEABLE,
            # which is why 84.7% of MISSED needed mass is within-file). Each target j wires to its
            # nearest-LINE same-file neighbor(s) AMONG ITS CAUSAL PAST {i : _fwd(i,j)} -- nearest lo
            # at/below j and nearest lo above j -- emitting i->j on rel 0. Building neighbors from the
            # causal past ONLY makes j's INCOMING set invariant to any FUTURE node, so the serve-time
            # prefix graph reproduces the full graph restricted to step<=t EXACTLY (the prefix==full /
            # train==serve parity invariant). A "sort present chunks by lo, zip consecutive" chain would
            # BREAK parity: a later-read span can insert between a prefix-adjacent pair and rewire its
            # neighbors. Sim: chain posCorr 0.863 / needGap -0.153 (separable) vs clique 0 / 0. lo=None
            # (whole-file reads, some greps, reasoning) get NO chain edge -> ride the R_FILE_OBS hub only
            # (a behavior delta from the file-identity clique). Data: line-adjacency lift 2.2x cc / 3.4x
            # mini, log1p(lo) AUC 0.67 -- the signal this propagates. AC_FILECHAIN=off restores the clique.
            lo = [getattr(c, "lo", None) for c in chunks]
            if _fast:
                stp_a = np.asarray(stp, dtype=np.int64)
                idx_a = np.arange(n)
                _fm: dict = {}
                codes = np.asarray([_fm.setdefault(c.file, len(_fm)) for c in chunks])
                has_lo = np.asarray([v is not None for v in lo])
                lo_a = np.asarray([v if v is not None else 0 for v in lo], dtype=np.int64)
                for j in range(n):
                    if not chunks[j].file or lo[j] is None:
                        continue
                    fwd = (stp_a < stp_a[j]) if _strict else (
                        (stp_a < stp_a[j]) | ((stp_a == stp_a[j]) & (idx_a <= j)))
                    cand = np.nonzero((codes == codes[j]) & has_lo & fwd & (idx_a != j))[0]
                    below = cand[lo_a[cand] <= lo_a[j]]
                    above = cand[lo_a[cand] > lo_a[j]]
                    if below.size:                       # max by (lo, i), as the loop's key
                        m = lo_a[below].max(); i = int(below[lo_a[below] == m].max())
                        src.append(i); dst.append(j); typ.append(0)
                    if above.size:                       # min by (lo, i)
                        m = lo_a[above].min(); i = int(above[lo_a[above] == m].min())
                        src.append(i); dst.append(j); typ.append(0)
            else:
                for j in range(n):
                    if not chunks[j].file or lo[j] is None:
                        continue
                    cand = [i for i in range(n) if i != j and chunks[i].file == chunks[j].file
                            and lo[i] is not None and _fwd(i, j)]
                    below = [i for i in cand if lo[i] <= lo[j]]
                    above = [i for i in cand if lo[i] > lo[j]]
                    if below:                            # nearest line at/above j, in the causal past
                        i = max(below, key=lambda i: (lo[i], i)); src.append(i); dst.append(j); typ.append(0)
                    if above:                            # nearest line below j, in the causal past
                        i = min(above, key=lambda i: (lo[i], i)); src.append(i); dst.append(j); typ.append(0)
        else:
            for i in range(n):                           # same-file (legacy CLIQUE: bag/w326 ckpts)
                for j in range(n):
                    if i != j and chunks[i].file and chunks[i].file == chunks[j].file:
                        if not causal or _fwd(i, j):     # legacy: both dirs; causal: forward copy
                            src.append(i); dst.append(j); typ.append(0)
    for i in range(n - 1):                               # temporal
        if causal:                                       # i -> i+1 only (i is earlier)
            if not _strict or stp[i] < stp[i + 1]:       # strict: skip same-step (parallel) temporal edge
                src.append(i); dst.append(i + 1); typ.append(1)
        else:                                            # i <-> i+1 (legacy)
            src += [i, i + 1]; dst += [i + 1, i]; typ += [1, 1]
    if n > 1 and not _prune:                             # AC_PRUNE drops the kNN-sim all-to-all homogenizer
        E = emb / (np.linalg.norm(emb, axis=1, keepdims=True) + 1e-8)
        sim = E @ E.T
        if not causal:                                   # legacy: top-8 among ALL nodes, i -> j
            for i in range(n):
                for j in np.argsort(-sim[i])[1:9]:
                    src.append(i); dst.append(int(j)); typ.append(2)
        else:
            # CAUSAL k-NN: each TARGET node j attends to its top-8 most-similar among nodes that are
            # EARLIER-or-equal to it ({i : _fwd(i, j)}), and we emit i -> j. Ranking over the causal
            # past ONLY makes j's incoming sim edges INVARIANT to any future node — so the prefix
            # graph reproduces them exactly (the parity invariant; a full-vs-prefix top-8-over-all
            # node set would otherwise shift edges and break parity, ~0.95 cosine instead of 1.0).
            _xfile = os.environ.get("AC_KNN_XFILE", "on") != "off"   # kNN CROSS-FILE only: a file's spans
            #   are mutually near-identical (cos 0.964) so they are each other's top-8 -> kNN re-forms the
            #   same-file CLIQUE the chain just removed. Excluding same-file candidates leaves kNN carrying
            #   only cross-file similarity (test<->source), the role it should play. (None-file != set-file
            #   is True, so reasoning/cross-file links survive; only same-file self-similarity is dropped.)
            if _fast:
                stp_a = np.asarray(stp, dtype=np.int64)
                idx_a = np.arange(n)
                _fm2: dict = {}
                codes = np.asarray([_fm2.setdefault(c.file, len(_fm2)) for c in chunks])
                for j in range(n):
                    fwd = (stp_a < stp_a[j]) if _strict else (
                        (stp_a < stp_a[j]) | ((stp_a == stp_a[j]) & (idx_a <= j)))
                    m = fwd & (idx_a != j)
                    if _xfile:
                        m &= codes != codes[j]
                    cand = np.nonzero(m)[0]
                    if not cand.size:
                        continue
                    cs = sim[j, cand]                    # same float32 values, C-speed gather
                    for k in np.argsort(-cs)[:8]:
                        src.append(int(cand[int(k)])); dst.append(j); typ.append(2)
            else:
                for j in range(n):
                    cand = [i for i in range(n) if i != j and _fwd(i, j)
                            and (not _xfile or chunks[i].file != chunks[j].file)]
                    if not cand:
                        continue
                    cs = np.asarray([sim[j, i] for i in cand])
                    for k in np.argsort(-cs)[:8]:
                        src.append(cand[int(k)]); dst.append(j); typ.append(2)
    # supersession: a near-duplicate pair (j newer than i). Legacy directs NEW->OLD (the newer copy
    # informs the old one it is covered); causal directs OLD->NEW (forward) so it carries the same
    # covered signal without flowing backward.
    if skip_supersession:
        # The v1/v2 serve path REPLACES rel-4 wholesale (v1graph drops every
        # et==4 edge and splices the client's pairs), so the O(n^2)
        # span-Jaccard double loop below fed the discard — ~4M Python
        # iterations at n=2000 (perf research 2026-09-02 §2.2). Opt-in
        # short-circuit; default emits the identical edges as before.
        return (torch.tensor([src, dst], dtype=torch.long) if src
                else torch.zeros((2, 0), dtype=torch.long),
                torch.tensor(typ, dtype=torch.long))
    sets = [{x for x in spans(c.text) if len(x) >= 5} for c in chunks]
    for i in range(n):
        for j in range(n):
            if chunks[j].step <= chunks[i].step or i == j:
                continue
            ov = 0.0
            ci, cj = chunks[i], chunks[j]
            if ci.file and cj.file == ci.file and ci.lo is not None and cj.lo is not None:
                inter = max(0, min(ci.hi, cj.hi) - max(ci.lo, cj.lo) + 1)
                ov = inter / max(1, ci.hi - ci.lo + 1)
            if ov < 0.6 and sets[i] and sets[j]:
                u = len(sets[i] | sets[j])
                if u:
                    ov = max(ov, len(sets[i] & sets[j]) / u)
            if ov >= 0.6:
                if causal:
                    src.append(i); dst.append(j); typ.append(4)   # OLD i -> NEW j (forward)
                else:
                    src.append(j); dst.append(i); typ.append(4)   # NEW j -> OLD i (legacy)
    return (torch.tensor([src, dst], dtype=torch.long) if src else torch.zeros((2, 0), dtype=torch.long),
            torch.tensor(typ, dtype=torch.long))


class ReadHeadPLR(nn.Module):
    """Readout head that lets the GNN capture the nonlinear TABULAR feature interactions a plain
    Linear-over-concat head wastes (the diagnosed 0.72->0.805 gap; raw scalars enter an MLP as
    degree-1 inputs and get swamped by the graph embedding). Three research-backed, INDUCTIVE +
    CACHE-SAFE pieces, all on the head — the RGCN trunk is untouched:
      (1) Periodic numeric embeddings (Gorishniy NeurIPS'22): each struct scalar -> [sin,cos] of
          trainable frequencies -> per-feature linear+ReLU. Turns flat scalars into a representation
          the head can actually use. We keep the BatchNorm-normalized raw cols too (categorical/binary
          one-hots survive intact).
      (2) DCN-v2 cross layers (Wang WWW'21): x_{l+1}=x0*(W x_l+b)+x_l -> explicit degree-(L+1) feature
          crosses (what a GBDT does), which an MLP only approximates.
      (3) Late fusion: cross-network tabular repr concatenated with the post-message-pass node
          embedding, then a small MLP -> logit.
    Inductive/cache-safe: a node's score is a pure function of its own struct (BatchNorm running stats
    + trainable freqs/weights, all frozen at serve) and its causal-prefix node embedding. No future,
    no other-node labels. BatchNorm uses batch stats in train (score_decided is called over the whole
    flat decided batch) and frozen running stats in eval -> train==serve parity under net.eval()."""

    def __init__(self, hidden: int, read_struct: int, de: int = 4, k: int = 6,
                 cross_layers: int = 2, drop: float = 0.3):
        super().__init__()
        self.F = read_struct
        self.bn = nn.BatchNorm1d(read_struct)
        self.freqs = nn.Parameter(torch.randn(read_struct, k))   # trainable per-feature frequencies
        self.feat_lin = nn.Linear(2 * k, de)                     # shared per-feature embed (2k -> de)
        tab_dim = read_struct + read_struct * de                 # normed raw cols + periodic embeds
        self.cross_w = nn.ModuleList([nn.Linear(tab_dim, tab_dim) for _ in range(cross_layers)])
        self.fuse = nn.Sequential(nn.Linear(hidden + tab_dim, hidden), nn.ReLU(),
                                  nn.Dropout(drop), nn.Linear(hidden, 1))

    def forward(self, h_idx, struct):
        xn = self.bn(struct)                                     # (B,F) normalized tabular
        v = (2.0 * math.pi) * xn.unsqueeze(-1) * self.freqs.unsqueeze(0)   # (B,F,k)
        pe = torch.relu(self.feat_lin(torch.cat([torch.sin(v), torch.cos(v)], dim=-1)))  # (B,F,de)
        x0 = torch.cat([xn, pe.flatten(1)], dim=1)               # (B, tab_dim)
        x = x0
        for w in self.cross_w:
            x = x0 * w(x) + x                                    # DCN-v2 cross layer
        return self.fuse(torch.cat([h_idx, x], dim=1)).squeeze(-1)


class PyGCurator(nn.Module):
    def __init__(self, emb_dim: int, struct_dim: int = 8, hidden: int = 256, layers: int = 3,
                 nrel: int = N_REL, drop: float = 0.3, use_emb: bool = True, arch: str = "rgcn",
                 doom_gf: int = 0, read_struct: int = 16, head_kind: str | None = None):
        super().__init__()
        self.use_emb = use_emb
        self.arch = arch
        self.doom_gf = doom_gf
        # projector width scales with the embedding input (content|action|head tri-embedding must
        # not be squeezed through the single-embedding bottleneck — w307 ranking dipped .896->.855)
        ew = 128 if emb_dim <= 1100 else 320
        self.emb_proj = nn.Linear(emb_dim, ew)
        self.in_proj = nn.Linear((ew if use_emb else 0) + struct_dim, hidden)
        # arch A/B: 'rgcn' (2018 lineage) vs attention message passing ('gatv2', 2021) vs graph-
        # transformer attention ('gt', Graphormer-style). Our graphs are <=70 nodes — global
        # attention is fully affordable; edge types ride in as learned edge features.
        if arch == "rgcn":
            self.convs = nn.ModuleList([RGCNConv(hidden, hidden, nrel) for _ in range(layers)])
        else:
            self.edge_emb = nn.Embedding(nrel, 32)
            Conv = GATv2Conv if arch == "gatv2" else TransformerConv
            self.convs = nn.ModuleList([Conv(hidden, hidden // 8, heads=8, edge_dim=32)
                                        for _ in range(layers)])
        self.norms = nn.ModuleList([nn.LayerNorm(hidden) for _ in range(layers)])
        self.drop = nn.Dropout(drop)
        self.head = nn.Sequential(nn.Linear(hidden, hidden), nn.ReLU(), nn.Dropout(drop),
                                  nn.Linear(hidden, 1))
        # value head V(state): pools the graph -> scalar baseline for advantage (reward - V). Added
        # for outcome (reward) training; absent from a dense-only warm-start ckpt (load strict=False).
        self.value_head = nn.Sequential(nn.Linear(hidden, hidden), nn.ReLU(), nn.Linear(hidden, 1))
        # doom head P(run is degenerating): pooled scalar trained on the mechanical doom labels.
        # Rides the SAME forward as curation -> the run-state signal costs zero extra tokens/calls.
        # doom_gf > 0: the mechanical loop-fraction trajectory (loop_feats) concatenates onto the
        # pooled state — the head sees the signal its labels are MADE FROM (the window pooling
        # alone can't: curation de-duplicates away the very repetition that defines doom).
        self.doom_head = nn.Sequential(nn.Linear(hidden + doom_gf, hidden), nn.ReLU(),
                                       nn.Linear(hidden, 1))
        # budget head q(decision): the per-DECISION drop budget — the parsec per-query-alpha /
        # dynamic-top-K pattern applied to curation. The per-chunk head RANKS; this head says how
        # deep to CUT for this specific cohort, trained on each decision's oracle dead-mass
        # fraction. Pools only the DECIDED chunks (the mask), not the whole graph.
        self.budget_head = nn.Sequential(nn.Linear(hidden, hidden), nn.ReLU(), nn.Linear(hidden, 1))
        # rule-utility head u_rule(rule_node): scores a candidate RULE node by LEARNED utility —
        # should this rule fire here / will it be applicable and followed? Rides the SAME R-GCN
        # forward: a candidate rule is added as a NODE connected to the task/decision, so message
        # passing gives it a TRACE-CONTEXT-informed representation (not just its raw text embedding,
        # which is all cosine sees). Trained jointly on the LLM-judged backfilled fires. Replaces
        # the cosine-0.30 firehose (78% NOT_APPLICABLE) with a learned selector.
        # input = [rule-node emb, its fire-STEP node emb] -> explicit rule x step (per-step decision);
        # one diluted message-pass hop wasn't enough for the head to tell which step a rule bites.
        self.rule_head = nn.Sequential(nn.Linear(hidden * 2, hidden), nn.ReLU(), nn.Dropout(drop),
                                       nn.Linear(hidden, 1))
        # TOOL-SCHEMA curation head u_tool(schema_node): the ONE head whose only job is keep/cut on the
        # static tool roster (attach_tools' step-0 schema nodes). On Claude Code the tool definitions are
        # ~26k tokens (~80% for tools never called) sitting in the request's tools FIELD — invisible to the
        # message curator, so this is a SEPARATE decision and a separate head. Scores a schema purely from
        # its contextualised node embedding (schema text + task/harness conditioning, message-passed); the
        # `used`-ever label supervises. Predicting it at birth = the dominant CC token saving. Persisted to
        # the ckpt; serving prunes the predicted-unused schemas from the tools field before the request.
        self.tool_head = nn.Sequential(nn.Linear(hidden, hidden), nn.ReLU(), nn.Dropout(drop),
                                       nn.Linear(hidden, 1))
        # SCOUT-GATE head g(brief_node, stats): ONE per-run decision at step 0 — serve the codescout
        # brief (ON) or nothing (OFF). Input = the brief node's message-passed embedding CONCAT 6
        # explicit scalars (brief-quality stats + neighbor brief-evidence; trace_graph.brief_stats).
        # Trained on the labeler's per-run helped/hurt 0/1 (gate_y). Additive head (strict=False).
        self.gate_head = nn.Sequential(nn.Linear(hidden + 6, hidden), nn.ReLU(), nn.Dropout(drop),
                                       nn.Linear(hidden, 1))
        # TRACE-LEVEL readout head (the design §5.2/5.3): the backbone message-passes the WHOLE run
        # graph once -> a contextualised embedding h per chunk; this head scores a chunk at a given
        # decision step from [h[chunk], struct_at_t] — struct is decision-relative (recency/dup/
        # supersession over the chunks in context AT that step), so it rides here, not in the node.
        # AC_HEAD=plr_cross: upgrade the readout to capture nonlinear TABULAR feature interactions
        # (periodic numeric embeddings + DCN-v2 cross + late fusion) — the diagnosed head bottleneck.
        # Default (unset) keeps the byte-identical shallow Linear-over-concat head. Persisted to the
        # ckpt by trace_run so serving rebuilds the SAME head shape.
        self._head_kind = head_kind or os.environ.get("AC_HEAD", "mlp")
        # AC_SKIP: over-smoothing fix. Dense same-file/kNN/command edges + N conv layers converge a file's
        # chunks to ~one vector (measured same-file cosine 0.964, needed-vs-dead 0.941 -> within-file
        # discrimination erased). Feed each chunk's PRE-message-passing repr (distinct per span) into the
        # readout alongside the smoothed h, so within-file content survives. Persisted to the ckpt.
        self._skip = os.environ.get("AC_SKIP") == "on"
        if self._head_kind == "plr_cross":
            self.read_head = ReadHeadPLR(hidden, read_struct, drop=drop)
        else:
            _in = hidden + (hidden if self._skip else 0) + read_struct
            self.read_head = nn.Sequential(nn.Linear(_in, hidden), nn.ReLU(),
                                           nn.Dropout(drop), nn.Linear(hidden, 1))

    def score_decided(self, h, idx, struct):
        """Per-step utility logits for the decided chunks (rows `idx`) given their step-`t` struct."""
        if isinstance(self.read_head, ReadHeadPLR):
            return self.read_head(h[idx], struct)
        parts = [h[idx]]
        if self._skip and getattr(self, "_h0", None) is not None:
            parts.append(self._h0[idx])          # pre-MP repr — counters within-file over-smoothing
        parts.append(struct)
        return self.read_head(torch.cat(parts, dim=1)).squeeze(-1)

    def score_tools(self, h, tool_idx):
        """Keep/cut logits for the tool-schema nodes (rows `tool_idx`) from their message-passed
        embeddings. Pure function of schema text + task/harness conditioning (no future calls) ->
        causal cut-at-birth + serve-mirrorable. Returns () when there are no tool nodes."""
        if tool_idx is None or (hasattr(tool_idx, "numel") and tool_idx.numel() == 0) or len(tool_idx) == 0:
            return h.new_zeros((0,))
        return self.tool_head(h[tool_idx]).squeeze(-1)

    def score_gate(self, h, gate_idx, gstats=None):
        """Serve-the-brief logits for the gate/brief nodes (rows `gate_idx`); () when none.
        gstats (G,6): brief-quality + neighbor brief-evidence scalars (trace_graph.brief_stats);
        zeros when unavailable — fail-open."""
        if gate_idx is None or (hasattr(gate_idx, "numel") and gate_idx.numel() == 0) or len(gate_idx) == 0:
            return h.new_zeros((0,))
        hg = h[gate_idx]
        gs = hg.new_zeros((hg.shape[0], 6)) if gstats is None else gstats.to(hg.dtype).to(hg.device)
        return self.gate_head(torch.cat([hg, gs], dim=1)).squeeze(-1)

    def forward_rules(self, xe, xs, ei, et, rule_idx):
        """Utility logits for the rule nodes (rows `rule_idx`), from their R-GCN representations."""
        h = self._embed_nodes(xe, xs, ei, et)
        return self.rule_head(h[rule_idx]).squeeze(-1)

    def score_rules(self, h, rule_idx, step_idx):
        """Per-(rule,step) relevance/helpful logits: head([h[rule], h[step]]) — the rule node emb
        concatenated with its fire-STEP node emb, giving the head EXPLICIT rule x step context
        (one message-pass hop alone couldn't discriminate which step a rule bites). Returns ()
        when there are no rule nodes."""
        if rule_idx is None or (hasattr(rule_idx, "numel") and rule_idx.numel() == 0) or len(rule_idx) == 0:
            return h.new_zeros((0,))
        return self.rule_head(torch.cat([h[rule_idx], h[step_idx]], dim=1)).squeeze(-1)

    def _embed_nodes(self, xe, xs, ei, et):
        feats = [torch.relu(self.emb_proj(xe)), xs] if self.use_emb else [xs]
        h = torch.relu(self.in_proj(torch.cat(feats, -1)))
        self._h0 = h                              # AC_SKIP: distinct pre-MP node repr, read off at the head
        ea = self.edge_emb(et) if self.arch != "rgcn" else None
        for conv, norm in zip(self.convs, self.norms):
            m = conv(h, ei, et) if self.arch == "rgcn" else conv(h, ei, ea)
            h = norm(h + self.drop(torch.relu(m)))
        return h

    def forward(self, xe, xs, ei, et):
        return self.head(self._embed_nodes(xe, xs, ei, et)).squeeze(-1)

    def forward_uv(self, xe, xs, ei, et):
        """Per-node utility logits AND the pooled state value V(state) — for reward/advantage training."""
        h = self._embed_nodes(xe, xs, ei, et)
        u = self.head(h).squeeze(-1)
        v = self.value_head(h.mean(dim=0, keepdim=True)).reshape(())
        return u, v

    def _doom_in(self, pooled, gfeat):
        if not self.doom_gf:
            return pooled
        g = (gfeat if gfeat is not None
             else torch.zeros(self.doom_gf, dtype=pooled.dtype, device=pooled.device))
        return torch.cat([pooled, g.reshape(1, -1)], dim=1)

    def forward_uvd(self, xe, xs, ei, et, n_cur=None, gfeat=None):
        """Utility logits + value + doom logit — one forward serving all three control surfaces.
        n_cur = number of CURRENT-trace nodes (chunks + task): value/doom pool over those ONLY.
        Audit fix: pooling over the full graph let ~90% neighbor mass (per-trace-CONSTANT) drown
        the per-decision state signal once neighborhoods attached."""
        h = self._embed_nodes(xe, xs, ei, et)
        u = self.head(h).squeeze(-1)
        pooled = (h[:n_cur] if n_cur else h).mean(dim=0, keepdim=True)
        return (u, self.value_head(pooled).reshape(()),
                self.doom_head(self._doom_in(pooled, gfeat)).reshape(()))

    def forward_uvdq(self, xe, xs, ei, et, mask=None, n_cur=None, gfeat=None):
        """All heads + the per-decision drop-budget logit q (pooled over the DECIDED chunks)."""
        h = self._embed_nodes(xe, xs, ei, et)
        u = self.head(h).squeeze(-1)
        pooled = (h[:n_cur] if n_cur else h).mean(dim=0, keepdim=True)
        mpool = h[mask].mean(dim=0, keepdim=True) if mask is not None and mask.any() else pooled
        return (u, self.value_head(pooled).reshape(()),
                self.doom_head(self._doom_in(pooled, gfeat)).reshape(()),
                self.budget_head(mpool).reshape(()))

    @torch.no_grad()
    def score(self, xe, xs, ei, et) -> np.ndarray:
        self.eval()
        return torch.sigmoid(self(xe, xs, ei, et)).cpu().numpy()
