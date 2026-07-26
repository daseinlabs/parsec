#!/usr/bin/env python3
"""Convert a recorded conversation into the §8.1 golden fixture
(packages/proxy/parity/fixtures/golden_conversation.json).

Two inputs (auto-detected):
  directory  — a PARSEC_RECORD_DIR conversation dir (turn_<n>.json files,
               each a full /v1/messages body as the client sent it)
  *.jsonl    — a Claude Code transcript (~/.claude/projects/<proj>/<id>.jsonl);
               the Anthropic request envelope (system + tools) is synthesized
               and per-turn bodies are rebuilt as growing message prefixes.

Output: {"turns": [body_1, body_2, ...]} — body_k is the full Anthropic
/v1/messages body of the k-th request of the conversation.

Sanitization (default ON — committed fixtures must never carry raw content):
every text / tool_result / thinking string is rewritten line-by-line with
deterministic pseudo-random ASCII of the SAME character length (seeded from
sha256 of the line, so re-runs are byte-stable and duplicate lines stay
duplicates — dup-feature structure survives). Line count, leading whitespace,
and a leading case-insensitive 'returncode' token (the freeze digest keys on
it, engine/src/freeze.rs) are preserved. File paths inside tool_use inputs
keep only their basename, prefixed 'redacted/'. Roles, block types, tool
names, ids, ordering, and lengths are untouched — the structure is exactly
what the §8.1 golden test exercises.

Usage:
  scripts/record_to_fixture.py TRANSCRIPT.jsonl -o fixture.json --max-turns 21
  scripts/record_to_fixture.py $PARSEC_RECORD_DIR/<conv_id> -o fixture.json
"""

import argparse
import hashlib
import json
import pathlib
import re
import sys

# ── sanitizer ────────────────────────────────────────────────────────────────

_ALPHABET = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
# Leading returncode token (any of the wire shapes chunking.py parses) —
# preserved verbatim so digest/rc extraction behaves as on the raw trace.
_RC_RE = re.compile(
    r"(?i)^(<returncode>[\s]*-?\d+[\s]*</returncode>|returncode[:=][\s]*-?\d+|returncode)"
)
_PATH_KEYS = {"file_path", "path", "notebook_path", "filename", "file", "cwd"}


def _scramble_line(line: str) -> str:
    lead = line[: len(line) - len(line.lstrip())]
    rest = line[len(lead):]
    keep = ""
    m = _RC_RE.match(rest)
    if m:
        keep = m.group(0)
        rest = rest[len(keep):]
    if not rest:
        return lead + keep
    seed = hashlib.sha256(line.encode("utf-8")).digest()
    out: list[str] = []
    i = 0
    while len(out) < len(rest):
        block = hashlib.sha256(seed + i.to_bytes(4, "big")).digest()
        out.extend(_ALPHABET[b % len(_ALPHABET)] for b in block)
        i += 1
    return lead + keep + "".join(out[: len(rest)])


def scramble_text(text: str) -> str:
    return "\n".join(_scramble_line(ln) for ln in text.split("\n"))


def _redact_path(p: str) -> str:
    base = p.rstrip("/").rsplit("/", 1)[-1] or "root"
    return f"redacted/{base}"


def _sanitize_tool_input(v, key=None):
    if isinstance(v, str):
        if key in _PATH_KEYS:
            return _redact_path(v)
        return scramble_text(v)
    if isinstance(v, list):
        return [_sanitize_tool_input(x) for x in v]
    if isinstance(v, dict):
        return {k: _sanitize_tool_input(x, key=k) for k, x in v.items()}
    return v


def _sanitize_block(b: dict) -> dict:
    t = b.get("type")
    out = dict(b)
    if t == "text" and isinstance(out.get("text"), str):
        out["text"] = scramble_text(out["text"])
    elif t == "thinking":
        for k in ("thinking", "signature"):
            if isinstance(out.get(k), str):
                out[k] = scramble_text(out[k])
    elif t == "tool_use":
        if isinstance(out.get("input"), dict):
            out["input"] = _sanitize_tool_input(out["input"])
    elif t == "tool_result":
        c = out.get("content")
        if isinstance(c, str):
            out["content"] = scramble_text(c)
        elif isinstance(c, list):
            out["content"] = [
                {**p, "text": scramble_text(p["text"])}
                if isinstance(p, dict) and isinstance(p.get("text"), str)
                else p
                for p in c
            ]
    return out


def _sanitize_content(c):
    if isinstance(c, str):
        return scramble_text(c)
    if isinstance(c, list):
        return [_sanitize_block(b) if isinstance(b, dict) else b for b in c]
    return c


