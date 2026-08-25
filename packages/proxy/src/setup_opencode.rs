//! `parsec setup opencode` — install the opencode shim as a managed file drop
//! (docs/tool-integrations-survey.md §3.5, Phase 1).
//!
//! The shim source is packages/opencode-plugin/index.js, embedded at compile
//! time so the binary stays self-contained (no network, no npm). It lands in
//! opencode's global plugin dir (`~/.config/opencode/plugin/parsec.js`), which
//! opencode auto-loads — no opencode.json edit, nothing of the user's is
//! rewritten. Ownership follows the settings.json discipline in `setup`:
//! a file we wrote carries the sentinel and may be refreshed/removed; a file
//! without it is the user's and is never touched.
//!
//! Deliberately NOT touched here: `~/.parsec/setup_state.json`. That file
//! drives Claude Code's auto-setup phases — marking it `ready` from an
//! opencode-only install would make a later Claude Code setup skip its
//! settings routing. The shim and this command both fall back to the default
//! port when no state exists, so they agree without sharing state.

use std::path::{Path, PathBuf};

/// The shim, embedded verbatim. include_str! keeps the two artifacts (npm
/// package and file drop) byte-identical by construction.
const PLUGIN_JS: &str = include_str!("../../opencode-plugin/index.js");

/// Marks a plugin file as parsec-managed. Present in every version we have
/// ever shipped (it lives in the shim's header comment), so ownership
/// survives shim updates the way the statusLine binary-path rule does.
const SENTINEL: &str = "parsec-managed-plugin";

/// opencode's global config dir: `$XDG_CONFIG_HOME/opencode`, else
/// `~/.config/opencode` (opencode uses XDG paths on every platform).
fn opencode_config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::setup::home_dir().join(".config"))
        .join("opencode")
}

pub fn plugin_path() -> PathBuf {
    opencode_config_dir().join("plugin").join("parsec.js")
}

/// What a write attempt decided — pure, path-free core for tests.
#[derive(Debug, PartialEq)]
enum WriteDecision {
    /// No file, or an older managed one → (re)write.
    Write,
    /// Byte-identical managed file already there.
    Current,
    /// A file without our sentinel: the user's, refuse to touch.
    Foreign,
}

fn decide(existing: Option<&str>) -> WriteDecision {
    match existing {
        None => WriteDecision::Write,
        Some(cur) if cur == PLUGIN_JS => WriteDecision::Current,
        Some(cur) if cur.contains(SENTINEL) => WriteDecision::Write,
        Some(_) => WriteDecision::Foreign,
    }
}

/// `parsec setup opencode`: drop the managed shim, warm the proxy on the
/// port the shim will look for, print what happened. Non-destructive
/// everywhere; repeat runs converge.
pub fn setup() -> anyhow::Result<()> {
    let path = plugin_path();
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    match decide(existing.as_deref()) {
        WriteDecision::Foreign => {
            anyhow::bail!(
                "{} exists and is not parsec-managed — move it aside, or add \
                 \"parsec-opencode\" to the `plugin` array in your opencode config instead",
                path.display()
            );
        }
        WriteDecision::Current => {
            println!("opencode shim already current at {}", path.display());
        }
        WriteDecision::Write => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("js.parsec-tmp");
            std::fs::write(&tmp, PLUGIN_JS)?;
            std::fs::rename(&tmp, &path)?;
            println!("opencode shim written to {}", path.display());
        }
    }

    // Give the shim a stable binary path: the plugin-cache location this
    // binary runs from changes on every update, and the shim (plus the
    // /parsec-* command templates it registers) probes the alias path when
    // `parsec` is not on PATH.
    if let Err(e) = refresh_bin_alias() {
        println!(
            "could not refresh the {} alias ({e}) — the shim will look for \
             `parsec` on PATH instead",
            bin_alias_path().display()
        );
    }

    // Warm the proxy on the port the shim resolves (state-file port, else
    // default) so the first opencode session finds it listening. The shim
    // also runs `parsec up` itself — this just removes the first-run gap.
    let port = crate::setup::load_state()
        .map(|st| st.port)
        .filter(|p| *p > 0)
        .unwrap_or_else(crate::setup::default_port);
    // Identity, not just liveness. A bare `port_listening` reports success
    // for ANY process holding the port — including a stale pre-update parsec
    // supervisor, or something else entirely — which is the false-success
    // this whole probe exists to prevent. The shim itself already gets this
    // right (`proxyHealthy` requires /health to answer as parsec before it
    // routes); the installer was the half that did not.
    use crate::setup::PortOccupant;
    match crate::setup::classify_port(port, &["anthropic"]) {
        PortOccupant::Compatible => println!("proxy already listening on 127.0.0.1:{port}"),
        PortOccupant::Free => match crate::setup::spawn_proxy_detached(port, &[]) {
            Ok(()) => println!("proxy starting on 127.0.0.1:{port}"),
            Err(e) => println!(
                "proxy pre-warm failed ({e}) — the shim will start it when opencode launches"
            ),
        },
        PortOccupant::StaleParsec => println!(
            "a parsec proxy on 127.0.0.1:{port} predates this build and does not serve the \
             anthropic wire — run `parsec up --restart` so the installed binary serves"
        ),
        // Never spawn onto, and never claim, a port we do not own. The shim
        // will find no parsec /health here and stay unrouted — fail open.
        PortOccupant::Foreign => println!(
            "127.0.0.1:{port} is held by a non-parsec process — NOT routing opencode at it. \
             Free the port and re-run, or run `parsec setup` to pick a different one."
        ),
    }

    println!(
        "done. Restart opencode to activate curation (Anthropic API-key \
         providers only — subscription OAuth is out of scope). Undo anytime: \
         parsec disable opencode"
    );
    Ok(())
}

