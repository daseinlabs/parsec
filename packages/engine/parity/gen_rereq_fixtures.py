#!/usr/bin/env python3
"""Generate RE-REQUEST column parity fixtures from the HS training reference.

Runs gnn-train's torch_curator.rereq_features_base + rereq_apply_cut (the 6 re-request columns
of the HS curator's node row and decided row, spec v6828t3) and the r3 recognizer helpers
(scripts/fight_targets_helpers: _norm, _bash_read_targets) over hand-built chunk sets, and dumps
chunks + the expected float32 matrices as JSON. tests/parity_rereq.rs replays them through the
Rust port (features::rereq_columns, features::bash_read_targets) and asserts f32 bit-equality.

Cut state is a fixture input: each case lists the chunk indices that are cut, and the reference's
cut_fn(prev_chunk, t) answers "prev is cut" from that set. At serve the proxy answers the same
question from the Freezer's dropped registry.

Needs numpy + torch (torch_curator imports torch) and the gnn-train checkout at the HS revision:
  GNN_TRAIN=~/code/daseinlabs/gnn-train <venv>/bin/python parity/gen_rereq_fixtures.py [out.json]
"""

import importlib.util
import json
import os
import sys
import types
from dataclasses import asdict, dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "fixtures", "rereq.json")
os.environ["AC_CHUNK_MODE"] = "fixed"

GT = os.environ.get("GNN_TRAIN") or os.path.abspath(
    os.path.join(HERE, "..", "..", "..", "..", "gnn-train")
)
sys.path.insert(0, GT)


class _Missing(types.ModuleType):
    """Stand-in for a module absent from the gnn-train sync (attribution, labelers, ...). Importing
    it succeeds; USING it raises. The functions under test never touch these modules, so a fixture
    can only be produced if that stays true."""

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        raise RuntimeError(f"{self.__name__}.{name} used, but the module is not in the sync")


for _m in ("attribution", "labelers", "doom", "neighbors", "symbol_graph"):
    _full = f"adaptive_context.optimizer.{_m}"
    if importlib.util.find_spec(_full) is None:
        sys.modules[_full] = _Missing(_full)

import numpy as np  # noqa: E402
from adaptive_context.optimizer import torch_curator as TC  # noqa: E402

from scripts import fight_targets_helpers as FT  # noqa: E402


@dataclass
class Chunk:
    """Field-for-field the reference chunking.Chunk (and the Rust engine Chunk), built here so the
    generator does not import chunking.py (it needs labelers, absent from the sync). The rereq
    functions read chunks only through getattr(file/lo/hi/step/cmd)."""

    text: str
    file: str | None
    lo: int | None
    hi: int | None
    step: int
    kind: str
    tokens: int = 1
    evict: str = "content"
    cmd: str = ""
    rc: int | None = None
    head: str = ""
    struct_class: str = ""


# torch_curator falls back SILENTLY to a no-op bash parser + a non-stripping _norm when the r3
# helpers fail to import. Fixtures generated from the fallback would pin the wrong behaviour.
assert TC._ft_bash is FT._bash_read_targets, "torch_curator is on the fallback bash parser"
assert TC._ft_norm is FT._norm, "torch_curator is on the fallback _norm"
assert TC.REREQ_WIDTH == 6


def ch(step, kind="read", file=None, lo=None, hi=None, cmd="", text="x"):
    return Chunk(text, file, lo, hi, step, kind, cmd=cmd)


