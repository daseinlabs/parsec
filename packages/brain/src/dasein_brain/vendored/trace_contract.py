"""Trace contract v1 — the single capture schema BOTH harnesses emit.

VENDORED TRIM (dasein-brain serving): only the tool-schema normalization the tool head scores
survives (_tok / normalize_tool_def / tool_schema_chunks) — byte-identical bodies. The capture
schema, bash-twin projection and SDK-stream conversion live client-side (Rust port) now.

``tools_available`` is the list of tool definitions the agent had — the schemas the
curator trims. Each entry is one curatable TOOL-SCHEMA CHUNK::

    {name, description, parameters, source, text, tokens}
        name        — tool name, the join key to extra.actions[].tool (the call signal)
        description — the tool's natural-language description (the bulk of the prose mass)
        parameters  — the JSON-schema of its arguments (dict), for finer per-field trim later
        source      — "native" / "mcp:<server>" / "plugin:<name>" (provenance; MCP/plugin
                      tools are the long tail that's almost always unused → first to cut)
        text        — the serialized schema (what gets embedded/chunked/scored)
        tokens      — token weight of this definition (the trim COST of keeping it)
"""

from __future__ import annotations

import json
from typing import Iterable, Optional

CONTRACT_VERSION = 1


def _tok(s: str) -> int:
    """Token estimate (chars/4) — the convention chunking.blob_tokens uses."""
    return len(s) // 4


def normalize_tool_def(t: dict, source: str = "native") -> Optional[dict]:
    """Normalize ONE tool definition (Anthropic ``{name,description,input_schema}`` OR
    OpenAI ``{type:function, function:{name,description,parameters}}``) into a curatable
    tool-schema chunk ``{name, description, parameters, source, text, tokens}``.

    ``text`` is the canonical serialized schema (name + description + params) — the unit
    the curator embeds/scores; ``tokens`` is its weight (the cost of keeping the tool)."""
    if not isinstance(t, dict):
        return None
    fn = t.get("function") if isinstance(t.get("function"), dict) else t
    name = fn.get("name") or t.get("name") or ""
    if not name:
        return None
    desc = fn.get("description") or t.get("description") or ""
    params = (fn.get("parameters") or fn.get("input_schema")
              or t.get("input_schema") or t.get("parameters") or {})
    if not isinstance(params, dict):
        params = {}
    text = json.dumps({"name": name, "description": desc, "parameters": params},
                      ensure_ascii=False, sort_keys=True)
    return {"name": name, "description": desc, "parameters": params,
            "source": source, "text": text, "tokens": _tok(text)}


def tool_schema_chunks(tools: Iterable[dict], source: str = "native") -> list[dict]:
    """Build the ``tools_available`` list (curatable tool-schema chunks) from a tools array.

    Pass each provenance group with its ``source`` and concatenate the results (native +
    one call per MCP server / plugin) so the long-tail MCP/plugin schemas are tagged and
    first to cut. De-dupes by name (last wins)."""
    out: dict[str, dict] = {}
    for t in tools or []:
        c = normalize_tool_def(t, source)
        if c is not None:
            out[c["name"]] = c
    return list(out.values())
