"""Fast context-curator GNN in PyTorch (CPU now, GPU-ready). Predicts per-chunk future-use so the
curator keeps what will be reused and evicts stale chunks (test dumps it's done with, one-off reads).

Why torch not the numpy autograd: vectorized matmuls + no Python-per-op overhead -> trains the whole
30-trace dataset in seconds on CPU and moves to GPU unchanged when we scale to thousands of traces.

Model: per chunk x = [embedding | cheap structural features]. One message-passing step over a
same-file adjacency (chunks of one file inform each other), then a sigmoid head -> P(still needed).
"""
from __future__ import annotations

import re

import numpy as np

STRUCT = ["age", "log_tokens", "is_code", "file_focus", "overlap_recent",
          "kind_read", "kind_grep", "kind_other", "kind_reasoning",
          "sup_newer", "dup_earlier", "token_frac", "step_pos", "is_current"]


def struct_features(chunks, recent_text: str, rows: list[int] | None = None) -> np.ndarray:
    from .attribution import spans
    from .chunking import dup_feats_fast
    rec = {s for s in spans(recent_text) if len(s) >= 5}
    maxstep = max((c.step for c in chunks), default=1)
    ff = {}
    for c in chunks:
        ff[c.file] = ff.get(c.file, 0) + 1
    # rows: emit ONLY those chunk indices (in order). The set-relative terms (ff/tot_tok/maxstep) are
    # still computed over the FULL chunk set, so each emitted row is byte-identical to the full pass.
    sup, dup = dup_feats_fast(chunks, rows=rows)         # O(n) HASH dup (this is the per-curate SERVING
    #   feature, so it must be cheap; the old dup_stats span-Jaccard was ~160s of a 137s serve run). The
    #   thorough fuzzy dup_stats stays the OFFLINE label (trace_graph) where cost is irrelevant.
    tot_tok = sum(c.tokens for c in chunks) or 1
    obs_tok = {}                                                   # total tokens of each chunk's obs
    for c in chunks:
        k = (c.step, c.cmd)
        obs_tok[k] = obs_tok.get(k, 0) + c.tokens
    X = []
    for i in (range(len(chunks)) if rows is None else rows):
        c = chunks[i]
        cs = {s for s in spans(c.text) if len(s) >= 5} if rec else None   # skip the per-chunk spans()
        #   tokenize entirely when recent_text is "" (the node_struct/serve path): the only consumer is
        #   the recent-overlap col below, which is 0 against an empty rec -> bit-identical, no regex.
        ot = obs_tok[(c.step, c.cmd)]
        # NO keyword tables, NO format-specific parsing — domain semantics (what action produced
        # this, how it ended) ride in as EMBEDDINGS of c.cmd and the observation head, attached in
        # the model input. Scalars here are domain-free shape only.
        X.append([
            np.log1p(ot) / 8.0,                                    # how big the whole dump was
            c.tokens / max(1, ot),                                 # this chunk's share of its dump
        ] + [
            (maxstep - c.step) / max(1, maxstep),                 # age (0 newest .. 1 oldest)
            np.log1p(c.tokens) / 8.0,
            1.0 if c.file else 0.0,
            ff[c.file] / max(1, len(chunks)) if c.file else 0.0,   # file focus
            len(cs & rec) / max(1, len(cs)) if cs else 0.0,        # overlap with recent actions
            1.0 if c.kind == "read" else 0.0,
            1.0 if c.kind == "grep" else 0.0,
            1.0 if c.kind == "other" else 0.0,
            1.0 if c.kind == "reasoning" else 0.0,
            sup[i],                                                # covered by a NEWER chunk (0..1)
            min(1.0, dup[i] / 5.0),                                # earlier near-duplicates (loop signal)
            c.tokens / tot_tok,                                    # share of the window this chunk costs
            c.step / max(1, maxstep),                              # relative position in the trajectory
            1.0 if c.step >= maxstep else 0.0,                     # the observation being acted on NOW
        ])
    return np.asarray(X, dtype=np.float32).reshape(-1, 16)         # (0,16) for empty -> step-0 tool-spec


