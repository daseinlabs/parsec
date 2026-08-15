//! SessionStart /trim injection tests through the real `parsec hook
//! SessionStart` binary — isolated HOME per test (the hook reads/deletes
//! ~/.parsec/trim/<key>.json), so parallel runs never share state.
//!
//! PARSEC_DISABLE env in the child guards against the autosetup/upgrade
//! branches touching the network or user settings — if those paths change,
//! these tests only care about the trim pickup semantics.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn project_key(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

struct TempHome(PathBuf);

impl TempHome {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("parsec-hook-trim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        TempHome(dir.canonicalize().unwrap())
    }

    fn proj(&self) -> String {
        self.0.join("proj").to_string_lossy().into_owned()
    }

    fn pending(&self) -> PathBuf {
        self.0
            .join(".parsec")
            .join("trim")
            .join(format!("{}.json", project_key(&self.proj())))
    }

    fn seed(&self, status: &str, created_at: u64, directives: Option<&str>) {
        let payload = json!({
            "version": 1,
            "project_dir": self.proj(),
            "session_id": "sess-1",
            "created_at": created_at,
            "status": status,
            "body": "kept_chunk_alpha\nkept_chunk_beta",
            "directives": directives,
            "tokens_total_est": 4000,
            "tokens_body_est": 8,
        });
        std::fs::create_dir_all(self.pending().parent().unwrap()).unwrap();
        std::fs::write(self.pending(), payload.to_string()).unwrap();
    }

    fn seed_raw(&self, raw: &str) {
        std::fs::create_dir_all(self.pending().parent().unwrap()).unwrap();
        std::fs::write(self.pending(), raw).unwrap();
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn run_session_start(home: &TempHome, source: &str) -> Value {
    let payload = json!({
        "session_id": "sess-1",
        "cwd": home.proj(),
        "hook_event_name": "SessionStart",
        "source": source,
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_parsec"));
    cmd.args(["hook", "SessionStart"])
        .env("HOME", &home.0)
        .env("USERPROFILE", &home.0)
        // keep the autosetup/upgrade/autostart branches inert in tests
        .env("PARSEC_AUTOSETUP", "0")
        .env_remove("PARSEC_TRIM_TTL_SECS")
        .current_dir(home.0.join("proj"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn parsec hook SessionStart");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("hook run");
    assert!(
        out.status.success(),
        "SessionStart must exit 0 (fail-open), got {:?}",
        out.status
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    if stdout.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(stdout.trim()).expect("hook stdout must be one JSON object")
    }
}

fn additional_context(v: &Value) -> Option<&str> {
    v.get("hookSpecificOutput")?
        .get("additionalContext")?
        .as_str()
}

#[test]
fn clear_injects_ready_payload_and_consumes_file() {
    let home = TempHome::new("clear-ready");
    home.seed("ready", now(), Some("- never guess line numbers"));
    let out = run_session_start(&home, "clear");
    let ctx = additional_context(&out).expect("additionalContext present");
    assert!(ctx.contains("kept_chunk_alpha\nkept_chunk_beta"), "{ctx}");
    assert!(ctx.contains("STANDING DIRECTIVES:\n- never guess line numbers"));
    assert_eq!(
        out["hookSpecificOutput"]["hookEventName"], "SessionStart",
        "{out}"
    );
    assert!(!home.pending().exists(), "payload must be one-shot");
    // second clear: nothing left to inject
    let out2 = run_session_start(&home, "clear");
    assert!(additional_context(&out2).is_none(), "{out2}");
}

#[test]
fn det_only_payload_injects_without_directives_section() {
    let home = TempHome::new("det-only");
    home.seed("det", now(), None);
    let out = run_session_start(&home, "startup");
    let ctx = additional_context(&out).expect("det-only is the fallback, still injectable");
    assert!(ctx.contains("kept_chunk_alpha"));
    assert!(!ctx.contains("STANDING DIRECTIVES"), "{ctx}");
    assert!(!home.pending().exists());
}

#[test]
fn resume_and_compact_do_not_inject() {
    let home = TempHome::new("resume");
    home.seed("ready", now(), Some("- x"));
    for source in ["resume", "compact"] {
        let out = run_session_start(&home, source);
        assert!(
            additional_context(&out).is_none(),
            "{source} must not inject: {out}"
        );
        assert!(
            home.pending().exists(),
            "{source} must leave the payload for the eventual /clear"
        );
    }
}

#[test]
fn expired_payload_is_pruned_not_injected() {
    let home = TempHome::new("expired");
    home.seed("ready", now() - 7200, Some("- x")); // > 1800s TTL
    let out = run_session_start(&home, "clear");
    assert!(additional_context(&out).is_none(), "{out}");
    assert!(!home.pending().exists(), "stale payload must be dropped");
}

#[test]
fn corrupt_payload_is_silent_and_exit_zero() {
    let home = TempHome::new("corrupt");
    home.seed_raw("{not json");
    let out = run_session_start(&home, "clear");
    assert!(additional_context(&out).is_none(), "{out}");
    assert!(!home.pending().exists(), "corrupt payload must be dropped");
}

#[test]
fn other_projects_payload_is_untouched() {
    let home = TempHome::new("otherproj");
    // seed a payload whose project_dir does NOT match the hook cwd
    let payload = json!({
        "version": 1,
        "project_dir": "/somewhere/else",
        "created_at": now(),
        "status": "ready",
        "body": "b",
        "directives": "d",
        "tokens_total_est": 1,
        "tokens_body_est": 1,
    });
    std::fs::create_dir_all(home.pending().parent().unwrap()).unwrap();
    std::fs::write(home.pending(), payload.to_string()).unwrap();
    let out = run_session_start(&home, "clear");
    assert!(additional_context(&out).is_none(), "{out}");
    assert!(
        home.pending().exists(),
        "a mismatched payload is left alone (not consumed)"
    );
}
