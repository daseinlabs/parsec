//! `parsec setup desktop` — route Claude Desktop's Cowork / Agent-mode
//! inference through the local parsec proxy
//! (docs/claude-desktop-integration.md).
//!
//! Every other surface parsec captures asks the client politely:
//! `ANTHROPIC_BASE_URL` for Claude Code (`setup`), a plugin file drop for
//! opencode (`setup_opencode`), a config profile for Codex (`setup_codex`).
//! Claude Desktop has no such knob — its embedded SDK is hardwired to
//! api.anthropic.com (`docs/routing-and-liveness.md` §1.2). The only path
//! that reaches it is process-scoped TLS interception, which is why this
//! module is **opt-in, never part of `parsec setup`**, and why it is the one
//! place parsec reaches for a tool it does not ship.
//!
//! Trust posture, stated plainly because it is a real escalation:
//!
//! - This requires **mitmproxy** (a Python tool) on PATH. DIRECTION §7b's
//!   "one auditable static binary, no runtime bet" holds for everything
//!   parsec ships; it cannot hold for an interception layer whose macOS
//!   Network Extension / Windows WinDivert / Linux eBPF plumbing has no Rust
//!   equivalent. We depend on it only when the user asks for Desktop.
//! - It requires mitmproxy's **CA in the system trust store**. We never
//!   install that silently: `--install-ca` is an explicit opt-in, and
//!   `parsec disable desktop` prints the exact removal command.
//! - Interception puts the proxy in the path of **subscription OAuth
//!   tokens**, not just plaintext loopback. It stays on the user's machine —
//!   the addon adds no header and parsec forwards auth verbatim
//!   (`server.rs` `forward_auth_headers`) — so the §3 data-plane rule holds,
//!   but it is a bigger ask than routing Claude Code.
//!
//! Everything below is lifecycle: detect mitmproxy, check the platform's
//! interception mechanism is approved, write the managed addon, spawn/stop
//! `mitmdump`, and optionally install a boot service. The *serving* side
//! needed no change at all — a redirected `/v1/messages` arrives at the
//! routed port indistinguishable from Claude Code's.

use crate::quiet::QuietCommand;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Marks the addon file as parsec-managed. Present in the header comment of
/// every version we ship, so ownership survives addon updates the same way
/// the opencode shim's sentinel does.
const SENTINEL: &str = "parsec-managed-interceptor";

/// The addon, embedded at compile time so the binary stays self-contained
/// (no network, no pip, nothing to keep in sync).
const ADDON_PY: &str = include_str!("desktop_addon.py");

const LAUNCHD_LABEL: &str = "rocks.dasein.parsec.interceptor";
const SYSTEMD_UNIT: &str = "parsec-interceptor";
const WIN_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// Legacy (pre-installer) autostart: a Run-key value. It fires UNELEVATED at
/// sign-in, and WinDivert needs elevation, so it never actually worked across
/// a reboot (docs/claude-desktop-integration.md §7). Kept only so old installs
/// are recognised and migrated.
const WIN_RUN_NAME: &str = "ParsecInterceptor";
/// Current Windows autostart: a Scheduled Task with `/RL HIGHEST`, which is
/// the one per-user mechanism that runs elevated at sign-in without a UAC
/// prompt. Registering it needs administrator once — the installer's elevated
/// step, or an elevated `parsec setup desktop --autostart`.
const WIN_TASK_NAME: &str = "ParsecInterceptor";

// The proxy's own boot service — installed and removed in lockstep with the
// interceptor's. The interceptor service without it IS the reboot hang: the
// service manager revives mitmdump, which faithfully redirects every Desktop
// request into a port nothing answers, and Desktop has no hook surface to
// revive the proxy the way Claude Code's SessionStart does.
const PROXY_LAUNCHD_LABEL: &str = "rocks.dasein.parsec.proxy";
const PROXY_SYSTEMD_UNIT: &str = "parsec-proxy";
const WIN_PROXY_RUN_NAME: &str = "ParsecProxy";

// ── paths ───────────────────────────────────────────────────────────────────

pub fn interceptor_dir() -> PathBuf {
    crate::setup::parsec_home().join("interceptor")
}

pub fn addon_path() -> PathBuf {
    interceptor_dir().join("addon.py")
}

fn pid_path() -> PathBuf {
    interceptor_dir().join("mitmdump.pid")
}

fn log_path() -> PathBuf {
    interceptor_dir().join("mitmdump.log")
}

/// mitmproxy generates its CA here on first run, on every platform.
fn ca_cert_path() -> PathBuf {
    crate::setup::home_dir()
        .join(".mitmproxy")
        .join("mitmproxy-ca-cert.pem")
}

fn launchd_plist() -> PathBuf {
    crate::setup::home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

fn systemd_unit_path() -> PathBuf {
    crate::setup::home_dir()
        .join(".config")
        .join("systemd")
        .join("user")
        .join(format!("{SYSTEMD_UNIT}.service"))
}

fn proxy_launchd_plist() -> PathBuf {
    crate::setup::home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{PROXY_LAUNCHD_LABEL}.plist"))
}

fn proxy_systemd_unit_path() -> PathBuf {
    crate::setup::home_dir()
        .join(".config")
        .join("systemd")
        .join("user")
        .join(format!("{PROXY_SYSTEMD_UNIT}.service"))
}

fn state_file() -> PathBuf {
    interceptor_dir().join("state.json")
}

/// Left by `parsec setup desktop --prepare` when the macOS Network Extension
/// still needs the user's toggle. The menu-bar app reads it on launch and
/// starts its guided wait with no click — the installer's way of finishing
/// the one step macOS refuses to script.
pub fn approval_pending_marker() -> PathBuf {
    crate::setup::parsec_home().join("desktop-approval-pending.json")
}

/// What the guided completion must do once the extension is approved.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingApproval {
    /// `--prepare --autostart` was asked for: the completion installs the
    /// boot services as well as starting the interceptor.
    #[serde(default)]
    pub autostart: bool,
}

pub fn read_pending_approval() -> Option<PendingApproval> {
    serde_json::from_str(&std::fs::read_to_string(approval_pending_marker()).ok()?).ok()
}

fn write_pending_approval(p: &PendingApproval) -> std::io::Result<()> {
    std::fs::create_dir_all(crate::setup::parsec_home())?;
    std::fs::write(approval_pending_marker(), serde_json::to_string_pretty(p)?)
}

pub fn clear_pending_approval() {
    let _ = std::fs::remove_file(approval_pending_marker());
}

/// What `parsec setup desktop` decided, so `desktop start` / `status` and
/// `uninstall` behave the same across processes. CC-Router keeps the
/// equivalent pair (`desktopEnabled` / `desktopAutoStart`) in its client
/// config; parsec keeps it beside the addon so a full teardown removes the
/// state with the thing it describes.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct DesktopState {
    /// Provisioned at least once: mitmproxy found, addon written, gates passed.
    #[serde(default)]
    pub enabled: bool,
    /// Boot service installed, so interception survives a reboot.
    #[serde(default)]
    pub autostart: bool,
    /// The proxy URL the addon was rendered against.
    #[serde(default)]
    pub target: String,
}

pub fn load_desktop_state() -> Option<DesktopState> {
    serde_json::from_str(&std::fs::read_to_string(state_file()).ok()?).ok()
}

fn save_desktop_state(st: &DesktopState) -> std::io::Result<()> {
    std::fs::create_dir_all(interceptor_dir())?;
    std::fs::write(state_file(), serde_json::to_string_pretty(st)?)
}

/// Is Claude Desktop even installed? Not a gate — a user may be setting up
/// before installing it — but silence here is the difference between "why is
/// nothing intercepted" and a one-line answer.
pub fn claude_desktop_installed() -> bool {
    if cfg!(target_os = "macos") {
        Path::new("/Applications/Claude.app").exists()
    } else if cfg!(target_os = "windows") {
        windows_desktop_present()
    } else {
        // No canonical Linux install location; assume present rather than
        // warn wrongly.
        true
    }
}

/// Windows install probe. Desktop ships as a Squirrel app: the launcher at the
/// root of `AnthropicClaude\` sits beside versioned `app-<ver>\` payload
/// directories, and a machine-wide install lands somewhere else again — so one
/// hardcoded path answers "not installed" on machines that have it. Presence
/// only: interception matches the process by NAME, so a path we cannot find
/// never blocks capture, which is why this warns and never gates.
///
/// Not `#[cfg(windows)]`: `claude_desktop_installed` branches with `cfg!`, so
/// every arm has to compile everywhere. Off Windows the env vars are absent
/// and it answers false without touching the disk.
fn windows_desktop_present() -> bool {
    // 1. Running right now — the most authoritative answer available, and the
    //    one `Find-ClaudeDesktop` in install.ps1 has always checked first.
    //    Without it the installer could enable desktop on a machine where
    //    `parsec desktop status` then reported NOT FOUND for a Desktop that
    //    was open on screen, which reads as a broken setup and is not.
    if desktop_process_running() {
        return true;
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        roots.push(local.join("AnthropicClaude").join("Claude.exe"));
        roots.push(local.join("Programs").join("Claude").join("Claude.exe"));
        // Some Desktop builds ship under this name. install.ps1 probes it;
        // this did not, which is the other half of the same disagreement.
        roots.push(
            local
                .join("Programs")
                .join("claude-desktop")
                .join("Claude.exe"),
        );
        // MSIX install (the current Desktop installer): the payload lands
        // under Program Files\WindowsApps, whose root denies enumeration —
        // but every registered package gets a per-user state dir at
        // LOCALAPPDATA\Packages\<Name>_<publisher-hash>, and the hash is
        // derived from Anthropic's signing cert, so it is stable across
        // versions. A directory works fine in the exists() sweep below.
        roots.push(local.join("Packages").join("Claude_pzs8sxrjxfjjc"));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(dir) = std::env::var(var) {
            roots.push(PathBuf::from(dir).join("Claude").join("Claude.exe"));
        }
    }
    if roots.iter().any(|p| p.exists()) {
        return true;
    }
    // Squirrel payload: app-1.2.3\claude.exe next to the stub launcher.
    let Ok(local) = std::env::var("LOCALAPPDATA") else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(PathBuf::from(local).join("AnthropicClaude")) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name().to_string_lossy().starts_with("app-") && e.path().join("claude.exe").exists()
    })
}

// ── platform facts ──────────────────────────────────────────────────────────

/// The process mitmproxy's local mode attaches to. Linux truncates comm to
/// 15 chars, but "claude" is well inside that.
pub fn desktop_process_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Claude"
    } else if cfg!(target_os = "windows") {
        "Claude.exe"
    } else {
        "claude"
    }
}

/// Windows only: refuse interception on an ARM64 kernel. mitmproxy's
/// redirector (WinDivert) is a kernel driver with no ARM64 build, and x64
/// emulation does not extend to kernel drivers — mitmdump launches under
/// emulation, fails to load the driver, and dies with a cryptic embedded-
/// Python fatal error. Detection must NOT use the process environment:
/// under x64 emulation the loader rewrites PROCESSOR_ARCHITECTURE to AMD64.
/// The machine-wide registry value keeps the real architecture.
fn gate_windows_arm64() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let arm64 = std::process::Command::new("reg")
            .no_window()
            .args([
                "query",
                r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
                "/v",
                "PROCESSOR_ARCHITECTURE",
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("ARM64"))
            .unwrap_or(false);
        if arm64 {
            anyhow::bail!(
                "Claude Desktop interception cannot work on ARM64 Windows: mitmproxy's \
                 redirector (WinDivert) is a kernel driver with no ARM64 build, and x64 \
                 emulation does not cover kernel drivers.\nEverything else parsec does \
                 works on this machine — only `desktop` is off the table."
            );
        }
    }
    Ok(())
}

/// Absolute path to `mitmdump`, or None when it is not installed. Resolved
/// absolutely because launchd/systemd start with a minimal PATH.
pub fn mitmdump_path() -> Option<PathBuf> {
    let probe = if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    };
    if let Ok(out) = Command::new(probe).no_window().arg("mitmdump").output() {
        if out.status.success() {
            if let Some(first) = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
            {
                return Some(PathBuf::from(first));
            }
        }
    }
    // Not on PATH — or there is no useful PATH. The menu-bar app is started
    // by launchd with the bare system PATH, so from there `which` never
    // finds a Homebrew or pipx mitmdump and the guided completion reported
    // "mitmproxy is not installed" on machines where it plainly was. Probe
    // the conventional install locations directly.
    if cfg!(target_os = "windows") {
        return None;
    }
    let home = crate::setup::home_dir();
    [
        PathBuf::from("/opt/homebrew/bin/mitmdump"),
        PathBuf::from("/usr/local/bin/mitmdump"),
        home.join(".local").join("bin").join("mitmdump"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install mitmproxy"
    } else if cfg!(target_os = "windows") {
        "winget install mitmproxy.mitmproxy   (or: pip install mitmproxy)"
    } else {
        "pip install mitmproxy   (or your distro's mitmproxy package)"
    }
}

/// Approval state of mitmproxy's macOS Network Extension. Only macOS gates
/// interception on user approval; elsewhere this is always `Ready`.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ExtensionStatus {
    /// Approved and active — interception will actually capture traffic.
    Ready,
    /// Installed but not yet toggled on by the user. mitmdump starts fine
    /// and captures NOTHING, which is the confusing failure we pre-empt.
    AwaitingApproval,
    /// mitmdump has never run, or the extension was removed.
    NotInstalled,
    /// `systemextensionsctl` could not be read — proceed and let mitmdump
    /// report its own error rather than blocking on a probe.
    Unknown,
}

/// Parse `systemextensionsctl list` output. Pure — the string shapes are the
/// interesting part, so they are unit-tested rather than probed.
fn parse_extension_status(list_output: &str) -> ExtensionStatus {
    let Some(line) = list_output
        .lines()
        .find(|l| l.to_lowercase().contains("mitmproxy"))
    else {
        return ExtensionStatus::NotInstalled;
    };
    // Newer macOS appends a human-readable state; older builds only have the
    // two flag columns ("enabled" then "active"), where "*" means set.
    if line.contains("[activated enabled]") {
        return ExtensionStatus::Ready;
    }
    if line.to_lowercase().contains("waiting for user") {
        return ExtensionStatus::AwaitingApproval;
    }
    let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
    match (cols.first().copied(), cols.get(1).copied()) {
        (Some("*"), Some("*")) => ExtensionStatus::Ready,
        (Some(""), Some("*")) | (None, Some("*")) => ExtensionStatus::AwaitingApproval,
        _ => ExtensionStatus::NotInstalled,
    }
}

