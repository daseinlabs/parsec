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