def decided_extra_feats(decided, alive, task_text: str) -> np.ndarray:
    """ARM-1 extra per-DECIDED readout features, computed by ONE function called identically in the
    trainer (trace_graph._readout) AND serving (curator._trace_scores) -> train==serve parity by
    construction (the recurring break is duplicated feature code drifting; sharing this avoids it).
    Returns (len(decided), 3): [rc0, task_overlap, dup_recency].
      rc0          : 1.0 if the observation exited 0 (success) else 0.0 — rc as an EXPLICIT scalar the
                     head can weight, instead of only riding the obs-head embedding ('rc0->keep' confound).
      task_overlap : graded rare-token overlap (>=8 chars) of the atom with the task statement — a
                     'relevant-to-the-goal' signal the model lacks (err-trends: AUC~0.37 toward needed;
                     helps the first-sighting needed reads it currently cuts).
      dup_recency  : recency-weighted count of EARLIER alive near-duplicates (span/line overlap>=0.6) of
                     this atom -> surfaces the 42%-near-dup re-read signal straight into read_head (the
                     #1 mis-key: 'seen this file -> keep'). dup_earlier is already in struct_features but
                     drowned; this is the dedicated, recency-aware handle.
    """
    from .attribution import spans
    tsp = {s for s in spans(task_text or "") if len(s) >= 8}
    asets = [({x for x in spans(d.text or "") if len(x) >= 5}, d) for d in alive]   # precompute once
    out = []
    for c in decided:
        rc0 = 1.0 if getattr(c, "rc", None) == 0 else 0.0
        cl = {s for s in spans(c.text or "") if len(s) >= 8}
        tov = min(1.0, len(cl & tsp) / 3.0) if cl and tsp else 0.0
        cset = {x for x in spans(c.text or "") if len(x) >= 5}
        dr = 0.0
        for ds, d in asets:
            if d is c or d.step >= c.step:
                continue
            ov = 0.0
            if c.file and d.file == c.file and c.lo is not None and d.lo is not None:
                ov = max(0, min(c.hi, d.hi) - max(c.lo, d.lo) + 1) / max(1, c.hi - c.lo + 1)
            if ov < 0.6 and cset and ds:
                u = len(cset | ds)
                if u:
                    ov = max(ov, len(cset & ds) / u)
            if ov >= 0.6:
                dr += 1.0 / (1.0 + (c.step - d.step))      # nearer re-reads weigh more
        out.append([rc0, tov, min(dr, 3.0)])
    return np.asarray(out, dtype=np.float32).reshape(len(decided), 3)




_TB_RE = re.compile(r'File "([^"]+)"')


def decided_rerank_feats(decided, alive, steps, t: int) -> np.ndarray:
    """FILE-RERANKER raw signals as FIRST-CLASS per-decided features (the future-relevance axis the
    curator lacks — distinct from content-dedup in decided_extra_feats). All CAUSAL (from the prefix
    at step t) and serve-mirrorable (curator builds steps_of(prefix) identically). Returns (N, 8):
      recency_raw  : steps since this file was last alive earlier (T if first read).
      recency_inv  : 1/(1+recency) — live working file ~1, stale ~0.
      in_tb        : file appears in a File "..." traceback of the last observation (near-deterministic
                     'about to be opened' flag — the reranker's strongest cue).
      in_obs       : file appears in the last observation (in the live working set).
      in_lastcmd   : file appears in the last/current command.
      is_test      : 'test' in basename.
      first_read   : 1.0 if never seen earlier (a fresh first read, not a re-admission).
      depth        : path depth recovered from the producing command (top-level vs deep).
    The FP-math shows FPs are touched-again ~71% (so the reranker SCORE can't cut them); these RAW
    cues + dup_recency (content dedup) + absolute step let the GNN split stale / dup / fresh FPs."""
    import os.path as _osp
    T = len(steps)
    last_obs = steps[t - 1][1] if 0 < t <= T and t - 1 < T else ""
    last_cmd = steps[t - 1][0] if 0 < t <= T and t - 1 < T else ""
    cur_cmd = steps[t][0] if 0 <= t < T else ""
    tb = _TB_RE.findall(last_obs)
    out = []
    for c in decided:
        f = c.file or ""
        earlier = [a.step for a in alive if f and a.file == f and a.step < c.step]
        if earlier:
            rec = float(c.step - max(earlier)); first = 0.0
        else:
            rec = float(T); first = 1.0
        in_tb = 1.0 if (f and any(f in x for x in tb)) else 0.0
        in_obs = 1.0 if (f and f in last_obs) else 0.0
        in_cmd = 1.0 if (f and (f in last_cmd or f in cur_cmd)) else 0.0
        is_test = 1.0 if (f and "test" in _osp.basename(f)) else 0.0
        depth = 0.0
        if f and c.cmd:
            mm = re.findall(r"/[\w./-]+/" + re.escape(f), c.cmd)
            if mm:
                depth = float(mm[0].count("/"))
        out.append([rec, 1.0 / (1.0 + rec), in_tb, in_obs, in_cmd, is_test, first, depth])
    return np.asarray(out, dtype=np.float32).reshape(len(decided), 8)


