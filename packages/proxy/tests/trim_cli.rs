//! End-to-end `parsec trim` runs through the real binary
//! (CARGO_BIN_EXE_parsec), each in its own subprocess with an isolated HOME
//! tempdir (the command writes ~/.parsec/trim/), so parallel tests never
//! share state.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

struct TempHome(PathBuf);

impl TempHome {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("parsec-trim-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        // canonicalize: macOS temp_dir lives behind the /var -> /private/var
        // symlink, and the binary keys the pending file on the RESOLVED cwd.
        TempHome(dir.canonicalize().unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn proj(&self) -> PathBuf {
        self.0.join("proj")
    }

    fn write_transcript(&self, name: &str, lines: &[Value]) -> PathBuf {
        let p = self.0.join(name);
        let text = lines
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&p, text).unwrap();
        p
    }

    fn pending(&self) -> PathBuf {
        let key: String = self
            .proj()
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        self.0
            .join(".parsec")
            .join("trim")
            .join(format!("{key}.json"))
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn asst(cmd: &str) -> Value {
    json!({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": "t", "name": "Bash", "input": {"command": cmd}}]}})
}

fn tool(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t", "content": text}]}})
}

fn lines(prefix: &str, lo: usize, hi: usize) -> String {
    (lo..=hi)
        .map(|i| format!("{prefix}_{i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A session long/dense enough to trim, with a keepable ctrl.py chunk
/// (precisely re-read at the end): the parity suite's ctrl pattern.
fn keepable_session() -> Vec<Value> {
    let mut v = vec![
        asst("cat plan.md"),
        tool(&lines("plan_item_number", 1, 60)),
        asst("sed -n '1,10p' ctrl.py"),
        tool(&lines("ctrl_source_line", 1, 10)),
    ];
    for i in 0..4 {
        v.push(asst(&format!("echo filler_{i}")));
        v.push(tool("ok"));
    }
    v.push(asst("sed -n '3,4p' ctrl.py"));
    v.push(tool("ctrl_source_line_3\nctrl_source_line_4"));
    v.push(asst("echo done"));
    v.push(tool("done"));
    v
}

fn run_trim(home: &TempHome, args: &[&str], stdin: Option<&str>) -> (String, i32) {
    run_trim_env(home, args, stdin, &[])
}

fn run_trim_env(
    home: &TempHome,
    args: &[&str],
    stdin: Option<&str>,
    envs: &[(&str, &str)],
) -> (String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_parsec"));
    cmd.arg("trim")
        .args(args)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("PARSEC_TRIM_TTL_SECS")
        .env_remove("PARSEC_TRIM_MAX_EST_TOKENS")
        .env_remove("PARSEC_TRIM_LEVEL")
        .envs(envs.iter().copied())
        .current_dir(home.proj())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn parsec trim");
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().expect("trim run");
    (
        String::from_utf8(out.stdout).unwrap(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn compute_then_finalize_roundtrip() {
    let home = TempHome::new("roundtrip");
    let tp = home.write_transcript("sess.jsonl", &keepable_session());
    let (stdout, code) = run_trim(
        &home,
        &["--transcript", &tp.to_string_lossy(), "--json"],
        None,
    );
    assert_eq!(code, 0, "compute failed: {stdout}");
    let stats: Value = serde_json::from_str(stdout.trim()).expect("json stats");
    assert!(stats["kept_chunks"].as_u64().unwrap() >= 1, "{stats}");
    assert!(stats["total_chunks"].as_u64().unwrap() > stats["kept_chunks"].as_u64().unwrap());
    assert_eq!(stats["status"], "det");
    // staged payload on disk, complete before any /clear
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(home.pending()).unwrap()).unwrap();
    assert_eq!(payload["version"], 1);
    assert_eq!(payload["status"], "det");
    assert_eq!(
        payload["project_dir"].as_str().unwrap(),
        home.proj().to_string_lossy()
    );
    assert!(payload["body"]
        .as_str()
        .unwrap()
        .contains("ctrl_source_line_3"));
    assert!(payload["directives"].is_null());

    // finalize with directives on stdin
    let (stdout, code) = run_trim(
        &home,
        &["--finalize", "--json"],
        Some("- keep fixing ctrl.py\n- never guess line numbers\n"),
    );
    assert_eq!(code, 0, "finalize failed: {stdout}");
    let stats: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(stats["status"], "ready");
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(home.pending()).unwrap()).unwrap();
    assert_eq!(payload["status"], "ready");
    assert!(payload["directives"]
        .as_str()
        .unwrap()
        .contains("never guess line numbers"));
}

#[test]
fn sidechain_lines_do_not_change_the_keep_set() {
    let home = TempHome::new("sidechain");
    let plain = home.write_transcript("plain.jsonl", &keepable_session());
    let (out_plain, code) = run_trim(
        &home,
        &["--transcript", &plain.to_string_lossy(), "--json"],
        None,
    );
    assert_eq!(code, 0);

    let mut with_side = keepable_session();
    let mut side = asst("cat sidechain_noise.py");
    side["isSidechain"] = json!(true);
    let mut side_obs = tool(&lines("sidechain_noise_row", 1, 40));
    side_obs["isSidechain"] = json!(true);
    with_side.insert(2, side);
    with_side.insert(3, side_obs);
    let tp = home.write_transcript("side.jsonl", &with_side);
    let (out_side, code) = run_trim(
        &home,
        &["--transcript", &tp.to_string_lossy(), "--json"],
        None,
    );
    assert_eq!(code, 0);

    let a: Value = serde_json::from_str(out_plain.trim()).unwrap();
    let b: Value = serde_json::from_str(out_side.trim()).unwrap();
    for k in ["kept_chunks", "total_chunks", "steps", "tokens_body_est"] {
        assert_eq!(a[k], b[k], "{k} changed when sidechain lines were added");
    }
}

#[test]
fn level_flag_env_and_precedence() {
    let home = TempHome::new("level");
    let tp = home.write_transcript("sess.jsonl", &keepable_session());
    let t = tp.to_string_lossy();

    // default is level 3 (already covered stats-wise by the roundtrip test)
    let (stdout, code) = run_trim(&home, &["--transcript", &t, "--json"], None);
    assert_eq!(code, 0, "{stdout}");
    let stats: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(stats["level"], 3);
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(home.pending()).unwrap()).unwrap();
    assert_eq!(payload["level"], 3);

    // explicit flag is stamped into stats and payload
    let (stdout, code) = run_trim(&home, &["--transcript", &t, "--json", "--level", "5"], None);
    assert_eq!(code, 0, "{stdout}");
    let stats: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(stats["level"], 5);
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(home.pending()).unwrap()).unwrap();
    assert_eq!(payload["level"], 5);

    // env fallback applies without the flag...
    let (stdout, code) = run_trim_env(
        &home,
        &["--transcript", &t, "--json"],
        None,
        &[("PARSEC_TRIM_LEVEL", "2")],
    );
    assert_eq!(code, 0, "{stdout}");
    let stats: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(stats["level"], 2);

    // ...and the flag beats the env
    let (stdout, code) = run_trim_env(
        &home,
        &["--transcript", &t, "--json", "--level", "4"],
        None,
        &[("PARSEC_TRIM_LEVEL", "1")],
    );
    assert_eq!(code, 0, "{stdout}");
    let stats: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(stats["level"], 4);

    // out-of-range values are rejected loudly, nothing staged over the last
    let (_, code) = run_trim(&home, &["--transcript", &t, "--level", "6"], None);
    assert_ne!(code, 0, "clap must reject --level 6");
    let (_, code) = run_trim_env(
        &home,
        &["--transcript", &t],
        None,
        &[("PARSEC_TRIM_LEVEL", "banana")],
    );
    assert_ne!(code, 0, "a non-numeric env level must error, not default");
}

#[test]
fn short_session_exits_two() {
    let home = TempHome::new("short");
    let mut msgs = Vec::new();
    for i in 0..5 {
        msgs.push(asst(&format!("cat f{i}.py")));
        msgs.push(tool("body\nlines"));
    }
    let tp = home.write_transcript("short.jsonl", &msgs);
    let (stdout, code) = run_trim(&home, &["--transcript", &tp.to_string_lossy()], None);
    assert_eq!(code, 2, "stdout: {stdout}");
    assert!(stdout.contains("too short"), "{stdout}");
    assert!(!home.pending().exists());
}

#[test]
fn missing_transcript_exits_two() {
    let home = TempHome::new("notranscript");
    let (stdout, code) = run_trim(&home, &[], None);
    assert_eq!(code, 2, "stdout: {stdout}");
    assert!(stdout.contains("no session transcript"), "{stdout}");
}

// ── Codex rollouts ─────────────────────────────────────────────────────────

impl TempHome {
    /// Write a Codex rollout where the tree really lives:
    /// `$HOME/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, with a session_meta
    /// line naming this project's cwd.
    fn write_rollout(&self, name: &str, items: &[Value]) -> PathBuf {
        let dir = self.0.join(".codex/sessions/2026/08/24");
        std::fs::create_dir_all(&dir).unwrap();
        let mut lines = vec![json!({
            "timestamp": "2026-08-24T00:00:00Z",
            "type": "session_meta",
            "payload": {"session_id": "s1", "cwd": self.proj().to_string_lossy(),
                        "cli_version": "0.147.0"}
        })
        .to_string()];
        lines.extend(items.iter().map(|it| {
            json!({"timestamp": "2026-08-24T00:00:01Z", "type": "response_item", "payload": it})
                .to_string()
        }));
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }
}

fn cx_call(cmd: &str) -> Value {
    json!({"type": "custom_tool_call", "call_id": "c", "name": "exec",
           "input": format!("const r = await tools.exec_command({{cmd:\"{cmd}\",\"workdir\":\"/r\"}});")})
}

fn cx_out(text: &str) -> Value {
    json!({"type": "custom_tool_call_output", "call_id": "c",
           "output": [{"type": "input_text", "text": text}]})
}

/// The same keepable shape as the Claude Code fixture, in Codex's schema.
fn codex_session() -> Vec<Value> {
    let mut v = vec![
        json!({"type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "<skills_instructions>…"}]}),
        json!({"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "fix ctrl.py"}]}),
        cx_call("cat plan.md"),
        cx_out(&lines("plan_item_number", 1, 60)),
        cx_call("sed -n '1,10p' ctrl.py"),
        cx_out(&lines("ctrl_source_line", 1, 10)),
    ];
    for i in 0..4 {
        v.push(cx_call(&format!("echo filler_{i}")));
        v.push(cx_out("ok"));
    }
    v.push(cx_call("sed -n '3,4p' ctrl.py"));
    v.push(cx_out("ctrl_source_line_3\nctrl_source_line_4"));
    v.push(cx_call("echo done"));
    v.push(cx_out("done"));
    v
}

#[test]
fn codex_rollout_is_discovered_and_trimmed() {
    let home = TempHome::new("codex");
    home.write_rollout("rollout-2026-08-24T00-00-00-abc.jsonl", &codex_session());

    let (out, code) = run_trim(&home, &[], None);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("codex session"), "source not reported: {out}");

    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(home.pending()).unwrap()).unwrap();
    assert_eq!(payload["tool"], "codex");
    assert_eq!(payload["status"], "det");
    // The needed-set found the precisely re-read ctrl.py lines through the
    // command buried in Codex's JS snippet.
    assert!(
        payload["body"]
            .as_str()
            .unwrap()
            .contains("ctrl_source_line_3"),
        "keep-set missed the re-read: {}",
        payload["body"]
    );
}

/// The cross-tool accident: a project with BOTH histories trims the fresher
/// one, and a Codex-staged payload is never injected into Claude Code.
#[test]
fn the_fresher_transcript_wins_and_payloads_do_not_cross_tools() {
    let home = TempHome::new("both");
    // Claude Code transcript first, then a NEWER Codex rollout.
    let cc_dir = home.path().join(".claude/projects").join(
        home.proj()
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>(),
    );
    std::fs::create_dir_all(&cc_dir).unwrap();
    let cc = cc_dir.join("sess.jsonl");
    std::fs::write(
        &cc,
        keepable_session()
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    home.write_rollout("rollout-2026-08-24T00-00-00-abc.jsonl", &codex_session());

    let (out, code) = run_trim(&home, &[], None);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("codex session"), "fresher source lost: {out}");

    // Forcing the other way still works.
    let (out, code) = run_trim(&home, &["--tool", "claude"], None);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("claude session"), "{out}");

    // A bad --tool is loud, not silently auto.
    let (out, code) = run_trim(&home, &["--tool", "nope"], None);
    assert_ne!(code, 0, "{out}");
}

fn run_up(home: &TempHome, args: &[&str]) -> (String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_parsec"))
        .arg("up")
        .args(args)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        // Point the proxy at a port nothing will answer on and forbid the
        // spawn: `up` still runs its trim pickup first, which is what this
        // asserts. Its own proxy chatter goes to the same stdout.
        .env("PARSEC_PROXY_PORT", "1")
        .env("PARSEC_PROXY_AUTOSTART", "0")
        .env_remove("PARSEC_TRIM_TTL_SECS")
        .current_dir(home.proj())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn parsec up");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Codex has no additionalContext hook field; it surfaces SessionStart hook
/// STDOUT to the model as a developer message. `parsec up --session-start` is
/// therefore the Codex injection channel — and it must be the FLAG that
/// consumes, so a manual `parsec up` cannot burn a staged payload.
#[test]
fn session_start_injects_a_codex_trim_and_a_bare_up_does_not() {
    let home = TempHome::new("inject");
    home.write_rollout("rollout-2026-08-24T00-00-00-abc.jsonl", &codex_session());
    let (out, code) = run_trim(&home, &[], None);
    assert_eq!(code, 0, "{out}");
    let (out, code) = run_trim(&home, &["--finalize"], Some("Never touch ctrl.py again."));
    assert_eq!(code, 0, "{out}");
    assert!(home.pending().exists());

    // A bare `up` leaves it alone.
    let (out, _) = run_up(&home, &[]);
    assert!(
        !out.contains("ctrl_source_line_3"),
        "bare up burned it: {out}"
    );
    assert!(home.pending().exists(), "bare up consumed the payload");

    // `--session-start` prints it, directives and all, exactly once.
    let (out, _) = run_up(&home, &["--session-start"]);
    assert!(
        out.contains("ctrl_source_line_3"),
        "trim not injected: {out}"
    );
    assert!(
        out.contains("STANDING DIRECTIVES"),
        "directives missing: {out}"
    );
    assert!(out.contains("Never touch ctrl.py again."), "{out}");
    assert!(!home.pending().exists(), "payload must be one-shot");

    let (out2, _) = run_up(&home, &["--session-start"]);
    assert!(
        !out2.contains("ctrl_source_line_3"),
        "payload replayed: {out2}"
    );
}

/// A Codex-staged payload is invisible to the Claude Code hook, and vice
/// versa — one project shares one pending file, and injecting the other
/// harness's trim would describe a conversation this session never had.
#[test]
fn a_codex_payload_is_not_injected_into_claude_code() {
    let home = TempHome::new("nocross");
    home.write_rollout("rollout-2026-08-24T00-00-00-abc.jsonl", &codex_session());
    let (out, code) = run_trim(&home, &[], None);
    assert_eq!(code, 0, "{out}");

    let hook = Command::new(env!("CARGO_BIN_EXE_parsec"))
        .args(["hook", "SessionStart"])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("PARSEC_PROXY_AUTOSTART", "0")
        .current_dir(home.proj())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut c| {
            c.stdin
                .take()
                .unwrap()
                .write_all(
                    json!({"hook_event_name": "SessionStart", "source": "clear",
                           "session_id": "s", "cwd": home.proj().to_string_lossy()})
                    .to_string()
                    .as_bytes(),
                )
                .map(|_| c)
        })
        .and_then(|c| c.wait_with_output())
        .expect("hook run");
    let out = String::from_utf8_lossy(&hook.stdout).into_owned();
    assert!(
        !out.contains("ctrl_source_line_3"),
        "claude code hook picked up a codex trim: {out}"
    );
    assert!(home.pending().exists(), "the codex payload must survive");
}
