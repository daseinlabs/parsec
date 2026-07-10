"""Per-agent-TYPE 'consumed-by' labelers — VENDORED TRIM: chunking.py's serve-path dependency only
(parse_grep_candidate + _is_path). The label rules themselves are training-only and deleted.

Source: adaptive-context-clean/adaptive_context/optimizer/labelers.py
"""

from __future__ import annotations

import re

_EXT = re.compile(r"\.[A-Za-z][A-Za-z0-9]{0,4}$")


def _is_path(tok: str) -> bool:
    return bool(tok) and (("/" in tok) or bool(_EXT.search(tok)))


def parse_grep_candidate(line: str) -> tuple[str, int | None] | None:
    """Robustly parse one grep/find result line -> (file_basename, line|None). Handles BOTH
    `file:line:content` (grep -n) and `file:content` (grep without -n) and bare `file` (find/grep -l).
    Most agent greps omit -n, so this must not drop them."""
    line = (line or "").strip()
    if not line or line.startswith("[reranked"):
        return None
    parts = line.split(":", 2)
    if len(parts) >= 3 and parts[1].strip().isdigit() and _is_path(parts[0]):
        return parts[0].split("/")[-1], int(parts[1])          # file:line:content
    if len(parts) >= 2 and _is_path(parts[0]):
        return parts[0].split("/")[-1], None                   # file:content (no -n)
    tok = line.split()[0].rstrip(":")
    return (tok.split("/")[-1], None) if _is_path(tok) else None   # bare path
