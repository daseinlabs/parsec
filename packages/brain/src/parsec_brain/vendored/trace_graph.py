"""TRACE-LEVEL graph builder — ONE GRAPH PER RUN + CAUSAL EDGE MASK (Option 1, reverted).

VENDORED TRIM (parsec-brain serving): only the SERVE-side surface survives — build_tool_spec,
the scout/brief sidecar helpers, and the issue/file identity helpers the scorer's readout mirror
imports. The corpus loaders, needed-label machinery and build_trace_graph (training) are deleted;
the kept function bodies are byte-identical to the rulehead drop that trained curator_v4_prod.pt.

The bug the per-decision/per-prefix edition fixed (future-node leakage) is now fixed a cheaper way:
ONE graph per run (every chunk a node, sorted by step) message-passed ONCE, but the edges are
DIRECTED EARLIER->LATER (pyg_model.edges / attach_task / attach_blocks). Because RGCNConv aggregates
a TARGET from its SOURCES and no edge ever points from a later step to an earlier one, node t's
embedding depends ONLY on nodes whose step <= t — the future chunks in the graph cannot affect it.
"""
from __future__ import annotations

import os
import re

import numpy as np

# Trace-path read-atom granularity G. Default 10 (the granularity_probe knee: ~2.5-3x the G=40 atom
# count, capturing most of the per-line droppable-mass gain at a fraction of the atoms). The LEGACY
# per-decision/w326 path does NOT pass read_lines -> it stays at the win=40 default, byte-identical.
CHUNK_LINES = int(os.environ.get("AC_CHUNK_LINES", "10"))

from .chunking import (accumulated_chunks, assistant_chunks_of,
                       reasoning_chunks_of, steps_of)
from .torch_curator import node_struct_with_type

_SCOUT_CACHE = None
_SC_IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]{3,}")


def _scout_map():
    """{iid: {files:set, tokens:set, spans:{basename:[(lo,hi,hop)..]}, lead_files:set}} — the
    codescout walk artifacts (generated anyway per task; persisted by the backfill). Zeros when a
    task is absent -> the model trains under missingness instead of depending on the scout."""
    global _SCOUT_CACHE
    if _SCOUT_CACHE is None:
        sp = os.environ.get("AC_SCOUTFEAT", "")
        try:
            import pickle as _spk
            _SCOUT_CACHE = _spk.load(open(sp, "rb")) if sp and os.path.exists(sp) else {}
        except Exception:
            _SCOUT_CACHE = {}
    return _SCOUT_CACHE


def scout_feats(decided, iid_or_rec):
    """The 4 codescout-walk readout cols per decided chunk: [in-seed-file, 1/(1+hop) of the chunk's
    span in the walk, seed-token overlap frac, in-lead-file]. O(1) dict lookups per chunk vs the
    per-TASK walk artifacts; zeros when absent. SINGLE SOURCE for trainer (_readout) and serving
    (curator._het_readout) — the hand-mirror drift rule. Accepts the iid (trainer) or the resolved
    task record dict (serving, which has no iid)."""
    _sm = iid_or_rec if isinstance(iid_or_rec, dict) else (_scout_map().get(iid_or_rec) or {})
    _sfiles = _sm.get("files") or set()
    _stoks = _sm.get("tokens") or set()
    _sspans = _sm.get("spans") or {}
    _slead = _sm.get("lead_files") or set()
    sc4 = np.zeros((len(decided), 4), dtype=np.float32)
    if _sfiles or _stoks:
        for _k, _c in enumerate(decided):
            _cf = _c.file or _chunk_file(_c) or ""
            _bn = _cf.split("/")[-1].lower()
            if _bn and _bn in _sfiles:
                sc4[_k, 0] = 1.0
            if _bn and _bn in _slead:
                sc4[_k, 3] = 1.0
            _sp = _sspans.get(_bn)
            if _sp and getattr(_c, "lo", None) is not None:
                _hi = _c.hi if getattr(_c, "hi", None) is not None else _c.lo
                _hops = [h for (a, b, h) in _sp if a <= _c.lo <= b or a <= _hi <= b]
                if _hops:
                    sc4[_k, 1] = 1.0 / (1.0 + float(min(_hops)))
            if _stoks:
                _cw = set(w.lower() for w in _SC_IDENT_RE.findall((_c.text or "")[:2000]))
                if _cw:
                    sc4[_k, 2] = len(_cw & _stoks) / max(len(_stoks), 1)
    return sc4


def brief_stats(brief, task_text, rec=None):
    """6 gate-conditioning scalars — SHARED trainer/serving (drift rule): [log1p(#lead files),
    log1p(#seed files), log1p(brief tokens), brief∩issue identifier overlap frac, neighbor
    brief-evidence helped-rate, log1p(evidence n)]. Zeros when unavailable — fail-open."""
    rec = rec or {}
    b = brief or ""
    bi = set(w.lower() for w in _SC_IDENT_RE.findall(b[:4000]))
    ti = set(w.lower() for w in _SC_IDENT_RE.findall((task_text or "")[:4000]))
    ev = rec.get("evidence") or [0.0, 0.0]
    return np.asarray([
        np.log1p(float(len(rec.get("lead_files") or ()))),
        np.log1p(float(len(rec.get("files") or ()))),
        np.log1p(len(b) / 4.0),
        (len(bi & ti) / max(len(ti), 1)) if ti else 0.0,
        float(ev[0]), np.log1p(float(ev[1])),
    ], dtype=np.float32)


