"""Symbol-graph centrality signal (Aider repo-map style) — a tree-sitter feature for the curator GNN.

The GNN's graph today is similarity/temporal/same-file edges over BGE embeddings; it has NO code-
dependency structure, so it can't tell "central, referenced-everywhere code" from "one-off peripheral
read" — the exact discriminator the tail-failure analysis says is missing. This module supplies it:
tree-sitter extracts per-chunk DEFINED and REFERENCED symbols, we build a directed file graph
(referencer -> definer, weighted a la Aider's repomap), run PageRank over the CAUSAL alive set at a
readout (power iteration; no networkx dep -> trivially serve-portable), and emit per-decided-atom
features: how central is this atom's FILE, and does this atom DEFINE a central symbol.

Fed as READOUT features INTO the GNN (same place as decided_extra_feats; never a hard cut), so it is
causal (alive = steps<=t, identical at serve) and respects the "deterministic signal -> GNN input,
never replace the GNN" rule. Python-only extraction (~92% of corpus read-mass); other langs / parse
failures -> zero centrality (graceful). Bench binding = tree-sitter-language-pack PyO3: parse(str),
callable accessors (root_node/kind/start_position), child_by_field_name present, NO node.text (slice
the source by start/end Points).
"""
import math
import os

import numpy as np

K_CENTRALITY = 5   # [file_pr, def_rank, defines_any, log1p(n_defs), log1p(n_refs)] per decided atom

_PY_EXT = (".py", ".pyi")
_DEF_KINDS = ("function_definition", "class_definition")


def _call(x):
    return x() if callable(x) else x


def _pt(node, which):
    p = _call(getattr(node, which))
    r = p.row if hasattr(p, "row") else (p[0] if isinstance(p, (tuple, list)) else int(p))
    c = p.column if hasattr(p, "column") else (p[1] if isinstance(p, (tuple, list)) and len(p) > 1 else 0)
    return r, c


def _text(node, lines):
    """Source text of a node, sliced by its start/end Points (this binding has no node.text)."""
    r0, c0 = _pt(node, "start_position")
    r1, c1 = _pt(node, "end_position")
    if not (0 <= r0 < len(lines)):
        return ""
    if r0 == r1:
        return lines[r0][c0:c1]
    seg = [lines[r0][c0:]]
    for r in range(r0 + 1, min(r1, len(lines) - 1) + 1):
        seg.append(lines[r][:c1] if r == r1 else lines[r])
    return "\n".join(seg)


def _name_of(node, lines):
    """The def-name identifier of a function/class node (field 'name', else first child identifier)."""
    nm = node.child_by_field_name("name") if hasattr(node, "child_by_field_name") else None
    if nm is None:
        for i in range(_call(node.named_child_count)):
            ch = node.named_child(i)
            if _call(ch.kind) == "identifier":
                nm = ch
                break
    return _text(nm, lines) if nm is not None else ""


# PARSEC-PATCH(broken-grammar fallback): the language pack's prebuilt python grammar can fail to
# load in some serving environments (observed here: x86_64 Rosetta python dlopen'ing an arm64
# grammar dylib -> LanguageNotFoundError). The reference silently fail-opened (chunk_symbols
# catches everything) and served ZERO symbols, so the centrality readout cols (37-41) were zero
# while the checkpoint TRAINED on real parsing — a quietly-off-distribution brain, not a crash.
# Fall back to the official tree-sitter-python wheel (0.23.6 — the same grammar version the
# engine port compiles; see packages/engine/parity/gen_readout_fixtures.py for the original
# shim), wrapped to the PyO3-binding surface this module expects (parse(str), .kind,
# .start_position/.end_position Points, .named_child_count, child_by_field_name).
# bundle.load_bundle logs which backend is live at startup (loud warning when neither is).

_PY_PARSER = None   # cached (parser|None, backend) after the first probe


class _TSPNode:
    __slots__ = ("_n",)

    def __init__(self, n):
        self._n = n

    @property
    def kind(self):
        return self._n.type

    @property
    def start_position(self):
        return self._n.start_point          # Point(row, column) — column in BYTES

    @property
    def end_position(self):
        return self._n.end_point

    @property
    def named_child_count(self):
        return self._n.named_child_count

    def named_child(self, i):
        return _TSPNode(self._n.named_child(i))

    def child_by_field_name(self, name):
        c = self._n.child_by_field_name(name)
        return None if c is None else _TSPNode(c)


class _TSPTree:
    __slots__ = ("_t",)

    def __init__(self, t):
        self._t = t

    @property
    def root_node(self):
        return _TSPNode(self._t.root_node)


