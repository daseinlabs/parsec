//! `parsec setup` — one-shot Pro activation: download the local bge-large
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

/// `~/.parsec/setup_state.json` — the single source of truth for where
/// first-run setup stands. Phases:
/// - `spawned`: the SessionStart hook claimed the slot and spawned
///   `setup --auto` (the claim closes the two-sessions-start-at-once race);
///   the spawned process takes over from exactly this phase.
/// - `downloading`: a setup process owns the download (freshness of
///   `updated_unix` distinguishes live from crashed — see [`stale`]).
/// - `ready`: model verified; env written unless `base_url_conflict`.
/// - `failed`: retryable error (`parsec setup` runs again).
/// - `unsupported`: this binary has no `onnx` feature — terminal, silent.
/// - `disabled`: user ran `parsec disable` — autosetup never re-runs.
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

pub fn parsec_home() -> PathBuf {
    home_dir().join(".parsec")
}

pub fn state_path() -> PathBuf {
    parsec_home().join("setup_state.json")
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
    std::env::var("PARSEC_PROXY_PORT")
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
/// port when the proxy port is already in use"). A parsec proxy already on
/// `preferred` is reused as-is — re-running setup must not strand it.
pub fn choose_free_port(preferred: u16) -> u16 {
    if port_bindable(preferred) {
        return preferred;
    }
    if parsec_owns(preferred) {
        return preferred; // our own supervisor — keep the port it owns
    }
    for p in (preferred.saturating_add(1))..=(preferred.saturating_add(64)) {
        // Bindable OR already ours. Without the second half, a parsec proxy
        // that an earlier installer moved to 8083 looks "taken" here, so the
        // next tool routes at 8084 and the two harnesses end up pointed at
        // different ports — defeating the shared-proxy design at exactly the
        // moment the ports are being renegotiated.
        if port_bindable(p) || parsec_owns(p) {
            tracing::warn!(
                "port {preferred} is held by a non-parsec process — routing to {p} instead"
            );
            return p;
        }
    }
    preferred // nothing free nearby; the supervisor bind will report it loudly
}

/// True when a parsec proxy answers `/health` on `port`. Identity, not
/// liveness: the whole point is to tell our own supervisor apart from a
/// stranger's server before we route a user's credentials at it.
pub(crate) fn parsec_owns(port: u16) -> bool {
    proxy_request(port, "GET", "/health").is_some_and(|h| h.contains("parsec-proxy"))
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

    // Make `parsec` resolvable from a terminal. Until this ran here, a
    // Claude-Code-only install left no callable binary anywhere: the plugin
    // copy lives in a cache dir whose path changes on every update, and
    // install.sh only writes the alias + PATH when it detects codex or
    // opencode. Every "run `parsec …`" instruction we print was therefore
    // dead for plugin-only users.
    ensure_callable(auto);

    // Warm proxy so the next session's first request finds a live port.
    let spawn_err = spawn_proxy_detached(st.port, &[]).err();

    st.phase = "ready".into();
    st.error = None;
    st.updated_unix = now_unix();
    save_state(&st)?;

    match (&st.base_url_conflict, st.env_written) {
        (Some(url), _) => println!(
            "routing NOT written: ANTHROPIC_BASE_URL is already {url} — parsec will not \
             overwrite it. Point it at http://127.0.0.1:{} yourself to enable curation.",
            st.port
        ),
        (None, true) => println!(
            "routing written to Claude Code settings (127.0.0.1:{}) — restart Claude Code \
             to activate curation. Undo anytime: parsec disable",
            st.port
        ),
        (None, false) => println!("routing already pointed at a local parsec proxy — kept as-is"),
    }
    if let Some(e) = spawn_err {
        println!("proxy pre-warm failed ({e}) — the SessionStart hook will start it next session");
    }
    Ok(())
}

/// Is `~/.parsec/bin` already on PATH? Checked against PATH rather than by
/// probing `which parsec`, because the answer we need is "will a NEW shell
/// resolve it", not "does this process happen to have it".
fn alias_dir_on_path() -> bool {
    let Some(dir) = crate::setup_opencode::bin_alias_path()
        .parent()
        .map(|p| p.to_path_buf())
    else {
        return false;
    };
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|e| e == dir))
        .unwrap_or(false)
}

/// The shell rc that a login shell of `shell` will actually read. Pure, so
/// the per-shell mapping is pinned by tests rather than by whoever runs them:
/// fish in particular needs a different FILE and a different SYNTAX, and
/// getting either wrong writes a line that silently never executes.
#[cfg(unix)]
fn rc_for_shell(
    shell: &str,
    zdotdir: Option<&str>,
    xdg_config: Option<&str>,
    home: &Path,
) -> PathBuf {
    match Path::new(shell).file_name().and_then(|s| s.to_str()) {
        Some("zsh") => zdotdir
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.to_path_buf())
            .join(".zshrc"),
        Some("bash") => home.join(".bashrc"),
        Some("fish") => xdg_config
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("fish")
            .join("conf.d")
            .join("parsec.fish"),
        _ => home.join(".profile"),
    }
}