def scout_rec_for(task_text):
    """Resolve a task's sidecar record by ps200 substring (for callers with no iid, e.g. the
    gate CLI); {} when unresolved — fail-open."""
    for _iid, _r in (_scout_map() or {}).items():
        _ps = (_r.get("ps200") or "").strip()
        if _ps and _ps in (task_text or ""):
            return _r
    return {}


def _msg_text(m):
    c = m.get("content", "")
    if isinstance(c, list):
        c = " ".join(p.get("text", "") for p in c if isinstance(p, dict))
    return c or ""


def build_tool_spec(messages, tools, iid="serve"):
    """SERVE-TIME spec for the tool-schema head: the SAME chunk pipeline build_trace_graph uses
    (steps_of -> accumulated_chunks + assistant/reasoning chunks, sorted by step) + the request's
    tool roster as tool_nodes, but NO needed-labels and NO obs readouts (the tool head scores the
    static roster, conditioned on task only). Returns a spec assemble_trace consumes, or None.

    `messages` is the internal flat list (anthropic_shapes.to_internal output); its assistant
    actions already carry bash-twin `command`s (so reads/greps type correctly). `tools` is the
    request `tools` array (native Anthropic tool defs)."""
    from .trace_contract import tool_schema_chunks
    tool_nodes = tool_schema_chunks(tools or [])
    if not tool_nodes:
        return None
    for tn in tool_nodes:
        tn.setdefault("used", 0.0)                        # serving: no label; assemble_trace reads tool_y
    steps = steps_of(messages)
    T = max(1, len(steps))
    chunks = sorted(accumulated_chunks(steps, T - 1, read_lines=CHUNK_LINES)
                    + [ac for ac in assistant_chunks_of(messages) if ac.step <= T - 1]
                    + [rc for rc in reasoning_chunks_of(messages) if rc.step <= T - 1],
                    key=lambda c: c.step)
    # PARSEC-PATCH: step-0 fallback taken verbatim from the reference SERVING copy
    # (adaptive-context-clean/scripts/trace_graph.py) — the rulehead training drop lacks it, but
    # the proxy freezes the tool keep-set on the FIRST request (no observations yet); without it
    # assemble_trace crashes on an empty chunk set and step-0 pruning permanently fails open.
    if not chunks:                                       # step-0 tool-spec: no obs yet -> condition
        from .chunking import chunk_observation          # the head on the TASK
        _tt = next((_msg_text(m) for m in messages if m.get("role") == "user"
                    and _msg_text(m).strip()), "")[:2000]
        chunks = chunk_observation("", _tt, 0, read_lines=CHUNK_LINES)
    task_text = next((_msg_text(m) for m in messages
                      if m.get("role") == "user" and _msg_text(m).strip()), "")[:2000]
    sys_text = next((_msg_text(m) for m in messages
                     if m.get("role") == "system" and _msg_text(m).strip()), "")[:2000]
    return dict(chunks=chunks, node_struct=node_struct_with_type(chunks),
                task_text=task_text, sys_text=sys_text, iid=iid, readouts=[],
                tool_nodes=tool_nodes)


_OF_PATH_RE = re.compile(r'([\w./\-]+\.(?:py|rst|txt|cfg|ini|toml|md|json|yaml|yml|c|h|cpp|js|ts))')
_OF_EDIT_RE = re.compile(r'The file (\S+) has been updated')


def _chunk_file(c):
    """File IDENTITY for ANY chunk. file/grep chunks carry .file; 'other' chunks (edit-echo, git/cmd
    output) bury the path in .text with .file=None, so every file-keyed feature is blind to them. The
    dig showed needed 'other' chunks reference an issue-named / already-active file 8-17x more than dead
    ones. This recovers that identity (basename) WITHOUT touching c.file (the label keys on c.file)."""
    f = getattr(c, "file", None)
    if f:
        return f.rsplit("/", 1)[-1]
    t = c.text or ""
    m = _OF_EDIT_RE.search(t) or _OF_PATH_RE.search(t)
    return m.group(1).rsplit("/", 1)[-1] if m else None


# SPAN-LEVEL issue match (the read-class binding signal): which 10-line span of a whole-file read is the
# fix locus? At birth the only legal cue is whether the span TEXT carries an identifier named in the ISSUE
# (turn-0, causal). File-level identity failed on reads (popular-file != relevant-span); span-level symbol
# match is finer — it distinguishes spans WITHIN a file. dotted/camel/snake idents >= 6 chars.
_IDENT_RE = re.compile(r'[A-Za-z_][A-Za-z0-9_]{5,}(?:\.[A-Za-z_][A-Za-z0-9_]*)*')
_DEFCLASS_RE = re.compile(r'(?:^|\n)\s*(?:def|class)\s+([A-Za-z_][A-Za-z0-9_]*)')


def _issue_idents(task_text):
    return {m.group(0) for m in _IDENT_RE.finditer(task_text or "")}


_TB_FRAME_RE = re.compile(r'File "([^"]+)"')


def _tb_frame_files(task_text):
    """Basenames named in the issue's Python traceback frames (File "...", line N) — prime fix-locus."""
    return {m.group(1).rsplit("/", 1)[-1] for m in _TB_FRAME_RE.finditer(task_text or "")}
