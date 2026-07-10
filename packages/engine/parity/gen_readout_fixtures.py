#!/usr/bin/env python3
"""Generate READOUT parity fixtures (the 49-col decided_struct) from the
VENDORED brain modules — the serving truth for curator_v4_prod.pt.

Reference: dasein_brain.scorer.TraceScorer._het_readout (the vendored mirror
of curator._het_readout, incl. the always-emit changeprone/scout patches),
driven over diverse internal-view conversations parsed by the scorer's own
parse_internal — exactly the serve pipeline. tests/parity_readout.rs replays
each case through engine::readout::decided_struct and asserts f32 bit-equality
(dupcos col 47 carries a documented epsilon: numpy's float32 norm/matvec
summation order is SIMD/BLAS machine-dependent).

Content embeddings are deterministic per-unique-text pseudo-vectors (sha256
seed) serialized INTO the fixture — both sides consume identical inputs, so
dupcos parity is about arithmetic, not the embedder.

tree-sitter note: the venv's tree_sitter_language_pack cannot dlopen its
downloaded grammar here (x86_64 Rosetta python vs arm64 dylib), so this
generator shims get_parser with the official tree-sitter-python 0.23.6 wheel —
the SAME grammar version the engine compiles (Cargo.lock) — wrapped to the
PyO3-binding surface the vendored symbol_graph expects (parse(str), .kind,
.start_position byte-column Points).

Run with the brain venv (PYTHONHASHSEED pinned for regeneration determinism —
the vendored centrality path iterates Python sets, see the readout.rs
ordering note):
  packages/brain/.venv/bin/python parity/gen_readout_fixtures.py [out.json]
"""
import hashlib
import importlib.util
import json
import os
import sys
from dataclasses import asdict

if os.environ.get("PYTHONHASHSEED") != "0":
    os.environ["PYTHONHASHSEED"] = "0"
    os.execv(sys.executable, [sys.executable] + sys.argv)

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "fixtures", "readout.json")
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(REPO, "packages", "brain", "src"))
os.environ.setdefault("DASEIN_EMBED_BACKEND", "hash")

import numpy as np  # noqa: E402

import dasein_brain  # noqa: E402,F401  _flags pins (ISSUEMATCH/SCOUTFEAT/...) before vendored imports

# ---- tree-sitter shim (see module doc) -------------------------------------
import tree_sitter_language_pack as _tlp  # noqa: E402

try:
    _tlp.get_parser("python").parse("x = 1")
except Exception:
    import tree_sitter as _ts  # noqa: E402
    import tree_sitter_python as _tsp  # noqa: E402

    class _Node:
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
            return _Node(self._n.named_child(i))

        def child_by_field_name(self, name):
            c = self._n.child_by_field_name(name)
            return None if c is None else _Node(c)

    class _Tree:
        def __init__(self, t):
            self._t = t

        @property
        def root_node(self):
            return _Node(self._t.root_node)

    class _Parser:
        def __init__(self):
            self._p = _ts.Parser(_ts.Language(_tsp.language()))

        def parse(self, src):
            return _Tree(self._p.parse(src.encode("utf-8") if isinstance(src, str) else src))

    def _get_parser(name):
        assert name == "python", name
        return _Parser()

    _tlp.get_parser = _get_parser

from dasein_brain.scorer import TraceScorer, parse_internal  # noqa: E402
from dasein_brain.vendored.symbol_graph import _py_defs_refs  # noqa: E402

# shim sanity: the vendored parse path must actually produce symbols
_d, _r = _py_defs_refs("def alpha(x):\n    return beta(x)\n")
assert _d == {"alpha"} and _r == ["beta"], (_d, _r)

CP_JSON = os.path.join(HERE, "fixtures", "changeprone.json")


class _Bundle:
    """Duck-typed Bundle: only the fields TraceScorer touches on the readout
    path (no model, no checkpoint — _het_readout is pre-forward)."""

    model = None
    calib_tau = 0.0
    read_struct = 49
    chunk_lines = 10
    changeprone = None