CASES = [
    # file-ranged reads of one file: overlap, adjacency (no overlap), most-recent copy
    (
        "file_ranges",
        [
            ch(1, file="a.py", lo=1, hi=10, cmd="cat /r/a.py"),
            ch(1, file="a.py", lo=11, hi=20, cmd="cat /r/a.py"),
            ch(2, file="a.py", lo=5, hi=14, cmd="sed -n '5,14p' /r/a.py"),
            ch(3, file="a.py", lo=10, hi=10, cmd="sed -n '10,10p' /r/a.py"),
            ch(3, file="b.py", lo=1, hi=10, cmd="cat /r/b.py"),
            ch(4, file="a.py", lo=21, hi=30, cmd="cat /r/a.py"),
        ],
        [0, 2],
    ),
    # same step never counts (strictly earlier only); ties on the most-recent step
    (
        "same_step_and_ties",
        [
            ch(1, file="m.rs", lo=1, hi=10, cmd="cat /x/m.rs"),
            ch(1, file="m.rs", lo=1, hi=10, cmd="cat /x/m.rs"),
            ch(2, file="m.rs", lo=1, hi=10, cmd="cat /x/m.rs"),
            ch(2, file="m.rs", lo=5, hi=6, cmd="cat /x/m.rs"),
        ],
        [1],
    ),
    # bash-read fallback (no file/lo/hi): sed range, cat whole-file, head, piped, quoted
    (
        "bash_fallback",
        [
            ch(1, kind="other", cmd="sed -n '100,140p' src/lib/core.py"),
            ch(2, kind="other", cmd="cat src/lib/core.py | grep foo"),
            ch(3, kind="other", cmd="head -n 50 'src/lib/core.py'"),
            ch(4, kind="other", cmd='sed -n "120,125p" src/lib/core.py && echo done'),
            ch(5, kind="other", cmd="cat ./notes.txt"),
            ch(6, kind="other", cmd="cat notes.txt"),
        ],
        [0, 3],
    ),
    # one command, several targets: a chunk with >1 range counts a prior copy once PER range
    (
        "multi_target",
        [
            ch(1, kind="other", cmd="cat src/a/x.py src/a/y.py"),
            ch(2, kind="other", cmd="cat src/a/x.py src/a/y.py"),
            ch(3, kind="other", cmd="cat src/a/y.py; cat src/a/z.py"),
        ],
        [0],
    ),
    # path normalisation: case folding, backslashes, surrounding quotes; short/flag tokens ignored
    (
        "normalisation",
        [
            ch(1, kind="other", cmd="cat SRC\\Main.PY"),
            ch(2, kind="other", cmd="cat src/main.py"),
            ch(3, kind="other", cmd="Get-Content 'src\\main.py'"),
            ch(4, kind="other", cmd="cat -n a/b"),
            ch(5, kind="other", cmd="cat a/b.c"),
        ],
        [1],
    ),
    # file ranges and the bash fallback are separate keyspaces (basename vs full path)
    (
        "keyspaces",
        [
            ch(1, file="core.py", lo=1, hi=40, cmd="cat src/lib/core.py"),
            ch(2, kind="other", cmd="sed -n '1,10p' src/lib/core.py"),
            ch(3, file="core.py", lo=1, hi=10, cmd="sed -n '1,10p' src/lib/core.py"),
            ch(4, kind="other", cmd="cat src/lib/core.py"),
        ],
        [0, 1],
    ),
    # non-observations: reasoning / asst / grep without ranges carry no ranges -> all-zero rows
    (
        "non_observations",
        [
            ch(1, kind="reasoning", text="thinking about a.py"),
            ch(1, kind="asst", text="I will read a.py"),
            ch(2, kind="grep", cmd="grep -n foo src", text="src/a.py:3: foo"),
            ch(3, file="a.py", lo=1, hi=5, cmd="cat /r/a.py"),
            ch(4, file="a.py", lo=1, hi=5, cmd="cat /r/a.py"),
        ],
        [3],
    ),
    ("empty", [], []),
]

BASH_CMDS = [
    "sed -n '5,14p' /r/a.py",
    "sed -n 5,14p /r/a.py",
    "sed -e x -n '5,14p' /r/a.py",
    "sed -n '14,5p' /r/a.py",
    "cat /r/a.py /r/b.txt",
    "cat /r/a.py | head -n 5",
    "head -n 20 src/x.rs; tail src/y.rs && less src/z.md",
    "grep -rn 'needle' src/lib/a.py",
    "awk '{print}' dir/f.csv",
    "GET-CONTENT C:\\Users\\me\\f.ps1",
    "type docs\\readme.md",
    'more "quoted path/with space.txt"',
    "cat -A ab.c",
    "cat a/b",
    "cat ./x.y",
    "echo cat src/a.py",
    "python run.py",
    "",
]


def ext_class(bn):
    """read_columns' extension one-hot, as the class the v3 wire carries: exactly the branch
    torch_curator.read_columns takes for column block 18-49 (extension, 'other', or 'none')."""
    if not bn:
        return "none"
    e = TC._path_v9_ext(bn)
    if e == "":
        return "none"
    return e if e in TC._READCOLS_EXT_IDX else "other"


def main():
    cases = []
    for name, chunks, cut in CASES:
        cut_ids = {id(chunks[i]) for i in cut}
        base, mr = TC.rereq_features_base(chunks)
        full = TC.rereq_apply_cut(
            base, mr, chunks, lambda prev, t, cut_ids=cut_ids: id(prev) in cut_ids
        )
        cases.append(
            {
                "name": name,
                "chunks": [asdict(c) for c in chunks],
                "cut": cut,
                "mr_idx": [int(x) for x in mr],
                "rereq": np.asarray(full, np.float32).reshape(len(chunks), 6).tolist(),
            }
        )
    bash = [
        {"cmd": c, "targets": [[p, int(o), int(lim)] for (p, o, lim) in FT._bash_read_targets(c)]}
        for c in BASH_CMDS
    ]
    norm = [
        {"in": s, "out": FT._norm(s)}
        for s in ["SRC\\Main.PY", "'a/b.py'", '"x"', "  a/b ", "'mixed\"", "Ünïcode/Ä.PY", ""]
    ]
    ext = [
        {"file": f, "class": ext_class(f)}
        for f in [
            None,
            "",
            "a.py",
            "A.PY",
            "Makefile",
            ".bashrc",
            "x.tar.gz",
            "notes.",
            "a.b.c.RS",
            "weird.Py~",
            "ünï.JSONL",
            "index.tsx",
            "lib.rs",
            "setup.cfg",
        ]
    ]
    with open(OUT, "w") as f:
        json.dump(
            {
                "source": f"gnn-train {os.path.basename(GT)} torch_curator + fight_targets_helpers",
                "cases": cases,
                "bash": bash,
                "norm": norm,
                "ext_class": ext,
            },
            f,
            indent=1,
        )
    print(f"wrote {OUT}: {len(cases)} rereq cases, {len(bash)} bash cmds, {len(norm)} norm")


if __name__ == "__main__":
    main()
