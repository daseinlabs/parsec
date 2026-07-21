//! `dasein setup` — one-shot Pro activation: download the local bge-large
//! ONNX export, write the Claude Code routing env, start the proxy warm.
//!
//! Spawned detached by the SessionStart hook on first run (`--auto`), and
//! runnable by hand to retry. This is NOT the serving path: wall clock,
//! network, and progress state are all fine here — determinism (§8.1)
//! applies to served bytes only.
//!
//! Ordering invariant: routing (`ANTHROPIC_BASE_URL` in the user's
//! settings.json) is written ONLY after both model files exist and verify —
//! a machine is never routed at a proxy whose embedder can't come up, and a
//! failed/offline download leaves the user's config untouched.
//!
//! Everything here degrades to "plugin keeps working, curation stays off":
//! failures land in the state file (surfaced by hook + statusline), never in
//! the session.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const STATE_CONTRACT: &str = "setup-state/v0";

/// `~/.dasein/setup_state.json` — the single source of truth for where
/// first-run setup stands. Phases:
/// - `spawned`: the SessionStart hook claimed the slot and spawned
///   `setup --auto` (the claim closes the two-sessions-start-at-once race);
///   the spawned process takes over from exactly this phase.
/// - `downloading`: a setup process owns the download (freshness of
///   `updated_unix` distinguishes live from crashed — see [`stale`]).
/// - `ready`: model verified; env written unless `base_url_conflict`.
/// - `failed`: retryable error (`dasein setup` runs again).
/// - `unsupported`: this binary has no `onnx` feature — terminal, silent.
/// - `disabled`: user ran `dasein disable` — autosetup never re-runs.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SetupState {
    pub contract_version: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub env_written: bool,
    /// The foreign ANTHROPIC_BASE_URL we refused to overwrite, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url_conflict: Option<String>,
    #[serde(default)]
    pub port: u16,
    pub updated_unix: u64,
}

impl SetupState {
    fn new(phase: &str) -> Self {
        SetupState {
            contract_version: STATE_CONTRACT.into(),
            phase: phase.into(),
            updated_unix: now_unix(),
            ..Default::default()
        }
    }

