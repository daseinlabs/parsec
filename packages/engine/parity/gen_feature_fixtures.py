#!/usr/bin/env python3
"""Generate NODE-featurization parity fixtures from the Python reference.

Runs adaptive-context-clean's torch_curator feature stack (struct_features,
struct_features_causal, struct_type_features, node_struct_with_type), the
trace_train role markers and attribution.spans over diverse chunk sets —
including the brain golden conversation (packages/brain/tests/test_service.py
MESSAGES, parsed by parsec_brain.scorer.parse_internal exactly as the serving
path does) — and dumps chunks + expected float32 matrices as JSON.
tests/parity_features.rs replays the chunks through the Rust port and asserts
f32 bit-equality (the reference emits float32; Rust computes f64 and casts
once — the same single rounding).

numpy + torch are required (torch_curator imports torch) and parsec_brain must
be importable (golden-conversation parse), so run with the brain venv:
  packages/brain/.venv/bin/python parity/gen_feature_fixtures.py [out.json]
"""
import importlib.util
import json
import os
import sys
from dataclasses import asdict

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "fixtures", "features.json")
os.environ["AC_CHUNK_MODE"] = "fixed"       # read-once module global in chunking.py

ACC = os.environ.get("ACC_ROOT") or os.path.abspath(
    os.path.join(HERE, "..", "..", "..", "..", "adaptive-context-clean"))
sys.path.insert(0, ACC)
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(REPO, "packages", "brain", "src"))

import numpy as np  # noqa: E402

from adaptive_context.optimizer import chunking as C  # noqa: E402
from adaptive_context.optimizer import torch_curator as TC  # noqa: E402
from adaptive_context.optimizer.attribution import spans  # noqa: E402
from parsec_brain.scorer import parse_internal  # noqa: E402