def struct_features_causal(chunks) -> np.ndarray:
    """PREFIX-INVARIANT node_struct: each node's struct row computed over ITS CAUSAL PREFIX (chunks
    with step <= that node's step), never the future.

    Why this exists: struct_features normalizes several scalars over the chunk SET (maxstep -> age/
    step_pos/is_current; len(chunks)+file counts -> file_focus; tot_tok -> token_frac; and sup_newer
    from dup_stats, which looks at NEWER chunks). Computed over the FULL run (training) those terms
    see the future, so a node's row differs from the same node's row computed over the LIVE PREFIX
    (serving) -> [serve] parity craters. Computing each node over its causal prefix makes the row
    IDENTICAL whether the graph is the full run or the live prefix at any cut t >= node.step: at a
    cut t the latest step in the prefix is t, and struct_features(prefix_t) for a node at step t is
    EXACTLY struct_features(prefix_node.step) for that node. So node_struct(full)[node] ==
    node_struct(prefix_t)[node] for every node in the prefix -> [serve] cosine ~= 1.000.

    Vectorised per DISTINCT step (one struct_features call per distinct step value, not per node), so
    it is O(distinct_steps * N) not O(N^2). recent_text is fixed "" (node_struct is recent-free; the
    decision-relative recent overlap rides in the per-step readout dstruct, which already runs over
    the live alive set in both train and serve)."""
    n = len(chunks)
    if n == 0:
        return struct_features(chunks, "")                 # empty -> correct-width (0, D) array
    # nodes sharing a step get their rows from the SAME causal-prefix struct_features call.
    by_step: dict[int, list[int]] = {}
    for i, c in enumerate(chunks):
        by_step.setdefault(c.step, []).append(i)
    out = None
    for t, idxs in by_step.items():
        prefix = [c for c in chunks if c.step <= t]        # causal prefix (step <= t)
        sf = struct_features(prefix, "")                   # set-relative terms see ONLY the past
        if out is None:                                    # width from struct_features, not len(STRUCT)
            out = np.zeros((n, sf.shape[1]), dtype=np.float32)
        # rows of the prefix AT step t are exactly the nodes we fill for this step. Map by identity:
        # struct_features preserves input order, so prefix[r] is the same object as some chunks[i].
        prow = {id(c): r for r, c in enumerate(prefix)}
        for i in idxs:
            out[i] = sf[prow[id(chunks[i])]]
    return out


STRUCT_TYPE = ["frac_signature", "frac_import", "frac_decl", "frac_body", "frac_comment"]
# python control-flow keywords that must NOT be read as a C/Java method signature (they end in `:`
# and superficially look like `kw expr(...):`). Guarded out of the C-signature branch below.
_CTRL = r"(?:for|while|if|elif|else|with|try|except|finally|return|yield|raise|assert|del|print|switch|case)"
_SIG_RE = re.compile(r"^\s*(?:async\s+def|def|class|@)\b"                      # py headers/decorators
                     r"|^\s*(?:function|func|fn|public|private|protected)\b"   # js/go/rust/java-ish
                     r"|^\s*(?!" + _CTRL + r"\b)[\w<>\[\], *&]+\s+\w+\s*\([^;{]*\)\s*\{\s*$")  # C/Java method sig (brace-opened)
_IMPORT_RE = re.compile(r"^\s*(?:import|from|#include|require|use)\b")
_DECL_RE = re.compile(r"^\s*[A-Za-z_][\w.]*\s*(?::[^=]+)?=(?!=)"               # NAME = / NAME: T = ...
                      r"|^\s*(?:const|let|var|val|static|final|public|private|export)\b.*=")
_COMMENT_RE = re.compile(r"^\s*(?:#|//|/\*|\*|--|;)|^\s*(?:'''|\"\"\")")       # line/block comments + docstrings


def _line_type(ln: str) -> int:
    """Deterministic per-line structural class index: 0 signature, 1 import, 2 decl, 3 body, 4 comment.
    Language-agnostic-ish (python-first). Pure regex on the line text — NO model, NO state, so it is
    identical at train and serve. A fully-blank line never reaches here (callers pass kept lines)."""
    if _COMMENT_RE.match(ln):
        return 4
    if _IMPORT_RE.match(ln):
        return 1
    if _SIG_RE.match(ln):
        return 0
    if _DECL_RE.match(ln):
        return 2
    return 3                                                   # everything else executable -> body


def struct_type_features(chunks, rows: list[int] | None = None) -> np.ndarray:
    """Per-atom STRUCTURAL-TYPE fractions (the substrate for a finer GNN): for each chunk, classify
    its NON-BLANK lines into {signature, import, decl, body, comment} and emit the 5 FRACTION columns
    (sum to 1 over non-blank lines; all-zero only for a chunk with no non-blank lines). Deterministic
    and prefix-INVARIANT (depends only on the atom's own text), so it is granularity-safe and identical
    in training (trace_graph) and serving (curator._trace_scores). Order matches STRUCT_TYPE.

    rows: if given, emit ONLY those chunk indices (in order) — own-text only, so byte-identical."""
    idxs = list(range(len(chunks))) if rows is None else list(rows)
    out = np.zeros((len(idxs), len(STRUCT_TYPE)), dtype=np.float32)
    for r, i in enumerate(idxs):
        c = chunks[i]
        lines = [ln for ln in (c.text or "").splitlines() if ln.strip()]
        if not lines:
            continue
        counts = np.zeros(len(STRUCT_TYPE), dtype=np.float32)
        for ln in lines:
            counts[_line_type(ln)] += 1.0
        out[r] = counts / counts.sum()
    return out


def node_struct_with_type(chunks) -> np.ndarray:
    """The TRACE-path node struct: prefix-invariant struct_features_causal + the per-atom structural-
    type fractions appended as NEW trailing columns. Used identically by trace_graph.build_trace_graph
    (training) and curator._trace_scores (serving) so train==serve node width stays consistent. The
    legacy per-decision/w326 path does NOT call this (it uses struct_features directly) -> untouched."""
    base = struct_features_causal(chunks)
    typ = struct_type_features(chunks)
    return np.hstack([base, typ]).astype(np.float32)
