//! Track C — Stop-hook SUBMIT adjudicator tests (DESIGN_CONTRACT item 4).
//!
//! Two layers:
//! 1. Unit tests of the pure ports (transcript parse, views, has-edit
//!    ladder, deterministic verdict) via the library crate — the same
//!    fixture cases as the reference docstrings.
//! 2. End-to-end hook runs through the REAL `parsec hook Stop` binary
//!    (CARGO_BIN_EXE_parsec). Every run is its own subprocess with its own
//!    HOME tempdir and env vars (the hook writes ~/.parsec/adjudicator.jsonl
//!    and ~/.parsec/sessions/…), so parallel test execution never shares or
//!    mutates this process's environment.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use parsec_proxy::adjudicator::{
    adjudicate, build_views, edit_on_disk, find_diff, parse_transcript, struct_src_edit, MAX_MSGS,
    OBS_CAP,
};

// ── fixtures ────────────────────────────────────────────────────────────────

fn t_user(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": text}})
}

fn t_asst_bash(cmd: &str) -> Value {
    json!({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "text", "text": "running a command"},
        {"type": "tool_use", "id": "t", "name": "Bash", "input": {"command": cmd}}]}})
}

fn t_asst_edit(path: &str) -> Value {
    json!({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "text", "text": "applying the fix"},
        {"type": "tool_use", "id": "t", "name": "Edit",
         "input": {"file_path": path, "old_string": "a", "new_string": "b"}}]}})
}

fn t_tool(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t", "content": text}]}})
}

fn transcript(lines: &[Value]) -> String {
    lines
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// A run that made a real structured source edit, then dithered: the same
/// test re-run 7 times with nothing new landing. SUBMIT (bank the fix).
fn dithering_with_edit() -> Vec<Value> {
    let mut v = vec![
        t_user("fix the off-by-one in app.py"),
        t_asst_edit("/repo/src/app.py"),
        t_tool("edited /repo/src/app.py"),
    ];
    for _ in 0..7 {
        v.push(t_asst_bash("python -m pytest tests/test_app.py -x"));
        v.push(t_tool("4 passed in 0.21s"));
    }
    v
}

/// A run looping with NO work product anywhere. CONTINUE.
fn no_edit_loop() -> Vec<Value> {
    let mut v = vec![t_user("investigate the flaky test")];
    for _ in 0..8 {
        v.push(t_asst_bash("grep -rn flaky src/"));
        v.push(t_tool("no matches"));
    }
    v
}

/// A struct edit followed by an EMPTY latest `git diff` — the reverted-edit
/// short-circuit (adjudicator_submit.py:101-105): clean tree overrides the
/// stale struct signal.
fn reverted_edit() -> Vec<Value> {
    vec![
        t_user("fix it"),
        t_asst_edit("/repo/src/app.py"),
        t_tool("edited"),
        t_asst_bash("git diff"),
        t_tool(""),
    ]
}

// ── layer 1: pure port unit tests ───────────────────────────────────────────

#[test]
fn transcript_parse_shapes() {
    let lines = vec![
        json!({"type": "summary", "summary": "x"}), // non user/assistant: skipped
        t_user("do the task"),
        json!({"type": "user", "message": {"role": "user", "content": "   "}}), // blank: dropped
        t_asst_bash("ls -la"),
        json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "part1"},
            {"type": "thinking", "thinking": "hmm"},
            {"type": "tool_use", "id": "1", "name": "Grep", "input": {"pattern": "foo"}},
            {"type": "tool_use", "id": "2", "name": "Edit", "input": {"file_path": "/s/a.py"}},
            {"type": "tool_use", "id": "3", "name": "WebSearch", "input": {"query": "x"}}]}}),
        t_tool("result text"),
        json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "1",
             "content": [{"type": "text", "text": "aa"}, {"type": "text", "text": "bb"}]}]}}),
    ];
    let text = transcript(&lines);
    let msgs = parse_transcript(&text, MAX_MSGS, OBS_CAP);
    // A non-JSON line is skipped, not fatal.
    assert_eq!(
        msgs,
        parse_transcript(&format!("not json\n{text}"), 80, 2000)
    );
    assert_eq!(msgs.len(), 5);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "do the task");
    // Bash tool_use → bash action with the command line.
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["extra"]["actions"][0]["tool"], "bash");
    assert_eq!(msgs[1]["extra"]["actions"][0]["command"], "ls -la");
    // text + thinking joined with \n; per-tool command lines.
    assert_eq!(msgs[2]["content"], "part1\nhmm");
    assert_eq!(msgs[2]["extra"]["actions"][0]["command"], "Grep foo");
    assert_eq!(msgs[2]["extra"]["actions"][1]["command"], "Edit /s/a.py");
    // Deviation under test: the tool_use input rides along so the
    // struct-edit rung can read file_path from a transcript.
    assert_eq!(msgs[2]["extra"]["actions"][1]["file_path"], "/s/a.py");
    assert_eq!(msgs[2]["extra"]["actions"][2]["command"], "WebSearch"); // unknown tool → name
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["content"], "result text");
    // list-shaped tool_result content joined with " ".
    assert_eq!(msgs[4]["content"], "aa bb");
}