class _TSPParser:
    def __init__(self):
        import tree_sitter as _ts
        import tree_sitter_python as _tsp
        self._p = _ts.Parser(_ts.Language(_tsp.language()))

    def parse(self, src):
        return _TSPTree(self._p.parse(src.encode("utf-8") if isinstance(src, str) else src))


def _probe(parser) -> bool:
    """True when `parser` actually serves the surface _py_defs_refs consumes."""
    try:
        root = _call(parser.parse("def _p(x):\n    return _q(x)\n").root_node)
        return isinstance(_call(root.kind), str)
    except Exception:
        return False


def _python_parser():
    """(parser, backend): the language pack when it works, else the tree-sitter-python wheel
    shim; (None, 'none') when neither parses — chunk_symbols then fail-opens to empty symbols."""
    global _PY_PARSER
    if _PY_PARSER is None:
        p = None
        try:
            from tree_sitter_language_pack import get_parser
            p = get_parser("python")
        except Exception:
            p = None
        if p is not None and _probe(p):
            _PY_PARSER = (p, "language_pack")
        else:
            try:
                p = _TSPParser()
            except Exception:
                p = None
            _PY_PARSER = ((p, "tree_sitter_python") if p is not None and _probe(p)
                          else (None, "none"))
    return _PY_PARSER


def python_parser_backend() -> str:
    """'language_pack' | 'tree_sitter_python' | 'none' — surfaced by the bundle startup log."""
    return _python_parser()[1]


def _py_defs_refs(src):
    """(defined_symbols set, referenced_symbols list) for a Python source slice via tree-sitter.
    defs = function/class (incl. nested method) names; refs = call targets (plain + attribute method),
    matching Aider's python-tags.scm def/ref split. Error-tolerant (tree-sitter recovers partials)."""
    parser, _ = _python_parser()   # PARSEC-PATCH: pack parser with wheel fallback (see above)
    if parser is None:
        raise RuntimeError("no working tree-sitter python grammar")
    root = _call(parser.parse(src).root_node)
    lines = src.split("\n")
    defs, refs = set(), []

    def walk(node):
        k = _call(node.kind)
        if k in _DEF_KINDS:
            t = _name_of(node, lines)
            if t:
                defs.add(t)
        elif k == "call" and hasattr(node, "child_by_field_name"):
            fn = node.child_by_field_name("function")
            if fn is not None:
                fk = _call(fn.kind)
                if fk == "identifier":
                    t = _text(fn, lines)
                    if t:
                        refs.append(t)
                elif fk == "attribute":
                    at = fn.child_by_field_name("attribute")
                    if at is not None:
                        t = _text(at, lines)
                        if t:
                            refs.append(t)
        for i in range(_call(node.named_child_count)):
            walk(node.named_child(i))

    walk(root)
    return defs, refs


def _file_key(chunk):
    """File identity for the symbol graph: basename up to a pytest '::' node-id suffix, or None for
    non-code / non-read chunks (which legitimately carry zero centrality)."""
    f = getattr(chunk, "file", None)
    if not f or getattr(chunk, "kind", "") != "read":
        return None
    name = f.split("::")[0]
    return name if os.path.splitext(name)[1].lower() in _PY_EXT else None


def chunk_symbols(chunk):
    """(frozenset defs, tuple refs) for a chunk, parsed once and cached on the chunk object."""
    cached = getattr(chunk, "_symcache", None)
    if cached is not None:
        return cached
    out = (frozenset(), ())
    if _file_key(chunk) is not None:
        try:
            d, r = _py_defs_refs(chunk.text)
            out = (frozenset(d), tuple(r))
        except Exception:
            out = (frozenset(), ())
    try:
        chunk._symcache = out
    except Exception:
        pass
    return out


