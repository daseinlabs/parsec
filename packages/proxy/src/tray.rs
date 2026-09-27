//! `parsec tray` — the menu-bar app, and the guided Claude Desktop setup that
//! lives in it (docs/claude-desktop-integration.md §3).
//!
//! Why this exists: the CLI can only *report* a blocked precondition and exit.
//! Desktop setup has three of them, two needing a human (a trust-store
//! authorization, a System Settings toggle), so the terminal flow is
//! necessarily "run, fail, go do a thing, re-run" — once per prerequisite.
//! A resident app can present the gate and then WAIT on it, which collapses
//! the whole thing to one command and one click.
//!
//! Shape, deliberately minimal:
//!
//! - **A role of the existing binary, not a new artifact.** `parsec tray run`
//!   is a subcommand like `proxy` or `mcp` (DIRECTION §7b: one binary,
//!   subcommands for every role). The `.app` is GENERATED on demand around
//!   the binary already installed — nothing extra to build, sign, notarize,
//!   or keep in version step.
//! - **User scope only.** A plist in `~/Library/LaunchAgents` and a bundle in
//!   `~/Library/Application Support` need no admin, which is how a login item
//!   is supposed to be installed.
//! - **macOS only for now.** It is the platform where Desktop interception
//!   works without a UAC prompt on every launch.
//!
//! The tray reads state and shells out to the same commands a user would run;
//! it owns no serving state of its own, so a crashed tray costs a menu bar
//! icon and nothing else.

use std::path::PathBuf;

/// What `parsec tray …` was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Run the menu-bar app in the foreground (what the LaunchAgent invokes).
    Run,
    /// Generate the .app bundle, register the login item, and start it.
    Install,
    /// Unregister and delete both.
    Uninstall,
    /// Report what is installed; change nothing.
    Status,
    /// Assemble an unsigned `.app` at `out` around `binary` for the release
    /// build to sign (macOS only; the installer ships the result).
    Bundle { out: PathBuf, binary: PathBuf },
}

/// Bundle / login-item identity. Only the platform install paths use it.
#[cfg(any(target_os = "macos", windows))]
const LABEL: &str = "rocks.dasein.parsec.tray";

// ── platform install paths ──────────────────────────────────────────────────
//
// macOS and Windows disagree about what "install a resident app" means, so
// only the SHAPE is shared: provision an artifact, register it to start at
// login, report what happened. `app.rs` — the tray UI itself — is common.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{bundle, install, installed, status, uninstall};

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub use windows::{install, installed, status, uninstall};
#[cfg(windows)]
pub fn bundle(_out: &std::path::Path, _binary: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("`parsec tray bundle` builds a macOS .app; there is no Windows equivalent")
}

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported {
    pub fn installed() -> bool {
        false
    }
    pub fn install() -> anyhow::Result<()> {
        anyhow::bail!("`parsec tray` supports macOS and Windows")
    }
    pub fn uninstall() -> anyhow::Result<()> {
        Ok(())
    }
    pub fn status() -> anyhow::Result<()> {
        println!("tray app: unsupported on this platform");
        Ok(())
    }
    pub fn bundle(_out: &std::path::Path, _binary: &std::path::Path) -> anyhow::Result<()> {
        anyhow::bail!("`parsec tray bundle` builds a macOS .app")
    }
}
#[cfg(not(any(target_os = "macos", windows)))]
pub use unsupported::{bundle, install, installed, status, uninstall};

/// Where the tray writes its own log, on every platform.
pub fn log_path() -> PathBuf {
    crate::setup::parsec_home().join("tray.log")
}

/// The binary a login item should launch. The ALIAS, never `current_exe()`:
/// the plugin cache path this process runs from changes on every plugin
/// update, and a login item must survive that.
pub fn target_binary() -> PathBuf {
    crate::setup_opencode::bin_alias_path()
}

pub fn run(action: Action) -> anyhow::Result<()> {
    match action {
        Action::Install => install(),
        Action::Uninstall => uninstall(),
        Action::Status => status(),
        Action::Bundle { out, binary } => bundle(&out, &binary),
        Action::Run => run_app(),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn run_app() -> anyhow::Result<()> {
    anyhow::bail!(
        "the tray app is macOS and Windows only — Linux has no single tray convention \
         to target (docs/claude-desktop-integration.md §7)"
    )
}

#[cfg(any(target_os = "macos", windows))]
pub use app::run_app;

#[cfg(any(target_os = "macos", windows))]
mod app;