/// The stable path the shim and its command templates probe when `parsec` is
/// not on PATH. Windows ships `parsec.exe`, and the shim's `BIN_NAME` must
/// agree with this or the probe finds nothing.
pub(crate) fn bin_alias_path() -> PathBuf {
    let name = if cfg!(windows) {
        "parsec.exe"
    } else {
        "parsec"
    };
    crate::setup::parsec_home().join("bin").join(name)
}

/// Point `~/.parsec/bin/parsec` at the running binary. Only a symlink is
/// ever replaced (a symlink there is ours by construction); a real file is
/// the user's and is left alone. `parsec uninstall` purges the whole dir,
/// so no separate removal path is needed. Shared with the codex installer:
/// its prompt files probe the same stable path.
#[cfg(unix)]
pub(crate) fn refresh_bin_alias() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let alias = bin_alias_path();
    let state = match std::fs::symlink_metadata(&alias) {
        Ok(md) if md.file_type().is_symlink() => AliasState::SymlinkTo(std::fs::read_link(&alias)?),
        Ok(_) => AliasState::RealFile,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AliasState::Missing,
        Err(e) => return Err(e.into()),
    };
    // `alias == exe` is install.sh's own layout: the downloaded binary IS the
    // probe path, and it is the image currently running. Relinking it would
    // point the alias at itself.
    if decide_alias(&state, &exe, alias == exe) == AliasAction::Keep {
        return Ok(());
    }
    if let Some(dir) = alias.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Atomic: symlink to a temp name, then rename over. A `parsec` typed at
    // exactly the wrong moment must never find the path missing.
    let tmp = alias.with_extension("parsec-tmp");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&exe, &tmp)?;
    if let Err(e) = std::fs::rename(&tmp, &alias) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    match state {
        AliasState::RealFile => println!(
            "replaced the stale binary at {} with a link to {} — `parsec` on your PATH now \
             tracks plugin updates instead of staying frozen at install time",
            alias.display(),
            exe.display()
        ),
        _ => println!("binary alias {} -> {}", alias.display(), exe.display()),
    }
    Ok(())
}

/// What is sitting at the alias path.
#[cfg(unix)]
#[derive(Debug, PartialEq)]
enum AliasState {
    Missing,
    SymlinkTo(PathBuf),
    RealFile,
}

#[cfg(unix)]
#[derive(Debug, PartialEq)]
enum AliasAction {
    Keep,
    Link,
}