pub fn extension_status() -> ExtensionStatus {
    if !cfg!(target_os = "macos") {
        return ExtensionStatus::Ready;
    }
    match Command::new("systemextensionsctl").arg("list").output() {
        Ok(out) if out.status.success() => {
            parse_extension_status(&String::from_utf8_lossy(&out.stdout))
        }
        _ => ExtensionStatus::Unknown,
    }
}

/// Open the macOS pane where mitmproxy's Network Extension is approved.
/// Best-effort: the Extensions pane is not directly deep-linkable, so this
/// lands on the closest one and the printed steps do the rest.
pub fn open_extension_settings() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let opened = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.LoginItems-Settings.extension")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !opened {
        let _ = Command::new("open")
            .arg("/System/Applications/System Settings.app")
            .status();
    }
}

/// Make macOS register mitmproxy's Network Extension so there is something
/// for the user to approve. The extension is installed the first time
/// mitmdump runs in local mode — until then System Settings has no
/// "Mitmproxy Redirector" row and the approval instructions point at nothing.
/// `gate_extension` used to tell the user to run mitmdump once by hand; this
/// is that step, automated: a throwaway local-mode run, polled until
/// `systemextensionsctl` sees the extension (or ~10 s pass), then killed.
///
/// Returns the status AFTER the attempt. No-op off macOS and when the
/// extension is already registered.
pub fn request_extension_registration(mitmdump: &Path) -> ExtensionStatus {
    let current = extension_status();
    if !cfg!(target_os = "macos") || current != ExtensionStatus::NotInstalled {
        return current;
    }
    let _ = std::fs::create_dir_all(interceptor_dir());
    let sink = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())
            .map(std::process::Stdio::from)
            .unwrap_or_else(|_| std::process::Stdio::null())
    };
    let Ok(mut child) = Command::new(mitmdump)
        .args([
            "--mode",
            &format!("local:{}", interception_spec()),
            "--set",
            "connection_strategy=lazy",
            "--quiet",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(sink())
        .stderr(sink())
        .spawn()
    else {
        return current;
    };
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if extension_status() != ExtensionStatus::NotInstalled {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    extension_status()
}

/// The #1 gotcha, spelled out: without approval `mitmdump` starts happily and
/// captures nothing, which reads as "parsec is broken" rather than "one
/// toggle is off".
pub fn print_extension_instructions() {
    if !cfg!(target_os = "macos") {
        return;
    }
    println!(
        "{}",
        crate::brand::panel(
            "one approval needed",
            &[
                "macOS installs a Network Extension (\"Mitmproxy Redirector\") the first \
                 time mitmproxy runs in local mode, and you must approve it by hand. \
                 Without it the interceptor captures ZERO traffic.",
                "1. System Settings → General → Login Items & Extensions",
                "2. Scroll to Network Extensions, click (i)",
                "3. Toggle \"Mitmproxy Redirector\" ON",
                "~Once per machine. The menu-bar app waits for this step instead of \
                 making you re-run setup: parsec tray install",
            ],
        )
    );
}

/// What is and is not routed. Printed before anything else on setup: users
/// who expect the normal chat sidebar to be curated hit confusion fast, and
/// this heads it off.
pub fn print_scope_explainer() {
    println!(
        "{}",
        crate::brand::panel(
            "claude desktop",
            &[
                "Desktop exposes no ANTHROPIC_BASE_URL, so parsec intercepts only the \
                 traffic it can actually serve.",
                // `^` = verbatim art: these are columns, and the wrapper
                // would otherwise collapse the padding that aligns them.
                "^  routed     /v1/messages   Cowork / Agent mode",
                "^  routed     /v1/messages   Claude Code in Desktop",
                "^  routed     /v1/models     model listing",
                "^  passthru   claude.ai      normal chat",
                "^  passthru   /v1/oauth      login",
                "^  passthru   /v1/environ…   Cowork bridge",
                "~Your credentials are never substituted: the addon sets no auth header \
                 and the proxy forwards yours verbatim, exactly as for the CLI.",
            ],
        )
    );
}

// ── CA certificate ──────────────────────────────────────────────────────────

pub fn ca_cert_present() -> bool {
    ca_cert_path().exists()
}

/// Run mitmdump on a throwaway port just long enough for it to generate
/// ~/.mitmproxy/. It writes the CA on first launch and there is no
/// dedicated "generate" command.
fn generate_ca(mitmdump: &Path) -> anyhow::Result<()> {
    if ca_cert_present() {
        return Ok(());
    }
    let mut child = Command::new(mitmdump)
        .no_window()
        .args(["--mode", "regular", "--set", "listen_port=0", "--quiet"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if ca_cert_present() {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    if ca_cert_present() {
        Ok(())
    } else {
        anyhow::bail!(
            "mitmdump ran but did not create {} — run `mitmdump` once by hand and check its output",
            ca_cert_path().display()
        )
    }
}

/// Whether the CA sitting in `~/.mitmproxy` is the one the system actually
/// trusts. Fingerprints are compared, never names: mitmproxy mints a NEW CA
/// whenever its directory is recreated, and the old one stays in the trust
/// store — so a name match happily reports "mitmproxy is trusted" while every
/// intercepted connection still fails. That is the failure this exists to
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaTrust {
    /// The CA on disk is in the trust store.
    Trusted,
    /// SOME mitmproxy CA is trusted, but not this one — a regenerated CA.
    Stale,
    /// No mitmproxy CA in the trust store.
    NotTrusted,
    /// The store could not be queried. Reported as unknown rather than
    /// guessed: both wrong answers actively mislead.
    Unknown,
}

/// SHA-1 fingerprints (uppercase hex, separators stripped) on lines carrying
/// `marker`. Works across `certutil`, `security` and `openssl` output because
/// all three print the digest after a label on its own line.
fn hex_after_marker(text: &str, marker: &str) -> Vec<String> {
    let m = marker.to_lowercase();
    text.lines()
        .filter_map(|l| {
            // Index and slice the SAME lowercased string: `to_lowercase` can
            // change byte length, so mixing the two would panic mid-char.
            let low = l.to_lowercase();
            let i = low.find(&m)?;
            let hex: String = low[i + m.len()..]
                .chars()
                .filter(char::is_ascii_hexdigit)
                .collect();
            (hex.len() == 40).then(|| hex.to_uppercase())
        })
        .collect()
}

/// SHA-1 of the CA parsec would have Desktop trust.
fn ca_file_sha1() -> Option<String> {
    let ca = ca_cert_path();
    if !ca.exists() {
        return None;
    }
    let (prog, args, marker): (&str, Vec<&str>, &str) = if cfg!(target_os = "windows") {
        ("certutil", vec!["-dump"], "cert hash(sha1)")
    } else {
        (
            "openssl",
            vec!["x509", "-noout", "-fingerprint", "-sha1", "-in"],
            "fingerprint",
        )
    };
    let out = Command::new(prog)
        .no_window()
        .args(args)
        .arg(&ca)
        .output()
        .ok()?;
    hex_after_marker(&String::from_utf8_lossy(&out.stdout), marker)
        .into_iter()
        .next()
}

/// Every mitmproxy CA the platform trust store holds. `Some(vec![])` is a real
/// answer ("none trusted"); `None` means we could not ask.
fn trusted_mitmproxy_sha1s() -> Option<Vec<String>> {
    if cfg!(target_os = "windows") {
        // Exits non-zero on a miss, so the exit code is not consulted — an
        // empty parse IS the "nothing trusted" answer.
        let out = Command::new("certutil")
            .no_window()
            .args(["-store", "root", "mitmproxy"])
            .output()
            .ok()?;
        Some(hex_after_marker(
            &String::from_utf8_lossy(&out.stdout),
            "cert hash(sha1)",
        ))
    } else if cfg!(target_os = "macos") {
        let out = Command::new("security")
            .args([
                "find-certificate",
                "-a",
                "-c",
                "mitmproxy",
                "-Z",
                "/Library/Keychains/System.keychain",
            ])
            .output()
            .ok()?;
        Some(hex_after_marker(
            &String::from_utf8_lossy(&out.stdout),
            "sha-1 hash",
        ))
    } else {
        // No portable query across Linux trust stores — say unknown.
        None
    }
}

/// Trust state of the CA on disk. Never mutates anything.
pub fn ca_trust_state() -> CaTrust {
    let (Some(mine), Some(trusted)) = (ca_file_sha1(), trusted_mitmproxy_sha1s()) else {
        return CaTrust::Unknown;
    };
    classify_trust(&mine, &trusted)
}

/// Split out from the shell-outs so the three-way call is pinned by tests
/// rather than by a live trust store.
fn classify_trust(mine: &str, trusted: &[String]) -> CaTrust {
    if trusted.iter().any(|t| t == mine) {
        CaTrust::Trusted
    } else if trusted.is_empty() {
        CaTrust::NotTrusted
    } else {
        CaTrust::Stale
    }
}

/// The exact command that trusts mitmproxy's CA. Printed, not run, unless
/// the user passed `--install-ca`: a root CA in the system store is a
/// hard-to-reverse, machine-wide change and the user should see it first.
pub fn ca_install_command() -> String {
    let ca = ca_cert_path();
    if cfg!(target_os = "macos") {
        format!(
            "sudo security add-trusted-cert -d -r trustRoot \
             -k /Library/Keychains/System.keychain {}",
            ca.display()
        )
    } else if cfg!(target_os = "windows") {
        format!("certutil -addstore -f root {}", ca.display())
    } else {
        format!(
            "sudo cp {} /usr/local/share/ca-certificates/mitmproxy.crt && \
             sudo update-ca-certificates",
            ca.display()
        )
    }
}

/// The matching removal command, printed by `parsec disable desktop`. We do
/// not run it either — on macOS the cert is matched by SHA-1, which we would
/// have to compute, and a wrong guess would delete someone else's trust root.
pub fn ca_remove_command() -> String {
    if cfg!(target_os = "macos") {
        "sudo security delete-certificate -c mitmproxy -t \
         /Library/Keychains/System.keychain"
            .to_string()
    } else if cfg!(target_os = "windows") {
        "certutil -delstore root mitmproxy".to_string()
    } else {
        "sudo rm /usr/local/share/ca-certificates/mitmproxy.crt && \
         sudo update-ca-certificates --fresh"
            .to_string()
    }
}

fn install_ca_now() -> anyhow::Result<()> {
    let ca = ca_cert_path();
    let status = if cfg!(target_os = "macos") {
        Command::new("sudo")
            .args([
                "security",
                "add-trusted-cert",
                "-d",
                "-r",
                "trustRoot",
                "-k",
                "/Library/Keychains/System.keychain",
            ])
            .arg(&ca)
            .status()?
    } else if cfg!(target_os = "windows") {
        Command::new("certutil")
            .no_window()
            .args(["-addstore", "-f", "root"])
            .arg(&ca)
            .status()?
    } else {
        let dest = "/usr/local/share/ca-certificates/mitmproxy.crt";
        let cp = Command::new("sudo").arg("cp").arg(&ca).arg(dest).status()?;
        if !cp.success() {
            anyhow::bail!("copying the CA to {dest} failed");
        }
        Command::new("sudo")
            .arg("update-ca-certificates")
            .status()?
    };
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("trust-store install exited {status}")
    }
}

// ── managed addon file ──────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum WriteDecision {
    /// No file, or an older managed one → (re)write.
    Write,
    /// Byte-identical managed file already there.
    Current,
    /// A file without our sentinel: the user's, refuse to touch.
    Foreign,
}

fn decide(existing: Option<&str>, rendered: &str) -> WriteDecision {
    match existing {
        None => WriteDecision::Write,
        Some(cur) if cur == rendered => WriteDecision::Current,
        Some(cur) if cur.contains(SENTINEL) => WriteDecision::Write,
        Some(_) => WriteDecision::Foreign,
    }
}

/// Bake the resolved proxy URL into the addon's default so the written file
/// is self-contained: `mitmdump -s addon.py` by hand, a launchd job whose
/// environment got trimmed, or a boot service started before our env is set
/// all behave identically to the supervised spawn. The env var still wins
/// when present. (CC-Router does the same substitution for the same reason.)
fn render_addon(target: &str) -> String {
    ADDON_PY.replace(
        r#"_DEFAULT_TARGET = "http://127.0.0.1:8082""#,
        &format!("_DEFAULT_TARGET = {}", json_string(target)),
    )
}

/// Minimal JSON string literal — the substituted value lands inside Python
/// source, so it must be quoted and escaped rather than interpolated raw.
fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"http://127.0.0.1:8082\"".to_string())
}

/// Drop the addon at ~/.parsec/interceptor/addon.py. Ownership follows the
/// settings.json discipline: a file we wrote may be refreshed, a file
/// without the sentinel is the user's and is never overwritten. Rewritten on
/// every setup/start so a changed port or an upgraded addon is picked up
/// without a fresh install.
fn write_addon(target: &str) -> anyhow::Result<()> {
    let path = addon_path();
    let rendered = render_addon(target);
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    match decide(existing.as_deref(), &rendered) {
        WriteDecision::Foreign => anyhow::bail!(
            "{} exists and is not parsec-managed — move it aside and re-run",
            path.display()
        ),
        WriteDecision::Current => Ok(()),
        WriteDecision::Write => {
            std::fs::create_dir_all(interceptor_dir())?;
            let tmp = path.with_extension("py.parsec-tmp");
            std::fs::write(&tmp, &rendered)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        }
    }
}

// ── Claude Desktop MCP registration ─────────────────────────────────────────

/// Marks an `mcpServers` entry as parsec-managed. MCP server configs have no
/// comment field, but they DO carry `env` — so the sentinel rides there,
/// visible to the user and durable across Desktop rewriting the file.
const MCP_KEY: &str = "parsec";
const MCP_SENTINEL: &str = "PARSEC_MANAGED";

/// Claude Desktop's config. Desktop loads MCP servers and nothing else — it
/// has no plugin system — so this is the only surface through which parsec's
/// tools can reach a Cowork session.
pub fn desktop_config_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| crate::setup::home_dir().join("AppData").join("Roaming"))
            .join("Claude")
            .join("claude_desktop_config.json")
    } else {
        crate::setup::home_dir()
            .join("Library")
            .join("Application Support")
            .join("Claude")
            .join("claude_desktop_config.json")
    }
}

#[derive(Debug, PartialEq)]
pub enum McpOutcome {
    /// Written (fresh, or refreshed because the binary path moved).
    Added,
    /// Already ours and already correct.
    Current,
    /// An `mcpServers.parsec` we did not write — the user's, left alone.
    Foreign,
}