def _golden_messages():
    spec = importlib.util.spec_from_file_location(
        "brain_test_service",
        os.path.join(REPO, "packages", "brain", "tests", "test_service.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.MESSAGES


# same corpus as gen_feature_fixtures.MIXED_MESSAGES (kept verbatim; that
# script imports the non-vendored reference tree, so no cross-import here)
MIXED_MESSAGES = [
    {"role": "system", "content": "sys prompt"},
    {"role": "user", "content": "Fix the läuft.py unicode bug in parse_config; see src/config.py."},
    {"role": "assistant", "content": "Grep first.",
     "reasoning_content": "Look for parse_config callers — überall.",
     "extra": {"actions": [{"tool": "Grep", "command": "grep -rn parse_config src/"}]}},
    {"role": "tool", "content": "src/config.py:12: def parse_config(path):\n"
     "src/läuft.py:3: cfg = parse_config('x')\nbinary noise"},
    {"role": "assistant", "content": [{"type": "text", "text": "Read the unicode file."},
                                      {"text": "Part two."}],
     "extra": {"actions": [{"query": "open src/läuft.py"}, {"command": ""}]}},
    {"role": "tool", "content": "é1\r\nline²\r\n\r\nsnake 🐍 ok\x1fmore"},
    {"role": "assistant", "content": "Now run it.", "extra": {"actions": [{"command": ""}]}},
    {"role": "tool", "content": "returncode: 1\nTraceback (most recent call last):\n"
     "  File \"src/läuft.py\", line 3\nValueError: nope"},
]

_QUERY_PY = "\n".join([
    "import os",
    "from cache_utils import fetch_shard",
    "",
    "def load_batch(shard_index):",
    "    data = fetch_shard(shard_index)",
    "    return normalize_rows(data)",
    "",
    "class DataLoader:",
    "    def __init__(self, root):",
    "        self.root = root",
    "",
    "    def load_batch(self, idx):",
    "        return load_batch(idx)",
    "",
    'unicode_probe = "é×π"; probe = load_batch(0)',
    "",
    "def normalize_rows(rows):",
    "    return [r.strip() for r in rows]",
])
_CACHE_PY = "\n".join([
    "import os",
    "",
    "def fetch_shard(shard_index):",
    "    path = os.path.join('shards', str(shard_index))",
    "    return open(path).read().splitlines()",
    "",
    "def _private_helper(x):",
    "    return fetch_shard(x)",
])
_TEST_PY = "\n".join([
    "from loaders.query import DataLoader, load_batch",
    "",
    "def test_load_batch():",
    "    assert load_batch(0)",
    "    assert DataLoader('.').load_batch(1)",
])

# python-heavy conversation: cross-file symbol graph (query.py <-> cache_utils
# <- test_query), issue idents + traceback frames, edit-echo 'other' chunk,
# re-read (dup/readmit/dupcos), deep path in cmd (rerank depth), pytest
# traceback as second-to-last obs (rerank in_tb/in_obs at the last readout).
PYHEAVY_MESSAGES = [
    {"role": "system", "content": "You are a coding agent."},
    {"role": "user", "content": "DataLoader.load_batch raises KeyError for a missing shard_index "
     "— fetch_shard should default to shard 0. Traceback (most recent call last):\n"
     '  File "/repo/src/loaders/query.py", line 12, in load_batch\n'
     '  File "/repo/src/loaders/cache_utils.py", line 4, in fetch_shard\n'
     "KeyError: 'shard_index'"},
    {"role": "assistant", "content": "Read the loader first.",
     "reasoning_content": "load_batch delegates to fetch_shard; check both.",
     "extra": {"actions": [{"tool": "Read", "command": "sed -n '1,18p' /repo/src/loaders/query.py"}]}},
    {"role": "tool", "content": "returncode: 0\n" + _QUERY_PY},
    {"role": "assistant", "content": "Now the cache helper.",
     "extra": {"actions": [{"tool": "Read", "command": "cat src/loaders/cache_utils.py"}]}},
    {"role": "tool", "content": "returncode: 0\n" + _CACHE_PY},
    {"role": "assistant", "content": "Check the test that covers it.",
     "extra": {"actions": [{"tool": "Read", "command": "cat tests/test_query.py"}]}},
    {"role": "tool", "content": "returncode: 0\n" + _TEST_PY},
    {"role": "assistant", "content": "Run the failing test.",
     "extra": {"actions": [{"tool": "Bash", "command": "python -m pytest tests/test_query.py -x"}]}},
    {"role": "tool", "content": "returncode: 1\nFAILED tests/test_query.py::test_load_batch\n"
     "Traceback (most recent call last):\n"
     '  File "/repo/src/loaders/query.py", line 12, in load_batch\n'
     "KeyError: 'shard_index'\n1 failed in 0.11s"},
    {"role": "assistant", "content": "Patch fetch_shard, then re-read the loader.",
     "extra": {"actions": [{"tool": "Edit", "command": "edit src/loaders/cache_utils.py"}]}},
    {"role": "tool", "content": "The file src/loaders/cache_utils.py has been updated. "
     "Review the diff before rerunning tests."},
    {"role": "assistant", "content": "Re-read the loader to confirm the call site.",
     "extra": {"actions": [{"tool": "Read", "command": "sed -n '1,18p' /repo/src/loaders/query.py"}]}},
    {"role": "tool", "content": "returncode: 0\n" + _QUERY_PY},
]

TINY_MESSAGES = [
    {"role": "user", "content": "Rename the util helper."},
    {"role": "assistant", "content": "Look at it.",
     "extra": {"actions": [{"tool": "Read", "command": "cat util.py"}]}},
    {"role": "tool", "content": "def helper():\n    return 1"},
]

EMB_DIM = 24


def _emb_for(text: str) -> np.ndarray:
    seed = int.from_bytes(hashlib.sha256(text.encode("utf-8")).digest()[:4], "little")
    return np.random.RandomState(seed).standard_normal(EMB_DIM).astype(np.float32)


def _chunks_json(chunks) -> list:
    return [{k: v for k, v in asdict(c).items() if k != "emb"} for c in chunks]


def _load_cp():
    d = json.load(open(CP_JSON))
    return (d["cnt"], max(int(d["total"]), 1))     # bundle.py clamp


def _case(name, messages, decided_sel, age=0.0, use_changeprone=True, alive_sel=None):
    p = parse_internal(messages, 10)
    n = len(p.chunks)
    alive_rows = alive_sel(p) if alive_sel else list(range(n))
    lc = [p.chunks[i] for i in alive_rows]
    decided_rows = decided_sel(p)                   # rows into p.chunks
    ap = {r: k for k, r in enumerate(alive_rows)}
    decided_idx = [ap[r] for r in decided_rows]     # positions within lc
    emb_all = np.stack([_emb_for(c.text[:2000]) for c in p.chunks]) if n else \
        np.zeros((0, EMB_DIM), np.float32)
    emb_lc = emb_all[np.asarray(alive_rows, np.int64)] if alive_rows else \
        np.zeros((0, EMB_DIM), np.float32)
    b = _Bundle()
    b.changeprone = _load_cp() if use_changeprone else None
    sc = TraceScorer(b)
    ds = sc._het_readout(lc, decided_idx, p.recent_cmds, p.task_text, age, emb_lc,
                         p.cur_step, p.het_steps, p.T)
    assert ds.shape == (len(decided_rows), 49), ds.shape
    return {
        "name": name,
        "chunks": _chunks_json(p.chunks),
        "alive": alive_rows,
        "decided": decided_rows,
        "task_text": p.task_text,
        "recent": p.recent_cmds,
        "steps": [[c, o] for (c, o) in p.het_steps],
        "cur_step": p.cur_step,
        "t_total": p.T,
        "age": age,
        "use_changeprone": use_changeprone,
        "embs": np.asarray(emb_all, np.float64).tolist(),
        "expected": np.asarray(ds, np.float64).tolist(),
    }


def _newborns(p):
    return [i for i, c in enumerate(p.chunks)
            if c.step == p.cur_step and c.evict != "provider"]


def _all(p):
    return list(range(len(p.chunks)))


def _prev_step(p):
    return [i for i, c in enumerate(p.chunks)
            if c.step == p.cur_step - 1 and c.kind != "reasoning"]


def _alive_subset(p):
    # drop a few mid-conversation rows (committed drops): keep the task-side
    # early rows, every current-step row, and an uneven middle selection.
    n = len(p.chunks)
    return [i for i in range(n)
            if p.chunks[i].step == p.cur_step or i % 3 != 1 or p.chunks[i].step == 0]


def main():
    golden = _golden_messages()
    fx = {"cases": [
        _case("golden_newborns", golden, _newborns),                        # the real serve mask
        _case("golden_all", golden, _all),
        _case("golden_age1", golden, _prev_step, age=1.0),
        _case("golden_no_changeprone", golden, _newborns, use_changeprone=False),
        _case("mixed_unicode_all", MIXED_MESSAGES, _all, use_changeprone=False),
        _case("pyheavy_newborns", PYHEAVY_MESSAGES, _newborns),
        _case("pyheavy_all", PYHEAVY_MESSAGES, _all),
        _case("pyheavy_alive_subset", PYHEAVY_MESSAGES, _newborns, alive_sel=_alive_subset),
        _case("tiny", TINY_MESSAGES, _all, use_changeprone=False),
    ]}
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(fx, f, ensure_ascii=False, indent=1)
    rows = sum(len(c["expected"]) for c in fx["cases"])
    print(f"wrote {OUT}: {len(fx['cases'])} cases / {rows} decided rows")


if __name__ == "__main__":
    main()