/// Pure decision, so the rule that used to be wrong is pinned by a test.
///
/// The bug it fixes: a REAL FILE here used to mean "the user's — leave it
/// alone", but `install.sh` *downloads a real binary to exactly this path*.
/// So on every machine where install.sh ran, the `parsec` on PATH was frozen
/// at install time and no plugin update ever refreshed it — the PATH binary
/// and the plugin binary silently drifted apart. (That is not a hypothetical:
/// it is how a proxy predating the `/v1/models` route stayed serving.)
///
/// `~/.parsec/bin/` is ours regardless of what is in it — `parsec uninstall`
/// already deletes the whole directory (`purge_data_files`), so treating one
/// file inside it as sacred was never consistent. A symlink is what belongs
/// there: the plugin cache path (`~/.claude/plugins/cache/…/parsec`) is
/// stable across versions, so the link keeps resolving to the current build
/// on its own.
#[cfg(unix)]
fn decide_alias(state: &AliasState, exe: &Path, alias_is_exe: bool) -> AliasAction {
    if alias_is_exe {
        return AliasAction::Keep;
    }
    match state {
        AliasState::SymlinkTo(target) if target == exe => AliasAction::Keep,
        _ => AliasAction::Link,
    }
}

/// Windows twin of the alias refresh. There is no symlink to reach for — an
/// unprivileged account cannot create one — so the alias is a COPY, which
/// means it CAN go stale in a way the unix symlink deliberately cannot.
/// Refreshing it on every `parsec setup opencode` is what keeps it honest.
///
/// Without this the shim's probe path (`%USERPROFILE%\.parsec\bin\parsec.exe`)
/// simply did not exist for anyone who installed through the Claude Code
/// plugin rather than install.ps1: `findParsecBin()` returned null, so the
/// whole `/parsec-*` command surface vanished and the shim could not run
/// `parsec up` to revive a dead proxy.
#[cfg(windows)]
pub(crate) fn refresh_bin_alias() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let alias = bin_alias_path();
    // install.ps1 drops the exe straight at the probe path — then the alias
    // IS the running image and there is nothing to copy (nor could we, with
    // the file locked by our own process).
    if alias == exe {
        return Ok(());
    }
    if let Some(dir) = alias.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let src = std::fs::read(&exe)?;
    let current = std::fs::metadata(&alias).map(|m| m.len()).unwrap_or(0) == src.len() as u64
        && std::fs::read(&alias).map(|cur| cur == src).unwrap_or(false);
    if current {
        return Ok(());
    }
    // Temp + rename so a proxy starting concurrently never maps a
    // half-written image. rename() replaces an existing file on Windows; a
    // target locked by a RUNNING proxy fails here, and the caller reports
    // that as a warning rather than failing the whole setup.
    let tmp = alias.with_extension("exe.parsec-tmp");
    std::fs::write(&tmp, &src)?;
    if let Err(e) = std::fs::rename(&tmp, &alias) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    copy_sibling_dlls(&exe, &alias);

    println!(
        "binary alias {} -> copy of {}",
        alias.display(),
        exe.display()
    );
    Ok(())
}

/// Carry the app-local VC++ CRT across with the exe.
///
/// parsec.exe imports msvcp140/vcruntime140, which are absent on a clean
/// Windows box, and the loader searches ONLY next to the exe. release.yml
/// ships them beside the binary for that reason — copying the exe alone to
/// the alias path stripped them, so the copy everything points at died before
/// main() with 0xC0000135 and no stderr. That is undiagnosable anywhere, and
/// invisible in the tray, which starts through a hidden-window launcher.
///
/// Best-effort per file: a release that no longer imports the CRT publishes
/// none of these, and their absence is not an error.
#[cfg(windows)]
fn copy_sibling_dlls(exe: &Path, alias: &Path) {
    let (Some(from), Some(to)) = (exe.parent(), alias.parent()) else {
        return;
    };
    if from == to {
        return;
    }
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension()
            .and_then(|x| x.to_str())
            .map(str::to_ascii_lowercase)
            != Some("dll".into())
        {
            continue;
        }
        let Some(name) = p.file_name() else { continue };
        let dest = to.join(name);
        // Same-content check keeps repeat setups from rewriting ~750 KB.
        let same = std::fs::metadata(&dest).map(|m| m.len()).ok()
            == std::fs::metadata(&p).map(|m| m.len()).ok();
        if !same {
            let _ = std::fs::copy(&p, &dest);
        }
    }
}

