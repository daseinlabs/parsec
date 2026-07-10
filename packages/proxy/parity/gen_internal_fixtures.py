#!/usr/bin/env python3
"""Generate to_internal parity fixtures from service/anthropic_shapes.py.

Covers: system flattening (str / blocks / empty / whitespace-join), the
per-message text view (tool_result inner joins incl. empty-string parts,
skipped thinking/image/document blocks), assistant tool_use -> tool_calls +
extra.actions (splat overrides, non-dict inputs), and the bash-twin /
derive_command / derive_query projections across every tool-name set and
offset/limit coercion edge — plus direct-call fixtures for the three
projection functions (non-dict args, Python str()/repr() rendering, clips).
"""
import json
import os
import sys

ACC = os.environ.get("ACC_ROOT") or os.path.abspath(
    os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", "adaptive-context-clean"))
sys.path.insert(0, ACC)

from service import anthropic_shapes as S  # noqa: E402
# Import directly so a broken twin import fails HERE instead of silently
# flipping _actions_from_tool_use onto its degraded fallback path.
from adaptive_context.eval.trace_contract import (  # noqa: E402
    bash_twin_command, derive_command, derive_query)

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    os.path.dirname(__file__), "fixtures", "internal.json")


def B(msgs, system=None):
    b = {"model": "claude-x", "max_tokens": 8, "messages": msgs}
    if system is not None:
        b["system"] = system
    return b


def tu(name=None, input=..., id="tu_1"):
    b = {"type": "tool_use", "id": id}
    if name is not None:
        b["name"] = name
    if input is not ...:
        b["input"] = input
    return b


def asst(*blocks):
    return {"role": "assistant", "content": list(blocks)}


LONG = "α" * 170                                   # >160 chars, non-ASCII (char clip)
QLONG = "x'y" * 80                                 # >200 chars with quotes (grep clip)
TR = {"type": "tool_result", "tool_use_id": "t1", "content": "obs text " * 5}
TR_EMPTY_PARTS = {"type": "tool_result", "tool_use_id": "t2",
                  "content": [{"type": "text", "text": ""}, {"type": "text", "text": "x"},
                              {"type": "image", "source": "s"}]}
TRE = {"type": "tool_result", "tool_use_id": "t3", "is_error": True, "content": "boom"}

