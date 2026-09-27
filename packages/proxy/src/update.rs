//! Idle auto-update — the supervisor's half of "a running proxy picks up new
//! releases without anyone re-running an installer".
//!
//! The SessionStart hook already replaces a proxy that predates a PLUGIN
//! update (hook.rs `maybe_upgrade_proxy`), but that needs a Claude Code
//! session to start after the plugin moved — and Codex / opencode / pi users
//! have no such hook at all. This loop closes the gap from the proxy side:
//!
//! 1. Every `PARSEC_UPDATE_INTERVAL_S` (default 6 h), and only while the
//!    supervisor has been idle for `PARSEC_UPDATE_IDLE_S` (default 10 min,
//!    no request in flight), fetch `releases/latest/download/manifest.json` —
//!    the same manifest the install scripts and the plugin shim trust.
//! 2. If it names a strictly NEWER stable version, download this platform's
//!    assets from that tag, verify each against the manifest sha256 (a
//!    missing sha is a refusal, not a skip), and check the staged binary
//!    reports `parsec <version>`.
//! 3. Re-check idleness, swap the files into `~/.parsec/bin/` (rename onto a
//!    fresh inode; on Windows the mapped image is renamed aside first), and
//!    spawn the NEW binary's `up --restart --port <ours>` detached — the exact
//!    restart the install scripts run, including the Claude Desktop
//!    interceptor bounce. That process shuts this supervisor down and starts
//!    the new one on the same port.
//!
//! Scope is deliberately narrow: only a binary running AS the managed copy
//! (`~/.parsec/bin/parsec[.exe]`, a real file, not the plugin-cache symlink)
//! updates itself — a dev build, a plugin-bundled binary, or a
//! `/usr/local/parsec` payload run directly is left alone, since something
//! else owns those files. Never downgrades, never takes a pre-release.
//!
//! Off the serving path entirely: nothing here touches request bytes, and
//! every failure leaves the running proxy exactly as it was (counted in
//! `/health`'s `update.failures`, logged at warn).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::setup::{parsec_home, semver_triple};

const DEFAULT_RELEASE_BASE: &str = "https://github.com/daseinlabs/parsec/releases";
const DEFAULT_INTERVAL_S: u64 = 6 * 3600;
const DEFAULT_IDLE_S: u64 = 10 * 60;

/// Suffix of a staged download next to its destination (same directory, so
/// the final rename never crosses a filesystem).
const STAGE_SUFFIX: &str = ".parsec-update";

#[cfg(windows)]
const EXE_NAME: &str = "parsec.exe";
#[cfg(not(windows))]
const EXE_NAME: &str = "parsec";

static CHECKS: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// `PARSEC_AUTO_UPDATE`: `0` / `off` / `false` / `no` turns the idle
/// self-update off (default on). The proxy then only changes version when an
/// installer, `parsec up --restart`, or the plugin hook replaces it.
pub fn enabled() -> bool {
    !matches!(
        std::env::var("PARSEC_AUTO_UPDATE")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Ok("0" | "off" | "false" | "no")
    )
}

/// `PARSEC_RELEASE_BASE`: the GitHub Releases root to update from (default
/// `https://github.com/daseinlabs/parsec/releases`) — the same override the
/// install scripts and the plugin shim honour, so a fork or a local mirror
/// serves all four routes.
fn release_base() -> String {
    std::env::var("PARSEC_RELEASE_BASE")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_BASE.to_string())
}

/// `PARSEC_UPDATE_INTERVAL_S`: seconds between release checks (default 6 h;
/// floor 60). `PARSEC_UPDATE_IDLE_S`: how long the proxy must have served
/// nothing before it checks or restarts (default 600; floor 0). Both exist
/// for testing and for mirrors; the defaults are what users get.
fn secs_env(name: &str, default: u64, floor: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
        .max(floor)
}

fn fail(msg: String) {
    FAILURES.fetch_add(1, Ordering::Relaxed);
    tracing::warn!("auto-update: {msg} — keeping the running version");
    *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) = Some(msg);
}

/// The `update` object on the supervisor's `/health`: whether the loop runs,
/// and its check / failure counters (fail-open events are counted, §8.3).
pub fn health_json() -> Value {
    json!({
        "enabled": enabled(),
        "checks": CHECKS.load(Ordering::Relaxed),
        "failures": FAILURES.load(Ordering::Relaxed),
        "last_error": LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()).clone(),
    })
}

#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    tag: String,
    assets: std::collections::BTreeMap<String, String>,
}

/// This build's release asset name, or None on a platform we do not ship.
fn platform_asset() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("parsec-darwin-arm64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("parsec-linux-x64")
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        Some("parsec-win-x64.exe")
    } else {
        None
    }
}