/// Keyed off `$SHELL` and not the running shell, for the same reason
/// install.sh does it: a hook-spawned setup is not the user's interactive
/// shell.
#[cfg(unix)]
fn shell_rc() -> PathBuf {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let zdotdir = std::env::var("ZDOTDIR").ok();
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    rc_for_shell(&shell, zdotdir.as_deref(), xdg.as_deref(), &home_dir())
}

/// Append the guarded PATH line, exactly as install.sh does. Idempotent by
/// content match, so re-running setup never stacks duplicates.
#[cfg(unix)]
fn add_dir_to_path(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let rc = shell_rc();
    if let Ok(cur) = std::fs::read_to_string(&rc) {
        if cur.contains(".parsec/bin") {
            return Ok(None);
        }
    }
    if let Some(parent) = rc.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = if rc.extension().and_then(|e| e.to_str()) == Some("fish") {
        format!(
            "\n# parsec\nfish_add_path --prepend \"{}\"\n",
            dir.display()
        )
    } else {
        format!("\n# parsec\nexport PATH=\"{}:$PATH\"\n", dir.display())
    };
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rc)?;
    f.write_all(line.as_bytes())?;
    Ok(Some(rc))
}

/// Windows twin: user-scope PATH via the same call install.ps1 makes. Not
/// `setx` — that truncates at 1024 chars and has eaten people's PATH.
#[cfg(windows)]
fn add_dir_to_path(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    // PowerShell concatenation, no embedded double quotes — the quoting
    // three languages deep (Rust -> cmdline -> PowerShell) is where this
    // kind of helper usually breaks.
    // Guarding the empty case matters: a user-scope PATH that is unset makes
    // `$p` null, and a naive `$p + ';' + $d` writes a LEADING semicolon —
    // an empty PATH entry, which Windows resolves as the current directory.
    let script = format!(
        "$d = '{}'; \
         $p = [Environment]::GetEnvironmentVariable('Path','User'); \
         if ([string]::IsNullOrEmpty($p)) {{ \
           [Environment]::SetEnvironmentVariable('Path', $d, 'User') }} \
         elseif (($p -split ';') -notcontains $d) {{ \
           [Environment]::SetEnvironmentVariable('Path', ($p.TrimEnd(';') + ';' + $d), 'User') }}",
        dir.display()
    );
    let ok = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(Some(PathBuf::from("your user PATH")))
    } else {
        anyhow::bail!("could not update the user PATH")
    }
}

/// Make `parsec` callable from a terminal, not just from inside Claude Code.
///
/// The plugin binary lives in the plugin cache, whose path changes on every
/// update, and NOTHING on the Claude Code install route used to leave a
/// stable, callable copy: `install.sh` only writes the alias + PATH when it
/// detects codex or opencode, and `refresh_bin_alias` was called only from
/// those two setups. A plugin-only user therefore had no `parsec` on PATH at
/// all — so every instruction that says "run `parsec …` in a terminal"
/// (the Desktop CA step, the interceptor commands, `parsec up`) simply did
/// not resolve for them.
///
/// The alias is always refreshed — cheap, non-destructive (only a symlink we
/// own is ever replaced), and purged by `parsec uninstall` with the rest of
/// ~/.parsec. The PATH edit is gated to MANUAL runs: a hook-spawned
/// `--auto` first run must not silently rewrite someone's shell rc.
pub(crate) fn ensure_callable(auto: bool) {
    if let Err(e) = crate::setup_opencode::refresh_bin_alias() {
        println!("could not refresh the parsec binary alias ({e})");
        return;
    }
    if alias_dir_on_path() {
        return;
    }
    let Some(dir) = crate::setup_opencode::bin_alias_path()
        .parent()
        .map(|p| p.to_path_buf())
    else {
        return;
    };
    if auto {
        println!(
            "note: `parsec` is not on your PATH. Run `parsec setup` from a terminal, or add              manually:\n  export PATH=\"{}:$PATH\"",
            dir.display()
        );
        return;
    }
    match add_dir_to_path(&dir) {
        Ok(Some(where_)) => println!(
            "added {} to PATH in {} — open a new terminal to use `parsec` directly",
            dir.display(),
            where_.display()
        ),
        Ok(None) => println!(
            "{} is already in your shell config — open a new terminal to use `parsec`",
            dir.display()
        ),
        Err(e) => println!(
            "could not update PATH ({e}) — add manually:\n  export PATH=\"{}:$PATH\"",
            dir.display()
        ),
    }
}

