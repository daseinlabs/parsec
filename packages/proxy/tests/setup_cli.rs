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