/// Upgrade only to a strictly newer STABLE version. Anything unparseable is
/// a no — an update must never be a guess.
fn is_newer(latest: &str, current: &str) -> bool {
    if latest.contains('-') {
        return false;
    }
    match (semver_triple(latest), semver_triple(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// (asset name in the release, file name in the bin dir) pairs to install.
/// The binary always; on Windows also the CRT DLLs the manifest lists, which
/// must sit next to the exe.
fn install_plan(m: &Manifest, asset: &str) -> Vec<(String, String)> {
    let mut plan = vec![(asset.to_string(), EXE_NAME.to_string())];
    if cfg!(windows) {
        for name in m.assets.keys().filter(|n| n.ends_with(".dll")) {
            plan.push((name.clone(), name.clone()));
        }
    }
    plan
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The file this process may replace: `~/.parsec/bin/parsec[.exe]`, and only
/// when that is (a) a real file, not the plugin-cache symlink
/// `refresh_bin_alias` maintains, and (b) the very image we are running.
fn managed_target(bin_dir: &Path) -> Option<PathBuf> {
    let dest = bin_dir.join(EXE_NAME);
    let meta = std::fs::symlink_metadata(&dest).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    (exe == dest.canonicalize().ok()?).then_some(dest)
}

/// `<exe> --version` must print exactly `parsec <version>`.
fn reports_version(exe: &Path, version: &str) -> bool {
    std::process::Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).trim() == format!("parsec {version}")
        })
        .unwrap_or(false)
}

/// Remove leftovers of earlier swaps: staged downloads and renamed-aside
/// images (Windows can rename a mapped exe/DLL but not delete it, so the
/// `.old` copies are swept once nothing maps them any more). Best-effort.
fn sweep(bin_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(bin_dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(STAGE_SUFFIX) || is_aside_name(&name) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `x.old` or `x.old.<n>` — the names `aside_path` hands out.
fn is_aside_name(name: &str) -> bool {
    name.ends_with(".old")
        || name
            .rsplit_once(".old.")
            .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// A free `.old` / `.old.<n>` name beside `path` (mirrors the Windows
/// installer's `Displace`: an aside that is still mapped cannot be removed,
/// so a second swap before the sweep needs a new name).
fn aside_path(path: &Path) -> Option<PathBuf> {
    let base = path.file_name()?.to_string_lossy().into_owned();
    std::iter::once(format!("{base}.old"))
        .chain((1..=20).map(|n| format!("{base}.old.{n}")))
        .map(|n| path.with_file_name(n))
        .find(|p| !p.exists())
}

/// Move each staged file onto its destination. Unix: a plain rename replaces
/// the name atomically (and lands a fresh inode, which the macOS
/// code-signing cache requires); the plan there is the binary alone. Windows:
/// the running image is renamed aside first — permitted for a mapped file —
/// then the stage takes its name, and a failure part-way renames the asides
/// back, so the exe and its DLLs move all or nothing.
fn swap_in(pairs: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let rollback = |done: &[(PathBuf, Option<PathBuf>)]| {
        for (dest, aside) in done.iter().rev() {
            if let Some(a) = aside {
                let _ = std::fs::rename(a, dest);
            }
        }
    };
    for (stage, dest) in pairs {
        let aside = if cfg!(windows) && dest.exists() {
            let Some(a) = aside_path(dest) else {
                rollback(&done);
                return Err(format!("no free aside name for {}", dest.display()));
            };
            if let Err(e) = std::fs::rename(dest, &a) {
                rollback(&done);
                return Err(format!("rename {} aside: {e}", dest.display()));
            }
            Some(a)
        } else {
            None
        };
        if let Err(e) = std::fs::rename(stage, dest) {
            if let Some(a) = &aside {
                let _ = std::fs::rename(a, dest);
            }
            rollback(&done);
            return Err(format!("install {}: {e}", dest.display()));
        }
        done.push((dest.clone(), aside));
    }
    Ok(())
}

fn write_stage(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// A verified release, held in memory until the proxy is idle enough to swap.
struct Pending {
    version: String,
    /// (file name in the bin dir, bytes)
    files: Vec<(String, Vec<u8>)>,
}

async fn fetch_manifest(client: &reqwest::Client, base: &str) -> Result<Manifest, String> {
    let url = format!("{base}/latest/download/manifest.json");
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("fetch {url}: {e}"))?;
    resp.json::<Manifest>()
        .await
        .map_err(|e| format!("parse {url}: {e}"))
}

/// Download every planned asset from the manifest's tag and verify each one's
/// sha256. Files already on disk with the right hash (the Windows DLLs, which
/// rarely change) are skipped.
async fn download(
    client: &reqwest::Client,
    base: &str,
    m: &Manifest,
    asset: &str,
    bin_dir: &Path,
) -> Result<Pending, String> {
    let mut files = Vec::new();
    for (name, local) in install_plan(m, asset) {
        let want = m
            .assets
            .get(&name)
            .ok_or_else(|| format!("manifest for {} has no sha256 for {name}", m.tag))?
            .to_ascii_lowercase();
        if local != EXE_NAME {
            if let Ok(have) = std::fs::read(bin_dir.join(&local)) {
                if sha256_hex(&have) == want {
                    continue;
                }
            }
        }
        let url = format!("{base}/download/{}/{name}", m.tag);
        let bytes = client
            .get(&url)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("download {url}: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("download {url}: {e}"))?;
        let got = sha256_hex(&bytes);
        if got != want {
            return Err(format!(
                "{name}: sha256 {got} does not match the manifest ({want})"
            ));
        }
        files.push((local, bytes.to_vec()));
    }
    Ok(Pending {
        version: m.version.clone(),
        files,
    })
}

/// Stage, sanity-run, swap, and hand off to `up --restart`. Blocking I/O —
/// run on the blocking pool.
fn install_and_restart(p: Pending, bin_dir: &Path, dest: &Path, port: u16) -> Result<(), String> {
    let mut pairs = Vec::new();
    for (local, bytes) in &p.files {
        let final_path = bin_dir.join(local);
        let stage = bin_dir.join(format!("{local}{STAGE_SUFFIX}"));
        write_stage(&stage, bytes).map_err(|e| format!("stage {}: {e}", stage.display()))?;
        pairs.push((stage, final_path));
    }
    // Sanity-run the staged binary before it replaces anything. On Windows
    // it loads the DLLs already beside it — the CRT is backward compatible,
    // so an older copy there still runs a newer exe.
    if let Some((stage, _)) = pairs.iter().find(|(_, d)| d == dest) {
        if !reports_version(stage, &p.version) {
            sweep(bin_dir);
            return Err(format!(
                "downloaded binary does not report `parsec {}`",
                p.version
            ));
        }
    }
    if let Err(e) = swap_in(&pairs) {
        sweep(bin_dir);
        return Err(e);
    }
    tracing::info!(
        from = env!("CARGO_PKG_VERSION"),
        to = %p.version,
        "auto-update: installed {} — restarting the proxy on 127.0.0.1:{port}",
        p.version
    );
    crate::setup::spawn_restart_detached(dest, port)
        .map_err(|e| format!("spawn `{} up --restart`: {e}", dest.display()))
}

/// The update loop. `idle_s` reports how long the proxy has served nothing
/// (None while a request is in flight). Returns only when the loop is off or
/// the handoff to the new binary was spawned.
pub async fn run(client: reqwest::Client, port: u16, idle_s: impl Fn() -> Option<u64>) {
    if !enabled() {
        tracing::info!("auto-update off (PARSEC_AUTO_UPDATE)");
        return;
    }
    let Some(asset) = platform_asset() else {
        return;
    };
    let bin_dir = parsec_home().join("bin");
    let Some(dest) = managed_target(&bin_dir) else {
        tracing::info!(
            "auto-update: not running as {} — updates come from whatever installed this binary",
            bin_dir.join(EXE_NAME).display()
        );
        return;
    };
    sweep(&bin_dir);
    let base = release_base();
    let interval = secs_env("PARSEC_UPDATE_INTERVAL_S", DEFAULT_INTERVAL_S, 60);
    let idle_needed = secs_env("PARSEC_UPDATE_IDLE_S", DEFAULT_IDLE_S, 0);
    let tick = Duration::from_secs(interval.min(60));
    let is_idle = || idle_s().is_some_and(|s| s >= idle_needed);

    // Seconds since the last completed check; starts "due" so the first idle
    // window after startup checks. Counted in ticks, not wall clock, so a
    // suspended laptop does not fire a burst on resume.
    let mut since_check = interval;
    let mut pending: Option<Pending> = None;
    loop {
        tokio::time::sleep(tick).await;
        since_check = since_check.saturating_add(tick.as_secs());
        if !is_idle() {
            continue;
        }
        if pending.is_none() {
            if since_check < interval {
                continue;
            }
            since_check = 0;
            CHECKS.fetch_add(1, Ordering::Relaxed);
            let m = match fetch_manifest(&client, &base).await {
                Ok(m) => m,
                Err(e) => {
                    fail(e);
                    continue;
                }
            };
            if !is_newer(&m.version, env!("CARGO_PKG_VERSION")) {
                tracing::debug!(latest = %m.version, "auto-update: up to date");
                continue;
            }
            tracing::info!(latest = %m.version, "auto-update: new release — downloading");
            match download(&client, &base, &m, asset, &bin_dir).await {
                Ok(p) => pending = Some(p),
                Err(e) => {
                    fail(e);
                    continue;
                }
            }
        }
        // A download can take a while: only swap if the proxy is STILL idle,
        // otherwise hold the verified bytes for the next quiet tick.
        if !is_idle() {
            continue;
        }
        let Some(p) = pending.take() else { continue };
        let (bd, d) = (bin_dir.clone(), dest.clone());
        match tokio::task::spawn_blocking(move || install_and_restart(p, &bd, &d, port)).await {
            Ok(Ok(())) => return, // the new binary's `up --restart` takes it from here
            Ok(Err(e)) => fail(e),
            Err(e) => fail(format!("install task: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "parsec-update-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_strictly_newer_stable() {
        assert!(is_newer("0.2.21", "0.2.20"));
        assert!(is_newer("0.3.0", "0.2.20"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.20", "0.2.20"));
        assert!(!is_newer("0.2.19", "0.2.20")); // never downgrade
        assert!(!is_newer("0.3.0-rc.1", "0.2.20")); // never a pre-release
        assert!(!is_newer("garbage", "0.2.20"));
        assert!(!is_newer("0.2.21", "dev"));
    }

    #[test]
    fn manifest_shape_matches_release_yml() {
        let m: Manifest = serde_json::from_str(
            r#"{"version":"0.2.20","tag":"v0.2.20","assets":{
                "parsec-darwin-arm64":"aa","parsec-linux-x64":"bb",
                "parsec-win-x64.exe":"cc","msvcp140.dll":"dd","parsec-plugin.zip":"ee"}}"#,
        )
        .unwrap();
        assert_eq!(m.tag, "v0.2.20");
        let plan = install_plan(&m, "parsec-linux-x64");
        assert_eq!(plan[0], ("parsec-linux-x64".into(), EXE_NAME.into()));
        // The plugin zip is never installed; DLLs only ride along on Windows.
        assert!(plan.iter().all(|(n, _)| n != "parsec-plugin.zip"));
        assert_eq!(plan.len(), if cfg!(windows) { 2 } else { 1 });
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn aside_names_and_sweep() {
        let d = tmpdir("sweep");
        let exe = d.join("parsec.exe");
        std::fs::write(&exe, b"x").unwrap();
        assert_eq!(aside_path(&exe).unwrap(), d.join("parsec.exe.old"));
        std::fs::write(d.join("parsec.exe.old"), b"x").unwrap();
        assert_eq!(aside_path(&exe).unwrap(), d.join("parsec.exe.old.1"));
        std::fs::write(d.join("parsec.exe.old.1"), b"x").unwrap();
        std::fs::write(d.join(format!("parsec{STAGE_SUFFIX}")), b"x").unwrap();
        std::fs::write(d.join("parsec.older"), b"keep").unwrap();
        sweep(&d);
        let mut left: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["parsec.exe", "parsec.older"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn swap_replaces_and_rolls_back() {
        let d = tmpdir("swap");
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::write(&a, b"old-a").unwrap();
        std::fs::write(&b, b"old-b").unwrap();
        let (sa, sb) = (d.join("a.stage"), d.join("b.stage"));
        std::fs::write(&sa, b"new-a").unwrap();
        std::fs::write(&sb, b"new-b").unwrap();
        swap_in(&[(sa, a.clone()), (sb, b.clone())]).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"new-a");
        assert_eq!(std::fs::read(&b).unwrap(), b"new-b");

        // Second pair's stage is missing ⇒ the whole swap fails and, where
        // an aside was taken (Windows), the first file is restored.
        let sa2 = d.join("a.stage2");
        std::fs::write(&sa2, b"newer-a").unwrap();
        let err = swap_in(&[(sa2, a.clone()), (d.join("missing"), b.clone())]);
        assert!(err.is_err());
        assert_eq!(std::fs::read(&b).unwrap(), b"new-b");
        if cfg!(windows) {
            assert_eq!(std::fs::read(&a).unwrap(), b"new-a");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn env_knobs() {
        // Unset / unparseable fall back to the default; the floor holds.
        assert_eq!(secs_env("PARSEC_TEST_UNSET_KNOB", 42, 10), 42);
        assert_eq!(secs_env("PARSEC_TEST_UNSET_KNOB", 5, 10), 10);
    }
}