    /// A `spawned` claim whose owner stopped writing progress.
    pub fn stale(&self) -> bool {
        self.phase == "spawned" && now_unix().saturating_sub(self.updated_unix) > 300
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// User home for all on-disk state. HOME is a Unix variable: native Windows
/// (cmd/PowerShell — how Claude Code launches hooks) only sets USERPROFILE,
/// and Git Bash setting HOME masks that in manual testing. Without the
/// fallback every path here silently lands in /tmp (C:\tmp) on Windows.
pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub fn dasein_home() -> PathBuf {
    home_dir().join(".dasein")
}

pub fn state_path() -> PathBuf {
    dasein_home().join("setup_state.json")
}

pub fn load_state() -> Option<SetupState> {
    let data = std::fs::read_to_string(state_path()).ok()?;
    let st: SetupState = serde_json::from_str(&data).ok()?;
    (st.contract_version == STATE_CONTRACT).then_some(st)
}

/// Atomic (tmp + rename) so hook/statusline never read a torn write.
pub fn save_state(st: &SetupState) -> std::io::Result<()> {
    let path = state_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(st).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)
}

pub fn default_port() -> u16 {
    std::env::var("DASEIN_PROXY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8082)
}

fn port_bindable(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The port setup writes into settings.json — resolved at write time, the one
/// moment we may freely pick (Claude Code has not yet read routing). Prefer
/// `preferred`; if a FOREIGN process squats it, scan upward for a free port so
/// routing never lands on someone else's server (bug: "fall back to a random
/// port when the proxy port is already in use"). A dasein proxy already on
/// `preferred` is reused as-is — re-running setup must not strand it.
pub fn choose_free_port(preferred: u16) -> u16 {
    if port_bindable(preferred) {
        return preferred;
    }
    if proxy_request(preferred, "GET", "/health").is_some_and(|h| h.contains("dasein-proxy")) {
        return preferred; // our own supervisor — keep the port it owns
    }
    for p in (preferred.saturating_add(1))..=(preferred.saturating_add(64)) {
        if port_bindable(p) {
            tracing::warn!(
                "port {preferred} is held by a non-dasein process — routing to {p} instead"
            );
            return p;
        }
    }
    preferred // nothing free nearby; the supervisor bind will report it loudly
}

// ── model source resolution ─────────────────────────────────────────────────

pub fn run(auto: bool) -> anyhow::Result<()> {
    if auto {
        if let Some(st) = load_state() {
            if matches!(st.phase.as_str(), "disabled" | "unsupported" | "ready") {
                return Ok(());
            }
        }
    }
    // No model download since 2026-07-20: the embedder lives in the brain
    // (docs/server-side-embedding.md), so setup is settings routing + a warm
    // proxy. The `downloading` phase and its resumable state machine are gone.
    let mut st = SetupState::new("routing");
    st.port = choose_free_port(default_port());
    save_state(&st)?;

    match write_settings_env(st.port) {
        Ok(outcome) => {
            st.env_written = outcome.routed;
            st.base_url_conflict = outcome.conflict;
        }
        Err(e) => {
            st.phase = "failed".into();
            st.error = Some(format!("settings.json: {e}"));
            st.updated_unix = now_unix();
            let _ = save_state(&st);
            return Err(e);
        }
    }

    // Warm proxy so the next session's first request finds a live port.
    let spawn_err = spawn_proxy_detached(st.port, &[]).err();

    st.phase = "ready".into();
    st.error = None;
    st.updated_unix = now_unix();
    save_state(&st)?;

    match (&st.base_url_conflict, st.env_written) {
        (Some(url), _) => println!(
            "routing NOT written: ANTHROPIC_BASE_URL is already {url} — dasein will not \
             overwrite it. Point it at http://127.0.0.1:{} yourself to enable curation.",
            st.port
        ),
        (None, true) => println!(
            "routing written to Claude Code settings (127.0.0.1:{}) — restart Claude Code \
             to activate curation. Undo anytime: dasein disable",
            st.port
        ),
        (None, false) => println!("routing already pointed at a local dasein proxy — kept as-is"),
    }
    if let Some(e) = spawn_err {
        println!("proxy pre-warm failed ({e}) — the SessionStart hook will start it next session");
    }
    Ok(())
}

/// `dasein up` — bring the proxy back on the routed port. The manual twin of
/// the SessionStart hook's autostart, for the rare case the supervisor itself
/// died MID-session: routing env is read at Claude Code launch and cannot
/// change, so revival means putting a supervisor back on the same port.
/// (A dead *worker* needs no intervention — the supervisor respawns it and
/// falls back to Anthropic in the gap.) Idempotent —
/// a live proxy (ours or the user's own) is never double-spawned.
pub fn up() -> anyhow::Result<()> {
    let port = routed_port();
    let log = dasein_home().join("proxy.log");
    if crate::hook::port_listening(port) {
        println!("proxy already listening on 127.0.0.1:{port} — nothing to do");
        return Ok(());
    }
    // Nothing to re-derive: the proxy needs no embedder env since the
    // embedder moved server-side (docs/server-side-embedding.md).
    let extra: Vec<(String, String)> = Vec::new();
    spawn_proxy_detached(port, &extra)?;
    for _ in 0..40 {
        if crate::hook::port_listening(port) {
            println!(
                "proxy up on 127.0.0.1:{port} — a stuck session recovers on its next \
                 request (log: {})",
                log.display()
            );
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!(
        "proxy spawned for 127.0.0.1:{port} but never started listening — check {}",
        log.display()
    )
}

/// The port a routed Claude Code session is actually pointed at: this shell's
/// ANTHROPIC_BASE_URL if it names a local proxy, else the one written into
/// settings.json (what sessions launch with), else the default.
fn routed_port() -> u16 {
    if let Some(p) = std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .and_then(|b| crate::hook::local_proxy_port(&b))
    {
        return p;
    }
    if let Some(p) = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|d| serde_json::from_str::<Value>(&d).ok())
        .and_then(|root| {
            root.get("env")?
                .get("ANTHROPIC_BASE_URL")?
                .as_str()
                .and_then(crate::hook::local_proxy_port)
        })
    {
        return p;
    }
    default_port()
}

/// `dasein disable` — remove exactly (and only) what setup wrote.
fn strip_managed_settings() -> anyhow::Result<()> {
    let path = settings_path();
    match std::fs::read_to_string(&path) {
        Ok(data) => {
            let root: Value = serde_json::from_str(&data)
                .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", path.display()))?;
            let (root, removed) = remove_managed_env(root);
            if removed.is_empty() {
                println!("no dasein-managed env keys found in {}", path.display());
            } else {
                write_settings_file(&path, &root)?;
                println!(
                    "removed {} from {} — restart Claude Code to route directly again",
                    removed.join(", "),
                    path.display()
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "no Claude Code settings file at {} — nothing to undo",
                path.display()
            );
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub fn disable() -> anyhow::Result<()> {
    strip_managed_settings()?;
    let mut st = SetupState::new("disabled");
    st.port = default_port();
    save_state(&st)?;
    println!("auto-setup is now off. Re-enable with: dasein setup");
    Ok(())
}

/// Minimal one-shot HTTP exchange with the local proxy. Raw TcpStream on
/// purpose: no client dep, works the same on every platform, and short
/// timeouts keep uninstall snappy when nothing is listening.
fn proxy_request(port: u16, method: &str, path: &str) -> Option<String> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .ok()?;
    s.set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .ok()?;
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    Some(buf)
}

/// Stop a proxy on `port`, but only after /health proves the listener is OURS
/// — a foreign server that happens to sit on the port is left alone.
fn stop_proxy(port: u16) -> String {
    if !crate::hook::port_listening(port) {
        return format!("no proxy listening on 127.0.0.1:{port}");
    }
    match proxy_request(port, "GET", "/health") {
        Some(h) if h.contains("dasein-proxy") => {
            let _ = proxy_request(port, "POST", "/shutdown");
            for _ in 0..20 {
                if !crate::hook::port_listening(port) {
                    return format!("proxy on 127.0.0.1:{port} stopped");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            format!(
                "proxy on 127.0.0.1:{port} did not stop when asked — kill it by \
                 hand if it lingers (routing is already removed)"
            )
        }
        Some(_) => {
            format!("port {port} is serving something that is not the dasein proxy — left alone")
        }
        None => format!("listener on port {port} did not answer a health probe — left alone"),
    }
}

/// Delete every dasein-owned data file under `home` EXCEPT the state file
/// (setup_state.json): the "disabled" marker there must survive until the
/// plugin itself is uninstalled, or a
/// still-open session's auto-setup hook would immediately redownload models.
fn purge_data_files(home: &Path) -> (Vec<String>, Vec<String>) {
    let (mut removed, mut failed) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(home) else {
        return (removed, failed);
    };
    let marker = state_path();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if Some(e.file_name().as_os_str()) == marker.file_name() {
            continue;
        }
        let p = e.path();
        let res = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        match res {
            Ok(()) => removed.push(name),
            Err(err) => failed.push(format!("{name}: {err}")),
        }
    }
    (removed, failed)
}

/// `dasein uninstall` — full local cleanup, run BEFORE `claude plugin
/// uninstall` (afterwards this binary is gone). Order matters: mark disabled
/// first so still-open sessions' auto-setup hooks won't restart anything,
/// resolve the routed port BEFORE stripping settings (routed_port reads
/// them), then stop the proxy and purge data.
pub fn uninstall() -> anyhow::Result<()> {
    let mut st = SetupState::new("disabled");
    st.port = default_port();
    save_state(&st)?;
    let port = routed_port();
    strip_managed_settings()?;
    println!("{}", stop_proxy(port));
    let home = dasein_home();
    let (removed, failed) = purge_data_files(&home);
    if !removed.is_empty() {
        println!("removed from {}: {}", home.display(), removed.join(", "));
    }
    for f in &failed {
        eprintln!("could not remove {f} — delete by hand");
    }
    println!(
        "local cleanup done. To finish:\n  1. claude plugin uninstall dasein\n  \
         2. (optional) rm -rf {} — removes the last marker file",
        home.display()
    );
    Ok(())
}

// ── download (resumable, sha-pinned, atomic finalize) ───────────────────────

pub fn settings_path() -> PathBuf {
    // CLAUDE_CONFIG_DIR is Claude Code's own relocation knob for ~/.claude.
    let dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"));
    dir.join("settings.json")
}

#[derive(Debug, PartialEq)]
pub struct MergeOutcome {
    /// ANTHROPIC_BASE_URL now points at a local dasein proxy (whether we
    /// wrote it just now or it was already there).
    pub routed: bool,
    pub conflict: Option<String>,
    pub changed: bool,
}

/// Merge the managed env keys into a settings root. Additive only: a key the
/// user already set is NEVER overwritten, and a foreign ANTHROPIC_BASE_URL
/// is reported as a conflict instead of being touched. Pure for tests.
fn merge_settings(mut root: Value, port: u16) -> anyhow::Result<(Value, MergeOutcome)> {
    if root.is_null() {
        root = serde_json::json!({});
    }
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings root is not a JSON object"))?;
    let env = obj.entry("env").or_insert_with(|| serde_json::json!({}));
    let env = env
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings `env` is not a JSON object"))?;

    let mut out = MergeOutcome {
        routed: false,
        conflict: None,
        changed: false,
    };
    match env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str) {
        None => {
            env.insert(
                "ANTHROPIC_BASE_URL".into(),
                Value::String(format!("http://127.0.0.1:{port}")),
            );
            out.routed = true;
            out.changed = true;
        }
        Some(existing) => match crate::hook::local_proxy_port(existing) {
            Some(_) => out.routed = true, // already ours (any port) — keep
            None => out.conflict = Some(existing.to_string()),
        },
    }
    // No embedder env is written any more: the brain embeds
    // (docs/server-side-embedding.md). Previously this planted
    // DASEIN_EMBED_BACKEND=onnx + DASEIN_ONNX_DIR.
    Ok((root, out))
}

/// The managed statusLine command for `exe`. Quoted — the plugin cache path
/// the binary lives under moves on every version bump and may contain spaces.
fn statusline_cmd(exe: &str) -> String {
    format!("\"{exe}\" statusline")
}

/// A statusLine value setup wrote (under any past binary path) — a
/// user-authored command never matches and is never touched.
fn is_managed_statusline(v: &Value) -> bool {
    v.pointer("/command")
        .and_then(Value::as_str)
        .map(|c| c.contains("dasein") && c.trim_end().ends_with("statusline"))
        .unwrap_or(false)
}

/// Merge the managed statusLine into a settings root. Plugins cannot ship a
/// `statusLine` key (plugin settings.json supports only agent /
/// subagentStatusLine), so setup writes it into the user's settings with the
/// same ownership discipline as the env merge: absent → written; ours under
/// an older binary path → repointed (version bumps move the plugin cache
/// dir, and SessionStart's ensure_routing re-asserts); user-authored →
/// NEVER touched. Pure for tests.
fn merge_statusline(mut root: Value, exe: &str) -> anyhow::Result<(Value, bool)> {
    if root.is_null() {
        root = serde_json::json!({});
    }
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings root is not a JSON object"))?;
    let want = serde_json::json!({"type": "command", "command": statusline_cmd(exe)});
    let changed = match obj.get("statusLine") {
        None => {
            obj.insert("statusLine".into(), want);
            true
        }
        Some(v) if is_managed_statusline(v) && *v != want => {
            obj.insert("statusLine".into(), want);
            true
        }
        Some(_) => false,
    };
    Ok((root, changed))
}

/// Strip managed keys, but only when they still hold values setup would have
/// written — a user-customized value is theirs, not ours to delete.
fn remove_managed_env(mut root: Value) -> (Value, Vec<String>) {
    let mut removed = Vec::new();
    if root
        .get("statusLine")
        .map(is_managed_statusline)
        .unwrap_or(false)
    {
        if let Some(obj) = root.as_object_mut() {
            obj.remove("statusLine");
            removed.push("statusLine".to_string());
        }
    }
    if let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) {
        let ours = |k: &str, v: &Value| match (k, v.as_str()) {
            ("ANTHROPIC_BASE_URL", Some(s)) => crate::hook::local_proxy_port(s).is_some(),
            ("DASEIN_EMBED_BACKEND", Some(s)) => s == "onnx",
            ("DASEIN_ONNX_DIR", Some(_)) => true,
            _ => false,
        };
        for k in [
            "ANTHROPIC_BASE_URL",
            "DASEIN_EMBED_BACKEND",
            "DASEIN_ONNX_DIR",
        ] {
            if env.get(k).map(|v| ours(k, v)).unwrap_or(false) {
                env.remove(k);
                removed.push(k.to_string());
            }
        }
    }
    (root, removed)
}

fn write_settings_env(port: u16) -> anyhow::Result<MergeOutcome> {
    let path = settings_path();
    let root: Value = match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).map_err(|e| {
            // Refusing beats clobbering: a settings file we can't parse is
            // one we must not rewrite.
            anyhow::anyhow!(
                "cannot parse {}: {e} — fix it, then run `dasein setup`",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(e.into()),
    };
    let (root, mut outcome) = merge_settings(root, port)?;
    // The statusline rides the same managed-settings write (delivery
    // mechanism of docs/plugin-user-messaging.md Part 1 §1 — plugins cannot
    // ship the key themselves).
    let root = match std::env::current_exe() {
        Ok(exe) => {
            let (root, changed) = merge_statusline(root, &exe.to_string_lossy())?;
            outcome.changed |= changed;
            root
        }
        Err(_) => root,
    };
    if outcome.changed {
        write_settings_file(&path, &root)?;
    }
    Ok(outcome)
}

fn write_settings_file(path: &Path, root: &Value) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.dasein-tmp");
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(root)?))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Re-assert the managed routing env from the SessionStart hook: the same
/// additive merge as setup (a key the user set is never overwritten), for
/// the case where a plugin update/reinstall made Claude Code rewrite
/// settings.json and drop the managed keys while state still says
/// `env_written`.
pub fn ensure_routing(port: u16) -> anyhow::Result<MergeOutcome> {
    write_settings_env(port)
}

// ── detached spawns (shared with the SessionStart hook) ─────────────────────

/// Spawn `dasein proxy` (the SUPERVISOR) on `port`, detached, logging to
/// ~/.dasein/proxy.log. The supervisor owns the port for its whole life and
/// spawns/restarts the curating worker itself — there is no idle self-exit
/// any more (removed 2026-07-21: it wedged still-active sessions that went
/// briefly idle, and a self-killing worker would just be respawned).
pub fn spawn_proxy_detached(port: u16, extra_env: &[(String, String)]) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("proxy").env("DASEIN_PROXY_PORT", port.to_string());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    spawn_detached(cmd, "proxy.log")
}

/// Spawn `dasein setup --auto` detached, logging to ~/.dasein/setup.log.
pub fn spawn_setup_detached() -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("setup").arg("--auto");
    spawn_detached(cmd, "setup.log")
}

fn spawn_detached(mut cmd: std::process::Command, log_name: &str) -> anyhow::Result<()> {
    let log_dir = dasein_home();
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join(log_name))?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    #[cfg(unix)]
    {
        // Own process group: outlives the hook AND the session that spawned it.
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    }
    #[cfg(windows)]
    {
        // The detach equivalent: DETACHED_PROCESS drops the hook's console,
        // whose CTRL_CLOSE_EVENT would otherwise kill the proxy when the
        // session window goes away (stdio is already null/log-file);
        // CREATE_NEW_PROCESS_GROUP exempts it from the parent's Ctrl-C.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        std::os::windows::process::CommandExt::creation_flags(
            &mut cmd,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
        );
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_writes_routing_into_empty_settings() {
        let (root, out) = merge_settings(Value::Null, 8082).unwrap();
        assert!(out.routed && out.changed && out.conflict.is_none());
        assert_eq!(root["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8082");
        // routing is the ONLY managed env key now — no embedder to configure
        assert!(root["env"]["DASEIN_EMBED_BACKEND"].is_null());
        assert!(root["env"]["DASEIN_ONNX_DIR"].is_null());
    }

    #[test]
    fn merge_never_touches_user_values() {
        let existing = json!({
            "model": "opus",
            "env": {
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:9999",
                "DASEIN_EMBED_BACKEND": "remote",
                "FOO": "bar"
            }
        });
        let (root, out) = merge_settings(existing, 8082).unwrap();
        assert!(out.routed); // 9999 is still a local dasein-shaped proxy
        assert_eq!(out.conflict, None);
        assert_eq!(root["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9999");
        assert_eq!(root["env"]["DASEIN_EMBED_BACKEND"], "remote"); // user's key untouched
        assert_eq!(root["env"]["FOO"], "bar");
        assert_eq!(root["model"], "opus");
        assert!(!out.changed); // nothing to add: routing was already ours
    }

    #[test]
    fn merge_reports_foreign_base_url_as_conflict() {
        let existing = json!({ "env": { "ANTHROPIC_BASE_URL": "https://my-gateway.corp" } });
        let (root, out) = merge_settings(existing, 8082).unwrap();
        assert!(!out.routed);
        assert_eq!(out.conflict.as_deref(), Some("https://my-gateway.corp"));
        assert_eq!(root["env"]["ANTHROPIC_BASE_URL"], "https://my-gateway.corp");
    }

    #[test]
    fn merge_is_idempotent() {
        let (once, _) = merge_settings(Value::Null, 8082).unwrap();
        let (twice, out) = merge_settings(once.clone(), 8082).unwrap();
        assert_eq!(once, twice);
        assert!(!out.changed);
    }

    #[test]
    fn merge_rejects_non_object_roots() {
        assert!(merge_settings(json!([1, 2]), 8082).is_err());
        assert!(merge_settings(json!({"env": "oops"}), 8082).is_err());
    }

    #[test]
    fn statusline_merge_writes_repoints_ours_never_users() {
        // Absent → written.
        let (root, changed) = merge_statusline(Value::Null, "/cache/v1/bin/dasein").unwrap();
        assert!(changed);
        assert_eq!(root["statusLine"]["type"], "command");
        assert_eq!(
            root["statusLine"]["command"],
            "\"/cache/v1/bin/dasein\" statusline"
        );

        // Ours under an old binary path → repointed (plugin version bump).
        let (root, changed) = merge_statusline(root, "/cache/v2/bin/dasein").unwrap();
        assert!(changed);
        assert_eq!(
            root["statusLine"]["command"],
            "\"/cache/v2/bin/dasein\" statusline"
        );

        // Idempotent at the same path.
        let (root, changed) = merge_statusline(root, "/cache/v2/bin/dasein").unwrap();
        assert!(!changed);

        // User-authored statusLine: never touched, and never removed.
        let mut user = root;
        user["statusLine"] = json!({"type": "command", "command": "~/bin/my-status.sh"});
        let (user, changed) = merge_statusline(user, "/cache/v3/bin/dasein").unwrap();
        assert!(!changed);
        assert_eq!(user["statusLine"]["command"], "~/bin/my-status.sh");
        let (user, removed) = remove_managed_env(user);
        assert!(removed.is_empty());
        assert_eq!(user["statusLine"]["command"], "~/bin/my-status.sh");
    }

    #[test]
    fn disable_removes_managed_statusline() {
        let root = json!({
            "statusLine": {"type": "command", "command": "\"/cache/v1/bin/dasein\" statusline"},
            "env": {"FOO": "bar"}
        });
        let (root, removed) = remove_managed_env(root);
        assert_eq!(removed, vec!["statusLine"]);
        assert!(root.get("statusLine").is_none());
        assert_eq!(root["env"]["FOO"], "bar");
    }

    #[test]
    fn disable_removes_only_what_setup_wrote() {
        let root = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:8082",
                "DASEIN_EMBED_BACKEND": "onnx",
                "DASEIN_ONNX_DIR": "/home/u/.dasein/models/bge-large-onnx",
                "FOO": "bar"
            }
        });
        let (root, removed) = remove_managed_env(root);
        assert_eq!(
            removed,
            vec![
                "ANTHROPIC_BASE_URL",
                "DASEIN_EMBED_BACKEND",
                "DASEIN_ONNX_DIR"
            ]
        );
        assert_eq!(root["env"]["FOO"], "bar");
        assert!(root["env"].get("ANTHROPIC_BASE_URL").is_none());

        // customized values stay: foreign URL, non-onnx backend
        let root = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://my-gateway.corp",
                "DASEIN_EMBED_BACKEND": "remote"
            }
        });
        let (root, removed) = remove_managed_env(root);
        assert!(removed.is_empty());
        assert_eq!(root["env"]["ANTHROPIC_BASE_URL"], "https://my-gateway.corp");
    }

    #[test]
    fn purge_keeps_only_the_disabled_marker() {
        let dir = std::env::temp_dir().join(format!("dasein-purge-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(dir.join("models/embedder.onnx"), b"x").unwrap();
        std::fs::write(dir.join("proxy.log"), b"x").unwrap();
        std::fs::write(dir.join("ledger.jsonl"), b"x").unwrap();
        std::fs::write(dir.join("setup_state.json"), b"{}").unwrap();

        let (mut removed, failed) = purge_data_files(&dir);
        removed.sort();
        assert_eq!(removed, vec!["ledger.jsonl", "models", "proxy.log"]);
        assert!(failed.is_empty());
        assert!(dir.join("setup_state.json").exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn choose_free_port_scans_past_a_foreign_squatter() {
        // Occupy a port with a non-HTTP listener that accepts one connection
        // and drops it — so the /health identity probe reads empty (fails
        // fast) and choose_free_port treats it as foreign and scans onward.
        let squat = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = squat.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((s, _)) = squat.accept() {
                drop(s); // FIN → the probe's read returns "" immediately
            }
            // hold the port bound until the test's choose_free_port has run
            std::thread::sleep(std::time::Duration::from_millis(500));
        });
        let chosen = choose_free_port(port);
        // The scan range is preferred+1.., so it can never return the squatted
        // port — the point is that it did NOT mistake the squatter for ours.
        assert_ne!(chosen, port);
    }

    #[test]
    fn stop_proxy_leaves_missing_listener_alone() {
        // Port 1 is never listening; the probe must come back without touching
        // anything and say so.
        assert!(stop_proxy(1).contains("no proxy listening"));
    }

    #[test]
    fn state_roundtrips_and_staleness() {
        let mut st = SetupState::new("spawned");
        let json = serde_json::to_string(&st).unwrap();
        let back: SetupState = serde_json::from_str(&json).unwrap();
        assert_eq!(st, back);
        assert!(!st.stale()); // just written
        st.updated_unix = 1; // 1970 — long dead
        assert!(st.stale());
        st.phase = "ready".into();
        assert!(!st.stale()); // staleness only applies to an unfinished claim
    }
}