def _pagerank(nodes, edges, d=0.85, iters=80, tol=1e-7, pers=None):
    """Weighted directed PageRank via power iteration (edge = (src, dst, weight)). Small graphs converge
    in a few ms. No networkx dep (serve-portable). pers (dict node->weight) = PERSONALIZED teleport: rank
    restarts/dangles to the seed distribution instead of uniform -> issue-seeded structural localization.
    pers=None -> uniform (byte-identical to the generic-centrality path)."""
    n = len(nodes)
    if n == 0:
        return {}
    idx = {nm: i for i, nm in enumerate(nodes)}
    if pers:
        pv = [max(float(pers.get(nm, 0.0)), 0.0) for nm in nodes]
        tot = sum(pv)
        pv = [x / tot for x in pv] if tot > 0 else [1.0 / n] * n
    else:
        pv = [1.0 / n] * n
    outw = [0.0] * n
    inl = [[] for _ in range(n)]            # per-dst: [(src_idx, w), ...]
    for (s, t, w) in edges:
        si, ti = idx[s], idx[t]
        outw[si] += w
        inl[ti].append((si, w))
    r = list(pv)
    for _ in range(iters):
        dang = d * sum(r[i] for i in range(n) if outw[i] == 0.0)      # dangling mass -> teleport (pv)
        nr = [(1.0 - d) * pv[i] + dang * pv[i] for i in range(n)]
        for ti in range(n):
            acc = 0.0
            for (si, w) in inl[ti]:
                acc += r[si] * w / outw[si]
            nr[ti] += d * acc
        s = sum(nr)
        if s > 0:
            nr = [x / s for x in nr]
        if max(abs(nr[i] - r[i]) for i in range(n)) < tol:
            r = nr
            break
        r = nr
    return {nodes[i]: r[i] for i in range(n)}


def _ident_mul(ident, n_definers, task_lower):
    """Aider-style per-identifier edge multiplier: downweight common/private, boost well-named/task."""
    mul = 1.0
    if n_definers > 5:                                  # defined in many files -> ambiguous/common
        mul *= 0.1
    if ident.startswith("_"):                           # private
        mul *= 0.1
    long_named = len(ident) >= 8 and (("_" in ident) or
                                      (any(c.isupper() for c in ident) and any(c.islower() for c in ident)))
    if long_named:
        mul *= 3.0
    if task_lower and ident.lower() in task_lower:      # mentioned in the task -> central to the goal
        mul *= 3.0
    return mul


def decided_centrality_feats(decided, alive, task_text=""):
    """Per-decided-atom centrality features [len(decided), K_CENTRALITY], built over the CAUSAL alive
    set (steps<=t). Aider repomap: file graph referencer->definer, PageRank, distribute rank onto
    (file, ident) definitions. Mirrors decided_extra_feats' signature so trace_graph + curator wire it
    in identically -> train==serve."""
    defines = {}        # ident -> set(file) that define it
    refs_by_file = {}   # file -> {ident: count}
    task_lower = (task_text or "").lower()
    for c in alive:
        f = _file_key(c)
        if f is None:
            continue
        d, r = chunk_symbols(c)
        for i in d:
            defines.setdefault(i, set()).add(f)
        if r:
            cnt = refs_by_file.setdefault(f, {})
            for i in r:
                cnt[i] = cnt.get(i, 0) + 1
    # edges: referencer_file -> definer_file, one per (ident, referencer), weight = mul * sqrt(num_refs)
    edges = []          # (src, dst, w, ident)
    node_set = set(defines_file for files in defines.values() for defines_file in files)
    node_set |= set(refs_by_file)
    for rf, cnt in refs_by_file.items():
        for ident, num in cnt.items():
            definers = defines.get(ident)
            if not definers:
                continue
            w = _ident_mul(ident, len(definers), task_lower) * math.sqrt(num)
            for df in definers:
                edges.append((rf, df, w, ident))
    nodes = list(node_set)
    pr = _pagerank(nodes, [(s, t, w) for (s, t, w, _i) in edges])
    # distribute each file's rank onto its out-edges -> per-(file, ident) definition rank (Aider)
    outw, out_edges = {}, {}
    for (s, t, w, ident) in edges:
        outw[s] = outw.get(s, 0.0) + w
        out_edges.setdefault(s, []).append((t, w, ident))
    defrank = {}
    for s, oes in out_edges.items():
        tw = outw.get(s, 0.0)
        if tw <= 0:
            continue
        sr = pr.get(s, 0.0)
        for (t, w, ident) in oes:
            defrank[(t, ident)] = defrank.get((t, ident), 0.0) + sr * w / tw
    maxpr = max(pr.values()) if pr else 0.0
    maxdr = max(defrank.values()) if defrank else 0.0
    feats = np.zeros((len(decided), K_CENTRALITY), dtype=np.float32)
    for k, c in enumerate(decided):
        f = _file_key(c)
        if f is None:
            continue
        d, r = chunk_symbols(c)
        fpr = (pr.get(f, 0.0) / maxpr) if maxpr > 0 else 0.0
        dr = 0.0
        for ident in d:
            v = defrank.get((f, ident), 0.0)
            if v > dr:
                dr = v
        dr = (dr / maxdr) if maxdr > 0 else 0.0
        feats[k] = [fpr, dr, 1.0 if d else 0.0, math.log1p(len(d)), math.log1p(len(r))]
    return feats