/// `parsec disable opencode` — remove exactly (and only) the managed shim.
pub fn disable() -> anyhow::Result<()> {
    let path = plugin_path();
    match std::fs::read_to_string(&path) {
        Ok(cur) if cur.contains(SENTINEL) => {
            std::fs::remove_file(&path)?;
            println!(
                "removed {} — restart opencode to route directly again",
                path.display()
            );
        }
        Ok(_) => println!("{} is not parsec-managed — left alone", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no opencode shim at {} — nothing to undo", path.display());
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Uninstall hook: same removal as [`disable`], quiet when absent — called
/// from `parsec uninstall` so a full cleanup never leaves the shim pointing
/// at a proxy that no longer exists.
pub fn remove_if_managed() {
    let path = plugin_path();
    if let Ok(cur) = std::fs::read_to_string(&path) {
        if cur.contains(SENTINEL) {
            match std::fs::remove_file(&path) {
                Ok(()) => println!("removed opencode shim {}", path.display()),
                Err(e) => eprintln!(
                    "could not remove opencode shim {}: {e} — delete by hand",
                    path.display()
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_stale_real_binary_at_the_alias_path_is_replaced() {
        let exe = Path::new("/plugins/cache/parsec-marketplace/parsec/bin/parsec");

        // The regression: install.sh downloads a REAL binary to the alias
        // path. Treating it as "the user's" froze `parsec` on PATH at install
        // time forever.
        assert_eq!(
            decide_alias(&AliasState::RealFile, exe, false),
            AliasAction::Link
        );
        // Nothing there yet — link it.
        assert_eq!(
            decide_alias(&AliasState::Missing, exe, false),
            AliasAction::Link
        );
        // Already pointing at this build — leave it alone (idempotent).
        assert_eq!(
            decide_alias(&AliasState::SymlinkTo(exe.to_path_buf()), exe, false),
            AliasAction::Keep
        );
        // Pointing at an older build — repoint.
        assert_eq!(
            decide_alias(&AliasState::SymlinkTo("/old/parsec".into()), exe, false),
            AliasAction::Link
        );
        // install.sh's own layout: the alias IS the running image. Relinking
        // would point it at itself.
        assert_eq!(
            decide_alias(&AliasState::RealFile, exe, true),
            AliasAction::Keep
        );
    }

    #[test]
    fn shim_carries_the_sentinel_and_the_tool_tag() {
        // The ownership rule is only sound if every shipped shim embeds the
        // sentinel, and attribution only works if it sets the header.
        assert!(PLUGIN_JS.contains(SENTINEL));
        assert!(PLUGIN_JS.contains("x-parsec-tool"));
    }

    #[test]
    fn shim_carries_the_full_command_surface() {
        // The opencode port of the Claude Code skill surface — a name
        // vanishing from the embedded shim silently drops that command
        // for every user on the next release.
        for cmd in [
            "parsec-savings",
            "parsec-proxy",
            "parsec-key",
            "parsec-setup",
            "parsec-uninstall",
        ] {
            assert!(PLUGIN_JS.contains(cmd), "{cmd} missing from shim");
        }
    }

    #[test]
    fn shim_probes_the_platform_binary_name() {
        // The installer writes the alias at bin_alias_path(); the shim probes
        // BIN_NAME. If those disagree the shim finds no binary, registers no
        // commands, and cannot run `parsec up` — silently, and only on the
        // platform nobody tests on.
        assert!(
            PLUGIN_JS.contains("parsec.exe"),
            "shim has no Windows binary name"
        );
        let expected = bin_alias_path();
        let name = expected.file_name().unwrap().to_string_lossy();
        assert!(
            PLUGIN_JS.contains(&format!("\"{name}\"")),
            "shim does not probe {name}"
        );
    }

    #[test]
    fn decide_writes_fresh_refreshes_ours_refuses_foreign() {
        assert_eq!(decide(None), WriteDecision::Write);
        assert_eq!(decide(Some(PLUGIN_JS)), WriteDecision::Current);
        // An older managed version (sentinel present, bytes differ).
        let old = format!("/* {SENTINEL} */ export const ParsecPlugin = 1;");
        assert_eq!(decide(Some(&old)), WriteDecision::Write);
        // A user's own plugin file: never ours to overwrite.
        assert_eq!(
            decide(Some("export const MyPlugin = async () => ({});")),
            WriteDecision::Foreign
        );
    }
}
