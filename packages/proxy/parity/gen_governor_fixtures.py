#!/usr/bin/env python3
"""Generate governor-signal parity fixtures from the reference
adaptive_context/meta/governor.py + adaptive_context/optimizer/doom.py.

Covers the exact ports the learner proxy makes (governor.rs): loop_fracs /
loop_feats (doom.py), _is_test_path, _edited_src_files (incl. the docstring
bug cases: tmp-heredoc body excision, `python x.py > log` run-vs-edit,
/testbed component rule, tee/patch/apply_patch markers), _struct_edited_src
(extra.actions + native tool_use blocks), _budget_features (char/4 double
cumsum, supervisor/reset exclusion), and _action_of over internal-view
message shapes. Sets are emitted sorted; floats ride Python repr (shortest
round-trip) so the Rust side can compare f64s exactly.
"""
import json
import os
import sys

ACC = os.environ.get("ACC_ROOT") or os.path.abspath(
    os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", "adaptive-context-clean"))
sys.path.insert(0, ACC)

from adaptive_context.optimizer.doom import loop_fracs, loop_feats  # noqa: E402
from adaptive_context.meta.governor import (  # noqa: E402
    StepGovernor, _edited_src_files, _struct_edited_src, _is_test_path)

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    os.path.dirname(__file__), "fixtures", "governor.json")

GOV = StepGovernor(None, None)  # rules/embedder unused by the ported methods


# ── loop math ────────────────────────────────────────────────────────────────

LOOP_FRACS_CASES = [
    ("empty", []),
    ("single", ["ls -la"]),
    ("two_distinct", ["ls -la", "cat foo.py"]),
    ("identical_eight", ["pytest -x tests/test_a.py"] * 8),
    ("alternating_pair", ["git diff", "pytest -q"] * 5),
    ("near_dup_boundary", [
        "python manage.py test app1 app2 app3",
        "python manage.py test app1 app2 app4",     # jaccard 4/6 >= 0.6
        "python manage.py test app5 app6 app7",     # 3/9 < 0.6
        "grep -rn TODO src",
        "python manage.py test app1 app2 app3",
        "python manage.py test app1 app2 app3",
        "make build",
    ]),
    ("recover_then_loop", ["a b c", "d e f", "g h i", "a b c", "a b c",
                           "a b c", "j k l", "a b c"]),
    ("empty_strings", ["", "", "ls", "", "ls"]),
    ("window_rollover", [f"cmd{i}" for i in range(5)] + ["cmd0"] * 7),
]

LOOP_FEATS_CASES = [
    ("empty", [], 0),
    ("single_t0", [0.5], 0),
    ("mid", [0.0, 0.1, 0.4, 0.5, 0.66, 0.83], 4),
    ("t_clamped_high", [0.0, 0.2, 0.9], 99),
    ("t_negative", [0.3, 0.4], -7),
    ("threshold_edge", [0.34, 0.34, 0.33, 0.34, 0.5, 0.34, 0.2], 6),
    ("slope_negative", [0.8, 0.7, 0.5, 0.2], 3),
]

# ── path / edit detection ────────────────────────────────────────────────────

IS_TEST_PATH_CASES = [
    "/testbed/django/db/models/query.py",
    "/testbed/tests/queries/test_qs.py",
    "tests/test_x.py",
    "test_foo.py",
    "tests_foo.py",
    "conftest.py",
    "a/test/b.c",
    "testing/module.rs",
    "foo_test.py",
    "foo_tests.go",
    "attest.py",
    "contest.py",
    "protest_x.py",
    "reproduce_issue.py",
    "repro.py",
    "repro_bug.sh",
    "bug.py",
    "debug.py",
    "src\\tests\\a.py",
    "'quoted/path/test_y.py'",
    "/TESTBED/PKG/MOD.PY",
    "latest.py",
]

HEREDOC_SCRATCH = (
    "cat > /tmp/fix.py << 'EOF'\n"
    "import re\n"
    "open('/testbed/pkg/mod.py','w').write(src)\n"
    "EOF\n"
    "python /tmp/fix.py"
)
HEREDOC_THEN_REAL_EDIT = (
    HEREDOC_SCRATCH + "\nsed -i 's/a/b/' /testbed/pkg/other.py"
)
HEREDOC_UNCLOSED = (
    "cat >> /tmp/x.sh <<PATCH\nopen('/testbed/live.py','w')\nno close here"
)
HEREDOC_TWO = (
    "cat > /tmp/a.py << 'A'\nopen('/testbed/one.py','w')\nA\n"
    "cat > /tmp/b.py << 'B'\nopen('/testbed/two.py','w')\nB\n"
)

