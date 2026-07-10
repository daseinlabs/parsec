#!/usr/bin/env python3
"""Generate chunking/featurization parity fixtures from the Python reference.

Runs adaptive-context-clean's chunking.py on a corpus of synthetic cases and
dumps inputs + expected outputs as JSON. The Rust suite (tests/parity.rs)
replays the inputs and asserts exact equality — this file is the source of
truth for the §7b cross-language parity contract.

Usage: gen_fixtures.py [fixed|cst] [out.json]
The chunk mode must be set before importing chunking.py (module-global).
"""
import json
import os
import sys
from dataclasses import asdict

MODE = (sys.argv[1] if len(sys.argv) > 1 else "fixed").lower()
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(
    os.path.dirname(__file__), "fixtures", f"chunking_{MODE}.json")
os.environ["AC_CHUNK_MODE"] = MODE

ACC = os.environ.get("ACC_ROOT") or os.path.abspath(
    os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", "adaptive-context-clean"))
sys.path.insert(0, ACC)

from adaptive_context.optimizer import chunking as C  # noqa: E402

PY_SRC = "\n".join([
    "import os",
    "from collections import defaultdict",
    "",
    "CONST = 1",
    "",
    "",
    "def alpha(x):",
    "    if x > 0:",
    "        return x * 2",
    "    return -x",
    "",
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
    "",
    "def gamma():",
    "    return Beta().method(3)",
])

BIG = "\n".join(f"line {i}: value = {i * 7}" for i in range(1, 118))
GREP = "\n".join([
    "src/service/app.py:42:    def handle(self):",
    "src/service/app.py:77:        return handle_all()",
    "docs/notes.md:9:handle with care",
    "binary file matches (dev/null)",
    "src/util.py:  helper text without lineno",
    "src/:5:empty basename (Python-falsy file)",
    "a.py:9223372036854775807:line number at i64::MAX",
    "[reranked 3 results]",
    "Makefile",
    "no path here just words",
])
TEST_OUT = "\n".join(
    ["=== test session starts ==="]
    + [f"test_mod.py::test_{i} PASSED" for i in range(1, 95)]
    + ["<returncode>1</returncode>"]
)

CHUNK_CASES = [
    dict(name="cat_small", cmd="cat src/app.py", obs=PY_SRC, step=2, read_lines=None),
    dict(name="cat_big_windows", cmd="cat data/gen.txt", obs=BIG, step=0, read_lines=None),
    dict(name="sed_range", cmd="sed -n '30,60p' pkg/mod.py", obs=BIG, step=5, read_lines=None),
    dict(name="sed_single", cmd="sed -n 12p pkg/mod.py", obs="x = 1", step=1, read_lines=None),
    dict(name="grep_mixed", cmd="grep -rn handle src/", obs=GREP, step=3, read_lines=None),
    dict(name="git_grep", cmd="git grep -n TODO", obs=GREP, step=4, read_lines=None),
    dict(name="find_files", cmd="find . -name '*.py'", obs="./a.py\n./sub/b.py\n", step=1, read_lines=None),
    dict(name="other_test_output", cmd="python -m pytest -x", obs=TEST_OUT, step=6, read_lines=None),
    dict(name="other_empty", cmd="ls -la", obs="", step=0, read_lines=None),
    dict(name="other_blank", cmd="make build", obs="\n\n  \n", step=0, read_lines=None),
    dict(name="read_empty", cmd="cat missing.py", obs="", step=0, read_lines=None),
    dict(name="read_atoms_g10", cmd="cat src/app.py", obs=PY_SRC, step=2, read_lines=10),
    dict(name="read_atoms_sed", cmd="sed -n '40,80p' src/big.py", obs=PY_SRC, step=3, read_lines=10),
    dict(name="read_atoms_blankonly", cmd="cat empty.py", obs="\n\n\n", step=1, read_lines=10),
    dict(name="read_atoms_nofile", cmd="cat", obs=PY_SRC, step=1, read_lines=10),
    dict(name="unicode_crlf", cmd="cat läuft.py", obs="é1\r\nline²\r\n\r\nsnake 🐍 ok", step=7, read_lines=None),
    dict(name="unicode_atoms", cmd="cat läuft.py", obs="é1 next\x0cform\r\nplain", step=7, read_lines=10),
    dict(name="rc_returncode_eq", cmd="python run.py", obs="failed\nreturncode=2\n", step=2, read_lines=None),
    dict(name="grep_search_beats_read", cmd="grep -n cat src/x.py", obs=GREP, step=1, read_lines=10),
]

if MODE == "cst":  # CST only changes the read_lines code-read branch
    CHUNK_CASES += [
        dict(name="cst_python_read", cmd="cat src/app.py", obs=PY_SRC, step=2, read_lines=10),
        dict(name="cst_sed_offset", cmd="sed -n '100,140p' src/app.py", obs=PY_SRC, step=4, read_lines=10),
        dict(name="cst_unknown_ext", cmd="cat notes.txt", obs=PY_SRC, step=1, read_lines=10),
        dict(name="cst_too_small", cmd="cat t.py", obs="x = 1\ny = 2\nz = 3", step=1, read_lines=10),
        dict(name="cst_flat_dump", cmd="cat dump.py", obs="\n".join(f"k{i} = {i}" for i in range(30)), step=1, read_lines=10),
    ]

ASSISTANT_CASES = [
    dict(name="asst_plan", txt="First I will look at the config.\nThen run tests.", step=1),
    dict(name="asst_blank", txt="  \n\n", step=0),
    dict(name="asst_long", txt="\n".join(f"thought {i}" for i in range(90)), step=3),
]

STEPS = [
    ["grep -rn handle src/", GREP],
    ["cat src/app.py", PY_SRC],
    ["python -m pytest -x", TEST_OUT],
]
ACCUM_CASES = [
    dict(name="accum_all", upto=2, read_lines=10),
    dict(name="accum_clamped", upto=9, read_lines=10),
    dict(name="accum_first_legacy", upto=0, read_lines=None),
]

MESSAGES = [
    {"role": "system", "content": "you are an agent"},
    {"role": "assistant", "content": "Let me search.",
     "extra": {"actions": [{"command": "grep -rn handle src/"}]},
     "reasoning_content": "I should find the handler définition first.",
     "tool_calls": [{"provider_specific_fields": {"sig": "abc123", "n": 3, "t": 0.00001,
                                                  "p": 1.1534175185142759, "big": 1e16}}]},
    {"role": "tool", "content": GREP},
    {"role": "assistant", "content": [{"type": "text", "text": "Now read the file."}, {"text": "Part two."}],
     "extra": {"actions": [{"query": "open src/app.py"}, {"command": ""}]},
     "thinking_blocks": [{"thinking": "unicode 🐍 blob"}]},
    {"role": "user", "content": PY_SRC},
    {"role": "assistant", "content": [{"type": "image", "text": "nope"}],
     "extra": {"actions": []}},
    {"role": "tool", "content": [{"type": "text", "text": "obs a"}, "bare string part"]},
    {"role": "assistant", "content": "trailing, no obs -> ignored"},
]


def chunks_json(chunks):
    return [{k: v for k, v in asdict(c).items() if k != "emb"} for c in chunks]


def main():
    fx = {"mode": MODE, "chunk_cases": [], "assistant_cases": [], "accum_cases": [],
          "dup_cases": [], "message_case": {}}
    for c in CHUNK_CASES:
        out = C.chunk_observation(c["cmd"], c["obs"], c["step"], read_lines=c["read_lines"])
        fx["chunk_cases"].append({**c, "expected": chunks_json(out)})
    for a in ASSISTANT_CASES:
        fx["assistant_cases"].append({**a, "expected": chunks_json(C.chunk_assistant(a["txt"], a["step"]))})
    for a in ACCUM_CASES:
        out = C.accumulated_chunks([tuple(s) for s in STEPS], a["upto"], read_lines=a["read_lines"])
        fx["accum_cases"].append({**a, "steps": STEPS, "expected": chunks_json(out)})

    # dup features over an accumulated window with re-reads and exact dups
    dup_steps = [tuple(s) for s in STEPS] + [
        ("cat src/app.py", PY_SRC),                       # exact re-read -> dups both ways
        ("sed -n '4,12p' src/app.py", "\n".join(PY_SRC.splitlines()[3:12])),  # partial overlap
    ]
    dup_chunks = C.accumulated_chunks(dup_steps, 99, read_lines=10)
    sup, dup_earlier = C.dup_feats_fast(dup_chunks)
    rows = list(range(0, len(dup_chunks), 3))
    sup_r, dup_r = C.dup_feats_fast(dup_chunks, rows)
    fx["dup_cases"].append({
        "name": "dup_accum", "steps": [list(s) for s in dup_steps], "upto": 99, "read_lines": 10,
        "expected_sup": sup, "expected_dup_earlier": dup_earlier,
        "rows": rows, "expected_sup_rows": sup_r, "expected_dup_earlier_rows": dup_r,
    })

    fx["message_case"] = {
        "messages": MESSAGES,
        "expected_steps": [list(s) for s in C.steps_of(MESSAGES)],
        "expected_reasoning": chunks_json(C.reasoning_chunks_of(MESSAGES)),
        "expected_assistant": chunks_json(C.assistant_chunks_of(MESSAGES)),
        "expected_blob_tokens": [C.blob_tokens(m) for m in MESSAGES],
    }

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(fx, f, ensure_ascii=False, indent=1)
    n = len(fx["chunk_cases"]) + len(fx["assistant_cases"]) + len(fx["accum_cases"])
    print(f"wrote {OUT}: {n} chunk cases, mode={MODE}")


if __name__ == "__main__":
    main()
