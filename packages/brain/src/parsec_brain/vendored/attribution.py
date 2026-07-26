"""Universal, gold-free dense supervision: per-candidate UTILITY from downstream dependency.

The one idea that makes outcome-learning work across every agent archetype (code repair, RAG-QA,
tool/API, web/GUI, SQL, OS-use): a surfaced context item is USEFUL iff its distinctive content is
*consumed by* a later high-value action, weighted by whether the episode actually succeeded.

There are NO golds in production. The ENVIRONMENT OUTCOME is the arbiter; "consumed-by" tells us
*which* candidates were involved, the outcome tells us whether that involvement was good:
  - consumed in a SUCCESS  -> positive
  - consumed in a FAILURE  -> negative (it misled the agent), push it down
  - never consumed         -> ~0 (relevance != utility; this is the whole thesis)

"Consumed-by" has three modality-free channels (we scan the WHOLE downstream region = every later
action's text/arguments PLUS the final output, not just the answer):
  1. output containment   — candidate content reappears in the final output (answer/patch/SQL).   [QA, code]
  2. argument provenance  — candidate content becomes an ARG/operand of a later action            [tools, web, grep]
                            (next tool call, `cat <path>`, clicked selector, SQL identifier).
  3. reasoning reference  — candidate content echoed in later reasoning (embedding-soft).          [planning, research]

Detection is deterministic + cheap (distinctive-span overlap, IDF-weighted so boilerplate doesn't
count) with an embedding-similarity backstop for paraphrase. No per-task parser, no extra LLM call,
runs entirely off the hot path over already-captured (candidates, trajectory, outcome).
"""

from __future__ import annotations

import re
from functools import lru_cache

# distinctive spans: identifiers / dotted-paths / file paths / CamelCase / snake_case / numbers /
# quoted strings. Deliberately broad — IDF down-weights anything common across the candidate pool.
_SPAN = re.compile(r"""[A-Za-z_][A-Za-z0-9_]*(?:[./-][A-Za-z0-9_]+)+   # paths / dotted / a-b-c
                     | [A-Za-z_][A-Za-z0-9_]{3,}                        # bare identifiers (>=4 chars)
                     | "[^"\n]{2,}" | '[^'\n]{2,}'                      # quoted strings
                     | \b\d{2,}\b""", re.VERBOSE)

# extremely common code/English tokens that are never "distinctive" regardless of IDF
_STOP = {"self", "true", "false", "none", "null", "return", "import", "from", "class", "value",
         "string", "result", "object", "the", "and", "for", "with", "that", "this", "your",
         "def", "function", "print", "test", "tests", "file", "code", "line", "name"}


@lru_cache(maxsize=65536)
def _spans_cached(text: str) -> frozenset[str]:
    out = set()
    for m in _SPAN.findall(text):
        s = m.strip("\"'").lower()
        if s and s not in _STOP and not s.isdigit() or (s.isdigit() and len(s) >= 3):
            out.add(s)
    return frozenset(out)


def spans(text: str) -> set[str]:
    # MEMOIZED: spans() is a pure function of `text` and was 1.1M calls / 58s in the curate profile
    # (re-run on the same chunk text every step). Every caller wraps the result in a fresh
    # comprehension/set() and never mutates it, so returning a shared (frozen) value is bit-identical.
    # frozenset keeps the value immutable so the cache can never be corrupted; set ops (|,&,len,in,
    # iteration) all work identically on it.
    if not text:
        return set()
    return _spans_cached(text)