BODIES = [
    ("plain_string", B([{"role": "user", "content": "hello wörld"}])),
    ("text_blocks", B([{"role": "user", "content": [
        {"type": "text", "text": "a"}, {"type": "text", "text": ""},
        {"type": "text", "text": "b"}]}])),
    ("tool_result_str", B([{"role": "user", "content": [TR]}])),
    # inner join is UNFILTERED (" x"); outer join drops empties
    ("tool_result_empty_parts", B([{"role": "user", "content": [TR_EMPTY_PARTS,
                                                                {"type": "text", "text": "tail"}]}])),
    ("parallel_tool_results", B([{"role": "user", "content": [TR, TR_EMPTY_PARTS, TRE]}])),
    ("thinking_blocks_skipped", B([asst(
        {"type": "thinking", "thinking": "let me think", "signature": "sig"},
        {"type": "redacted_thinking", "data": "xxxx"},
        {"type": "text", "text": "visible"})])),
    ("image_document_skipped", B([{"role": "user", "content": [
        {"type": "image", "source": {"type": "base64", "data": "aGk="}},
        {"type": "document", "source": {"type": "text", "data": "doc"}},
        {"type": "text", "text": "caption"}]}])),
    ("non_dict_parts", B([{"role": "user", "content": [{"type": "text", "text": "x"}, "bare", 7]}])),
    ("system_str", B([{"role": "user", "content": "hi"}], system="You are an agent.")),
    ("system_blocks", B([{"role": "user", "content": "hi"}], system=[
        {"type": "text", "text": "block one"}, {"type": "image", "source": "x"},
        {"type": "text", "text": "block twö"}])),
    ("system_empty_str", B([{"role": "user", "content": "hi"}], system="")),
    # "\n".join(["", ""]) == "\n" is TRUTHY -> a system entry of just "\n"
    ("system_blocks_whitespace", B([{"role": "user", "content": "hi"}], system=[
        {"type": "text", "text": ""}, {"type": "text", "text": ""}])),
    ("system_non_list_dict", B([{"role": "user", "content": "hi"}],
                               system={"type": "text", "text": "x"})),
    ("no_messages", B([], system="sys")),
    ("messages_missing", {"model": "claude-x", "max_tokens": 8, "system": "sys"}),
    ("role_missing_defaults_user", B([{"content": "x"}])),
    ("role_null_passthrough", B([{"role": None, "content": "x"}])),
    ("role_tool_passthrough", B([{"role": "tool", "content": "t out"}])),
    ("assistant_no_tool_use", B([asst({"type": "text", "text": "just prose"})])),
    ("user_tool_use_ignored", B([{"role": "user", "content": [
        tu("Bash", {"command": "ls"}), {"type": "text", "text": "u"}]}])),
    # ── shell twin ──
    ("bash_simple", B([asst(tu("Bash", {"command": "ls -la /tmp"}))])),
    ("bash_ws_command_derive_literal", B([asst(tu("Bash", {"command": "   "}))])),
    ("bash_empty_command", B([asst(tu("bash", {"command": ""}))])),
    ("bash_nonstr_command", B([asst(tu("Bash", {"command": 123, "description": "d"}))])),
    ("bash_output_tool", B([asst(tu("BashOutput", {"command": "tail -f log"}, id="tu_a"),
                                 tu("bash_output", {"bash_id": "b1"}, id="tu_b"))])),
    ("shell_unicode_command", B([asst(tu("shell", {"command": "echo 'héllo 🐍'"}))])),
    # ── read twin: offset/limit truthiness + coercion ──
    ("read_cat", B([asst(tu("Read", {"file_path": "/a/b.py"}))])),
    ("read_offset_limit", B([asst(tu("Read", {"file_path": "/a/b.py", "offset": 10, "limit": 5}))])),
    ("read_offset_only", B([asst(tu("Read", {"file_path": "/a/b.py", "offset": 10}))])),
    ("read_limit_only", B([asst(tu("Read", {"file_path": "/a/b.py", "limit": 40}))])),
    ("read_offset0", B([asst(tu("Read", {"file_path": "/a/b.py", "offset": 0, "limit": 5}))])),
    ("read_limit0", B([asst(tu("Read", {"file_path": "/a/b.py", "offset": 5, "limit": 0}))])),
    ("read_null_offset", B([asst(tu("Read", {"file_path": "/a/b.py", "offset": None, "limit": 3}))])),
    ("read_str_nums", B([asst(tu("Read", {"file_path": "/a/b.py", "offset": "12", "limit": " 3 "}))])),
    ("read_invalid_offset_nulls_both", B([asst(tu("Read", {"file_path": "/a/b.py",
                                                           "offset": "abc", "limit": 5}))])),
    ("read_invalid_limit_nulls_both", B([asst(tu("Read", {"file_path": "/a/b.py",
                                                          "offset": 10, "limit": "x"}))])),
    ("read_float_bool", B([asst(tu("Read", {"file_path": "/a/b.py", "offset": 5.9, "limit": True}))])),
    ("read_negative_limit", B([asst(tu("Read", {"file_path": "/a/b.py", "offset": 7, "limit": -3}))])),
    ("read_name_variants", B([asst(tu("view", {"path": "/x/y"}, id="tu_a"),
                                   tu("NotebookRead", {"filename": "n.ipynb"}, id="tu_b"),
                                   tu("open_file", {"file": "f.txt"}, id="tu_c"),
                                   tu("cat", {"file_path": "z"}, id="tu_d"))])),
    ("read_file_path_falsy_falls_through", B([asst(tu("Read", {"file_path": "", "path": "/real"}))])),
    ("read_file_nonstr_derives", B([asst(tu("Read", {"file_path": [1], "path": "/real"}))])),
    # ── edit twin ──
    ("edit_basename", B([asst(tu("Edit", {"file_path": "/a/b/c.py", "old_string": "x",
                                          "new_string": "y"}))])),
    ("write_no_slash", B([asst(tu("Write", {"path": "notes.md", "content": "body " * 40}))])),
    ("edit_trailing_slash", B([asst(tu("NotebookEdit", {"file_path": "/a/b/"}))])),
    ("str_replace_editor", B([asst(tu("str_replace_based_edit_tool", {"path": "/w/e.rs"}))])),
    ("multiedit", B([asst(tu("MultiEdit", {"file_path": "src/main.rs", "edits": [{"old": "a"}]}))])),
    # ── grep twin ──
    ("grep_pattern_path", B([asst(tu("Grep", {"pattern": "fn main", "path": "src/"}))])),
    ("grep_quotes_swapped", B([asst(tu("Grep", {"pattern": "it's a 'test'", "include": "*.rs"}))])),
    ("grep_long_pattern_clip", B([asst(tu("Grep", {"pattern": QLONG}))])),
    ("glob_pattern", B([asst(tu("Glob", {"pattern": "**/*.py"}))])),
    ("search_query_key", B([asst(tu("search", {"query": "needle"}))])),
    ("searchcode_q_key", B([asst(tu("search_code", {"q": "tok"}))])),
    ("rg_empty", B([asst(tu("rg", {}))])),
    ("grep_nonstr_pattern_path", B([asst(tu("find", {"pattern": [1, "a'b"], "path": 5}))])),
    # twin is pattern-first, derive_query is query-first
    ("grep_pattern_vs_query_order", B([asst(tu("search", {"pattern": "P", "query": "Q"}))])),
    # ── derive_command fallback ──
    ("unknown_tool_sorted_render", B([asst(tu("mcp__custom", {
        "zeta": "z", "alpha": LONG, "num": 3.5, "flag": True, "nul": None,
        "list": [1, 2], "obj": {"k": "v'w"}}))])),
    ("unknown_tool_total_clip", B([asst(tu("WebFetch", {
        "a": "p" * 200, "b": "q" * 200, "c": "r" * 200}))])),
    ("unknown_tool_empty_args", B([asst(tu("TodoWrite", {}))])),
    # ── splat ordering ──
    ("splat_overrides_tool_name_command", B([asst(tu("customtool", {
        "tool": "X", "name": "Y", "command": "rm -rf /", "query": "   "}))])),
    ("splat_query_overwritten_same_slot", B([asst(tu("customtool", {"query": "real", "z": 1}))])),
    ("splat_bash_command_twin_wins", B([asst(tu("bash", {"command": "echo hi", "extra": 1}))])),
    # ── non-dict / missing input ──
    ("nondict_input_str", B([asst(tu("Bash", "ls -la"))])),
    ("nondict_input_list", B([asst(tu("Grep", [1, 2]))])),
    ("nondict_input_null", B([asst(tu("Read", None))])),
    ("nondict_input_zero", B([asst(tu("mystery", 0))])),
    ("input_missing", B([asst(tu("Bash"))])),
    ("block_type_only", B([asst({"type": "tool_use"})])),
    ("name_whitespace_stripped", B([asst(tu("  Read  ", {"file_path": "/a"}))])),
    ("unicode_args_ascii_escape", B([asst(tu("Bash", {"command": "grep 'é' — 🐍"}))])),
    # ── parallel + interleaved ──
    ("parallel_tool_use_interleaved", B([asst(
        {"type": "text", "text": "first I read"},
        tu("Read", {"file_path": "/a.py", "offset": 3, "limit": 4}, id="tu_a"),
        {"type": "text", "text": "then I grep"},
        tu("Grep", {"pattern": "todo", "glob": "**/*.md"}, id="tu_b"))])),
    ("conversation_mixed", B([
        {"role": "user", "content": "fix the bug"},
        asst({"type": "text", "text": "reading"},
             tu("Read", {"file_path": "/src/x.py"}, id="tu_a")),
        {"role": "user", "content": [TR]},
        asst(tu("Grep", {"pattern": "def x", "path": "src"}, id="tu_b"),
             tu("Edit", {"file_path": "/src/x.py", "old_string": "a", "new_string": "b"},
                id="tu_c")),
        {"role": "user", "content": [TR, TR_EMPTY_PARTS]},
    ], system="You fix bugs.")),
]