EDITED_SRC_CASES = [
    ("run_redirect_log", "python x.py > log"),
    ("run_redirect_txt", "python x.py > out.txt"),
    ("redirect_to_src", "echo 'fn main() {}' >> src/lib.rs"),
    ("redirect_dev_null", "pytest -q > /dev/null"),
    ("redirect_tmp_scratch", "git show HEAD:setup.py > /tmp/setup_old.py"),
    ("redirect_tmp_relative", "cat a.py > tmp/scratch.py"),
    ("sed_i_testbed", "sed -i 's/a/b/' /testbed/django/db/models/query.py"),
    ("sed_i_test_excluded", "sed -i 's/x/y/' tests/test_app.py"),
    ("tee_src", "echo 'code' | tee /src/app.js"),
    ("patch_marker_no_src_ext", "patch -p1 < changes.diff"),
    ("git_apply_patchfile", "git apply fix.patch"),
    ("git_apply_names_src", "git apply --check fix.patch && git apply fix.patch # touches src/main.rs"),
    ("apply_patch_block", "apply_patch << 'EOF'\n*** Update File: src/main.rs\nEOF"),
    ("open_w_testbed", "python -c \"open('/testbed/pkg/util.py','w').write(s)\""),
    ("dot_write_marker", "python -c \"f=open('/testbed/mod.py', 'w'); f.write(x)\""),
    ("str_replace_marker", "str_replace /w/e.rs old new"),
    ("heredoc_scratch", HEREDOC_SCRATCH),
    ("heredoc_then_real_edit", HEREDOC_THEN_REAL_EDIT),
    ("heredoc_unclosed", HEREDOC_UNCLOSED),
    ("heredoc_two_bodies", HEREDOC_TWO),
    ("quoted_path", "sed -i 's/a/b/' '/testbed/pkg/space name.py'"),
    ("no_marker_no_redirect", "grep -rn 'def main' src/"),
    ("empty", ""),
    ("uppercase_marker", "SED -I s/a/b/ /SRC/APP.PY"),
    ("multiline_mixed", "ls -la\ntee src/one.py < in\npython run.py > run.log"),
]

STRUCT_EDIT_CASES = [
    ("extra_actions_edit", {"role": "assistant", "content": "", "extra": {"actions": [
        {"tool": "Edit", "name": "Edit", "file_path": "/src/x.py",
         "old_string": "a", "new_string": "b", "command": "edit x.py"}]}}),
    ("extra_actions_bash_no_edit", {"role": "assistant", "content": "", "extra": {"actions": [
        {"tool": "Bash", "name": "Bash", "command": "ls", "file_path": "/src/x.py"}]}}),
    ("native_tool_use_write", {"role": "assistant", "content": [
        {"type": "text", "text": "writing"},
        {"type": "tool_use", "id": "t1", "name": "Write",
         "input": {"file_path": "src/y.go", "content": "pkg"}}]}),
    ("native_type_underscore", {"role": "assistant", "content": [
        {"_type": "ToolUseBlock", "name": "MultiEdit",
         "input": {"file_path": "lib/z.ts", "edits": []}}]}),
    ("path_key_variants", {"role": "assistant", "content": [
        {"type": "tool_use", "id": "a", "name": "str_replace_editor", "input": {"path": "/w/e.rs"}},
        {"type": "tool_use", "id": "b", "name": "apply_patch", "input": {"file": "m/n.c"}},
        {"type": "tool_use", "id": "c", "name": "notebookedit", "input": {"filename": "q.py"}}]}),
    ("tmp_and_tests_excluded", {"role": "assistant", "content": [
        {"type": "tool_use", "id": "a", "name": "Edit", "input": {"file_path": "/tmp/s.py"}},
        {"type": "tool_use", "id": "b", "name": "Edit", "input": {"file_path": "tmp/t.py"}},
        {"type": "tool_use", "id": "c", "name": "Edit", "input": {"file_path": "tests/test_a.py"}},
        {"type": "tool_use", "id": "d", "name": "Edit", "input": {"file_path": "README.md"}}]}),
    ("case_and_padding", {"role": "assistant", "content": [
        {"type": "tool_use", "id": "a", "name": " EDIT ", "input": {"file_path": "/src/UP.PY"}}]}),
    ("missing_name", {"role": "assistant", "content": [
        {"type": "tool_use", "id": "a", "input": {"file_path": "/src/x.py"}}]}),
    ("nonstr_path_skipped", {"role": "assistant", "content": [
        {"type": "tool_use", "id": "a", "name": "Edit", "input": {"file_path": ["x.py"]}}]}),
    ("falsy_path_falls_through", {"role": "assistant", "content": [
        {"type": "tool_use", "id": "a", "name": "Edit",
         "input": {"file_path": "", "path": "/real/deal.py"}}]}),
    ("quoted_path_stripped", {"role": "assistant", "content": [
        {"type": "tool_use", "id": "a", "name": "Edit", "input": {"file_path": "'/src/q.py'"}}]}),
    ("plain_text_message", {"role": "assistant", "content": "no tools here"}),
]

