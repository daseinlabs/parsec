//! CLI dispatch tests for the `setup`/`disable` tool argument — in
//! particular that `claude` is an accepted alias for the bare (Claude Code)
//! path. Runs the real binary with an isolated HOME so nothing of the
//! user's is read or written.

use std::path::{Path, PathBuf};
use std::process::Command;

struct TempHome(PathBuf);

impl TempHome {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("parsec-setup-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempHome(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(home: &TempHome, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_parsec"))
        .args(args)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("CLAUDE_CONFIG_DIR")
        .current_dir(home.path())
        .output()
        .expect("run parsec");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn disable_claude_is_the_bare_disable_path() {
    let home = TempHome::new("disable-claude");
    let (stdout, _, code) = run(&home, &["disable", "claude"]);
    assert_eq!(code, 0, "{stdout}");
    // the bare-path messages: no settings file to strip, auto-setup off
    assert!(stdout.contains("nothing to undo"), "{stdout}");
    assert!(stdout.contains("auto-setup is now off"), "{stdout}");
    // and it wrote the disabled state under the isolated HOME
    assert!(home
        .path()
        .join(".parsec")
        .join("setup_state.json")
        .exists());
}

#[test]
fn unknown_tool_lists_claude_as_supported() {
    let home = TempHome::new("unknown");
    for cmd in [&["setup", "nonsense"][..], &["disable", "nonsense"][..]] {
        let (_, stderr, code) = run(&home, cmd);
        assert_ne!(code, 0);
        assert!(
            stderr.contains("supported: claude, opencode, codex"),
            "{cmd:?}: {stderr}"
        );
    }
}

#[test]
fn byok_rejected_for_claude_before_any_setup_runs() {
    let home = TempHome::new("byok");
    let (_, stderr, code) = run(&home, &["setup", "claude", "--byok"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("--byok only applies to `parsec setup codex`"),
        "{stderr}"
    );
    // nothing was written: the guard fires before setup
    assert!(!home.path().join(".parsec").exists());
}

/// Run `parsec hook SessionStart` with a fresh-startup payload on stdin,
/// with the proxy-managing env vars stripped so the hook only exercises the
/// messaging paths (no spawn, no port probing).
fn run_session_start(home: &TempHome) -> String {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_parsec"))
        .args(["hook", "SessionStart"])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("PARSEC_API_KEY")
        .env_remove("PARSEC_BRAIN_KEY")
        .env_remove("PARSEC_API_KEY_NOTE")
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn parsec hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"hook_event_name":"SessionStart","source":"startup","session_id":"t","cwd":"/"}"#,
        )
        .unwrap();
    let out = child.wait_with_output().expect("run parsec hook");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Seed a HOME where every pre-fix startup banner would fire: measured
/// savings on disk (lifetime note) and no API key (get-a-key banner).
fn seed_savings_no_key(home: &TempHome, phase: &str) {
    let parsec = home.path().join(".parsec");
    std::fs::create_dir_all(parsec.join("sessions")).unwrap();
    std::fs::write(
        parsec.join("setup_state.json"),
        format!(
            r#"{{"contract_version":"setup-state/v0","phase":"{phase}","port":8082,"updated_unix":1}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        parsec.join("sessions").join("old.json"),
        r#"{"reads":{},"denials":{},"cmd_counts":{},"blocked_rereads":3,"tokens_saved":1200,"loops_broken":1}"#,
    )
    .unwrap();
}

#[test]
fn disabled_state_silences_session_start_banners() {
    let home = TempHome::new("hook-disabled");
    seed_savings_no_key(&home, "disabled");
    let stdout = run_session_start(&home);
    // `parsec disable` must silence the startup surfaces too — no get-a-key
    // banner, no "parsec active" lifetime note, no branded panel at all.
    assert_eq!(
        stdout.trim(),
        "",
        "disabled install still greeted: {stdout}"
    );
}

#[test]
fn ready_state_still_emits_session_start_banners() {
    let home = TempHome::new("hook-ready");
    seed_savings_no_key(&home, "ready");
    let stdout = run_session_start(&home);
    // Control: the same seed with an active install keeps its messaging.
    assert!(stdout.contains("lifetime"), "{stdout}");
}