TWIN_CASES = [
    ("strip_lower_name", " Read ", {"file_path": "/a"}),
    ("nondict_args_read", "READ", "not a dict"),
    ("nondict_args_grep", "grep", 5),
    ("bash_unstripped_return", "Bash", {"command": "  x  "}),
    ("bash_empty_none", "bash", {"command": ""}),
    ("shell_null_command", "shell", {"command": None}),
    ("edit_nested", "edit", {"file_path": "a/b/c"}),
    ("unknown_none", "unknown", {"command": "ls"}),
    ("read_underscore_int", "read", {"file_path": "/f", "offset": "1_2", "limit": "2"}),
    ("read_signed_str_ints", "read", {"file_path": "/f", "offset": " -2 ", "limit": "+3"}),
    ("read_bad_underscore", "read", {"file_path": "/f", "offset": "1__2", "limit": "2"}),
    ("read_float_str_invalid", "read", {"file_path": "/f", "offset": "5.0", "limit": 2}),
    ("grep_path_only", "glob", {"path": "src/**"}),
    ("grep_falsy_pattern_chain", "rg", {"pattern": "", "query": 0, "q": "hit"}),
]

DCMD_CASES = [
    ("nondict_str_clip", "Tool", "s" * 320),
    ("nondict_none", "Tool", None),
    ("nondict_bool", "Tool", True),
    ("nondict_float", "Tool", 3.5),
    ("nondict_list_repr", "Tool", [1, "a'b", None, 2.5, "d\"q"]),
    ("nondict_list_unicode", "Tool", ["héllo 🐍", "tab\there\nnl"]),
    ("empty_tool_name", "", {"a": "b"}),
    ("shell_upper", "BASH", {"command": "x"}),
    ("shell_empty_command", "Bash", {"command": ""}),
    ("shell_command_nonstr", "run", {"command": [1]}),
    ("raw_str_value_kept", "Tool", {"s": "line1\nline2\ttab"}),
    ("value_clip_unicode", "Tool", {"k": "β" * 170}),
    ("sorted_keys_unicode", "Tool", {"b": 1, "a": 2, "é": 3, "Z": 4}),
]