def sanitize_body(body: dict) -> dict:
    out = dict(body)
    if isinstance(out.get("system"), str):
        out["system"] = scramble_text(out["system"])
    elif isinstance(out.get("system"), list):
        out["system"] = [
            _sanitize_block(b) if isinstance(b, dict) else b for b in out["system"]
        ]
    if isinstance(out.get("messages"), list):
        out["messages"] = [
            {**m, "content": _sanitize_content(m.get("content"))}
            if isinstance(m, dict)
            else m
            for m in out["messages"]
        ]
    if isinstance(out.get("metadata"), dict):
        out["metadata"] = {
            k: scramble_text(v) if isinstance(v, str) else v
            for k, v in out["metadata"].items()
        }
    return out


# ── transcript mode: rebuild request bodies from a CC session jsonl ─────────

# Representative envelope for transcript-rebuilt turns. Deliberately NOT a
# metadata-call phrasing (server.rs is_client_metadata_call must not match).
SYSTEM = (
    "You are Claude Code, Anthropic's official CLI for Claude. You are an "
    "interactive CLI tool that helps users with software engineering tasks. "
    "Use the instructions below and the tools available to you to assist the "
    "user.\n\n"
    "# Tone and style\n"
    "You should be concise, direct, and to the point. Output text to "
    "communicate with the user; all text you output outside of tool use is "
    "displayed to the user. Only use emojis if the user explicitly requests "
    "it.\n\n"
    "# Doing tasks\n"
    "The user will primarily request you perform software engineering tasks: "
    "solving bugs, adding new functionality, refactoring code, explaining "
    "code. Use the available search tools to understand the codebase and the "
    "user's query. Verify solutions with tests when possible.\n\n"
    "# Tool usage policy\n"
    "When doing file search, prefer to reduce context usage. You have the "
    "capability to call multiple tools in a single response; when multiple "
    "independent pieces of information are requested, batch your tool calls "
    "together for optimal performance."
)

# Plausible input_schema stubs per tool name; unknown names get the default.
_S = lambda: {"type": "string"}  # noqa: E731
_N = lambda: {"type": "number"}  # noqa: E731
TOOL_STUBS: dict[str, tuple[str, dict, list]] = {
    "Bash": (
        "Executes a given bash command in a persistent shell session.",
        {"command": _S(), "timeout": _N(), "description": _S()},
        ["command"],
    ),
    "Read": (
        "Reads a file from the local filesystem.",
        {"file_path": _S(), "offset": _N(), "limit": _N()},
        ["file_path"],
    ),
    "Write": (
        "Writes a file to the local filesystem.",
        {"file_path": _S(), "content": _S()},
        ["file_path", "content"],
    ),
    "Edit": (
        "Performs exact string replacements in files.",
        {
            "file_path": _S(),
            "old_string": _S(),
            "new_string": _S(),
            "replace_all": {"type": "boolean"},
        },
        ["file_path", "old_string", "new_string"],
    ),
    "MultiEdit": (
        "Makes multiple edits to a single file in one operation.",
        {"file_path": _S(), "edits": {"type": "array"}},
        ["file_path", "edits"],
    ),
    "Grep": (
        "A powerful search tool built on ripgrep.",
        {"pattern": _S(), "path": _S(), "glob": _S(), "output_mode": _S()},
        ["pattern"],
    ),
    "Glob": (
        "Fast file pattern matching tool that works with any codebase size.",
        {"pattern": _S(), "path": _S()},
        ["pattern"],
    ),
    "Task": (
        "Launch a new agent to handle complex, multi-step tasks autonomously.",
        {"description": _S(), "prompt": _S(), "subagent_type": _S()},
        ["description", "prompt"],
    ),
    "TodoWrite": (
        "Create and manage a structured task list for the current session.",
        {"todos": {"type": "array"}},
        ["todos"],
    ),
    "WebFetch": (
        "Fetches content from a specified URL and processes it.",
        {"url": _S(), "prompt": _S()},
        ["url", "prompt"],
    ),
    "WebSearch": (
        "Search the web and use the results to inform responses.",
        {"query": _S()},
        ["query"],
    ),
    "NotebookEdit": (
        "Replaces the contents of a specific cell in a Jupyter notebook.",
        {"notebook_path": _S(), "new_source": _S()},
        ["notebook_path", "new_source"],
    ),
}


def tool_schema(name: str) -> dict:
    desc, props, req = TOOL_STUBS.get(
        name, ("A tool available in this session.", {}, [])
    )
    schema: dict = {"type": "object", "properties": props}
    if req:
        schema["required"] = req
    return {"name": name, "description": desc, "input_schema": schema}


# API-visible block keys — transcripts carry CC-internal extras (`caller`,
# usage annotations) that a real request body never had.
_BLOCK_KEYS = {
    "text": ("type", "text"),
    "thinking": ("type", "thinking", "signature"),
    "tool_use": ("type", "id", "name", "input"),
    "tool_result": ("type", "tool_use_id", "content", "is_error"),
}


def _clean_block(b: dict) -> dict:
    keys = _BLOCK_KEYS.get(b.get("type"))
    if keys is None:
        return b
    return {k: b[k] for k in keys if k in b}