# ── budget features (internal-view shapes) ──────────────────────────────────


def asst(text, args=None):
    m = {"role": "assistant", "content": text}
    if args is not None:
        m["tool_calls"] = [{"id": "t", "type": "function",
                            "function": {"name": "Bash", "arguments": args}}]
    return m


def obs(text, role="user"):
    return {"role": role, "content": text}


BUDGET_CASES = [
    ("too_short", [asst("a" * 50), obs("b" * 50)] * 3),
    ("six_plain_steps", [x for i in range(6) for x in (asst("x" * 100), obs("y" * 100))]),
    ("supervisor_excluded", [x for i in range(6) for x in (asst("x" * 100), obs("y" * 100))]
     + [obs("[SUPERVISOR] MANDATORY RULE ..."), obs("[CONTEXT RESET marker here")]),
    ("tool_call_args_counted", [x for i in range(6) for x in (
        asst("", '{"command": "' + "c" * 88 + '"}'), obs("o" * 40, role="tool"))]),
    ("obs_before_first_step_dropped", [obs("preamble " * 20)]
     + [x for i in range(6) for x in (asst("x" * 30), obs("y" * 30))]),
    ("unicode_chars_counted", [x for i in range(6) for x in (asst("α" * 80), obs("β" * 80))]),
    ("trailing_assistant_no_obs", [x for i in range(6) for x in (asst("x" * 60), obs("y" * 60))]
     + [asst("z" * 60)]),
]

# ── _action_of (internal-view message shapes) ────────────────────────────────

ACTION_OF_CASES = [
    ("extra_actions_command", {"role": "assistant", "content": "prose",
                               "extra": {"actions": [{"tool": "Bash", "name": "Bash",
                                                      "command": "ls -la /tmp"}]}}),
    ("extra_actions_query_fallback", {"role": "assistant", "content": "",
                                      "extra": {"actions": [{"tool": "Grep", "name": "Grep",
                                                             "command": "", "query": "needle"}]}}),
    ("extra_actions_multi_join", {"role": "assistant", "content": "",
                                  "extra": {"actions": [{"command": "cat a.py"},
                                                        {"command": "", "query": "def x"},
                                                        {"command": ""}]}}),
    ("tool_calls_args_command", {"role": "assistant", "content": "",
                                 "tool_calls": [{"id": "1", "type": "function",
                                                 "function": {"name": "Bash",
                                                              "arguments": "{\"command\": \"make test\"}"}}]}),
    ("tool_calls_args_nonjson", {"role": "assistant", "content": "",
                                 "tool_calls": [{"id": "1", "type": "function",
                                                 "function": {"name": "X",
                                                              "arguments": "not json at all"}}]}),
    ("tool_calls_args_json_list", {"role": "assistant", "content": "",
                                   "tool_calls": [{"id": "1", "type": "function",
                                                   "function": {"name": "X",
                                                                "arguments": "[1, 2]"}}]}),
    ("bash_code_block", {"role": "assistant",
                         "content": "I'll run this:\n```bash\npytest -x\n```\ndone"}),
    ("sh_code_block", {"role": "assistant", "content": "```sh\necho hi\n```"}),
    ("anon_code_block", {"role": "assistant", "content": "```\nls\n```"}),
    ("plain_text", {"role": "assistant", "content": "just thinking out loud"}),
    ("empty_content", {"role": "assistant", "content": ""}),
]


def main():
    fx = {
        "loop_fracs": [
            {"name": n, "cmds": c, "expected": loop_fracs(c)} for n, c in LOOP_FRACS_CASES],
        "loop_feats": [
            {"name": n, "lf": lf, "t": t, "expected": loop_feats(lf, t)}
            for n, lf, t in LOOP_FEATS_CASES],
        "is_test_path": [
            {"name": p, "path": p, "expected": _is_test_path(p)} for p in IS_TEST_PATH_CASES],
        "edited_src_files": [
            {"name": n, "action": a, "expected": sorted(_edited_src_files(a))}
            for n, a in EDITED_SRC_CASES],
        "struct_edited_src": [
            {"name": n, "message": m, "expected": sorted(_struct_edited_src(m))}
            for n, m in STRUCT_EDIT_CASES],
        "budget_features": [
            {"name": n, "messages": m,
             "expected": list(GOV._budget_features(m))} for n, m in BUDGET_CASES],
        "action_of": [
            {"name": n, "message": m, "expected": StepGovernor._action_of(m)}
            for n, m in ACTION_OF_CASES],
    }
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(fx, f, ensure_ascii=False, indent=1)
    print(f"wrote {OUT}: " + ", ".join(f"{k}={len(v)}" for k, v in fx.items()))


if __name__ == "__main__":
    main()