/// The server entry. `command` is the ABSOLUTE alias path, never a bare
/// `parsec`: Claude Desktop is a GUI app launched by the window server and
/// inherits no shell PATH, so a bare name resolves to nothing.
fn mcp_entry(command: &str) -> serde_json::Value {
    serde_json::json!({
        "command": command,
        "args": ["mcp"],
        "env": { MCP_SENTINEL: "1" },
    })
}

/// Additive merge, pure for tests. The user's own keys are never touched and
/// a foreign `parsec` entry is reported rather than clobbered — the same
/// ownership discipline as `setup::merge_settings` and the opencode shim.
///
/// `serde_json`'s `preserve_order` (workspace-wide) is what keeps the rest of
/// the user's config in its original key order through the round-trip.
fn merge_mcp(
    mut root: serde_json::Value,
    command: &str,
) -> anyhow::Result<(serde_json::Value, McpOutcome)> {
    if root.is_null() {
        root = serde_json::json!({});
    }
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("claude_desktop_config.json root is not a JSON object"))?;
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`mcpServers` is not a JSON object"))?;
    let wanted = mcp_entry(command);
    let outcome = match servers.get(MCP_KEY) {
        None => {
            servers.insert(MCP_KEY.into(), wanted);
            McpOutcome::Added
        }
        Some(cur) if cur == &wanted => McpOutcome::Current,
        // Ours, but stale — the alias path moved. Refresh it.
        Some(cur) if cur.pointer(&format!("/env/{MCP_SENTINEL}")).is_some() => {
            servers.insert(MCP_KEY.into(), wanted);
            McpOutcome::Added
        }
        Some(_) => McpOutcome::Foreign,
    };
    Ok((root, outcome))
}

/// Remove exactly what we wrote. A foreign entry survives untouched.
fn remove_mcp(mut root: serde_json::Value) -> (serde_json::Value, bool) {
    let removed = root
        .get_mut("mcpServers")
        .and_then(|s| s.as_object_mut())
        .is_some_and(|servers| {
            let ours = servers
                .get(MCP_KEY)
                .and_then(|e| e.pointer(&format!("/env/{MCP_SENTINEL}")))
                .is_some();
            ours && servers.remove(MCP_KEY).is_some()
        });
    (root, removed)
}

fn read_desktop_config() -> anyhow::Result<serde_json::Value> {
    match std::fs::read_to_string(desktop_config_path()) {
        Ok(s) if s.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", desktop_config_path().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e.into()),
    }
}

fn write_desktop_config(root: &serde_json::Value) -> anyhow::Result<()> {
    let path = desktop_config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // One-time backup before we ever modify a file holding the user's own
    // Desktop preferences. Never overwritten, so it always holds the
    // pre-parsec state rather than the last thing we wrote.
    let backup = path.with_extension("json.parsec-backup");
    if path.exists() && !backup.exists() {
        let _ = std::fs::copy(&path, &backup);
    }
    let tmp = path.with_extension("json.parsec-tmp");
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(root)?))?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Register `parsec mcp` with Claude Desktop so Cowork gets the scout tools.
pub fn register_mcp() -> anyhow::Result<McpOutcome> {
    let command = crate::setup_opencode::bin_alias_path();
    let (root, outcome) = merge_mcp(read_desktop_config()?, &command.display().to_string())?;
    if outcome != McpOutcome::Current {
        write_desktop_config(&root)?;
    }
    Ok(outcome)
}

pub fn unregister_mcp() -> bool {
    let Ok(root) = read_desktop_config() else {
        return false;
    };
    let (root, removed) = remove_mcp(root);
    removed && write_desktop_config(&root).is_ok()
}

// ── interceptor process lifecycle ───────────────────────────────────────────

/// The port Claude Code is routed at — the interceptor aims Desktop at the
/// same supervisor, so both surfaces share one proxy, one ledger, one
/// savings number.
pub fn target_url() -> String {
    let port = crate::setup::load_state()
        .map(|st| st.port)
        .filter(|p| *p > 0)
        .unwrap_or_else(crate::setup::default_port);
    format!("http://127.0.0.1:{port}")
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// True when our PID file names a live `mitmdump`. Identity is checked, not
/// just existence: PIDs are reused, and killing an unrelated process because
/// a stale pidfile said so is exactly the bug this avoids.
pub fn running() -> bool {
    read_pid().is_some_and(pid_is_mitmdump)
}

fn pid_is_mitmdump(pid: u32) -> bool {
    if cfg!(target_os = "windows") {
        let Ok(out) = Command::new("tasklist")
            .no_window()
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
        else {
            return false;
        };
        let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
        s.contains("mitmdump")
    } else {
        let Ok(out) = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&out.stdout).contains("mitmdump")
    }
}

/// Is a Claude Desktop process live right now? Presence probe only, used
/// where the INSTALL PATH is the unreliable part — Desktop is a Squirrel app
/// with per-user, machine-wide and versioned-payload layouts, so the process
/// table answers a question the filesystem keeps getting wrong.
fn desktop_process_running() -> bool {
    if !cfg!(target_os = "windows") {
        return false;
    }
    let Ok(out) = Command::new("tasklist")
        .no_window()
        .args([
            "/FI",
            &format!("IMAGENAME eq {}", desktop_process_name()),
            "/NH",
        ])
        .output()
    else {
        return false;
    };
    // tasklist prints an INFO line ("No tasks are running which match…") on a
    // miss, so match the image name rather than trusting non-empty output.
    String::from_utf8_lossy(&out.stdout)
        .to_lowercase()
        .contains(&desktop_process_name().to_lowercase())
}

/// The `local:` intercept spec handed to mitmdump. Distinct from
/// `desktop_process_name()` (display + tasklist, where matching is
/// case-insensitive) because mitmproxy_rs matches this spec as a
/// CASE-SENSITIVE substring of the process's full image path
/// (`intercept_conf.rs::Pattern::matches` — `n.contains(name)`, no
/// normalization on any platform). On Windows the running binary is
/// lowercase `claude.exe` in every known layout — Squirrel
/// `app-X.Y.Z\claude.exe` and MSIX `…\app\claude.exe` — so the old spec
/// `local:Claude.exe` matched NOTHING, ever: Windows interception was
/// silently dead while macOS (whose binary really is `Claude`) worked.
/// Both casings are listed anyway; the comma is mitmproxy's own
/// multi-pattern separator.
fn interception_spec() -> &'static str {
    if cfg!(target_os = "macos") {
        "Claude"
    } else if cfg!(target_os = "windows") {
        "claude.exe,Claude.exe"
    } else {
        "claude"
    }
}

fn mitmdump_args(addon: &Path) -> Vec<String> {
    vec![
        "--mode".into(),
        format!("local:{}", interception_spec()),
        "-s".into(),
        addon.display().to_string(),
        "--set".into(),
        "connection_strategy=lazy".into(),
        // NOT --quiet. It was, and that made a crashed interceptor look
        // exactly like an idle one: mitmdump kept a live PID while its event
        // loop was dead, `running()` reported "running", and every Claude
        // Desktop request hung with nothing written anywhere. Flow lines and
        // handler errors go to interceptor/mitmdump.log, which is the only
        // place either failure is visible.
    ]
}

/// Spawn mitmdump detached, owning its own process group / console so it
/// outlives the session that started it — same discipline as
/// `setup::spawn_proxy_detached`.
fn spawn_interceptor(mitmdump: &Path, target: &str) -> anyhow::Result<u32> {
    std::fs::create_dir_all(interceptor_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())?;
    let mut cmd = Command::new(mitmdump);
    cmd.args(mitmdump_args(&addon_path()))
        .env("PARSEC_DESKTOP_TARGET", target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    #[cfg(unix)]
    {
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    }
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        std::os::windows::process::CommandExt::creation_flags(
            &mut cmd,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
        );
    }
    let child = cmd.spawn()?;
    let pid = child.id();
    std::fs::write(pid_path(), pid.to_string())?;
    Ok(pid)
}

/// PIDs of live `mitmdump` processes running OUR addon (unix only; Windows
/// has no `pgrep`, and its service fires at the next login anyway). The
/// addon path is the identity: it is unique to this install, so a user's own
/// unrelated mitmdump is never mistaken for the interceptor.
fn our_mitmdump_pids() -> Vec<u32> {
    if cfg!(target_os = "windows") {
        return Vec::new();
    }
    let Ok(out) = Command::new("pgrep")
        .args(["-f", &addon_path().display().to_string()])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter(|pid| pid_is_mitmdump(*pid))
        .collect()
}