def _golden_messages():
    """The brain golden conversation — single source: tests/test_service.py."""
    spec = importlib.util.spec_from_file_location(
        "brain_test_service", os.path.join(REPO, "packages", "brain", "tests", "test_service.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.MESSAGES


# ---- chunk-set builders --------------------------------------------------

PY_SRC = "\n".join([
    "import os",
    "from collections import defaultdict",
    "",
    "CONST = 1",
    "",
    "def alpha(x):",
    "    if x > 0:",
    "        return x * 2",
    "    return -x",
    "",
    "class Beta:",
    "    \"\"\"docstring.\"\"\"",
    "",
    "    def method(self, y):",
    "        # comment kept",
    "        total = 0",
    "        for i in range(y):",
    "            total += alpha(i)",
    "        return total",
    "",
    "def gamma():",
    "    return Beta().method(3)",
])
GREP = "\n".join([
    "src/service/app.py:42:    def handle(self):",
    "src/service/app.py:77:        return handle_all()",
    "docs/notes.md:9:handle with care",
    "src/:5:empty basename (Python-falsy file)",
    "no path here just words",
])
TEST_OUT = "=== session ===\n" + "\n".join(
    f"test_mod.py::test_{i} PASSED" for i in range(1, 40)) + "\n<returncode>1</returncode>"

DUP_STEPS = [
    ("grep -rn handle src/", GREP),
    ("cat src/app.py", PY_SRC),
    ("python -m pytest -x", TEST_OUT),
    ("cat src/app.py", PY_SRC),                                     # exact re-read
    ("sed -n '4,12p' src/app.py", "\n".join(PY_SRC.splitlines()[3:12])),  # partial overlap
    ("cat src/app.py", PY_SRC),                                     # third read -> dup>1
]

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

# _line_type zoo: every regex branch + the \s*-backtrack lookahead bypass
# (leading SPACES dodge the control-keyword guard, tabs don't).
LT_SIG = "\n".join([
    "def alpha(x):", "async  def beta():", "class Gamma:", "@decorator", "@ nope",
    "function f() {", "fn main() {", "public static void main(String[] args) {",
    "int main(int argc) {", "  int main(int argc) {", "form x(y) {",
])
LT_CTRL = "\n".join([
    "for x in range(10) {", " for x in range(10) {", "  for x in range(10) {",
    "\tfor x in range(10) {", " \tfor x in range(10) {", "\t for x in range(10) {",
    "while (x) foo(y) {", " while (x) foo(y) {", "print('hello') {",
    "switch (x) handle(y) {", " case cond(x) {",
])
LT_DECL = "\n".join([
    "x = 1", "x == 1", "x === 1", "x =:= y", "name: int = 3", "let y == 2",
    "const {a} = b", "a.b.c = 1", "X.Y=2", "x =", "x:= 5", "export default = weird",
])
LT_IMPORT = "\n".join([
    "import os", "  from x import y", "#include <stdio.h>", "require 'json'",
    "use std::fmt;", "importantly not",
])
LT_COMMENT = "\n".join([
    "# hash", " // slash", "/* block", "* star", "-- dash", "; semi",
    "''' doc", '""" doc', "\x1f# ctrl-ws comment", "*bold*",
])
LT_UNI = "é = 1\nλ = 3\ndef übung():\n码 = 5\n   \nplain body line"


def _mk(text, file=None, lo=None, hi=None, step=0, kind="other", **kw):
    return C.Chunk(text=text, file=file, lo=lo, hi=hi, step=step, kind=kind, **kw)


def _linetype_zoo():
    # unsorted steps on purpose (causal maps by identity, not sort order)
    return [
        _mk(LT_SIG, file="sig.py", lo=1, hi=11, step=2, kind="read",
            cmd="cat sig.py", rc=0, head="sig head"),
        _mk(LT_CTRL, file="ctrl.c", lo=1, hi=11, step=0, kind="read", cmd="cat ctrl.c"),
        _mk(LT_DECL, file=None, step=1, kind="other", cmd="python decls.py", rc=1),
        _mk(LT_IMPORT, file="", step=2, kind="grep", cmd="grep import .", head="imp"),
        _mk(LT_COMMENT, step=0, kind="asst"),
        _mk(LT_UNI, file="läuft.py", lo=3, hi=8, step=1, kind="read",
            cmd="cat läuft.py", head="é head"),
        _mk("\n \n\t\n", step=2, kind="other", cmd="blank only"),
        _mk("", step=1, kind="reasoning", evict="provider"),
    ]


def _single_step0():
    return [
        _mk("only step zero", file="a.py", lo=1, hi=1, step=0, kind="read", cmd="cat a.py"),
        _mk("also zero", file="", step=0, kind="grep", cmd="grep zero"),
        _mk("third zero", step=0, kind="other", cmd=""),
    ]


def _dup_clamp():
    same = "def repeated():\n    return 42"
    out = [_mk(same, file="rep.py", lo=1, hi=2, step=s, kind="read", cmd=f"cat rep.py #{s}")
           for s in range(7)]                       # 6 earlier dups on the last -> min(1, 6/5) clamps
    out.append(_mk("def repeated():", file="rep.py", lo=1, hi=1, step=7, kind="read",
                   cmd="sed -n 1p rep.py"))         # fractional sup on earlier rows
    out.append(_mk("big" * 5, step=3, kind="other", tokens=123456, cmd="wc -c big"))
    return out


def _mixed_case():
    p = parse_internal(MIXED_MESSAGES, 10)
    return p.chunks, p.recent_cmds


def _golden_case():
    p = parse_internal(_golden_messages(), 10)
    return p.chunks, p.recent_cmds


# ---- expected-output serialization ---------------------------------------

def _mat(a) -> list:
    return np.asarray(a, dtype=np.float64).tolist()


def _markers(chunks) -> list:
    # trace_train.py L59-62 (inline in assemble_trace; float32 either way)
    return [[1.0 if c.kind == "reasoning" else 0.0,
             1.0 if (getattr(c, "head", "") or "") else 0.0] for c in chunks]


def _chunks_json(chunks) -> list:
    return [{k: v for k, v in asdict(c).items() if k != "emb"} for c in chunks]


def _case(name, chunks, recent, rows=None):
    out = {
        "name": name,
        "recent": recent,
        "chunks": _chunks_json(chunks),
        "rows": rows,
        "struct_features": _mat(TC.struct_features(chunks, recent)),
        "causal": _mat(TC.struct_features_causal(chunks)),
        "type": _mat(TC.struct_type_features(chunks)),
        "node_struct": _mat(TC.node_struct_with_type(chunks)),
        "markers": _markers(chunks),
    }
    if rows is not None:
        out["struct_features_rows"] = _mat(TC.struct_features(chunks, recent, rows=rows))
        out["type_rows"] = _mat(TC.struct_type_features(chunks, rows=rows))
    return out


SPAN_TEXTS = [
    "Fix parse_config in src/config.py: ValueError at line 12345 'quoted str' "
    "\"another one\" the This form2 ab_cd_ef ٣٤٥٦٧ ٣٤ x²y",
    "",
    "grep 'parse_config' src ; sed -n '1,24p' config.py",
    "self True None import from CLASS 42 007 a-b-c pkg.mod.attr /usr/local/bin/tool",
    "don't worry, can't stop \"in\" '' \"ab\" 'xy'",
    "Traceback (most recent call last):\n  File \"src/läuft.py\", line 3",
]


def main():
    golden_chunks, golden_recent = _golden_case()
    mixed_chunks, mixed_recent = _mixed_case()
    dup_chunks = C.accumulated_chunks(DUP_STEPS, 99, read_lines=10)
    fx = {
        "span_cases": [{"text": t, "expected": sorted(spans(t))} for t in SPAN_TEXTS],
        "cases": [
            _case("golden_conv", golden_chunks, golden_recent, rows=[9, 10]),
            _case("mixed_unicode", mixed_chunks, mixed_recent,
                  rows=list(range(0, len(mixed_chunks), 2))),
            _case("dup_heavy", dup_chunks, "cat src/app.py alpha method range total",
                  rows=list(range(0, len(dup_chunks), 3))),
            _case("linetype_zoo", _linetype_zoo(), ""),
            _case("single_step0", _single_step0(), "grep zero cat a.py"),
            _case("dup_clamp", _dup_clamp(), "sed -n 1p rep.py repeated"),
            _case("empty", [], ""),
            _case("rows_empty", _single_step0(), "", rows=[]),
        ],
    }
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(fx, f, ensure_ascii=False, indent=1)
    n = sum(len(c["chunks"]) for c in fx["cases"])
    print(f"wrote {OUT}: {len(fx['cases'])} cases / {n} chunks, {len(fx['span_cases'])} span cases")


if __name__ == "__main__":
    main()