def load_transcript(path: pathlib.Path) -> list[dict]:
    """Merged message list: consecutive user lines coalesce (parallel
    tool_results arrive as separate jsonl lines but were ONE request
    message), and assistant lines sharing a message id are one API response
    streamed as multiple lines. Meta and sidechain lines are skipped."""
    merged: list[tuple[str, list, str | None]] = []
    for line in path.open():
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        if d.get("isSidechain") or d.get("isMeta"):
            continue
        if d.get("type") not in ("user", "assistant"):
            continue
        m = d.get("message") or {}
        role, content = m.get("role"), m.get("content")
        if role not in ("user", "assistant"):
            continue
        if isinstance(content, str):
            content = [{"type": "text", "text": content}]
        if not isinstance(content, list):
            continue
        blocks = [_clean_block(b) if isinstance(b, dict) else b for b in content]
        mid = m.get("id")
        if merged and merged[-1][0] == role and (
            role == "user" or (mid and merged[-1][2] == mid)
        ):
            merged[-1][1].extend(blocks)
        else:
            merged.append((role, list(blocks), mid))
    return [{"role": r, "content": c} for r, c, _ in merged]


def turns_from_transcript(path: pathlib.Path, max_turns: int) -> list[dict]:
    msgs = load_transcript(path)
    prefixes: list[list[dict]] = []
    for i, m in enumerate(msgs):
        if m["role"] == "assistant" and i > 0 and msgs[i - 1]["role"] == "user":
            prefixes.append(msgs[:i])
            if len(prefixes) == max_turns:
                break
    if not prefixes:
        sys.exit(f"no request turns reconstructable from {path}")
    used: list[str] = []
    for m in prefixes[-1]:
        for b in m["content"]:
            if isinstance(b, dict) and b.get("type") == "tool_use":
                n = b.get("name")
                if n and n not in used:
                    used.append(n)
    tools = [tool_schema(n) for n in used]
    return [
        {
            "model": "claude-sonnet-5",
            "max_tokens": 16384,
            "system": SYSTEM,
            "tools": tools,
            "messages": p,
        }
        for p in prefixes
    ]


# ── record-dir mode ──────────────────────────────────────────────────────────

def turns_from_record_dir(path: pathlib.Path, max_turns: int) -> list[dict]:
    files = sorted(
        (
            f
            for f in path.iterdir()
            if re.fullmatch(r"turn_\d+\.json", f.name)
        ),
        key=lambda f: int(f.stem.split("_")[1]),
    )
    if not files:
        sys.exit(f"no turn_<n>.json files in {path}")
    return [json.loads(f.read_bytes()) for f in files[:max_turns]]


# ── main ─────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("input", help="record-dir conversation OR CC transcript .jsonl")
    ap.add_argument("-o", "--out", required=True)
    ap.add_argument("--max-turns", type=int, default=10**9)
    ap.add_argument(
        "--no-sanitize",
        action="store_true",
        help="keep raw content (NEVER for committed fixtures)",
    )
    args = ap.parse_args()

    src = pathlib.Path(args.input)
    if src.is_dir():
        turns = turns_from_record_dir(src, args.max_turns)
        synthetic_envelope = False
    else:
        turns = turns_from_transcript(src, args.max_turns)
        synthetic_envelope = True

    if not args.no_sanitize:
        # Leak check: no content line of the source may survive into the
        # output. Collected over raw content STRINGS (not the JSON dump, whose
        # structural keys legitimately survive sanitization).
        def content_strings(v):
            if isinstance(v, str):
                yield v
            elif isinstance(v, list):
                for x in v:
                    yield from content_strings(x)
            elif isinstance(v, dict):
                for k, x in v.items():
                    if k in ("type", "id", "name", "tool_use_id", "role"):
                        continue
                    yield from content_strings(x)

        probes: set[str] = set()
        for m in turns[-1].get("messages", []):
            for s in content_strings(m.get("content")):
                for ln in s.split("\n"):
                    ln = ln.strip()
                    if len(ln) > 24:
                        probes.add(ln[:80])
        turns = [sanitize_body(t) for t in turns]
        if synthetic_envelope:
            # The synthesized envelope is already clean — keep it readable
            # (a representative English system prompt, not scrambled bytes).
            for t in turns:
                t["system"] = SYSTEM
        blob = "\n".join(
            s
            for t in turns
            for m in t.get("messages", [])
            for s in content_strings(m.get("content"))
        )
        leaked = [p for p in probes if p in blob]
        if leaked:
            sys.exit(f"sanitizer leak check FAILED, e.g. {leaked[0]!r}")

    out = pathlib.Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    with out.open("w") as f:
        json.dump({"turns": turns}, f)
        f.write("\n")
    size = out.stat().st_size
    n_msgs = len(turns[-1].get("messages", []))
    print(
        f"wrote {out} · {len(turns)} turns · final prefix {n_msgs} messages · "
        f"{size / 1024:.0f} KiB · sanitize={'OFF' if args.no_sanitize else 'on'}"
    )


if __name__ == "__main__":
    main()