/// After `install_service` on macOS / Linux the service manager has already
/// started an interceptor — one that never wrote our pidfile. Before this
/// existed, `running()` looked at the empty pidfile, said "no", and
/// `provision` hand-spawned a SECOND mitmdump: both then fought for the same
/// local-mode hook, the loser died with "failed to establish connection to
/// macOS system extension … deadline has elapsed", and `status` reported
/// only the pidfile'd one as healthy. Wait briefly for the service's process
/// and adopt its pid so every later `running()` / `stop()` sees it.
fn adopt_service_interceptor() -> bool {
    if cfg!(target_os = "windows") {
        return false;
    }
    for _ in 0..30 {
        if let Some(pid) = our_mitmdump_pids().into_iter().next() {
            return std::fs::write(pid_path(), pid.to_string()).is_ok();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// What mitmdump's own log says about the local-mode hook. A live pid is a
/// weak claim: the reporter of the macOS 26 saga watched `status` say
/// "running" for an hour while the redirector had never attached. mitmproxy
/// prints exactly one line when the hook is up and a recognisable one when
/// it is not, and the log is the only place either appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// "Local redirector started." was the last thing the redirector said.
    Started,
    /// A startup failure was the last thing it said; the payload is the
    /// marker that matched.
    Failed(&'static str),
    /// Neither marker seen (yet).
    Unknown,
}

const HEALTH_OK: &str = "Local redirector started";
const HEALTH_FAILURES: [&str; 4] = [
    "failed to establish connection",
    "deadline has elapsed",
    "Error logged during startup",
    "Address already in use",
];

/// Latest marker wins. Pure, so the string shapes are pinned by tests.
fn health_in(text: &str) -> Health {
    let ok = text.rfind(HEALTH_OK);
    let failed = HEALTH_FAILURES
        .iter()
        .filter_map(|m| text.rfind(m).map(|i| (i, *m)))
        .max_by_key(|(i, _)| *i);
    match (ok, failed) {
        (Some(o), Some((f, m))) if f > o => Health::Failed(m),
        (Some(_), _) => Health::Started,
        (None, Some((_, m))) => Health::Failed(m),
        (None, None) => Health::Unknown,
    }
}

/// Current length of the interceptor log — a bookmark taken BEFORE a start,
/// so an old "started" line from a previous run cannot vouch for this one.
fn log_len() -> u64 {
    std::fs::metadata(log_path()).map(|m| m.len()).unwrap_or(0)
}

/// Health from everything logged after `since`.
fn health_since(since: u64) -> Health {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(log_path()) else {
        return Health::Unknown;
    };
    if f.seek(SeekFrom::Start(since)).is_err() {
        return Health::Unknown;
    }
    let mut buf = String::new();
    // Bounded: a runaway log must not be read whole on every status call.
    if f.take(1 << 20).read_to_string(&mut buf).is_err() {
        return Health::Unknown;
    }
    health_in(&buf)
}

/// Health of the current interceptor, from the tail of the log. `status`
/// has no start bookmark, so the last 64 KiB stand in for "recent".
pub fn interceptor_health() -> Health {
    let len = log_len();
    health_since(len.saturating_sub(64 * 1024))
}

/// Give a just-started interceptor time to declare itself. Ten seconds:
/// mitmdump loads Python and the addon before the redirector attaches, and
/// a live run here took more than five to print its first line.
fn wait_for_health(since: u64) -> Health {
    for _ in 0..100 {
        let h = health_since(since);
        if h != Health::Unknown {
            return h;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Health::Unknown
}

/// One line for the start paths and `status`.
fn health_line(h: Health) -> String {
    match h {
        Health::Started => "redirector up (per mitmdump.log)".to_string(),
        Health::Failed(m) => format!(
            "redirector FAILED (\"{m}\" in {}) — capture is NOT working",
            log_path().display()
        ),
        Health::Unknown => format!(
            "redirector not confirmed yet — watch {} for \"{HEALTH_OK}\"",
            log_path().display()
        ),
    }
}

/// What `stop()` achieved. The three-way distinction is load-bearing on
/// Windows: the interceptor runs elevated (WinDivert needs its driver), so
/// `taskkill` from an unelevated shell is DENIED. A stop that swallowed that
/// failure and deleted the pidfile anyway orphaned a live mitmdump nothing
/// could ever reap — `running()` went blind, `status` reported "stopped", and
/// the orphan kept the local-mode hook so every later interceptor captured
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Nothing of ours was running: no pidfile, or it named a dead or
    /// recycled PID.
    NotRunning,
    /// Signalled, and confirmed gone from the process table.
    Stopped,
    /// Still alive after the kill. The pidfile is KEPT, so a retry (or
    /// `status`) can still find it.
    Failed(u32),
}

/// Stop the interceptor we started. Never kills a PID that is not mitmdump,
/// and never reports a success it did not verify.
pub fn stop() -> StopOutcome {
    let Some(pid) = read_pid() else {
        return StopOutcome::NotRunning;
    };
    if !pid_is_mitmdump(pid) {
        // Stale pidfile: the process is gone, or the PID has been recycled by
        // something unrelated. Clearing it is safe precisely because we refuse
        // to kill a PID we could not identify as ours.
        let _ = std::fs::remove_file(pid_path());
        return StopOutcome::NotRunning;
    }
    if cfg!(target_os = "windows") {
        // /T kills the process TREE. mitmdump on Windows runs as a
        // parent→child pair (a pip/uv launcher exe spawning the real worker
        // with an identical command line); killing only the pidfile PID
        // leaves the child alive and still holding the WinDivert hook — the
        // orphaned interceptor that makes every later one capture nothing.
        let _ = Command::new("taskkill")
            .no_window()
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    } else {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    // The process table is the authority here, not the killer's exit code:
    // `taskkill` can report success for a process that outlives it, and
    // SIGTERM is asynchronous either way. Poll until it is really gone.
    if wait_for(|| !pid_is_mitmdump(pid)) {
        let _ = std::fs::remove_file(pid_path());
        StopOutcome::Stopped
    } else {
        StopOutcome::Failed(pid)
    }
}

/// Stop the service manager's interceptor for THIS session while leaving the
/// login registration in place. Needed because the macOS service runs under
/// `KeepAlive`: a plain SIGTERM to its pid is answered by launchd with a
/// respawn seconds later, so `stop --keep-autostart` used to print
/// "stopped" over an interceptor that was already coming back. `bootout`
/// removes the job from the running session only — the plist stays on disk
/// and `RunAtLoad` brings it back at the next login, which is the promise
/// `--keep-autostart` makes. Linux mirrors it with `systemctl stop` (the
/// unit stays enabled). Windows' task fires only at login, so a kill there
/// is already final. Returns whether a service was told to stop.
fn stop_service_now() -> bool {
    if !service_installed() {
        return false;
    }
    if cfg!(target_os = "macos") {
        launchctl_unload()
    } else if cfg!(target_os = "windows") {
        false
    } else {
        Command::new("systemctl")
            .args(["--user", "stop", SYSTEMD_UNIT])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// `stop()` after the service manager has already been told to stand down.
/// The manager kills the process itself, so `stop()` then finds a stale
/// pidfile and says `NotRunning` — which would read as "nothing happened"
/// to someone who just watched it stop. Fold that back into `Stopped`.
fn stop_after_service(was_running: bool) -> StopOutcome {
    match stop() {
        StopOutcome::NotRunning if was_running => StopOutcome::Stopped,
        other => other,
    }
}

/// What to tell someone whose stop was refused. Windows is the only platform
/// where this is routine rather than pathological.
pub fn stop_failure_hint(pid: u32) -> String {
    if cfg!(target_os = "windows") {
        format!(
            "could not stop the interceptor (pid {pid}): it runs elevated for WinDivert, so \
             stopping it needs an administrator PowerShell:\n\n  taskkill /PID {pid} /T /F\n\n\
             The pidfile is kept, so parsec has not lost track of it."
        )
    } else {
        format!(
            "could not stop the interceptor (pid {pid}): it did not exit after SIGTERM. \
             Force it with:\n\n  kill -9 {pid}"
        )
    }
}

// ── boot service (opt-in: --autostart) ──────────────────────────────────────

pub fn service_installed() -> bool {
    if cfg!(target_os = "macos") {
        launchd_plist().exists()
    } else if cfg!(target_os = "windows") {
        win_task_registered() || win_legacy_run_value_present()
    } else {
        systemd_unit_path().exists()
    }
}

/// Windows: the scheduled task (the autostart that really runs elevated).
fn win_task_registered() -> bool {
    Command::new("schtasks")
        .no_window()
        .args(win_schtasks_query_args())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Windows: the pre-installer Run-key value, which fires unelevated and so
/// never survived a reboot in practice. Present ⇒ an old install to migrate.
fn win_legacy_run_value_present() -> bool {
    Command::new("reg")
        .no_window()
        .args(["query", WIN_RUN_KEY, "/v", WIN_RUN_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// One line for `status`: which autostart mechanism is in place, and whether
/// it will actually work at the next sign-in.
fn autostart_status_line() -> &'static str {
    if cfg!(target_os = "windows") {
        if win_task_registered() {
            "enabled (scheduled task, highest privileges, at sign-in)"
        } else if win_legacy_run_value_present() {
            "enabled via a legacy Run key — it fires UNELEVATED at sign-in, so WinDivert \
             cannot load and nothing is intercepted after a reboot. Migrate from an \
             administrator PowerShell: parsec setup desktop --autostart"
        } else {
            "disabled"
        }
    } else if service_installed() {
        "enabled (starts on login)"
    } else {
        "disabled"
    }
}

fn plist_xml(mitmdump: &Path, target: &str) -> String {
    let args: String = std::iter::once(mitmdump.display().to_string())
        .chain(mitmdump_args(&addon_path()))
        .map(|a| format!("        <string>{}</string>\n", xml_escape(&a)))
        .collect();
    let path_env = std::env::var("PATH")
        .unwrap_or_else(|_| "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin".into());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path_env}</string>
        <key>PARSEC_DESKTOP_TARGET</key>
        <string>{target}</string>
    </dict>
</dict>
</plist>
"#,
        args = args,
        log = xml_escape(&log_path().display().to_string()),
        path_env = xml_escape(&path_env),
        target = xml_escape(target),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Quote an argument that would otherwise be split by a whitespace-splitting
/// launcher (systemd `ExecStart`, a `.cmd` line). Only whitespace matters
/// here: every value we pass is a path or a literal flag we control.
fn quote_if_spaced(arg: &str) -> String {
    if arg.contains(char::is_whitespace) {
        format!("\"{arg}\"")
    } else {
        arg.to_string()
    }
}

fn systemd_unit_text(mitmdump: &Path, target: &str) -> String {
    // ExecStart is one line, so anything path-shaped gets quoted: systemd
    // splits on whitespace and a home directory with a space in it would
    // otherwise turn the addon path into two arguments.
    let exec = std::iter::once(mitmdump.display().to_string())
        .chain(mitmdump_args(&addon_path()))
        .map(|a| quote_if_spaced(&a))
        .collect::<Vec<_>>()
        .join(" ");
    let path_env = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    format!(
        "[Unit]\n\
         Description=parsec interceptor — routes Claude Desktop inference through the local parsec proxy\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         Environment=PATH={path_env}\n\
         Environment=PARSEC_DESKTOP_TARGET={target}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn win_launcher_path() -> PathBuf {
    interceptor_dir().join("start-interceptor.cmd")
}

/// The scheduled task launches this, not the `.cmd` directly: a task that
/// starts `cmd.exe` flashes a console at every sign-in, while `wscript` with
/// window style 0 runs it hidden — the same trick `tray/windows.rs` uses for
/// the tray's own Run entry.
fn win_launcher_vbs_path() -> PathBuf {
    interceptor_dir().join("start-interceptor.vbs")
}

/// VBScript that runs the `.cmd` launcher with no window. VBScript escapes a
/// quote inside a string by doubling it, so the quoted path becomes `""…""`.
fn win_launcher_vbs(cmd: &Path) -> String {
    let quoted = format!("\"{}\"", cmd.display()).replace('"', "\"\"");
    format!("CreateObject(\"WScript.Shell\").Run \"{quoted}\", 0, False\r\n")
}

/// `schtasks /Create …` for the interceptor: at sign-in, highest privileges
/// (so WinDivert's driver loads), replacing any existing task of that name.
fn win_schtasks_create_args(vbs: &Path) -> Vec<String> {
    vec![
        "/Create".into(),
        "/TN".into(),
        WIN_TASK_NAME.into(),
        "/SC".into(),
        "ONLOGON".into(),
        "/RL".into(),
        "HIGHEST".into(),
        "/TR".into(),
        format!("wscript.exe \"{}\"", vbs.display()),
        "/F".into(),
    ]
}

fn win_schtasks_delete_args() -> Vec<String> {
    vec![
        "/Delete".into(),
        "/TN".into(),
        WIN_TASK_NAME.into(),
        "/F".into(),
    ]
}

fn win_schtasks_query_args() -> Vec<String> {
    vec!["/Query".into(), "/TN".into(), WIN_TASK_NAME.into()]
}

/// The Run-key entry points here. Every path is quoted once, by us, in a file
/// — which is the whole reason this exists instead of an inline command line.
fn win_launcher_script(mitmdump: &Path, target: &str) -> String {
    let args = mitmdump_args(&addon_path())
        .into_iter()
        .map(|a| quote_if_spaced(&a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "@echo off\r\nset PARSEC_DESKTOP_TARGET={target}\r\nstart \"\" /b \"{}\" {args}\r\n",
        mitmdump.display()
    )
}

fn install_service(mitmdump: &Path, target: &str) -> anyhow::Result<()> {
    // The OS service takes over process ownership — a hand-spawned mitmdump
    // would fight it for the same local-mode hook, so a stop we could not
    // verify has to fail here rather than install a service that captures
    // nothing.
    if let StopOutcome::Failed(pid) = stop() {
        anyhow::bail!("{}", stop_failure_hint(pid));
    }
    if cfg!(target_os = "macos") {
        let plist = launchd_plist();
        std::fs::create_dir_all(plist.parent().unwrap())?;
        let _ = launchctl_unload();
        std::fs::write(&plist, plist_xml(mitmdump, target))?;
        let uid = unsafe_uid();
        let ok = Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{uid}")])
            .arg(&plist)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            || Command::new("launchctl")
                .arg("load")
                .arg(&plist)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        if !ok {
            anyhow::bail!(
                "wrote {} but launchctl would not load it — load by hand: launchctl load {}",
                plist.display(),
                plist.display()
            );
        }
        Ok(())
    } else if cfg!(target_os = "windows") {
        // A generated .cmd rather than an inline command line: every path is
        // quoted once, by us, in a file, so a space in the home directory
        // cannot split the mitmdump arguments. The .vbs runs it windowless.
        let launcher = win_launcher_path();
        std::fs::create_dir_all(interceptor_dir())?;
        std::fs::write(&launcher, win_launcher_script(mitmdump, target))?;
        let vbs = win_launcher_vbs_path();
        std::fs::write(&vbs, win_launcher_vbs(&launcher))?;
        // A Scheduled Task, not a Run-key value: the Run key fires unelevated
        // and WinDivert's driver then fails to load, which is why the old
        // autostart silently did nothing after a reboot. `/RL HIGHEST` needs
        // administrator to register — the installer's elevated step provides
        // it, and so does an elevated `parsec setup desktop --autostart`.
        let ok = Command::new("schtasks")
            .no_window()
            .args(win_schtasks_create_args(&vbs))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            anyhow::bail!(
                "registering the {WIN_TASK_NAME} scheduled task needs administrator \
                 (WinDivert must start elevated at sign-in) — re-run from an administrator \
                 PowerShell: parsec setup desktop --autostart"
            );
        }
        // Migrate: the legacy Run value would start a second, unelevated
        // mitmdump at sign-in that fights the task's for the local-mode hook.
        let _ = Command::new("reg")
            .no_window()
            .args(["delete", WIN_RUN_KEY, "/v", WIN_RUN_NAME, "/f"])
            .output();
        Ok(())
    } else {
        let unit = systemd_unit_path();
        std::fs::create_dir_all(unit.parent().unwrap())?;
        std::fs::write(&unit, systemd_unit_text(mitmdump, target))?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        let ok = Command::new("systemctl")
            .args(["--user", "enable", "--now", SYSTEMD_UNIT])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            anyhow::bail!(
                "wrote {} but systemctl would not enable it — run: systemctl --user enable --now {SYSTEMD_UNIT}",
                unit.display()
            )
        }
    }
}

fn unsafe_uid() -> u32 {
    // getuid() has no std wrapper and pulling in `libc` for one call is not
    // worth it; `id -u` is on every POSIX box and this path is macOS-only.
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(501)
}

/// Unregister, quietly. `launchctl bootout` prints "Boot-out failed: 3: No
/// such process" and `unload` prints "Unload failed: 5: Input/output error"
/// whenever nothing is registered — the NORMAL case on a first install — so
/// both are muted; a real failure surfaces through the caller's own message.
fn launchctl_unload() -> bool {
    let uid = unsafe_uid();
    Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        || Command::new("launchctl")
            .arg("unload")
            .arg(launchd_plist())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

fn uninstall_service() -> bool {
    if !service_installed() {
        return false;
    }
    if cfg!(target_os = "macos") {
        let _ = launchctl_unload();
        let _ = std::fs::remove_file(launchd_plist());
    } else if cfg!(target_os = "windows") {
        let _ = Command::new("schtasks")
            .no_window()
            .args(win_schtasks_delete_args())
            .output();
        let _ = Command::new("reg")
            .no_window()
            .args(["delete", WIN_RUN_KEY, "/v", WIN_RUN_NAME, "/f"])
            .output();
        let _ = std::fs::remove_file(win_launcher_path());
        let _ = std::fs::remove_file(win_launcher_vbs_path());
        if win_task_registered() {
            // Deleting a highest-privilege task is itself privileged. Say so
            // rather than report a removal that did not happen.
            println!(
                "the {WIN_TASK_NAME} scheduled task needs administrator to remove — from an \
                 administrator PowerShell: schtasks /Delete /TN {WIN_TASK_NAME} /F"
            );
        }
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", SYSTEMD_UNIT])
            .status();
        let _ = std::fs::remove_file(systemd_unit_path());
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
    }
    true
}

// ── proxy boot service (installed alongside the interceptor's) ──────────────
//
// docs/routing-and-liveness.md §4.3 option (2), scoped to the one client that
// needs it: every other routed client revives a dead proxy itself (Claude
// Code's SessionStart hook, the Codex hook, the opencode shim), but Desktop
// has no hook surface — so when its interception survives a reboot, the proxy
// must too.

pub fn proxy_service_installed() -> bool {
    if cfg!(target_os = "macos") {
        proxy_launchd_plist().exists()
    } else if cfg!(target_os = "windows") {
        Command::new("reg")
            .no_window()
            .args(["query", WIN_RUN_KEY, "/v", WIN_PROXY_RUN_NAME])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        proxy_systemd_unit_path().exists()
    }
}

/// The binary the service launches. The ALIAS, never `current_exe()`: the
/// plugin-cache path this process runs from changes on every update, and a
/// boot service must survive that (`tray.rs` learned this the hard way). The
/// alias is refreshed first so a fresh install has one; the running image is
/// only a fallback for layouts where no alias can exist.
fn proxy_service_binary() -> PathBuf {
    let _ = crate::setup_opencode::refresh_bin_alias();
    let alias = crate::setup_opencode::bin_alias_path();
    if alias.exists() {
        return alias;
    }
    std::env::current_exe().unwrap_or(alias)
}

/// `RunAtLoad` + `KeepAlive/SuccessfulExit=false`, the same policy as the
/// interceptor's plist, and it fits the supervisor exactly: a clean exit(0)
/// stays down (the idempotent "another parsec already owns the port" no-op,
/// and `/shutdown` from `parsec up --restart` / upgrades — launchd must not
/// resurrect what those flows are replacing), while a crash restarts.
fn proxy_plist_xml(binary: &Path, port: u16) -> String {
    let log = crate::setup::parsec_home().join("proxy.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{PROXY_LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>proxy</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PARSEC_PROXY_PORT</key>
        <string>{port}</string>
    </dict>
</dict>
</plist>
"#,
        bin = xml_escape(&binary.display().to_string()),
        log = xml_escape(&log.display().to_string()),
    )
}

fn proxy_systemd_unit_text(binary: &Path, port: u16) -> String {
    format!(
        "[Unit]\n\
         Description=parsec proxy — the supervisor that owns the routed loopback port\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec} proxy\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         Environment=PARSEC_PROXY_PORT={port}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec = quote_if_spaced(&binary.display().to_string()),
    )
}

fn win_proxy_launcher_path() -> PathBuf {
    interceptor_dir().join("start-proxy.cmd")
}

fn win_proxy_launcher_script(binary: &Path, port: u16) -> String {
    format!(
        "@echo off\r\nset PARSEC_PROXY_PORT={port}\r\nstart \"\" /b \"{}\" proxy\r\n",
        binary.display()
    )
}

fn install_proxy_service(port: u16) -> anyhow::Result<()> {
    let binary = proxy_service_binary();
    if cfg!(target_os = "macos") {
        let plist = proxy_launchd_plist();
        std::fs::create_dir_all(plist.parent().unwrap())?;
        let _ = proxy_launchctl_unload();
        std::fs::write(&plist, proxy_plist_xml(&binary, port))?;
        let uid = unsafe_uid();
        // RunAtLoad fires on bootstrap; if a proxy already owns the port the
        // new instance exits 0 and SuccessfulExit=false leaves it down.
        let ok = Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{uid}")])
            .arg(&plist)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            || Command::new("launchctl")
                .arg("load")
                .arg(&plist)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        if !ok {
            anyhow::bail!(
                "wrote {} but launchctl would not load it — load by hand: launchctl load {}",
                plist.display(),
                plist.display()
            );
        }
        Ok(())
    } else if cfg!(target_os = "windows") {
        let launcher = win_proxy_launcher_path();
        std::fs::create_dir_all(interceptor_dir())?;
        std::fs::write(&launcher, win_proxy_launcher_script(&binary, port))?;
        // The Run key fires at the NEXT login; the caller's ensure_proxy
        // already has one listening for this session.
        let ok = Command::new("reg")
            .no_window()
            .args([
                "add",
                WIN_RUN_KEY,
                "/v",
                WIN_PROXY_RUN_NAME,
                "/t",
                "REG_SZ",
                "/d",
            ])
            .arg(format!("\"{}\"", launcher.display()))
            .arg("/f")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            anyhow::bail!("could not write the {WIN_PROXY_RUN_NAME} Run-key entry")
        }
    } else {
        let unit = proxy_systemd_unit_path();
        std::fs::create_dir_all(unit.parent().unwrap())?;
        std::fs::write(&unit, proxy_systemd_unit_text(&binary, port))?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        // `--now` is safe for the same reason bootstrap is on macOS: a
        // second supervisor on an owned port is a clean exit-0 no-op.
        let ok = Command::new("systemctl")
            .args(["--user", "enable", "--now", PROXY_SYSTEMD_UNIT])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            anyhow::bail!(
                "wrote {} but systemctl would not enable it — run: systemctl --user enable --now {PROXY_SYSTEMD_UNIT}",
                unit.display()
            )
        }
    }
}

/// Same muting as `launchctl_unload`, for the same reason.
fn proxy_launchctl_unload() -> bool {
    let uid = unsafe_uid();
    Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{PROXY_LAUNCHD_LABEL}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        || Command::new("launchctl")
            .arg("unload")
            .arg(proxy_launchd_plist())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

/// Remove the registration WITHOUT killing a running proxy: a Claude Code
/// session may be riding it right now, and stopping interception is not a
/// reason to break the CLI. macOS/Linux keep the job loaded until logout
/// (crash-restart until then is harmless-to-helpful); the deleted plist/unit
/// is what stops it returning. `/shutdown`-based stops exit 0, which both
/// service managers already leave down.
fn uninstall_proxy_service() -> bool {
    if !proxy_service_installed() {
        return false;
    }
    if cfg!(target_os = "macos") {
        let _ = std::fs::remove_file(proxy_launchd_plist());
    } else if cfg!(target_os = "windows") {
        let _ = Command::new("reg")
            .no_window()
            .args(["delete", WIN_RUN_KEY, "/v", WIN_PROXY_RUN_NAME, "/f"])
            .status();
        let _ = std::fs::remove_file(win_proxy_launcher_path());
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", PROXY_SYSTEMD_UNIT])
            .status();
        let _ = std::fs::remove_file(proxy_systemd_unit_path());
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
    }
    true
}

// ── commands ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Trust mitmproxy's CA without a further prompt (runs sudo/certutil).
    pub install_ca: bool,
    /// Also install the boot service so Desktop stays routed across reboots.
    pub autostart: bool,
    /// Report readiness and exit; change nothing.
    pub status_only: bool,
    /// Installer mode: do everything up to the platform approval gate (CA
    /// generated, addon, MCP, proxy warm, extension registered, System
    /// Settings opened), leave a marker for the menu-bar app to finish, and
    /// exit 0 with a machine-readable report. Never trusts the CA — the
    /// installer does that itself with the privilege it already holds.
    pub prepare: bool,
}

/// Ensure the destination is answering before we point Desktop at it —
/// Claude Code's routing has `parsec up` for revival, Desktop's has nothing.
///
/// `restart` is the provisioning path (`parsec setup desktop`). It exists
/// because of a real failure mode: a proxy that predates the current binary
/// keeps serving the OLD image, and the addon is rendered against the route
/// table of the NEW one — `/v1/models` is redirected to a proxy that 404s it,
/// and Desktop's model listing breaks. Restarting on provision keeps the
/// addon and the proxy in step. `parsec desktop start` does NOT restart: it
/// is the routine path and must not drop a Claude Code session's in-flight
/// requests.
///
/// Deliberately NOT `setup::up()`: that resolves the port from Claude Code's
/// settings, which can differ from — or be absent, as after `parsec disable`
/// — the port this addon was rendered against. Restarting the wrong port
/// would leave Desktop aimed at a dead one. Same safety rule though: only a
/// process that identifies itself as a parsec proxy is ever shut down.
/// The port a rendered target URL aims at — shared by `ensure_proxy` and the
/// proxy boot service so they cannot disagree about which port to keep alive.
fn proxy_target_port(target: &str) -> Option<u16> {
    target.rsplit(':').next().and_then(|p| p.parse().ok())
}

fn ensure_proxy(target: &str, restart: bool) {
    let Some(port) = proxy_target_port(target) else {
        return;
    };
    if crate::hook::port_listening(port) {
        if !restart {
            println!("proxy listening on 127.0.0.1:{port}");
            return;
        }
        if !crate::setup::shutdown_parsec_on(port) {
            println!(
                "127.0.0.1:{port} is listening but is not a parsec proxy — not killing a \
                 foreign process. Free the port and re-run, or Desktop will be aimed at it."
            );
            return;
        }
        if !wait_for(|| !crate::hook::port_listening(port)) {
            println!(
                "old proxy on 127.0.0.1:{port} acknowledged shutdown but never released the \
                 port — check ~/.parsec/proxy.log"
            );
            return;
        }
        println!("old proxy on 127.0.0.1:{port} stopped — starting the current binary");
    }
    if let Err(e) = crate::setup::spawn_proxy_detached(port, &[]) {
        println!("proxy start failed ({e}) — run `parsec up` before using Desktop");
        return;
    }
    if wait_for(|| crate::hook::port_listening(port)) {
        println!("proxy up on 127.0.0.1:{port}");
        warn_if_proxy_predates_models_route(port);
    } else {
        println!(
            "proxy spawned for 127.0.0.1:{port} but never started listening — check \
             ~/.parsec/proxy.log"
        );
    }
}

/// True when a raw HTTP response says the route is absent. Split out so the
/// status-line shape is pinned by a test rather than by a live proxy: 404 is
/// "this build has no /v1/models"; anything else (including a 401/403 from
/// upstream) means the route exists and is doing its job.
fn models_route_missing(raw_response: &str) -> bool {
    raw_response
        .lines()
        .next()
        .is_some_and(|status| status.starts_with("HTTP/1.1 404"))
}

/// Poll a condition for ~2s. Same cadence as `setup::up`.
fn wait_for(cond: impl Fn() -> bool) -> bool {
    for _ in 0..40 {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// The addon redirects `/v1/models`; a proxy built before that route existed
/// 404s it, and Desktop shows a broken model picker. A restart fixes the
/// stale-PROCESS case — this catches the stale-BINARY case a restart cannot,
/// and says so rather than reporting a clean setup over a broken one. A
/// warning, not a failure: `/v1/messages` still works, which is the bulk of
/// the value (fail open, but measured).
fn warn_if_proxy_predates_models_route(port: u16) {
    let Some(resp) = crate::setup::proxy_request(port, "GET", "/v1/models") else {
        return;
    };
    if !models_route_missing(&resp) {
        return;
    }
    println!(
        "\nWARNING: the proxy on 127.0.0.1:{port} does not serve /v1/models — its binary \
         predates that route.\n  The addon redirects /v1/models, so Claude Desktop's model \
         listing will 404. Inference (/v1/messages) still works.\n  Fix: install the current \
         parsec build, then `parsec up --restart` and `parsec desktop restart`."
    );
}

/// Refuse to start on macOS when the Network Extension is not usable:
/// mitmdump would come up clean and capture nothing.
/// What to do when the extension sits in "activated waiting for user" but
/// System Settings shows no "Mitmproxy Redirector" row to approve. macOS only
/// lists a pending Network Extension while the app that requested it is
/// alive; the installer's registration run is deliberately short-lived, and
/// if the menu-bar app that was meant to keep the request alive is dead,
/// there is nothing to toggle. `systemextensionsctl uninstall` is no way out
/// either — SIP refuses it. Trashing the redirector app from Finder is the
/// one path that makes macOS drop the stale registration.
pub fn approval_recovery_hint() -> String {
    format!(
        "If System Settings shows NO \"Mitmproxy Redirector\" row, the approval request has \
         no live owner. Run mitmproxy by hand and LEAVE IT RUNNING while you approve:\n\n  \
         mitmdump --mode local:{} --set connection_strategy=lazy\n\nIf the row still does not \
         appear, drag \"/Applications/Mitmproxy Redirector.app\" to the Trash in Finder (this \
         is what makes macOS forget the stuck extension; `systemextensionsctl uninstall` is \
         blocked by SIP), run the command above again, approve the new request, then re-run.",
        desktop_process_name()
    )
}

fn gate_extension() -> anyhow::Result<()> {
    match extension_status() {
        ExtensionStatus::AwaitingApproval => {
            print_extension_instructions();
            open_extension_settings();
            anyhow::bail!(
                "mitmproxy's Network Extension is installed but not approved — approve it \
                 (System Settings was opened for you), then re-run.\n\n{}",
                approval_recovery_hint()
            );
        }
        ExtensionStatus::NotInstalled if cfg!(target_os = "macos") => {
            print_extension_instructions();
            anyhow::bail!(
                "mitmproxy's Network Extension is not installed yet. Run it once by hand so \
                 macOS offers the approval prompt:\n\n  mitmdump --mode local:{} --set \
                 connection_strategy=lazy\n\nApprove it, then re-run",
                desktop_process_name()
            );
        }
        _ => Ok(()),
    }
}

/// `parsec desktop start [--autostart]` — start intercepting with what setup
/// already provisioned. The counterpart of CC-Router's `client start-desktop`:
/// it re-checks the gates and refreshes the addon (the routed port may have
/// moved since setup) but never re-does the CA or the trust prompt.
pub fn start(autostart: bool) -> anyhow::Result<()> {
    run_start(autostart, false)
}

fn run_start(autostart: bool, restart_proxy: bool) -> anyhow::Result<()> {
    gate_windows_arm64()?;
    let Some(mitmdump) = mitmdump_path() else {
        anyhow::bail!("mitmproxy is not installed — {}", install_hint());
    };
    let target = target_url();
    write_addon(&target)?;
    gate_extension()?;
    provision(&mitmdump, &target, autostart, restart_proxy)
}

/// Everything past the approval gate: warm the proxy, install the boot
/// services if asked, and get an interceptor running. Split from `run_start`
/// so `prepare` can stop exactly at the gate and this half can be run later
/// (by the tray's guided completion) without re-deciding anything.
fn provision(
    mitmdump: &Path,
    target: &str,
    autostart: bool,
    restart_proxy: bool,
) -> anyhow::Result<()> {
    let target = target.to_string();
    ensure_proxy(&target, restart_proxy);

    let mut st = load_desktop_state().unwrap_or_default();
    st.enabled = true;
    st.target = target.clone();

    let log_mark = log_len();
    if autostart {
        install_service(mitmdump, &target)?;
        st.autostart = true;
        // The proxy gets its own login service whenever the interceptor
        // does — an interceptor that survives a reboot while the proxy does
        // not would redirect every Desktop request into a dead port (the
        // "sessions hang after restart" bug). Failure is a warning, not an
        // abort: the addon fails open on a dead target, so a missing service
        // costs curation-until-revived, never availability.
        if let Some(port) = proxy_target_port(&target) {
            match install_proxy_service(port) {
                Ok(()) => println!(
                    "proxy installed as a boot service too (it must outlive reboots for the \
                     interceptor to have somewhere to send traffic)"
                ),
                Err(e) => println!(
                    "could not install the proxy's boot service ({e}) — after a reboot, Desktop \
                     traffic passes through UNCURATED until something runs `parsec up`"
                ),
            }
        }
        // Starting the boot service is NOT the same as intercepting now.
        // macOS `launchctl load` and Linux `systemctl --now` start the
        // interceptor as part of install_service, but the Windows Run key
        // only fires at the NEXT login — so on Windows `setup desktop
        // --autostart` returned having registered autostart while nothing
        // was actually intercepting, which is why the installer left Windows
        // users needing a manual `parsec desktop start` that macOS never
        // did. If the service manager did not already bring it up, start it
        // now in this (elevated) session.
        if running() || adopt_service_interceptor() {
            println!(
                "interceptor installed as a boot service and is running (pid {})",
                read_pid().unwrap_or(0)
            );
            // A service the manager keeps alive is reported, not aborted:
            // it retries on its own, and the state above is still right.
            println!("  {}", health_line(wait_for_health(log_mark)));
        } else {
            let pid = spawn_interceptor(mitmdump, &target)?;
            std::thread::sleep(std::time::Duration::from_millis(1500));
            if !running() {
                anyhow::bail!(
                    "installed the boot service but the interceptor (pid {pid}) exited \
                     immediately — see {}",
                    log_path().display()
                );
            }
            println!(
                "interceptor installed as a boot service and started now (pid {pid}), \
                 redirecting {}'s /v1/messages + /v1/models → {target}",
                desktop_process_name()
            );
            println!("  {}", health_line(wait_for_health(log_mark)));
        }
    } else {
        // No `running()` guard: stop() reports NotRunning on its own, and
        // starting a second mitmdump while the first still holds the
        // local-mode hook is exactly the silent-capture-nothing failure.
        if let StopOutcome::Failed(pid) = stop() {
            anyhow::bail!("{}", stop_failure_hint(pid));
        }
        let pid = spawn_interceptor(mitmdump, &target)?;
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if !running() {
            anyhow::bail!(
                "mitmdump started (pid {pid}) but exited immediately — see {}",
                log_path().display()
            );
        }
        println!(
            "interceptor running (pid {pid}), redirecting {}'s /v1/messages + /v1/models → {target}",
            desktop_process_name()
        );
        let health = wait_for_health(log_mark);
        println!("  {}", health_line(health));
        if let Health::Failed(_) = health {
            // The one clear error line. State is saved first so `status`
            // still shows Desktop as configured; only the start failed.
            let _ = save_desktop_state(&st);
            anyhow::bail!(
                "the interceptor is alive but its redirector did not attach — nothing is \
                 being captured. Usual causes: another mitmdump holding the local-mode hook \
                 (`parsec desktop status` lists strays), or the Network Extension needs \
                 re-approval. See {}",
                log_path().display()
            );
        }
    }
    let _ = save_desktop_state(&st);
    print_relaunch_reminder();
    Ok(())
}

/// `parsec desktop stop [--keep-autostart]`. Without the flag this also
/// removes the boot service — otherwise "stopped" would silently un-stop
/// itself at the next login, which is CC-Router's reason for the same flag.
pub fn stop_cmd(keep_autostart: bool) -> anyhow::Result<()> {
    // Service first, process second: the other order lets a KeepAlive
    // service respawn the interceptor between the kill and the teardown.
    let was_running = running();
    let mut st = load_desktop_state().unwrap_or_default();
    if !keep_autostart {
        if uninstall_service() {
            println!("boot service removed");
        }
        // The proxy's login registration goes with it — but the RUNNING
        // proxy is left alone: Claude Code sessions may be routed through
        // it, and stopping Desktop interception is no reason to break them.
        if uninstall_proxy_service() {
            println!("proxy boot service removed (a running proxy is left running)");
        }
        st.autostart = false;
    } else if st.autostart {
        stop_service_now();
        println!("boot service kept — the interceptor returns at next login");
    }
    let outcome = stop_after_service(was_running);
    let _ = save_desktop_state(&st);
    match outcome {
        StopOutcome::Stopped => println!("interceptor stopped"),
        StopOutcome::NotRunning => println!("interceptor was not running"),
        // Not an error exit: the boot-service and state changes above already
        // happened and are worth keeping. But it must not read as success.
        StopOutcome::Failed(pid) => {
            println!("interceptor STILL RUNNING — {}", stop_failure_hint(pid))
        }
    }
    println!("Quit and relaunch Claude Desktop to drop any intercepted connections.");
    Ok(())
}

/// `parsec desktop restart` — pick up a moved proxy port or an upgraded
/// addon without re-running setup.
pub fn restart() -> anyhow::Result<()> {
    let autostart = load_desktop_state().map(|s| s.autostart).unwrap_or(false);
    let was_running = running();
    stop_service_now(); // otherwise KeepAlive respawns what stop() just killed
    if let StopOutcome::Failed(pid) = stop_after_service(was_running) {
        anyhow::bail!("{}", stop_failure_hint(pid));
    }
    // Strays the pidfile never knew about (the pre-adoption double spawn)
    // would otherwise keep the hook and make the new interceptor the loser.
    for pid in our_mitmdump_pids() {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    let _ = wait_for(|| our_mitmdump_pids().is_empty());
    start(autostart)
}

/// " from an administrator PowerShell" where that is what the user will need.
fn elevation_note() -> &'static str {
    if cfg!(target_os = "windows") {
        " from an administrator PowerShell"
    } else {
        ""
    }
}

/// Bounce the interceptor after the proxy underneath it was replaced. Part of
/// every update path rather than a separate chore, for two reasons: the addon
/// is re-rendered against the live port, and the OLD mitmdump keeps the
/// process-local hook — plus, on Windows, a WinDivert driver handle — until it
/// is reaped. A fresh proxy behind a stale interceptor captures nothing and
/// says nothing, which is the worst outcome this file has.
///
/// Never fails the caller: an update must not break because interception could
/// not be bounced. It reports instead.
pub fn restart_after_update() {
    // install.ps1 sets this for its unelevated `up --restart`: on Windows the
    // interceptor runs elevated, so a bounce from that shell can only fail —
    // and the elevated `parsec setup desktop` a few lines later does it
    // properly. Skipping beats warning about something already being fixed.
    if std::env::var("PARSEC_SKIP_DESKTOP_BOUNCE").ok().as_deref() == Some("1") {
        return;
    }
    let Some(st) = load_desktop_state() else {
        return;
    };
    if !st.enabled {
        return;
    }
    // Configured but deliberately stopped stays stopped: an update is not a
    // reason to start intercepting on someone's behalf.
    if !running() && !st.autostart {
        return;
    }
    println!("\nrestarting the Claude Desktop interceptor against the new build…");
    if let Err(e) = restart() {
        println!(
            "could not restart the Claude Desktop interceptor: {e}\n  Desktop stays on the \
             PREVIOUS interception until you run `parsec desktop restart`{}",
            elevation_note()
        );
    }
}

/// One line for an update path that must NOT print — the hook writes a single
/// JSON object to stdout, so anything else there corrupts it. None when no
/// live interception is at stake.
pub fn stale_interceptor_notice() -> Option<String> {
    let st = load_desktop_state()?;
    if !st.enabled || !running() {
        return None;
    }
    Some(format!(
        "the Claude Desktop interceptor is still the pre-update process — run \
         `parsec desktop restart`{} and relaunch Desktop",
        elevation_note()
    ))
}

fn print_relaunch_reminder() {
    println!(
        "\nNext: quit Claude Desktop completely (Cmd-Q, not just close the window) and reopen \
         it — mitmproxy hooks the process at launch, so a Desktop that was already running \
         stays unintercepted.\nThen use Cowork / Agent mode; the normal chat sidebar is not \
         routed (by design).\n\n  status:  parsec desktop status\n  stop:    parsec desktop \
         stop\n  remove:  parsec disable desktop"
    );
}

/// `parsec setup desktop [--install-ca] [--autostart] [--status]`.
/// Idempotent: repeat runs converge, and each precondition is reported
/// rather than assumed, because a silently-started mitmdump that captures
/// nothing is the worst outcome here.
pub fn setup(opts: Options) -> anyhow::Result<()> {
    if opts.status_only {
        return status();
    }
    if opts.prepare {
        return prepare(opts);
    }
    gate_windows_arm64()?;

    print_scope_explainer();
    if !claude_desktop_installed() {
        println!(
            "note: Claude Desktop does not appear to be installed on this machine — setup will \
             still complete, but nothing will be intercepted until it is.\n"
        );
    }

    let Some(mitmdump) = mitmdump_path() else {
        anyhow::bail!(
            "mitmproxy is not installed — Claude Desktop has no endpoint setting, so \
             interception is the only way to reach it.\n  Install: {}\n  Then re-run: \
             parsec setup desktop",
            install_hint()
        );
    };
    println!("mitmproxy found at {}", mitmdump.display());

    // 1. CA — generate, then trust (explicitly).
    if !ca_cert_present() {
        println!("generating mitmproxy's CA (one-time)…");
        generate_ca(&mitmdump)?;
    }
    println!("CA certificate at {}", ca_cert_path().display());
    if opts.install_ca {
        println!(
            "installing the CA into the system trust store (you will be asked to authenticate)…"
        );
        install_ca_now()?;
        println!("CA trusted.");
    } else {
        // Ask the store before saying anything. This used to print "NOT
        // trusted yet" unconditionally, contradicting `parsec desktop status`
        // on the same machine and pushing users to add a root CA a second
        // time — the one machine-wide change parsec makes, made twice.
        match ca_trust_state() {
            CaTrust::Trusted => println!("CA already trusted (system trust store)"),
            CaTrust::Stale => println!(
                "\nSTALE trust — a DIFFERENT mitmproxy CA is in the trust store, not this one \
                 (the CA was regenerated). Remove the old one, then trust this one:\n\n  {}\n  {}",
                ca_remove_command(),
                ca_install_command()
            ),
            CaTrust::NotTrusted => println!(
                "\nNOT trusted yet — this is the one machine-wide change parsec makes, so it \
                 is yours to run:\n\n  {}\n\nOr re-run with `parsec setup desktop --install-ca` \
                 to have parsec run it for you.\nWithout it, Claude Desktop will reject the \
                 intercepted TLS connection.",
                ca_install_command()
            ),
            CaTrust::Unknown => println!(
                "\ncould not query the trust store — if Claude Desktop rejects the intercepted \
                 connection, trust the CA with:\n\n  {}",
                ca_install_command()
            ),
        }
    }

    // 2–5: provision the addon, clear the platform gates, warm the proxy and
    //      run it. Shared with `parsec desktop start` so the two entry points
    //      cannot drift.
    println!("\naddon → {}", addon_path().display());

    // Interception routes Desktop's TRAFFIC; this gives a Cowork session
    // parsec's TOOLS. Separate mechanisms, both part of "set up Desktop".
    match register_mcp() {
        Ok(McpOutcome::Added) => println!(
            "registered parsec's tools with Claude Desktop ({})",
            desktop_config_path().display()
        ),
        Ok(McpOutcome::Current) => {
            println!("parsec's tools already registered with Claude Desktop")
        }
        Ok(McpOutcome::Foreign) => println!(
            "an `mcpServers.parsec` entry already exists in {} and is not ours — left alone",
            desktop_config_path().display()
        ),
        Err(e) => println!("could not register parsec's tools with Claude Desktop ({e})"),
    }
    // Provisioning restarts the proxy so the addon and the route table it was
    // rendered against belong to the same build.
    run_start(opts.autostart, true)
}

// ── installer mode: `parsec setup desktop --prepare` ────────────────────────

/// Prefix of the one machine-readable line `prepare` prints last. The
/// installer's postinstall greps for it, so both halves are pinned by tests.
pub const PREPARE_REPORT_PREFIX: &str = "PARSEC_DESKTOP_PREPARE ";

/// What `prepare` found and did. Field names are a contract with
/// `packages/installer/macos/scripts/desktop/postinstall`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct PrepareReport {
    /// Absolute mitmdump path, or null when it is not installed (in which
    /// case nothing else below was attempted).
    mitmproxy: Option<String>,
    ca_present: bool,
    ca_trust: &'static str,
    extension: &'static str,
    /// The approval marker was written: the menu-bar app has a job to do.
    marker: bool,
    autostart: bool,
}

fn render_report(r: &PrepareReport) -> String {
    format!(
        "{PREPARE_REPORT_PREFIX}{}",
        serde_json::to_string(r).unwrap_or_else(|_| "{}".into())
    )
}

fn extension_label(s: ExtensionStatus) -> &'static str {
    match s {
        ExtensionStatus::Ready => "ready",
        ExtensionStatus::AwaitingApproval => "awaiting_approval",
        ExtensionStatus::NotInstalled => "not_installed",
        ExtensionStatus::Unknown => "unknown",
    }
}

fn trust_label(t: CaTrust) -> &'static str {
    match t {
        CaTrust::Trusted => "trusted",
        CaTrust::Stale => "stale",
        CaTrust::NotTrusted => "untrusted",
        CaTrust::Unknown => "unknown",
    }
}

/// Installer mode. Everything `setup` does up to the approval gate, with
/// three deliberate differences: it never runs `sudo` (the installer trusts
/// the CA with the privilege it already holds), it never bails on the
/// extension (exit 0, and a marker tells the menu-bar app to wait for the
/// toggle), and it never starts the interceptor or its boot services — an
/// unapproved extension under a `KeepAlive` LaunchAgent would be a restart
/// loop capturing nothing. The tray's guided completion runs `provision`
/// once the toggle flips.
fn prepare(opts: Options) -> anyhow::Result<()> {
    gate_windows_arm64()?;
    let Some(mitmdump) = mitmdump_path() else {
        println!(
            "{}",
            render_report(&PrepareReport {
                mitmproxy: None,
                ca_present: ca_cert_present(),
                ca_trust: trust_label(ca_trust_state()),
                extension: extension_label(extension_status()),
                marker: false,
                autostart: opts.autostart,
            })
        );
        return Ok(());
    };
    println!("mitmproxy found at {}", mitmdump.display());

    if !ca_cert_present() {
        println!("generating mitmproxy's CA (one-time)…");
        // Not fatal: the report says `ca_present:false` and the installer's
        // summary tells the user what to run. Everything else still lands.
        if let Err(e) = generate_ca(&mitmdump) {
            println!("could not generate the CA ({e})");
        }
    }

    let target = target_url();
    write_addon(&target)?;
    println!("addon → {}", addon_path().display());
    match register_mcp() {
        Ok(McpOutcome::Added) => println!("registered parsec's tools with Claude Desktop"),
        Ok(McpOutcome::Current) => println!("parsec's tools already registered"),
        Ok(McpOutcome::Foreign) => {
            println!("a foreign `mcpServers.parsec` entry exists — left alone")
        }
        Err(e) => println!("could not register parsec's tools with Claude Desktop ({e})"),
    }
    ensure_proxy(&target, true);
    let mut st = load_desktop_state().unwrap_or_default();
    st.enabled = true;
    st.target = target;
    let _ = save_desktop_state(&st);

    let ext = request_extension_registration(&mitmdump);
    let marker = cfg!(target_os = "macos") && ext != ExtensionStatus::Ready;
    if marker {
        write_pending_approval(&PendingApproval {
            autostart: opts.autostart,
        })?;
        print_extension_instructions();
        open_extension_settings();
    }
    println!(
        "{}",
        render_report(&PrepareReport {
            mitmproxy: Some(mitmdump.display().to_string()),
            ca_present: ca_cert_present(),
            ca_trust: trust_label(ca_trust_state()),
            extension: extension_label(ext),
            marker,
            autostart: opts.autostart,
        })
    );
    Ok(())
}

/// Readiness report — every precondition, none of them changed. Mirrors the
/// DESKTOP INTERCEPTOR block in `cc-router client status`: configured vs
/// running vs auto-start, the macOS approval state called out loudly because
/// it is the one that fails silently, and the routed scope spelled out.
pub fn status() -> anyhow::Result<()> {
    let st = load_desktop_state().unwrap_or_default();
    let mitm = mitmdump_path();

    println!("DESKTOP INTERCEPTOR  (Cowork / Agent mode)");
    println!(
        "  configured:        {}",
        if st.enabled {
            "yes"
        } else {
            "no — enable with: parsec setup desktop"
        }
    );
    println!(
        "  interceptor:       {}",
        if running() {
            format!("running (pid {})", read_pid().unwrap_or(0))
        } else if st.enabled {
            "stopped — start with: parsec desktop start".to_string()
        } else {
            "not running".to_string()
        }
    );
    // Two interceptors compete for one local-mode hook and the loser
    // captures nothing; the pidfile only ever names one of them.
    let extra: Vec<u32> = our_mitmdump_pids()
        .into_iter()
        .filter(|p| Some(*p) != read_pid())
        .collect();
    if !extra.is_empty() {
        println!(
            "  WARNING:           {} other interceptor process(es) running our addon (pid {}) — \
             they fight for the same hook; run `parsec desktop restart` to converge on one",
            extra.len(),
            extra
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if running() {
        println!("  redirector:        {}", health_line(interceptor_health()));
    }
    println!("  auto-start:        {}", autostart_status_line());
    println!(
        "  proxy auto-start:  {}",
        if proxy_service_installed() {
            "enabled (the proxy starts on login too, so a rebooted machine has \
             somewhere to send intercepted traffic)"
        } else if service_installed() {
            "MISSING — after a reboot the interceptor runs with no proxy behind it \
             (traffic passes through uncurated); re-run: parsec desktop start --autostart"
        } else {
            "disabled (revived on demand by the Claude Code hook / the addon)"
        }
    );
    println!(
        "  Claude Desktop:    {}",
        if claude_desktop_installed() {
            "installed"
        } else {
            "NOT FOUND — nothing to intercept"
        }
    );
    println!(
        "  mitmproxy:         {}",
        mitm.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("NOT INSTALLED ({})", install_hint()))
    );
    println!(
        "  CA certificate:    {}",
        if ca_cert_present() {
            ca_cert_path().display().to_string()
        } else {
            "not generated".to_string()
        }
    );
    if ca_cert_present() {
        // This used to print `trust with:` unconditionally, which read as a
        // trust indicator while saying nothing about the store at all — the
        // single most misleading line in this report.
        match ca_trust_state() {
            CaTrust::Trusted => println!("    trust store:     TRUSTED"),
            CaTrust::NotTrusted => println!(
                "    trust store:     NOT TRUSTED — Claude Desktop will reject the \
                 intercepted connection\n    trust with:      {}",
                ca_install_command()
            ),
            CaTrust::Stale => println!(
                "    trust store:     STALE — a DIFFERENT mitmproxy CA is trusted, not this \
                 one (the CA was regenerated)\n    remove it:       {}\n    then trust:      {}",
                ca_remove_command(),
                ca_install_command()
            ),
            CaTrust::Unknown => println!(
                "    trust store:     unknown (could not query it)\n    trust with:      {}",
                ca_install_command()
            ),
        }
    }
    if cfg!(target_os = "macos") {
        let line = match extension_status() {
            ExtensionStatus::Ready => "approved".to_string(),
            ExtensionStatus::AwaitingApproval => format!(
                "NOT APPROVED — the interceptor will capture NOTHING until you turn on \
                 \"Mitmproxy Redirector\" in System Settings → General → Login Items & \
                 Extensions\n    {}",
                approval_recovery_hint().replace('\n', "\n    ")
            ),
            ExtensionStatus::NotInstalled => {
                "not installed — installed on the first interceptor start".to_string()
            }
            ExtensionStatus::Unknown => "unknown (systemextensionsctl unreadable)".to_string(),
        };
        println!("  network extension: {line}");
    }
    println!(
        "  addon:             {}",
        if addon_path().exists() {
            addon_path().display().to_string()
        } else {
            "not written".to_string()
        }
    );
    println!("  target:            {}", target_url());
    println!("  routed scope:      /v1/messages, /v1/models  (normal chat NOT routed)");
    Ok(())
}

/// `parsec disable desktop` — stop intercepting. Reverses everything parsec
/// did except the trust-store entry, which it never made silently and will
/// not remove silently either.
pub fn disable() -> anyhow::Result<()> {
    let was_running = running();
    let had_service = uninstall_service(); // before the kill: see stop_cmd
    let outcome = stop_after_service(was_running);
    let had_proxy_service = uninstall_proxy_service();
    let addon_removed = remove_addon_if_managed();
    let mcp_removed = unregister_mcp();
    let _ = std::fs::remove_file(state_file());

    match outcome {
        StopOutcome::Stopped => println!("interceptor: stopped"),
        StopOutcome::NotRunning => println!("interceptor: was not running"),
        StopOutcome::Failed(pid) => {
            println!("interceptor: STILL RUNNING — {}", stop_failure_hint(pid))
        }
    }
    println!(
        "boot service: {}",
        if had_service { "removed" } else { "none" }
    );
    println!(
        "proxy boot service: {}",
        if had_proxy_service {
            "removed (a running proxy is left running — Claude Code may be routed through it)"
        } else {
            "none"
        }
    );
    println!(
        "addon: {}",
        if addon_removed {
            "removed"
        } else {
            "none of ours to remove"
        }
    );
    println!(
        "Claude Desktop tools: {}",
        if mcp_removed {
            "unregistered"
        } else {
            "none of ours to remove"
        }
    );
    println!(
        "\nClaude Desktop goes straight to api.anthropic.com again after you quit and relaunch it.\n\
         \nIf you trusted mitmproxy's CA, it is still trusted: parsec never installed it silently \
         and will not remove it silently. To remove it:\n\n  {}\n",
        ca_remove_command()
    );
    Ok(())
}

fn remove_addon_if_managed() -> bool {
    let path = addon_path();
    match std::fs::read_to_string(&path) {
        Ok(cur) if cur.contains(SENTINEL) => std::fs::remove_file(&path).is_ok(),
        _ => false,
    }
}

/// Called from `parsec uninstall`: a full cleanup must not leave an
/// interceptor aiming Desktop at a port nothing will answer again. Quiet
/// and best-effort — uninstall keeps going regardless.
pub fn remove_if_managed() {
    let outcome = stop();
    if let StopOutcome::Failed(pid) = outcome {
        println!("{}", stop_failure_hint(pid));
    }
    let stopped = matches!(outcome, StopOutcome::Stopped);
    let service = uninstall_service();
    let _ = uninstall_proxy_service();
    let addon = remove_addon_if_managed();
    let mcp = unregister_mcp();
    let _ = std::fs::remove_file(state_file());
    if stopped || service || addon || mcp {
        println!(
            "stopped the Claude Desktop interceptor. If you trusted mitmproxy's CA for it, \
             remove that separately: {}",
            ca_remove_command()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_takes_the_latest_marker() {
        assert_eq!(health_in(""), Health::Unknown);
        assert_eq!(health_in("Local redirector started.\n"), Health::Started);
        // The reporter's log: started, then the extension timed out.
        let contention = "Local redirector started.\n\
             failed to establish connection to macOS system extension\n\
             Caused by:\n    deadline has elapsed\n";
        assert_eq!(
            health_in(contention),
            Health::Failed("deadline has elapsed")
        );
        // …and a restart after the failure is healthy again.
        let recovered = format!("{contention}[03:08:27] Local redirector started.\n");
        assert_eq!(health_in(&recovered), Health::Started);
        assert_eq!(
            health_in("Error logged during startup, exiting...\n"),
            Health::Failed("Error logged during startup")
        );
    }

    /// The installer's postinstall greps this line by prefix and field name;
    /// a rename here silently breaks the macOS install flow.
    #[test]
    fn prepare_report_json_field_names_are_stable() {
        let r = PrepareReport {
            mitmproxy: Some("/opt/homebrew/bin/mitmdump".into()),
            ca_present: true,
            ca_trust: "untrusted",
            extension: "awaiting_approval",
            marker: true,
            autostart: true,
        };
        assert_eq!(
            render_report(&r),
            "PARSEC_DESKTOP_PREPARE {\"mitmproxy\":\"/opt/homebrew/bin/mitmdump\",\
             \"ca_present\":true,\"ca_trust\":\"untrusted\",\"extension\":\"awaiting_approval\",\
             \"marker\":true,\"autostart\":true}"
        );
        let none = PrepareReport {
            mitmproxy: None,
            ..r
        };
        assert!(render_report(&none).contains("\"mitmproxy\":null"));
    }

    #[test]
    fn extension_and_trust_labels_are_snake_case() {
        for s in [
            ExtensionStatus::Ready,
            ExtensionStatus::AwaitingApproval,
            ExtensionStatus::NotInstalled,
            ExtensionStatus::Unknown,
        ] {
            let l = extension_label(s);
            assert!(l.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{l}");
        }
        for t in [
            CaTrust::Trusted,
            CaTrust::Stale,
            CaTrust::NotTrusted,
            CaTrust::Unknown,
        ] {
            let l = trust_label(t);
            assert!(l.chars().all(|c| c.is_ascii_lowercase()), "{l}");
        }
        assert_eq!(extension_label(ExtensionStatus::Ready), "ready");
        assert_eq!(trust_label(CaTrust::NotTrusted), "untrusted");
    }

    #[test]
    fn pending_marker_roundtrips() {
        let p = PendingApproval { autostart: true };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<PendingApproval>(&s).unwrap(), p);
        // A marker from an older build with no fields still parses.
        assert_eq!(
            serde_json::from_str::<PendingApproval>("{}").unwrap(),
            PendingApproval { autostart: false }
        );
    }

    #[test]
    fn win_task_command_lines_survive_spaces_in_paths() {
        let vbs = Path::new(r"C:\Users\First Last\.parsec\interceptor\start-interceptor.vbs");
        let args = win_schtasks_create_args(vbs);
        let joined = args.join(" ");
        assert!(joined.contains("/SC ONLOGON"), "{joined}");
        assert!(joined.contains("/RL HIGHEST"), "{joined}");
        assert!(joined.ends_with("/F"), "{joined}");
        let tr = &args[args.iter().position(|a| a == "/TR").unwrap() + 1];
        assert_eq!(
            tr,
            r#"wscript.exe "C:\Users\First Last\.parsec\interceptor\start-interceptor.vbs""#
        );
        assert_eq!(
            win_schtasks_delete_args(),
            vec!["/Delete", "/TN", "ParsecInterceptor", "/F"]
        );
        assert_eq!(
            win_schtasks_query_args(),
            vec!["/Query", "/TN", "ParsecInterceptor"]
        );
    }

    #[test]
    fn win_launcher_vbs_escapes_quotes_and_hides_the_window() {
        let vbs = win_launcher_vbs(Path::new(
            r"C:\Users\a b\.parsec\interceptor\start-interceptor.cmd",
        ));
        assert_eq!(
            vbs,
            "CreateObject(\"WScript.Shell\").Run \"\"\"C:\\Users\\a b\\.parsec\\interceptor\\\
             start-interceptor.cmd\"\"\", 0, False\r\n"
        );
    }

    #[test]
    fn task_name_is_not_the_proxy_run_name() {
        // Both are per-user autostart entries; a shared name would make one
        // overwrite the other's registration.
        assert_ne!(WIN_TASK_NAME, WIN_PROXY_RUN_NAME);
    }

    /// The three trust-store output shapes we parse. Real samples: `certutil`
    /// (Windows), `security -Z` (macOS), `openssl -fingerprint` (the CA file
    /// off Windows). Colon-separated and bare hex both have to survive.
    #[test]
    fn ca_fingerprints_parse_from_every_platform_tool() {
        let certutil = "Serial Number: 1c1a980d\n\
             Issuer: O=mitmproxy, CN=mitmproxy\n\
             Cert Hash(sha1): 9cb6093e452105dcc2e49f3ff7ce22a9058d6e0b\n\
             No key provider information";
        assert_eq!(
            hex_after_marker(certutil, "cert hash(sha1)"),
            vec!["9CB6093E452105DCC2E49F3FF7CE22A9058D6E0B"]
        );

        let security = "SHA-1 hash: 9CB6093E452105DCC2E49F3FF7CE22A9058D6E0B\n\
             keychain: \"/Library/Keychains/System.keychain\"";
        assert_eq!(
            hex_after_marker(security, "sha-1 hash"),
            vec!["9CB6093E452105DCC2E49F3FF7CE22A9058D6E0B"]
        );

        // openssl separates every byte with a colon — stripped, not split on.
        let openssl =
            "SHA1 Fingerprint=9C:B6:09:3E:45:21:05:DC:C2:E4:9F:3F:F7:CE:22:A9:05:8D:6E:0B";
        assert_eq!(
            hex_after_marker(openssl, "fingerprint"),
            vec!["9CB6093E452105DCC2E49F3FF7CE22A9058D6E0B"]
        );
    }

    /// A miss must parse as "none trusted", never as a fingerprint: certutil
    /// prints prose on a miss and the serial number on a hit, and a serial is
    /// hex too. Only 40-hex-digit runs after the digest label count.
    #[test]
    fn non_digest_lines_are_not_mistaken_for_fingerprints() {
        let miss = "root \"Trusted Root Certification Authorities\"\n\
             CertUtil: -store command completed successfully.";
        assert!(hex_after_marker(miss, "cert hash(sha1)").is_empty());
        // A serial on its own line is hex but carries no digest label.
        let serial = "Serial Number: 1c1a980daf932ece635bc524e6627192d4301191";
        assert!(hex_after_marker(serial, "cert hash(sha1)").is_empty());
    }

    /// The distinction the old status line could not make: a REGENERATED CA
    /// leaves the old one trusted, so a name match says "trusted" while every
    /// intercepted connection still fails.
    #[test]
    fn a_regenerated_ca_reads_as_stale_not_trusted() {
        let mine = "AAAA093E452105DCC2E49F3FF7CE22A9058D6E0B".to_string();
        let other = "BBBB093E452105DCC2E49F3FF7CE22A9058D6E0B".to_string();
        assert_eq!(
            classify_trust(&mine, std::slice::from_ref(&mine)),
            CaTrust::Trusted
        );
        assert_eq!(classify_trust(&mine, &[]), CaTrust::NotTrusted);
        assert_eq!(
            classify_trust(&mine, std::slice::from_ref(&other)),
            CaTrust::Stale
        );
        // Both present (trusted, then regenerated, then re-trusted) is trusted.
        assert_eq!(
            classify_trust(&mine, &[other, mine.clone()]),
            CaTrust::Trusted
        );
    }

    #[test]
    fn mcp_merge_is_additive_and_never_clobbers_the_users_config() {
        use serde_json::json;
        // A real Desktop config: the user's own keys must survive untouched,
        // and in order (serde_json preserve_order).
        let user = json!({
            "coworkUserFilesPath": "/Users/x/Claude",
            "preferences": { "sidebarMode": "chat" }
        });
        let (out, o) = merge_mcp(user, "/Users/x/.parsec/bin/parsec").unwrap();
        assert_eq!(o, McpOutcome::Added);
        assert_eq!(out["coworkUserFilesPath"], "/Users/x/Claude");
        assert_eq!(out["preferences"]["sidebarMode"], "chat");
        assert_eq!(
            out["mcpServers"]["parsec"]["command"],
            "/Users/x/.parsec/bin/parsec"
        );
        assert_eq!(out["mcpServers"]["parsec"]["args"], json!(["mcp"]));

        // Idempotent.
        let (again, o) = merge_mcp(out.clone(), "/Users/x/.parsec/bin/parsec").unwrap();
        assert_eq!(o, McpOutcome::Current);
        assert_eq!(again, out);

        // A moved alias path refreshes rather than duplicating.
        let (moved, o) = merge_mcp(out, "/opt/parsec/bin/parsec").unwrap();
        assert_eq!(o, McpOutcome::Added);
        assert_eq!(
            moved["mcpServers"]["parsec"]["command"],
            "/opt/parsec/bin/parsec"
        );

        // Someone else's `parsec` server is theirs — report, never overwrite.
        let foreign = json!({"mcpServers": {"parsec": {"command": "/their/own/thing"}}});
        let (kept, o) = merge_mcp(foreign, "/Users/x/.parsec/bin/parsec").unwrap();
        assert_eq!(o, McpOutcome::Foreign);
        assert_eq!(kept["mcpServers"]["parsec"]["command"], "/their/own/thing");

        // Other MCP servers are neighbours, not casualties.
        let neighbours = json!({"mcpServers": {"github": {"command": "gh-mcp"}}});
        let (with_both, _) = merge_mcp(neighbours, "/p").unwrap();
        assert_eq!(with_both["mcpServers"]["github"]["command"], "gh-mcp");
        assert_eq!(with_both["mcpServers"]["parsec"]["command"], "/p");
    }

    #[test]
    fn mcp_removal_takes_only_our_entry() {
        use serde_json::json;
        let (root, _) = merge_mcp(
            json!({"mcpServers": {"github": {"command": "gh-mcp"}}}),
            "/p",
        )
        .unwrap();
        let (cleaned, removed) = remove_mcp(root);
        assert!(removed);
        assert!(cleaned["mcpServers"].get("parsec").is_none());
        assert_eq!(cleaned["mcpServers"]["github"]["command"], "gh-mcp");

        // A parsec entry we did not write is not ours to delete.
        let foreign = json!({"mcpServers": {"parsec": {"command": "/their/own/thing"}}});
        let (kept, removed) = remove_mcp(foreign);
        assert!(!removed);
        assert_eq!(kept["mcpServers"]["parsec"]["command"], "/their/own/thing");

        // Nothing registered at all is not an error.
        let (_, removed) = remove_mcp(json!({"preferences": {}}));
        assert!(!removed);
    }

    #[test]
    fn mcp_command_is_absolute_because_desktop_has_no_shell_path() {
        // Claude Desktop is launched by the window server and inherits no
        // shell environment; a bare `parsec` would resolve to nothing.
        let e = mcp_entry("/Users/x/.parsec/bin/parsec");
        assert!(e["command"].as_str().unwrap().starts_with('/'));
        assert_eq!(e["env"][MCP_SENTINEL], "1");
    }

    #[test]
    fn embedded_addon_carries_the_sentinel() {
        // Ownership of ~/.parsec/interceptor/addon.py depends on it.
        assert!(ADDON_PY.contains(SENTINEL));
    }

    #[test]
    fn addon_only_redirects_the_paths_the_proxy_serves() {
        // Parity with CC-Router's addon: /v1/messages* and /v1/models. Both
        // are routed in server.rs — anything else would 404 at the proxy and
        // break Desktop instead of curating it.
        assert!(ADDON_PY.contains(r#"_REDIRECT_PREFIXES = ("/v1/messages", "/v1/models")"#));
        // …and only those: no executable line may name a path we do not serve.
        for path in ["/v1/oauth", "/v1/environments", "/api/"] {
            assert!(
                !addon_code().contains(path),
                "addon code must not touch {path}"
            );
        }
    }

    /// The addon with its comments stripped — the assertions below are about
    /// what it *does*, and its header prose names paths on purpose.
    fn addon_code() -> String {
        ADDON_PY
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn addon_tags_desktop_traffic_with_the_existing_attribution_seam() {
        // `x-parsec-tool` is server.rs's calling-tool seam, charset-gated to
        // [a-z0-9-]{1,32}; it is what separates Desktop savings from the
        // CLI's, and forward_auth_headers drops it before upstream.
        assert!(addon_code().contains(r#"_TOOL_TAG = "claude-desktop""#));
        assert!(addon_code().contains(r#"headers["x-parsec-tool"] = _TOOL_TAG"#));
        assert!("claude-desktop".len() <= 32);
        assert!("claude-desktop"
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-')));
    }

    #[test]
    fn rendered_addon_bakes_the_target_and_stays_valid_python_source() {
        let rendered = render_addon("http://127.0.0.1:9137");
        assert!(rendered.contains(r#"_DEFAULT_TARGET = "http://127.0.0.1:9137""#));
        // Exactly one default assignment survives the substitution.
        assert_eq!(rendered.matches("_DEFAULT_TARGET = ").count(), 1);
        // The env var must still win, so the read is unchanged.
        assert!(rendered.contains(r#"os.environ.get("PARSEC_DESKTOP_TARGET", _DEFAULT_TARGET)"#));
    }

    #[test]
    fn rendered_addon_escapes_a_hostile_target() {
        // The value lands inside Python source; quoting is not optional.
        let rendered = render_addon("http://x\" + __import__('os').system('id') + \"");
        assert!(!rendered.contains("__import__('os').system('id') + \""));
        assert!(rendered.contains("\\\""));
    }

    #[test]
    fn addon_never_touches_auth_headers() {
        // §3: the user's own credentials pass through verbatim.
        assert!(!ADDON_PY.contains("x-api-key\"]"));
        assert!(!ADDON_PY.contains("authorization\"]"));
    }

    #[test]
    fn foreign_addon_is_never_overwritten() {
        let rendered = render_addon("http://127.0.0.1:8082");
        assert_eq!(decide(None, &rendered), WriteDecision::Write);
        assert_eq!(decide(Some(&rendered), &rendered), WriteDecision::Current);
        // An addon rendered against a DIFFERENT port is still ours: rewrite,
        // so a moved routed port is picked up instead of silently stale.
        let other = render_addon("http://127.0.0.1:9999");
        assert_eq!(decide(Some(&other), &rendered), WriteDecision::Write);
        assert_eq!(
            decide(Some(&format!("# {SENTINEL}\n# older version\n")), &rendered),
            WriteDecision::Write
        );
        assert_eq!(
            decide(Some("def request(flow): pass\n"), &rendered),
            WriteDecision::Foreign
        );
    }

    #[test]
    fn extension_status_parses_both_output_shapes() {
        assert_eq!(
            parse_extension_status("*\t*\tS8XHQB96PW\torg.mitmproxy.macos-redirector (0.1)"),
            ExtensionStatus::Ready
        );
        assert_eq!(
            parse_extension_status("\t*\tS8XHQB96PW\torg.mitmproxy.macos-redirector (0.1)"),
            ExtensionStatus::AwaitingApproval
        );
        assert_eq!(
            parse_extension_status(
                "--- com.apple.system_extension.network_extension\nenabled\tactive\tteamID\
                 \tbundleID\torg.mitmproxy.macos-redirector\t[activated enabled]"
            ),
            ExtensionStatus::Ready
        );
        assert_eq!(
            parse_extension_status("org.mitmproxy.macos-redirector\t[activated waiting for user]"),
            ExtensionStatus::AwaitingApproval
        );
        assert_eq!(
            parse_extension_status("no extensions here"),
            ExtensionStatus::NotInstalled
        );
    }

    #[test]
    fn mitmdump_args_scope_interception_to_the_desktop_process() {
        let args = mitmdump_args(Path::new("/tmp/addon.py"));
        // local:<process> is what keeps this from being a system-wide MITM.
        assert!(args.contains(&format!("local:{}", interception_spec())));
        assert!(args.contains(&"connection_strategy=lazy".to_string()));
        // mitmproxy_rs matches the spec as a case-sensitive substring of the
        // full image path. On Windows the binary on disk is LOWERCASE
        // claude.exe in every known install layout; a capitalized-only spec
        // matches nothing (the bug that made Windows interception silently
        // dead through v0.2.2).
        #[cfg(windows)]
        assert!(interception_spec().split(',').any(|p| p == "claude.exe"));
    }

    #[test]
    fn service_command_lines_survive_spaces_in_paths() {
        // A home directory with a space in it must not split the addon path
        // into two arguments — the failure mode is a boot service that
        // starts and intercepts nothing.
        assert_eq!(quote_if_spaced("--quiet"), "--quiet");
        assert_eq!(
            quote_if_spaced("/home/a b/.parsec/interceptor/addon.py"),
            "\"/home/a b/.parsec/interceptor/addon.py\""
        );
        let unit = systemd_unit_text(Path::new("/usr/bin/mitmdump"), "http://127.0.0.1:8082");
        assert!(unit.contains("ExecStart=/usr/bin/mitmdump --mode local:"));
        let script = win_launcher_script(
            Path::new(r"C:\Program Files\mitmproxy\mitmdump.exe"),
            "http://127.0.0.1:8082",
        );
        assert!(script.contains(r#""C:\Program Files\mitmproxy\mitmdump.exe""#));
        assert!(script.contains("set PARSEC_DESKTOP_TARGET=http://127.0.0.1:8082"));
    }

    #[test]
    fn stale_proxy_detection_keys_on_404_only() {
        // The exact bug this guards: the addon redirects /v1/models, and a
        // proxy built before that route 404s it.
        assert!(models_route_missing(
            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n"
        ));
        // A working route — including an upstream auth failure relayed
        // through it — is not a stale binary.
        assert!(!models_route_missing(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"data\":[]}"
        ));
        assert!(!models_route_missing("HTTP/1.1 401 Unauthorized\r\n\r\n"));
        assert!(!models_route_missing("HTTP/1.1 502 Bad Gateway\r\n\r\n"));
        // A 404 mentioned in a body must not be mistaken for the status line.
        assert!(!models_route_missing(
            "HTTP/1.1 200 OK\r\n\r\nHTTP/1.1 404 Not Found"
        ));
        assert!(!models_route_missing(""));
    }

    #[test]
    fn plist_escapes_paths() {
        let xml = plist_xml(
            Path::new("/opt/homebrew/bin/mitmdump"),
            "http://127.0.0.1:8082",
        );
        assert!(xml.contains("<string>/opt/homebrew/bin/mitmdump</string>"));
        assert!(xml.contains(LAUNCHD_LABEL));
        assert!(!xml.contains("&&"));
    }

    #[test]
    fn addon_fails_open_when_the_proxy_is_dead() {
        // The reboot bug this pins: the interceptor's boot service outlives
        // the proxy, and an unconditional redirect aimed every Desktop
        // request at a dead port. The liveness gate must sit BEFORE the
        // rewrite so a dead target means untouched passthrough, never a hang.
        let code = addon_code();
        let gate = code
            .find("if not _target_alive():")
            .expect("addon must gate the redirect on target liveness");
        let rewrite = code
            .find("flow.request.scheme =")
            .expect("addon must still rewrite when the target answers");
        assert!(gate < rewrite, "liveness gate must precede the rewrite");
        // Fail open is observable, not silent — same reason _SEEN exists.
        assert!(code.contains(r#""failed_open""#));
        // The probe touches only the redirect target, never another host.
        assert!(code.contains("socket.create_connection((_host, _port)"));
    }

    #[test]
    fn addon_revive_is_throttled_detached_and_optional() {
        let code = addon_code();
        // `parsec up` is idempotent, but an attempt per request would still
        // be a fork storm on a busy session — the throttle is load-bearing.
        assert!(code.contains("_REVIVE_EVERY_S"));
        // Detached with silenced stdio: mitmdump's event loop must never
        // block on, or log-interleave with, the spawned reviver.
        assert!(code.contains("start_new_session"));
        assert!(code.contains("subprocess.DEVNULL"));
        // The stable alias is probed first — the same path the Codex hook
        // and tray use — with PATH as fallback, and a missing binary is a
        // quiet no-op, not a crash inside the addon.
        assert!(code.contains(r#"".parsec", "bin", name"#));
        assert!(code.contains("shutil.which"));
    }

    #[test]
    fn proxy_boot_service_restarts_on_crash_but_respects_clean_exits() {
        let xml = proxy_plist_xml(Path::new("/Users/u/.parsec/bin/parsec"), 8082);
        assert!(xml.contains(PROXY_LAUNCHD_LABEL));
        assert!(xml.contains("<string>/Users/u/.parsec/bin/parsec</string>"));
        assert!(xml.contains("<string>proxy</string>"));
        assert!(xml.contains("<key>RunAtLoad</key>"));
        // SuccessfulExit=false: a crash restarts, but the two DELIBERATE
        // exit(0)s — "another parsec already owns the port" and `/shutdown`
        // from restart/upgrade flows — must stay down, or launchd would
        // fight every proxy replacement forever.
        assert!(xml.contains("<key>SuccessfulExit</key>"));
        assert!(xml.contains("<key>PARSEC_PROXY_PORT</key>"));
        assert!(xml.contains("<string>8082</string>"));
        // Distinct label: it must never collide with the interceptor's job.
        assert_ne!(PROXY_LAUNCHD_LABEL, LAUNCHD_LABEL);
        assert_ne!(PROXY_SYSTEMD_UNIT, SYSTEMD_UNIT);
        assert_ne!(WIN_PROXY_RUN_NAME, WIN_RUN_NAME);
    }

    #[test]
    fn proxy_service_command_lines_survive_spaces_in_paths() {
        let unit = proxy_systemd_unit_text(Path::new("/home/a b/.parsec/bin/parsec"), 9137);
        assert!(unit.contains("ExecStart=\"/home/a b/.parsec/bin/parsec\" proxy"));
        assert!(unit.contains("Environment=PARSEC_PROXY_PORT=9137"));
        assert!(unit.contains("Restart=on-failure"));
        let script =
            win_proxy_launcher_script(Path::new(r"C:\Users\a b\.parsec\bin\parsec.exe"), 9137);
        assert!(script.contains(r#""C:\Users\a b\.parsec\bin\parsec.exe" proxy"#));
        assert!(script.contains("set PARSEC_PROXY_PORT=9137"));
    }

    #[test]
    fn target_port_parse_is_shared_and_sane() {
        assert_eq!(proxy_target_port("http://127.0.0.1:8082"), Some(8082));
        assert_eq!(proxy_target_port("not a url"), None);
    }
}