#[test]
fn transcript_caps() {
    // obs_cap: a 3000-char tool result keeps its first 2000 chars.
    let lines = vec![t_tool(&"x".repeat(3000))];
    let msgs = parse_transcript(&transcript(&lines), MAX_MSGS, OBS_CAP);
    assert_eq!(msgs[0]["content"].as_str().unwrap().chars().count(), 2000);
    // max_msgs: 100 entries → the LAST 80 survive.
    let many: Vec<Value> = (0..100).map(|i| t_user(&format!("m{i}"))).collect();
    let msgs = parse_transcript(&transcript(&many), MAX_MSGS, OBS_CAP);
    assert_eq!(msgs.len(), 80);
    assert_eq!(msgs[0]["content"], "m20");
    assert_eq!(msgs[79]["content"], "m99");
}

#[test]
fn views_caps_and_supervisor_skip() {
    let mut lines: Vec<Value> = Vec::new();
    for i in 0..30 {
        lines.push(
            json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "text", "text": format!("thought {i}\nsecond line {}", "y".repeat(300))},
            {"type": "tool_use", "id": "t", "name": "Bash",
             "input": {"command": format!("cmd-{i} {}", "z".repeat(300))}}]}}),
        );
        lines.push(t_tool(&format!("obs {i} {}", "w".repeat(500))));
    }
    lines.push(t_tool("[SUPERVISOR] injected directive — must be skipped"));
    let msgs = parse_transcript(&transcript(&lines), MAX_MSGS, OBS_CAP);
    let v = build_views(&msgs);
    assert_eq!(v.recent_actions.len(), 14);
    assert!(v.recent_actions.iter().all(|a| a.chars().count() <= 160));
    // The supervisor observation is skipped; the last real obs survives with
    // its LAST 400 chars.
    assert_eq!(v.recent_observations.len(), 4);
    assert!(v
        .recent_observations
        .iter()
        .all(|o| o.chars().count() <= 400 && !o.starts_with("[SUPERVISOR")));
    assert_eq!(v.history_tail.len(), 22);
    for t in &v.history_tail {
        let act = t["act"].as_str().unwrap();
        let think = t["think"].as_str().unwrap();
        assert!(act.chars().count() <= 140);
        assert!(think.chars().count() <= 240);
        assert!(!think.contains('\n'), "newlines flattened in think");
    }
    assert!(!v.has_edit); // no diff shown anywhere
}

#[test]
fn find_diff_reference_cases() {
    // Most recent diff wins; "diff --git" anchor; the preamble is dropped.
    let msgs = parse_transcript(
        &transcript(&[
            t_tool("diff --git a/old.py b/old.py\n-gone"),
            t_tool("preamble text diff --git a/x.py b/x.py\n+new"),
        ]),
        MAX_MSGS,
        OBS_CAP,
    );
    assert_eq!(find_diff(&msgs), "diff --git a/x.py b/x.py\n+new");
    // The 3000-char slice, on a direct message (through the transcript the
    // observation itself is already capped at obs_cap=2000, reference-alike).
    let big_diff = format!("diff --git a/x.py b/x.py\n{}", "+ new line\n".repeat(500));
    let direct = vec![json!({"role": "tool", "content": format!("preamble {big_diff}")})];
    let d = find_diff(&direct);
    assert!(d.starts_with("diff --git a/x.py"));
    assert_eq!(d.chars().count(), 3000);
    // Hunk-marker fallback: min of the "@@ " / "--- " indices.
    let msgs = parse_transcript(
        &transcript(&[t_tool("noise --- a/x.py\n+++ b/x.py\n@@ -1 +1 @@\n-a\n+b")]),
        MAX_MSGS,
        OBS_CAP,
    );
    assert!(find_diff(&msgs).starts_with("--- a/x.py"));
    // [SUPERVISOR observations never count as shown diffs.
    let msgs = parse_transcript(
        &transcript(&[t_tool("  [SUPERVISOR] diff --git a/fake b/fake")]),
        MAX_MSGS,
        OBS_CAP,
    );
    assert_eq!(find_diff(&msgs), "");
}

