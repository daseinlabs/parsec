#!/usr/bin/env python3
"""Generate rel-4 SUPERSESSION parity fixtures from the VENDORED reference.

Runs dasein_brain.vendored.pyg_model.edges (causal=True) over diverse chunk
sets — synthetic branch probes plus chunk sets parsed from realistic
conversations by dasein_brain.scorer.parse_internal — and dumps chunks +
the emitted rel-4 (src, dst) pairs IN EMISSION ORDER (i asc, j asc; order is
load-bearing: the brain's v1 path sorts the client pairs back into exactly
this order, so the client must produce the same set).
tests/parity_features.rs::parity_supersession_edges replays the chunks
through engine::features::supersession_edges and asserts pair-for-pair
equality including order.

Run with the brain venv (torch + the vendored tree are required):
  packages/brain/.venv/bin/python parity/gen_supersession_fixtures.py [out.json]
"""
import json
import os
import sys
from dataclasses import asdict

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "fixtures", "supersession.json")
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(REPO, "packages", "brain", "src"))

from dasein_brain import _flags  # noqa: F401,E402  parity pins before vendored imports
from dasein_brain.scorer import parse_internal  # noqa: E402
from dasein_brain.vendored.chunking import Chunk  # noqa: E402
from dasein_brain.vendored.pyg_model import edges  # noqa: E402


def rel4(chunks):
    """The vendored emission: rel-4 pairs in loop order (emb is irrelevant to
    the supersession block — any non-degenerate matrix works)."""
    emb = np.asarray([[1.0, float(k + 1), 0.5, -0.25] for k in range(len(chunks))],
                     dtype=np.float32)
    ei, et = edges(chunks, emb, causal=True)
    return [[int(a), int(b)] for a, b, t in
            zip(ei[0].tolist(), ei[1].tolist(), et.tolist()) if t == 4]


PY_SRC = "\n".join([
    "import os",
    "from collections import defaultdict",
    "",
    "def alpha(x):",
    "    if x > 0:",
    "        return process_records(x) * 2",
    "    return -x",
    "",
    "class Beta:",
    "    def method(self, y):",
    "        total = 0",
    "        for i in range(y):",
    "            total += alpha(i)",
    "        return total",
])
PY_TAIL = "\n".join(PY_SRC.splitlines()[7:])            # class Beta block only

CASES = []


def case(name, chunks):
    CASES.append({
        "name": name,
        "chunks": [{k: v for k, v in asdict(c).items() if k != "emb"} for c in chunks],
        "expected": rel4(chunks),
    })


# 1. line-overlap branch: exact re-read, partial overlaps both sides of 0.6,
#    asymmetric normalization (i's span length in the denominator).
case("line_overlap", [
    Chunk(PY_SRC, "app.py", 1, 14, 0, "read"),
    Chunk(PY_TAIL, "app.py", 8, 14, 1, "read"),          # 7/14 of ch0 < 0.6; 7/7 of ch1... j later
    Chunk(PY_SRC, "app.py", 1, 14, 2, "read"),           # exact re-read of ch0 AND covers ch1
    Chunk("unrelated tail words", "app.py", 12, 14, 3, "read"),   # 3/14, 3/7, 3/14 line ovs
])

# 2. Jaccard branch: fileless near-duplicates (reasoning / test output),
#    plus a pair below the 0.6 threshold.
NEAR_A = "run_batch load_shard normalize_rows validate_schema parse_config"
NEAR_B = "run_batch load_shard normalize_rows validate_schema parse_configs!"
FAR = "totally_different words_here nothing_shared at_all_zone"
case("jaccard", [
    Chunk(NEAR_A, None, None, None, 0, "other"),
    Chunk(NEAR_B, None, None, None, 1, "other"),
    Chunk(FAR, None, None, None, 2, "other"),
    Chunk(NEAR_A, None, None, None, 3, "reasoning"),
])

# 3. Python-falsy empty-basename file ("src/:5:match" grep lines): the line
#    branch is skipped, Jaccard still applies; mixed with a real file.
case("empty_basename", [
    Chunk("alpha_span beta_span gamma_span delta_span", "", 1, 4, 0, "grep"),
    Chunk("alpha_span beta_span gamma_span delta_span", "", 2, 5, 1, "grep"),
    Chunk("alpha_span beta_span gamma_span delta_span", "q.py", 1, 4, 2, "read"),
    Chunk("alpha_span beta_span gamma_span delta_span", "q.py", 2, 5, 3, "read"),
])

# 4. same-step pairs never edge; step order (not index order) decides OLD->NEW.
case("same_step_and_order", [
    Chunk(NEAR_A, None, None, None, 1, "other"),
    Chunk(NEAR_A, None, None, None, 1, "other"),
    Chunk(NEAR_A, None, None, None, 0, "other"),         # EARLIER step at a LATER index
])

# 5. lo=None whole-file reads: no line branch even with equal files.
case("lo_none", [
    Chunk(PY_SRC, "app.py", None, None, 0, "read"),
    Chunk(PY_SRC, "app.py", None, None, 1, "read"),
    Chunk(FAR, "app.py", None, None, 2, "read"),
])

# 6. realistic parse: a re-read conversation through the vendored chunker —
#    the exact chunk objects the serving path scores.
Q = "\n".join(f"def f{i}(x):\n    return g{i}(x)" for i in range(8))
MSGS = [
    {"role": "user", "content": "Fix f0 in q.py"},
    {"role": "assistant", "content": "Read it.",
     "extra": {"actions": [{"tool": "Read", "command": "cat q.py"}]}},
    {"role": "tool", "content": "returncode: 0\n" + Q},
    {"role": "assistant", "content": "Check the tail.",
     "extra": {"actions": [{"tool": "Read", "command": "sed -n '9,16p' q.py"}]}},
    {"role": "tool", "content": "returncode: 0\n" + "\n".join(Q.splitlines()[8:])},
    {"role": "assistant", "content": "Re-read the whole file.",
     "extra": {"actions": [{"tool": "Read", "command": "cat q.py"}]}},
    {"role": "tool", "content": "returncode: 0\n" + Q},
]
parsed = parse_internal(MSGS, 10)
case("parsed_reread", parsed.chunks)
assert CASES[-1]["expected"], "parsed_reread must emit rel-4 pairs"

json.dump({"cases": CASES}, open(OUT, "w"), indent=1, ensure_ascii=False)
n_edges = sum(len(c["expected"]) for c in CASES)
print(f"wrote {OUT}: {len(CASES)} cases, {n_edges} rel-4 pairs")