DQ_CASES = [
    ("search_key", "t", {"search": "found"}),
    ("query_ws_falls_to_pattern", "t", {"query": "  ", "pattern": "p"}),
    ("nonstr_skipped", "t", {"q": 5}),
    ("nondict", "t", "nondict"),
    ("unstripped_return", "t", {"query": " padded "}),
    ("none_query", "t", {"query": None, "search": "s"}),
]


def main():
    fx = {"to_internal": [], "bash_twin_command": [], "derive_command": [], "derive_query": []}
    for name, body in BODIES:
        fx["to_internal"].append({"name": name, "body": body, "expected": S.to_internal(body)})
    for name, tool, args in TWIN_CASES:
        fx["bash_twin_command"].append({"name": name, "tool": tool, "args": args,
                                        "expected": bash_twin_command(tool, args)})
    for name, tool, args in DCMD_CASES:
        fx["derive_command"].append({"name": name, "tool": tool, "args": args,
                                     "expected": derive_command(tool, args)})
    for name, tool, args in DQ_CASES:
        fx["derive_query"].append({"name": name, "tool": tool, "args": args,
                                   "expected": derive_query(tool, args)})

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(fx, f, ensure_ascii=False, indent=1)
    print(f"wrote {OUT}: " + ", ".join(f"{k}={len(v)}" for k, v in fx.items()))


if __name__ == "__main__":
    main()