#[test]
fn edit_on_disk_ladder_cases() {
    // Never self-diffed → None.
    let msgs = parse_transcript(&transcript(&no_edit_loop()), MAX_MSGS, OBS_CAP);
    assert_eq!(edit_on_disk(&msgs), None);
    // Latest git diff EMPTY → Some(false), even though an edit was made.
    let msgs = parse_transcript(&transcript(&reverted_edit()), MAX_MSGS, OBS_CAP);
    assert_eq!(edit_on_disk(&msgs), Some(false));
    assert!(struct_src_edit(&msgs));
    // Latest git diff NON-empty → Some(true); the LATEST diff action rules.
    let mut lines = reverted_edit();
    lines.push(t_asst_bash("git diff -- src/"));
    lines.push(t_tool(
        "diff --git a/src/app.py b/src/app.py\n@@ -1 +1 @@\n-a\n+b",
    ));
    let msgs = parse_transcript(&transcript(&lines), MAX_MSGS, OBS_CAP);
    assert_eq!(edit_on_disk(&msgs), Some(true));
    // Only the next <=3 messages after the diff action are its result.
    let lines = vec![
        t_asst_bash("git diff"),
        t_tool("no changes"),
        t_tool("still nothing"),
        t_tool("nothing here"),
        t_tool("diff --git a/too.py b/too.py (too far away)"),
    ];
    let msgs = parse_transcript(&transcript(&lines), MAX_MSGS, OBS_CAP);
    assert_eq!(edit_on_disk(&msgs), Some(false));
}

#[test]
fn verdict_matrix() {
    // Struct source edit + mechanical stall → SUBMIT via the struct rung.
    let msgs = parse_transcript(&transcript(&dithering_with_edit()), MAX_MSGS, OBS_CAP);
    let adj = adjudicate(&msgs);
    assert_eq!(adj.verdict, "SUBMIT");
    assert!(adj.has_edit && adj.mech_stalled && !adj.short_circuit);
    assert_eq!(adj.has_edit_rung, "struct_edit");
    assert!(*adj.lf_tail.last().unwrap() >= 0.5);

    // Loop with no edit anywhere → CONTINUE, rung none.
    let msgs = parse_transcript(&transcript(&no_edit_loop()), MAX_MSGS, OBS_CAP);
    let adj = adjudicate(&msgs);
    assert_eq!(adj.verdict, "CONTINUE");
    assert!(!adj.has_edit && adj.mech_stalled);
    assert_eq!(adj.has_edit_rung, "none");
    assert_eq!(adj.reason, "no submittable edit on disk");

    // Reverted edit: empty latest git diff OVERRIDES the struct edit —
    // the short-circuit with the reference reason verbatim.
    let msgs = parse_transcript(&transcript(&reverted_edit()), MAX_MSGS, OBS_CAP);
    let adj = adjudicate(&msgs);
    assert_eq!(adj.verdict, "CONTINUE");
    assert_eq!(adj.has_edit_rung, "disk_false");
    assert!(adj.short_circuit);
    assert_eq!(
        adj.reason,
        "latest git diff is empty (no edit on disk to submit)"
    );

    // Edit in place but still progressing (distinct commands) → CONTINUE.
    let mut lines = vec![t_user("fix"), t_asst_edit("/repo/src/app.py")];
    for i in 0..6 {
        lines.push(t_asst_bash(&format!("distinct-command-{i} --arg{i}")));
        lines.push(t_tool(&format!("fresh result {i}")));
    }
    let msgs = parse_transcript(&transcript(&lines), MAX_MSGS, OBS_CAP);
    let adj = adjudicate(&msgs);
    assert_eq!(adj.verdict, "CONTINUE");
    assert!(adj.has_edit && !adj.mech_stalled);

    // A non-empty PRINTED diff with no git-diff action → shown_diff rung;
    // with a stall it SUBMITs.
    let mut lines = vec![
        t_user("fix"),
        t_tool("diff --git a/src/m.py b/src/m.py\n@@ -1 +1 @@\n-a\n+b"),
    ];
    for _ in 0..7 {
        lines.push(t_asst_bash("python check.py"));
        lines.push(t_tool("same output"));
    }
    let msgs = parse_transcript(&transcript(&lines), MAX_MSGS, OBS_CAP);
    let adj = adjudicate(&msgs);
    assert_eq!(adj.verdict, "SUBMIT");
    assert_eq!(adj.has_edit_rung, "shown_diff");

    // Empty transcript: fail-open CONTINUE, empty lf tail.
    let adj = adjudicate(&[]);
    assert_eq!(adj.verdict, "CONTINUE");
    assert!(!adj.mech_stalled && adj.lf_tail.is_empty() && adj.n_msgs == 0);
}