/// `parsec up` — bring the proxy back on the routed port. The manual twin of
/// the SessionStart hook's autostart, for the rare case the supervisor itself
/// died MID-session: routing env is read at Claude Code launch and cannot
/// change, so revival means putting a supervisor back on the same port.
/// (A dead *worker* needs no intervention — the supervisor respawns it and
/// falls back to Anthropic in the gap.) Idempotent —
/// a live proxy (ours or the user's own) is never double-spawned.
/// `parsec up` — revive the routed proxy, and (on `--session-start`) hand a
/// staged Codex trim to the session that is starting.
///
/// Codex has no `additionalContext` hook field the way Claude Code does, but
/// it surfaces a SessionStart hook's STDOUT to the model as a `developer`
/// message — confirmed in a real rollout, where this function's own
/// "proxy already listening…" line appears as one. That is the injection
/// channel: printing the composed trim puts it in the next session's context,
/// which is exactly what the Claude Code hook achieves through
/// `additionalContext`.
///
/// Gated on the flag rather than on every `up`, so a manual `parsec up`
/// cannot silently burn a staged payload. `consume_pending` is one-shot and
/// TTL-bounded, so a resume inside the window gets it once and never again.
pub fn up(restart: bool, session_start: bool) -> anyhow::Result<()> {
    if session_start {
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(ctx) =
                crate::trim::consume_pending(&cwd.to_string_lossy(), crate::trim::Source::Codex)
            {
                println!("{ctx}");
            }
        }
    }
    let port = routed_port();
    let log = parsec_home().join("proxy.log");
    if crate::hook::port_listening(port) {
        // Liveness is not identity. Every harness's SessionStart hook calls
        // this, so a foreign process holding the routed port used to be
        // reported as a healthy proxy to all of them — including by the very
        // command warm_proxy tells users to run when a shutdown is refused.
        if !restart && !parsec_owns(port) {
            anyhow::bail!(
                "127.0.0.1:{port} is listening but is NOT a parsec proxy — refusing to \
                 report it healthy. Routed traffic is going to that process; stop it and \
                 re-run, or run `parsec setup` to route at a different port."
            );
        }
        if !restart {
            println!("proxy already listening on 127.0.0.1:{port} — nothing to do");
            return Ok(());
        }
        // `--restart` (used by the install scripts): a freshly installed
        // binary must actually serve — a proxy that predates the install
        // keeps running the OLD image otherwise. Only something that
        // identifies itself as a parsec proxy is ever shut down; a foreign
        // process on the port is left alone.
        if !shutdown_parsec_on(port) {
            anyhow::bail!(
                "127.0.0.1:{port} is listening but is not a parsec proxy (or would not \
                 shut down) — not killing a foreign process; free the port and re-run"
            );
        }
        let mut freed = false;
        for _ in 0..40 {
            if !crate::hook::port_listening(port) {
                freed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !freed {
            anyhow::bail!(
                "old proxy on 127.0.0.1:{port} acknowledged shutdown but never released \
                 the port — check {}",
                log.display()
            );
        }
        println!("old proxy on 127.0.0.1:{port} stopped — starting the installed binary");
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

/// Identity-checked shutdown: GET /health must answer as a parsec proxy
/// before POST /shutdown is sent (the supervisor's own uninstall probe uses
/// the same identity rule). false ⇒ nothing was shut down.
pub(crate) fn shutdown_parsec_on(port: u16) -> bool {
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    let is_parsec = client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.text().ok())
        .is_some_and(|t| t.contains("parsec-proxy"));
    if !is_parsec {
        return false;
    }
    client
        .post(format!("http://127.0.0.1:{port}/shutdown"))
        .send()
        .is_ok()
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

/// `parsec disable` — remove exactly (and only) what setup wrote.
fn strip_managed_settings() -> anyhow::Result<()> {
    let path = settings_path();
    match std::fs::read_to_string(&path) {
        Ok(data) => {
            let root: Value = serde_json::from_str(&data)
                .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", path.display()))?;
            let (root, removed) = remove_managed_env(root);
            if removed.is_empty() {
                println!("no parsec-managed env keys found in {}", path.display());
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
    println!("auto-setup is now off. Re-enable with: parsec setup");
    Ok(())
}

/// Minimal one-shot HTTP exchange with the local proxy. Raw TcpStream on
/// purpose: no client dep, works the same on every platform, and short
/// timeouts keep uninstall snappy when nothing is listening.
pub(crate) fn proxy_request(port: u16, method: &str, path: &str) -> Option<String> {
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

/// The version a parsec proxy on `port` reports via GET /health, or None
/// when nothing parsec-shaped answers. Lets the SessionStart hook spot a
/// proxy left serving by a pre-update binary.
/// The wire namespaces this build's router actually serves, advertised on
/// `/health` as `wires`. This exists because `version` cannot do the job:
/// every plugin build reports the crate version (0.1.0), so a supervisor from
/// an older plugin release is indistinguishable from the current one by
/// version alone — which is exactly how an alpha-9 process kept port 8082 and
/// answered `/health` 200 while 404-ing every Codex route, letting
/// `parsec setup codex` report success
/// (bugs/parsec_codex_user_message_trimming_report.md, "Additional setup
/// defect"). Add an entry here whenever a new namespace is routed; setup
/// flows require the ones they depend on.
pub const SERVED_WIRES: &[&str] = &["anthropic", "openai", "chatgpt"];

fn parse_health_wires(resp: &str) -> Option<Vec<String>> {
    let (_, body) = resp.split_once("\r\n\r\n")?;
    let v: Value = serde_json::from_str(body.trim()).ok()?;
    if v.get("service").and_then(Value::as_str) != Some("parsec-proxy") {
        return None;
    }
    Some(
        v.get("wires")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
    )
}

/// What the process on `port` is, from the point of view of a setup flow that
/// needs `required` wires served.
#[derive(Debug, PartialEq, Eq)]
pub enum PortOccupant {
    /// Nothing is listening — spawn freely.
    Free,
    /// Ours, and it serves every required wire.
    Compatible,
    /// Ours (or at least answers as ours) but cannot serve what is needed —
    /// an older build. Safe to shut down and replace.
    StaleParsec,
    /// Answers, but is not the parsec proxy. NEVER shut down: it belongs to
    /// something else on this machine.
    Foreign,
}

/// Classify the listener on `port` against the wires a setup flow needs.
/// A build that answers `/health` as ours without a `wires` field predates
/// the capability advertisement and is treated as stale — it is, by
/// definition, older than every build that can prove itself.
pub fn classify_port(port: u16, required: &[&str]) -> PortOccupant {
    if !crate::hook::port_listening(port) {
        return PortOccupant::Free;
    }
    let Some(resp) = proxy_request(port, "GET", "/health") else {
        return PortOccupant::Foreign; // listening but mute: not ours to stop
    };
    if !resp.contains("parsec-proxy") {
        return PortOccupant::Foreign;
    }
    match parse_health_wires(&resp) {
        Some(wires) if required.iter().all(|r| wires.iter().any(|w| w == r)) => {
            PortOccupant::Compatible
        }
        _ => PortOccupant::StaleParsec,
    }
}

pub(crate) fn proxy_health_version(port: u16) -> Option<String> {
    parse_health_version(&proxy_request(port, "GET", "/health")?)
}

/// Extract `version` from a raw /health HTTP exchange, but only when the
/// body identifies as ours — a foreign server's response never yields a
/// version, so callers can't be tricked into managing it. Pure for tests.
fn parse_health_version(resp: &str) -> Option<String> {
    let (_, body) = resp.split_once("\r\n\r\n")?;
    let v: Value = serde_json::from_str(body.trim()).ok()?;
    if v.get("service").and_then(Value::as_str) != Some("parsec-proxy") {
        return None;
    }
    v.get("version").and_then(Value::as_str).map(String::from)
}

/// Stop a proxy on `port`, but only after /health proves the listener is OURS
/// — a foreign server that happens to sit on the port is left alone.
fn stop_proxy(port: u16) -> String {
    if !crate::hook::port_listening(port) {
        return format!("no proxy listening on 127.0.0.1:{port}");
    }
    match proxy_request(port, "GET", "/health") {
        Some(h) if h.contains("parsec-proxy") => {
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
            format!("port {port} is serving something that is not the parsec proxy — left alone")
        }
        None => format!("listener on port {port} did not answer a health probe — left alone"),
    }
}

/// Delete every parsec-owned data file under `home` EXCEPT the state file
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

/// `parsec uninstall` — full local cleanup, run BEFORE `claude plugin
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
    // The opencode shim routes at the same proxy — a full uninstall must not
    // leave it pointing at a port nothing will listen on again.
    crate::setup_opencode::remove_if_managed();
    crate::setup_codex::remove_if_managed();
    // Same reason for Claude Desktop: an interceptor left running would keep
    // redirecting Desktop at a port nothing answers on.
    crate::setup_desktop::remove_if_managed();
    println!("{}", stop_proxy(port));
    let home = parsec_home();
    let (removed, failed) = purge_data_files(&home);
    if !removed.is_empty() {
        println!("removed from {}: {}", home.display(), removed.join(", "));
    }
    for f in &failed {
        eprintln!("could not remove {f} — delete by hand");
    }
    println!(
        "local cleanup done. To finish:\n  1. claude plugin uninstall parsec@parsec-marketplace\n  \
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
    /// ANTHROPIC_BASE_URL now points at a local parsec proxy (whether we
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
    // PARSEC_EMBED_BACKEND=onnx + PARSEC_ONNX_DIR.
    Ok((root, out))
}

/// The managed statusLine command for `exe`. Quoted — the plugin cache path
/// the binary lives under moves on every version bump and may contain spaces.
fn statusline_cmd(exe: &str) -> String {
    format!("\"{exe}\" statusline")
}

/// A statusLine value setup wrote (under any past binary path) — a
/// user-authored command never matches and is never touched. The
/// `subagent-statusline` exclusion matters: that command also ends in
/// "statusline", so without it our own subagent entry would read as a
/// managed statusLine.
fn is_managed_statusline(v: &Value) -> bool {
    v.pointer("/command")
        .and_then(Value::as_str)
        .map(|c| {
            let c = c.trim_end();
            c.contains("parsec") && c.ends_with("statusline") && !c.ends_with("subagent-statusline")
        })
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

/// The managed subagentStatusLine command for `exe`. Quoted for the same
/// reason as `statusline_cmd`.
fn subagent_statusline_cmd(exe: &str) -> String {
    format!("\"{exe}\" subagent-statusline")
}

/// A subagentStatusLine value setup wrote (under any past binary path).
fn is_managed_subagent_statusline(v: &Value) -> bool {
    v.pointer("/command")
        .and_then(Value::as_str)
        .map(|c| c.contains("parsec") && c.trim_end().ends_with("subagent-statusline"))
        .unwrap_or(false)
}

/// Merge the managed subagentStatusLine into a settings root. A plugin's own
/// settings.json CAN carry this key, but its value is read verbatim —
/// `${CLAUDE_PLUGIN_ROOT}` is only expanded for hooks declared in
/// hooks/hooks.json (verified against CC 2.1.220), so a plugin-shipped entry
/// could not name our binary. Hence the same user-settings merge as
/// statusLine, with identical ownership rules. Pure for tests.
fn merge_subagent_statusline(mut root: Value, exe: &str) -> anyhow::Result<(Value, bool)> {
    if root.is_null() {
        root = serde_json::json!({});
    }
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings root is not a JSON object"))?;
    let want = serde_json::json!({"type": "command", "command": subagent_statusline_cmd(exe)});
    let changed = match obj.get("subagentStatusLine") {
        None => {
            obj.insert("subagentStatusLine".into(), want);
            true
        }
        Some(v) if is_managed_subagent_statusline(v) && *v != want => {
            obj.insert("subagentStatusLine".into(), want);
            true
        }
        Some(_) => false,
    };
    Ok((root, changed))
}

/// The sentinel verb that marks a spinnerVerbs block as ours. Present in
/// every set we have ever written, so ownership survives verb-list edits
/// across versions the way the binary path does for statusLine.
const SPINNER_SENTINEL: &str = "Parsecing";

/// Parsec-flavoured spinner verbs, appended to Claude Code's defaults. Append
/// (not replace) on purpose: the stock verbs are part of the harness's
/// personality and we are a guest in it.
const SPINNER_VERBS: &[&str] = &[
    SPINNER_SENTINEL,
    "Compacting",
    "Condensing",
    "Deduping",
    "Distilling",
    "Eliding",
    "Pruning",
    "Tightening",
    "Traversing",
    "Trimming",
];

/// A spinnerVerbs value setup wrote — identified by our sentinel verb, not by
/// an exact list match, so a version that ships different verbs still
/// recognises (and updates) its own block. A user-authored list never
/// contains the sentinel and is never touched.
fn is_managed_spinner_verbs(v: &Value) -> bool {
    v.pointer("/verbs")
        .and_then(Value::as_array)
        .map(|verbs| verbs.iter().any(|x| x.as_str() == Some(SPINNER_SENTINEL)))
        .unwrap_or(false)
}

/// Merge the managed spinnerVerbs into a settings root. Not a key plugins can
/// ship (plugin settings.json supports only agent / subagentStatusLine), so
/// it rides the same managed-settings write with the same ownership rules.
/// Pure for tests.
fn merge_spinner_verbs(mut root: Value) -> anyhow::Result<(Value, bool)> {
    if root.is_null() {
        root = serde_json::json!({});
    }
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings root is not a JSON object"))?;
    let want = serde_json::json!({"mode": "append", "verbs": SPINNER_VERBS});
    let changed = match obj.get("spinnerVerbs") {
        None => {
            obj.insert("spinnerVerbs".into(), want);
            true
        }
        Some(v) if is_managed_spinner_verbs(v) && *v != want => {
            obj.insert("spinnerVerbs".into(), want);
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
    for (key, ours) in [
        ("statusLine", is_managed_statusline as fn(&Value) -> bool),
        ("subagentStatusLine", is_managed_subagent_statusline),
        ("spinnerVerbs", is_managed_spinner_verbs),
    ] {
        if root.get(key).map(ours).unwrap_or(false) {
            if let Some(obj) = root.as_object_mut() {
                obj.remove(key);
                removed.push(key.to_string());
            }
        }
    }
    if let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) {
        let ours = |k: &str, v: &Value| match (k, v.as_str()) {
            ("ANTHROPIC_BASE_URL", Some(s)) => crate::hook::local_proxy_port(s).is_some(),
            ("PARSEC_EMBED_BACKEND", Some(s)) => s == "onnx",
            ("PARSEC_ONNX_DIR", Some(_)) => true,
            _ => false,
        };
        for k in [
            "ANTHROPIC_BASE_URL",
            "PARSEC_EMBED_BACKEND",
            "PARSEC_ONNX_DIR",
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
                "cannot parse {}: {e} — fix it, then run `parsec setup`",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(e.into()),
    };
    let (root, mut outcome) = merge_settings(root, port)?;
    // The brand surfaces ride the same managed-settings write (delivery
    // mechanism of docs/plugin-user-messaging.md Part 1 §1 and §6 — plugins
    // cannot ship these keys themselves, or cannot resolve our binary path
    // in the one case where they can).
    let root = match std::env::current_exe() {
        Ok(exe) => {
            let exe = exe.to_string_lossy().into_owned();
            let (root, changed) = merge_statusline(root, &exe)?;
            outcome.changed |= changed;
            let (root, changed) = merge_subagent_statusline(root, &exe)?;
            outcome.changed |= changed;
            root
        }
        Err(_) => root,
    };
    let (root, changed) = merge_spinner_verbs(root)?;
    outcome.changed |= changed;
    if outcome.changed {
        write_settings_file(&path, &root)?;
    }
    Ok(outcome)
}

fn write_settings_file(path: &Path, root: &Value) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.parsec-tmp");
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

// ── per-account API key (`parsec key …`) ────────────────────────────────────

/// `parsec key set <psc_…> [--platform-url URL]` — store the account's API key
/// (and optional platform URL) in ~/.parsec/credentials.json. The proxy reads
/// it live per shipped row, so this takes effect on the NEXT request — no
/// restart. `PARSEC_API_KEY` in the env still overrides the file if set.
pub fn key_set(key: String, platform_url: Option<String>) -> anyhow::Result<()> {
    let key = key.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("empty key — pass the psc_ key minted in the dashboard");
    }
    if !key.starts_with("psc_") {
        println!("warning: key does not start with 'psc_' — storing it anyway");
    }
    let mut creds = crate::credentials::load();
    creds.api_key = Some(key.clone());
    if let Some(url) = platform_url {
        let url = url.trim().trim_end_matches('/').to_string();
        creds.platform_url = (!url.is_empty()).then_some(url);
    }
    crate::credentials::store(&creds)?;
    println!(
        "saved API key {} to {} — savings now report to your dashboard on the next \
         request (no restart needed).",
        crate::credentials::mask(&key),
        crate::credentials::path().display()
    );
    if std::env::var("PARSEC_API_KEY").is_ok_and(|v| !v.is_empty()) {
        println!(
            "note: PARSEC_API_KEY is set in the environment and OVERRIDES this file — \
             unset it to use the stored key."
        );
    }
    Ok(())
}

/// `parsec key show` — the configured key (masked) and where it resolves from.
pub fn key_show() -> anyhow::Result<()> {
    let env_key = std::env::var("PARSEC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    let creds = crate::credentials::load();
    match (&env_key, &creds.api_key) {
        (Some(k), _) => println!(
            "API key: {} (from PARSEC_API_KEY env)",
            crate::credentials::mask(k)
        ),
        (None, Some(k)) => println!(
            "API key: {} (from {})",
            crate::credentials::mask(k),
            crate::credentials::path().display()
        ),
        (None, None) => println!("API key: not set — run `parsec key set <psc_…>`"),
    }
    match crate::ledger_ship::resolve() {
        Some(_) => println!("shipping: active (key + platform URL both resolved)"),
        None => println!(
            "shipping: inactive — need both an API key and a platform URL (baked in \
             release builds; set PARSEC_PLATFORM_URL or `--platform-url` on a dev build)"
        ),
    }
    Ok(())
}

/// `parsec key clear` — remove the stored key (stops dashboard reporting).
pub fn key_clear() -> anyhow::Result<()> {
    crate::credentials::clear()?;
    println!(
        "cleared stored API key ({})",
        crate::credentials::path().display()
    );
    if std::env::var("PARSEC_API_KEY").is_ok_and(|v| !v.is_empty()) {
        println!("note: PARSEC_API_KEY is still set in the environment — unset it to fully stop reporting.");
    }
    Ok(())
}

// ── detached spawns (shared with the SessionStart hook) ─────────────────────

/// Spawn `parsec proxy` (the SUPERVISOR) on `port`, detached, logging to
/// ~/.parsec/proxy.log. The supervisor owns the port for its whole life and
/// spawns/restarts the curating worker itself — there is no idle self-exit
/// any more (removed 2026-07-21: it wedged still-active sessions that went
/// briefly idle, and a self-killing worker would just be respawned).
pub fn spawn_proxy_detached(port: u16, extra_env: &[(String, String)]) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("proxy").env("PARSEC_PROXY_PORT", port.to_string());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    spawn_detached(cmd, "proxy.log")
}

/// Spawn `parsec setup --auto` detached, logging to ~/.parsec/setup.log.
pub fn spawn_setup_detached() -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("setup").arg("--auto");
    spawn_detached(cmd, "setup.log")
}

fn spawn_detached(mut cmd: std::process::Command, log_name: &str) -> anyhow::Result<()> {
    let log_dir = parsec_home();
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

    #[cfg(unix)]
    #[test]
    fn rc_file_matches_the_users_login_shell() {
        let home = Path::new("/home/u");
        assert_eq!(
            rc_for_shell("/bin/zsh", None, None, home),
            home.join(".zshrc")
        );
        assert_eq!(
            rc_for_shell("/bin/zsh", Some("/home/u/cfg/zsh"), None, home),
            Path::new("/home/u/cfg/zsh/.zshrc")
        );
        assert_eq!(
            rc_for_shell("/bin/bash", None, None, home),
            home.join(".bashrc")
        );
        // fish gets its own conf.d drop-in — appending `export PATH=` to a
        // fish config would write a line fish never runs.
        assert_eq!(
            rc_for_shell("/usr/local/bin/fish", None, None, home),
            home.join(".config/fish/conf.d/parsec.fish")
        );
        assert_eq!(
            rc_for_shell("/usr/bin/fish", None, Some("/xdg"), home),
            Path::new("/xdg/fish/conf.d/parsec.fish")
        );
        // Unknown or empty $SHELL falls back to the POSIX profile.
        assert_eq!(
            rc_for_shell("/bin/ksh", None, None, home),
            home.join(".profile")
        );
        assert_eq!(rc_for_shell("", None, None, home), home.join(".profile"));
    }

    #[test]
    fn merge_writes_routing_into_empty_settings() {
        let (root, out) = merge_settings(Value::Null, 8082).unwrap();
        assert!(out.routed && out.changed && out.conflict.is_none());
        assert_eq!(root["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8082");
        // routing is the ONLY managed env key now — no embedder to configure
        assert!(root["env"]["PARSEC_EMBED_BACKEND"].is_null());
        assert!(root["env"]["PARSEC_ONNX_DIR"].is_null());
    }

    #[test]
    fn merge_never_touches_user_values() {
        let existing = json!({
            "model": "opus",
            "env": {
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:9999",
                "PARSEC_EMBED_BACKEND": "remote",
                "FOO": "bar"
            }
        });
        let (root, out) = merge_settings(existing, 8082).unwrap();
        assert!(out.routed); // 9999 is still a local parsec-shaped proxy
        assert_eq!(out.conflict, None);
        assert_eq!(root["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9999");
        assert_eq!(root["env"]["PARSEC_EMBED_BACKEND"], "remote"); // user's key untouched
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
        let (root, changed) = merge_statusline(Value::Null, "/cache/v1/bin/parsec").unwrap();
        assert!(changed);
        assert_eq!(root["statusLine"]["type"], "command");
        assert_eq!(
            root["statusLine"]["command"],
            "\"/cache/v1/bin/parsec\" statusline"
        );

        // Ours under an old binary path → repointed (plugin version bump).
        let (root, changed) = merge_statusline(root, "/cache/v2/bin/parsec").unwrap();
        assert!(changed);
        assert_eq!(
            root["statusLine"]["command"],
            "\"/cache/v2/bin/parsec\" statusline"
        );

        // Idempotent at the same path.
        let (root, changed) = merge_statusline(root, "/cache/v2/bin/parsec").unwrap();
        assert!(!changed);

        // User-authored statusLine: never touched, and never removed.
        let mut user = root;
        user["statusLine"] = json!({"type": "command", "command": "~/bin/my-status.sh"});
        let (user, changed) = merge_statusline(user, "/cache/v3/bin/parsec").unwrap();
        assert!(!changed);
        assert_eq!(user["statusLine"]["command"], "~/bin/my-status.sh");
        let (user, removed) = remove_managed_env(user);
        assert!(removed.is_empty());
        assert_eq!(user["statusLine"]["command"], "~/bin/my-status.sh");
    }

    #[test]
    fn subagent_statusline_merge_writes_repoints_ours_never_users() {
        let (root, changed) =
            merge_subagent_statusline(Value::Null, "/cache/v1/bin/parsec").unwrap();
        assert!(changed);
        assert_eq!(root["subagentStatusLine"]["type"], "command");
        assert_eq!(
            root["subagentStatusLine"]["command"],
            "\"/cache/v1/bin/parsec\" subagent-statusline"
        );

        // Ours under an old binary path → repointed; idempotent at the same one.
        let (root, changed) = merge_subagent_statusline(root, "/cache/v2/bin/parsec").unwrap();
        assert!(changed);
        let (root, changed) = merge_subagent_statusline(root, "/cache/v2/bin/parsec").unwrap();
        assert!(!changed);

        // A managed statusLine must NOT be mistaken for a managed
        // subagentStatusLine: "…parsec\" statusline" does not end in
        // "subagent-statusline".
        let (root, _) = merge_statusline(root, "/cache/v2/bin/parsec").unwrap();
        assert!(!is_managed_subagent_statusline(&root["statusLine"]));
        assert!(!is_managed_statusline(&root["subagentStatusLine"]));

        // User-authored: never touched, never removed.
        let mut user = root;
        user["subagentStatusLine"] = json!({"type": "command", "command": "~/bin/mine.sh"});
        let (user, changed) = merge_subagent_statusline(user, "/cache/v3/bin/parsec").unwrap();
        assert!(!changed);
        let (user, removed) = remove_managed_env(user);
        assert!(!removed.contains(&"subagentStatusLine".to_string()));
        assert_eq!(user["subagentStatusLine"]["command"], "~/bin/mine.sh");
    }

    #[test]
    fn spinner_verbs_merge_writes_updates_ours_never_users() {
        let (root, changed) = merge_spinner_verbs(Value::Null).unwrap();
        assert!(changed);
        assert_eq!(root["spinnerVerbs"]["mode"], "append");
        assert_eq!(root["spinnerVerbs"]["verbs"][0], SPINNER_SENTINEL);

        // Idempotent.
        let (root, changed) = merge_spinner_verbs(root).unwrap();
        assert!(!changed);

        // An older set of ours (sentinel present, verbs differ) → refreshed.
        let mut stale = root;
        stale["spinnerVerbs"] = json!({"mode": "append", "verbs": [SPINNER_SENTINEL, "Warping"]});
        let (refreshed, changed) = merge_spinner_verbs(stale).unwrap();
        assert!(changed);
        assert_eq!(
            refreshed["spinnerVerbs"]["verbs"].as_array().unwrap().len(),
            SPINNER_VERBS.len()
        );

        // User-authored (no sentinel): never touched, never removed.
        let mut user = refreshed;
        user["spinnerVerbs"] = json!({"mode": "replace", "verbs": ["Vibing"]});
        let (user, changed) = merge_spinner_verbs(user).unwrap();
        assert!(!changed);
        let (user, removed) = remove_managed_env(user);
        assert!(!removed.contains(&"spinnerVerbs".to_string()));
        assert_eq!(user["spinnerVerbs"]["verbs"][0], "Vibing");
    }

    #[test]
    fn disable_removes_all_managed_brand_surfaces() {
        let root = json!({
            "statusLine": {"type": "command", "command": "\"/cache/v1/bin/parsec\" statusline"},
            "subagentStatusLine": {
                "type": "command",
                "command": "\"/cache/v1/bin/parsec\" subagent-statusline"
            },
            "spinnerVerbs": {"mode": "append", "verbs": [SPINNER_SENTINEL, "Trimming"]},
            "env": {"FOO": "bar"}
        });
        let (root, removed) = remove_managed_env(root);
        assert_eq!(
            removed,
            vec!["statusLine", "subagentStatusLine", "spinnerVerbs"]
        );
        assert!(root.get("subagentStatusLine").is_none());
        assert!(root.get("spinnerVerbs").is_none());
        assert_eq!(root["env"]["FOO"], "bar");
    }

    #[test]
    fn disable_removes_managed_statusline() {
        let root = json!({
            "statusLine": {"type": "command", "command": "\"/cache/v1/bin/parsec\" statusline"},
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
                "PARSEC_EMBED_BACKEND": "onnx",
                "PARSEC_ONNX_DIR": "/home/u/.parsec/models/bge-large-onnx",
                "FOO": "bar"
            }
        });
        let (root, removed) = remove_managed_env(root);
        assert_eq!(
            removed,
            vec![
                "ANTHROPIC_BASE_URL",
                "PARSEC_EMBED_BACKEND",
                "PARSEC_ONNX_DIR"
            ]
        );
        assert_eq!(root["env"]["FOO"], "bar");
        assert!(root["env"].get("ANTHROPIC_BASE_URL").is_none());

        // customized values stay: foreign URL, non-onnx backend
        let root = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://my-gateway.corp",
                "PARSEC_EMBED_BACKEND": "remote"
            }
        });
        let (root, removed) = remove_managed_env(root);
        assert!(removed.is_empty());
        assert_eq!(root["env"]["ANTHROPIC_BASE_URL"], "https://my-gateway.corp");
    }

    #[test]
    fn purge_keeps_only_the_disabled_marker() {
        let dir = std::env::temp_dir().join(format!("parsec-purge-test-{}", std::process::id()));
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
    fn health_version_parses_ours_and_rejects_foreign() {
        let ours = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n\
                    {\"ok\":true,\"service\":\"parsec-proxy\",\"role\":\"supervisor\",\
                    \"version\":\"0.1.3\"}";
        assert_eq!(parse_health_version(ours).as_deref(), Some("0.1.3"));

        // A foreign server on the port must never yield a version, whatever
        // it answers — that is the identity gate for the hook's restart.
        let foreign = "HTTP/1.1 200 OK\r\n\r\n{\"service\":\"my-gateway\",\"version\":\"9.9\"}";
        assert_eq!(parse_health_version(foreign), None);
        assert_eq!(
            parse_health_version("HTTP/1.1 200 OK\r\n\r\nnot json"),
            None
        );
        assert_eq!(parse_health_version(""), None);

        // Ours but versionless (a pre-/health-version build): None — the
        // caller treats it as "cannot compare, leave alone".
        let old = "HTTP/1.1 200 OK\r\n\r\n{\"ok\":true,\"service\":\"parsec-proxy\"}";
        assert_eq!(parse_health_version(old), None);
    }

    fn health(body: &str) -> String {
        format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{body}")
    }

    #[test]
    fn health_wires_parse_only_from_our_own_service() {
        assert_eq!(
            parse_health_wires(&health(
                r#"{"service":"parsec-proxy","wires":["anthropic","openai","chatgpt"]}"#
            )),
            Some(vec![
                "anthropic".to_string(),
                "openai".to_string(),
                "chatgpt".to_string()
            ])
        );
        // A build that predates the field cannot prove itself.
        assert_eq!(
            parse_health_wires(&health(r#"{"service":"parsec-proxy","version":"0.1.0"}"#)),
            None
        );
        // Somebody else's server never yields wires, whatever it claims.
        assert_eq!(
            parse_health_wires(&health(r#"{"service":"other","wires":["openai"]}"#)),
            None
        );
    }

    #[test]
    fn classify_port_reports_free_when_nothing_listens() {
        // Port 1 is never listening.
        assert_eq!(classify_port(1, SERVED_WIRES), PortOccupant::Free);
    }

    #[test]
    fn parsec_owns_rejects_a_listener_that_is_not_ours() {
        // The identity half of the misroute guard, which liveness cannot give:
        // codex bakes the chosen port into config.toml as literal text and
        // nothing re-validates it, so mistaking a stranger for our supervisor
        // points a real session's OAuth Bearer at that process rather than
        // merely failing. Same accept-and-drop squatter as the scan test
        // above, so the probe fails fast instead of waiting out the timeout.
        let squat = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = squat.local_addr().unwrap().port();
        let held = std::thread::spawn(move || {
            for _ in 0..2 {
                if let Ok((s, _)) = squat.accept() {
                    drop(s); // FIN → the probe reads "" and returns immediately
                }
            }
        });
        assert!(!port_bindable(port), "squatter should hold the port");
        assert!(
            !parsec_owns(port),
            "a foreign listener must never read as ours"
        );
        assert_eq!(classify_port(port, SERVED_WIRES), PortOccupant::Foreign);
        let _ = held.join();
    }

    #[test]
    fn served_wires_covers_every_namespace_setup_flows_require() {
        for w in ["anthropic", "openai", "chatgpt"] {
            assert!(SERVED_WIRES.contains(&w), "{w} missing from SERVED_WIRES");
        }
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