// ── layer 2: end-to-end through the real hook binary ───────────────────────

struct TempHome(PathBuf);

impl TempHome {
    fn new(tag: &str) -> TempHome {
        let dir =
            std::env::temp_dir().join(format!("parsec-hookstop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempHome(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn log_file(&self) -> PathBuf {
        self.0.join(".parsec").join("adjudicator.jsonl")
    }

    fn rows(&self) -> Vec<Value> {
        match std::fs::read_to_string(self.log_file()) {
            Ok(s) => s
                .lines()
                .map(|l| serde_json::from_str(l).expect("row parse"))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn session(&self, sid: &str) -> Option<Value> {
        std::fs::read_to_string(self.0.join(format!(".parsec/sessions/{sid}.json")))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    fn write_transcript(&self, lines: &[Value]) -> PathBuf {
        let p = self.0.join("transcript.jsonl");
        std::fs::write(&p, transcript(lines)).unwrap();
        p
    }

    fn stop_payload(&self, session_id: &str, transcript_path: &Path, active: bool) -> Value {
        json!({
            "session_id": session_id,
            "transcript_path": transcript_path.to_string_lossy(),
            "cwd": self.0.to_string_lossy(),
            "hook_event_name": "Stop",
            "stop_hook_active": active,
        })
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `parsec hook Stop` in a child process with an isolated HOME + env —
/// env vars never touch the test process, so parallel tests are safe.
fn run_stop_hook(home: &Path, payload: &Value, envs: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_parsec"));
    cmd.args(["hook", "Stop"])
        .env("HOME", home)
        .env_remove("PARSEC_ADJUDICATOR")
        .env_remove("PARSEC_ADJ_MAX_BLOCKS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn parsec hook Stop");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("hook run");
    assert!(
        out.status.success(),
        "the Stop hook must exit 0 (fail-open), got {:?}",
        out.status
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn advise_mode_submit_row_no_stdout() {
    let home = TempHome::new("advise-submit");
    let tp = home.write_transcript(&dithering_with_edit());
    let payload = home.stop_payload("sess-adv-submit", &tp, false);
    let stdout = run_stop_hook(home.path(), &payload, &[]); // default = advise
    assert!(
        stdout.is_empty(),
        "advise mode must print nothing: {stdout}"
    );
    let rows = home.rows();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r["schema"], "adjudicator-stop/v0");
    assert_eq!(r["verdict"], "SUBMIT");
    assert_eq!(r["has_edit"], true);
    assert_eq!(r["has_edit_rung"], "struct_edit");
    assert_eq!(r["mech_stalled"], true);
    assert_eq!(r["mode"], "advise");
    assert_eq!(r["blocked"], false);
    assert_eq!(r["session_id"], "sess-adv-submit");
    assert!(r["lf_tail"].as_array().unwrap().len() >= 3);
    assert!(r["n_cmds"].as_u64().unwrap() >= 6);
    // The tempdir is not a git repo: the (telemetry-only) probe abstains.
    assert!(r["disk_probe"].is_null());
    // Views ride along, capped by construction.
    assert!(!r["views"]["recent_actions"].as_array().unwrap().is_empty());
}

#[test]
fn advise_mode_continue_never_blocks() {
    let home = TempHome::new("advise-continue");
    let tp = home.write_transcript(&no_edit_loop());
    let payload = home.stop_payload("sess-adv-cont", &tp, false);
    let stdout = run_stop_hook(home.path(), &payload, &[]);
    assert!(stdout.is_empty());
    let rows = home.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "CONTINUE");
    assert_eq!(rows[0]["has_edit"], false);
    assert_eq!(rows[0]["mech_stalled"], true);
    assert_eq!(rows[0]["blocked"], false);
    // advise never touches the session file.
    assert!(home.session("sess-adv-cont").is_none());
}

#[test]
fn block_mode_fires_and_respects_budget() {
    let home = TempHome::new("block-budget");
    let tp = home.write_transcript(&no_edit_loop());
    let payload = home.stop_payload("sess-block", &tp, false);
    let envs = [("PARSEC_ADJUDICATOR", "block")];
    // 1st + 2nd stop: blocked (default budget PARSEC_ADJ_MAX_BLOCKS=2).
    for expect_n in 1..=2u64 {
        let stdout = run_stop_hook(home.path(), &payload, &envs);
        let dec: Value = serde_json::from_str(stdout.trim()).expect("block decision JSON");
        assert_eq!(dec["decision"], "block");
        assert!(dec["reason"]
            .as_str()
            .unwrap()
            .contains("no submittable edit"));
        assert_eq!(
            home.session("sess-block").unwrap()["adj_blocks"],
            expect_n,
            "budget counter persisted"
        );
    }
    // 3rd stop: budget exhausted → allow silently; the row is still written.
    let stdout = run_stop_hook(home.path(), &payload, &envs);
    assert!(stdout.is_empty(), "budget exhausted must not block");
    let rows = home.rows();
    assert_eq!(rows.len(), 3);
    assert!(rows
        .iter()
        .all(|r| r["verdict"] == "CONTINUE" && r["mode"] == "block"));
    assert_eq!(rows[0]["blocked"], true);
    assert_eq!(rows[1]["blocked"], true);
    assert_eq!(rows[2]["blocked"], false);
}

#[test]
fn block_budget_env_dial() {
    let home = TempHome::new("block-dial");
    let tp = home.write_transcript(&no_edit_loop());
    let payload = home.stop_payload("sess-dial", &tp, false);
    let envs = [
        ("PARSEC_ADJUDICATOR", "block"),
        ("PARSEC_ADJ_MAX_BLOCKS", "0"),
    ];
    let stdout = run_stop_hook(home.path(), &payload, &envs);
    assert!(stdout.is_empty(), "budget 0 must never block");
    assert_eq!(home.rows().len(), 1);
    assert_eq!(home.rows()[0]["blocked"], false);
}

#[test]
fn block_mode_never_blocks_a_bankable_stop() {
    // SUBMIT verdict (edit + stall): the stop is the bank — allow it.
    let home = TempHome::new("block-submit");
    let tp = home.write_transcript(&dithering_with_edit());
    let payload = home.stop_payload("sess-bank", &tp, false);
    let stdout = run_stop_hook(home.path(), &payload, &[("PARSEC_ADJUDICATOR", "block")]);
    assert!(stdout.is_empty());
    let rows = home.rows();
    assert_eq!(rows[0]["verdict"], "SUBMIT");
    assert_eq!(rows[0]["blocked"], false);
}

#[test]
fn reverted_edit_short_circuit_row() {
    // Empty latest git-diff output in the transcript overrides the struct
    // edit (adjudicator_submit.py:101-105).
    let home = TempHome::new("reverted");
    let tp = home.write_transcript(&reverted_edit());
    let payload = home.stop_payload("sess-reverted", &tp, false);
    let stdout = run_stop_hook(home.path(), &payload, &[]);
    assert!(stdout.is_empty());
    let r = &home.rows()[0];
    assert_eq!(r["verdict"], "CONTINUE");
    assert_eq!(r["has_edit"], false);
    assert_eq!(r["has_edit_rung"], "disk_false");
    assert_eq!(r["short_circuit"], true);
    assert_eq!(
        r["reason"],
        "latest git diff is empty (no edit on disk to submit)"
    );
}

#[test]
fn stop_hook_active_exits_silently_no_row() {
    let home = TempHome::new("active");
    let tp = home.write_transcript(&no_edit_loop());
    let payload = home.stop_payload("sess-active", &tp, true);
    // Even in block mode: a forced continuation is never pinned again.
    let stdout = run_stop_hook(home.path(), &payload, &[("PARSEC_ADJUDICATOR", "block")]);
    assert!(stdout.is_empty());
    assert!(!home.log_file().exists(), "no row on stop_hook_active");
    assert!(home.session("sess-active").is_none());
}

#[test]
fn off_mode_skips_entirely() {
    let home = TempHome::new("off");
    let tp = home.write_transcript(&dithering_with_edit());
    let payload = home.stop_payload("sess-off", &tp, false);
    let stdout = run_stop_hook(home.path(), &payload, &[("PARSEC_ADJUDICATOR", "off")]);
    assert!(stdout.is_empty());
    assert!(!home.log_file().exists(), "off mode must write no row");
}

#[test]
fn unreadable_transcript_fails_open() {
    let home = TempHome::new("fail-open");
    let payload = json!({
        "session_id": "sess-failopen",
        "transcript_path": home.path().join("does-not-exist.jsonl").to_string_lossy(),
        "cwd": home.path().to_string_lossy(),
        "hook_event_name": "Stop",
        "stop_hook_active": false,
    });
    let stdout = run_stop_hook(home.path(), &payload, &[("PARSEC_ADJUDICATOR", "block")]);
    assert!(stdout.is_empty(), "unreadable transcript → allow silently");
    let rows = home.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "CONTINUE");
    assert_eq!(rows[0]["n_msgs"], 0);
    assert_eq!(rows[0]["blocked"], false); // no stall signal → no block
}
