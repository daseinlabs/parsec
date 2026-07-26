//! `parsec hook <event>` — Claude Code hook handler (free tier).
//!
//! Protocol (code.claude.com/docs/en/hooks): JSON on stdin; to deny a
//! PreToolUse call, print `hookSpecificOutput.permissionDecision: "deny"`
//! with a reason and exit 0. PostToolUse records what actually happened.
//! Hooks must NEVER fail the session: any internal error exits 0 silently
//! (fail-open, DIRECTION.md §8.3) — but is counted in the session file so a
//! silently dead hook is distinguishable from a quiet one.

use serde_json::{json, Value};

use crate::adjudicator;
use crate::noreread::{
    load_session, prune_sessions, read_tool_range, save_session, Gate, SessionState,
};

pub fn run(event: &str) -> anyhow::Result<()> {
    let mut input = String::new();
    // A stdin read error must not become a non-zero exit (hooks NEVER fail
    // the session) — treat it as an empty payload and fall through.
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("/")
        .to_string();
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool_input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or(event)
        .to_string();

    match event.as_str() {
        "PreToolUse" => {
            // Entitlement gate (apikey): no key ⇒ parsec saves nothing, so the
            // no-reread / loop-breaker never fires and no state accrues.
            if !crate::apikey::enabled() {
                return Ok(());
            }
            let mut st = load_session(&session_id);
            let gate = match tool {
                "Read" => match read_tool_range(&tool_input, &cwd) {
                    Some((path, rng)) => st.gate_read(&path, rng),
                    None => Gate::Allow,
                },
                "Bash" => match tool_input.get("command").and_then(Value::as_str) {
                    Some(cmd) => st.gate_bash(cmd, &cwd),
                    None => Gate::Allow,
                },
                _ => Gate::Allow,
            };
            let _ = save_session(&session_id, &st); // denial/savings counters
            if let Gate::Deny { reason, .. } = gate {
                println!(
                    "{}",
                    json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "deny",
                            "permissionDecisionReason": reason,
                        },
                        "suppressOutput": true,
                    })
                );
            }
        }
        "PostToolUse" => {
            // Same gate as PreToolUse: unentitled ⇒ record nothing.
            if !crate::apikey::enabled() {
                return Ok(());
            }
            let mut st = load_session(&session_id);
            record_post(&mut st, tool, &tool_input, &cwd);
            let _ = save_session(&session_id, &st);
        }
        "SessionStart" => {
            prune_sessions(7);
            let mut msgs = Vec::new();
            let is_startup = payload.get("source").and_then(Value::as_str) == Some("startup");
            // Top of the session (and the install flow — first run is a
            // startup): if there is no API key, parsec saves nothing — show the
            // prominent get-a-key banner. Fresh startups only (resume/clear/
            // compact must not re-nag). Single source: apikey::gate_banner.
            if is_startup {
                if let Some(m) = crate::apikey::gate_banner() {
                    msgs.push(m);
                }
            }
            if let Some(m) = maybe_autosetup() {
                msgs.push(m);
            }
            if let Some(m) = maybe_autostart_proxy() {
                msgs.push(m);
            }
            // One-time awareness line (docs/plugin-user-messaging.md Part 1
            // §3): fresh startups only — resume/clear/compact re-fire
            // SessionStart and must not re-nag.
            if is_startup {
                if let Some(m) = crate::statusline::lifetime_note() {
                    msgs.push(m);
                }
            }
            if !msgs.is_empty() {
                println!(
                    "{}",
                    json!({
                        "systemMessage": crate::brand::notice(&msgs),
                        "suppressOutput": true
                    })
                );
            }
        }
        "Stop" => {
            // Already a forced continuation from a previous Stop block —
            // never pin the agent in a loop: exit silently, no row (the
            // reference cc_runner.py:376-378 semantics).
            if payload
                .get("stop_hook_active")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(());
            }
            run_stop(&payload, &session_id, &cwd);
        }
        _ => {}
    }
    Ok(())
}

/// The continue directive fed back on a blocked stop — the reference steer
/// (arms/parsec.py `_CONTINUE_STEER` + the DELIVER text) adapted to the
/// "stopped with no submittable work product while looping" case.
const STOP_BLOCK_REASON: &str = "Your work is not yet complete: there is no submittable edit on \
disk and your recent actions were repeating without producing new information. Do not stop yet — \
take a DIFFERENT concrete step toward the code change that resolves the task, make the edit, \
verify it, then stop.";

/// Stop-hook SUBMIT adjudicator (Track C). `PARSEC_ADJUDICATOR` =
/// - `advise` (default): record a JSONL row, print NOTHING;
/// - `block`: additionally print ONE `{"decision":"block"}` when the stop
///   looks premature (CONTINUE verdict, no submittable edit, mechanical
///   stall) within the per-session `PARSEC_ADJ_MAX_BLOCKS` budget (default
///   2 — the governor's validated coach+bank ceiling);
/// - `off`: skip entirely (no row).
///
/// Fail-open discipline: every internal error path returns silently (exit
/// 0); stdout carries ONLY the block-decision JSON, never anything else.
fn run_stop(payload: &Value, session_id: &str, cwd: &str) {
    let mode_env = std::env::var("PARSEC_ADJUDICATOR").unwrap_or_default();
    let mode = match mode_env.trim() {
        "off" => return,
        "block" => "block",
        _ => "advise",
    };
    let messages = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(|p| {
            adjudicator::messages_from_transcript(p, adjudicator::MAX_MSGS, adjudicator::OBS_CAP)
        })
        .unwrap_or_default();
    // Pure function of the transcript bytes — the probe below is telemetry
    // only and never feeds the verdict.
    let adj = adjudicator::adjudicate(&messages);
    let probe = adjudicator::disk_probe(cwd);
    let mut blocked = false;
    if mode == "block" && adj.verdict == "CONTINUE" && !adj.has_edit && adj.mech_stalled {
        let max_blocks: u32 = std::env::var("PARSEC_ADJ_MAX_BLOCKS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(2);
        let mut st = load_session(session_id);
        if st.adj_blocks < max_blocks {
            st.note_adj_block();
            // The budget must be durable BEFORE we block: an unsaved counter
            // could block every stop, so a failed save downgrades to advise.
            if save_session(session_id, &st).is_ok() {
                blocked = true;
                println!(
                    "{}",
                    json!({ "decision": "block", "reason": STOP_BLOCK_REASON })
                );
            }
        }
    }
    let _ = adjudicator::append_row(&adjudicator::row(&adj, session_id, mode, probe, blocked));
}

/// First-run auto-setup (the "whole experience in one go" install flow): no
/// setup state on disk means this plugin has never activated on this machine
/// — spawn `parsec setup --auto` detached (download embedder → write routing
/// env → warm proxy) and tell the user plainly what is happening, including
/// how to undo it. Later sessions surface progress / the restart nudge /
/// failures from the state file. Deliberately LOUD: setup rewrites the
/// user's API routing, and that must never happen silently.
///
/// `PARSEC_AUTOSETUP=0` opts out of both the spawn and the messaging.
/// Terminal states (`disabled`, `unsupported`) are permanently silent.
fn maybe_autosetup() -> Option<String> {
    if std::env::var("PARSEC_AUTOSETUP").ok().as_deref() == Some("0") {
        return None;
    }
    let spawn_first_run = |verb: &str| -> String {
        // Claim the slot BEFORE spawning so two sessions starting together
        // don't both spawn a downloader; the spawned setup consumes exactly
        // this `spawned` phase and flips it to `downloading`.
        let mut st = crate::setup::SetupState {
            contract_version: crate::setup::STATE_CONTRACT.into(),
            phase: "spawned".into(),
            port: crate::setup::default_port(),
            updated_unix: unix_now(),
            ..Default::default()
        };
        if let Err(e) = crate::setup::save_state(&st) {
            return format!("⌁ parsec: first-run setup could not record state ({e}) — skipped");
        }
        match crate::setup::spawn_setup_detached() {
            Ok(()) => format!(
                "⌁ parsec: {verb} — downloading the local embedder (~1.3 GB) to \
                 ~/.parsec/models in the background. When it finishes, Claude Code's \
                 settings gain an env block routing API traffic through the local parsec \
                 proxy (127.0.0.1 only); curation activates on your next session. \
                 Undo: `parsec disable` · opt out: PARSEC_AUTOSETUP=0 · log: ~/.parsec/setup.log"
            ),
            Err(e) => {
                st.phase = "failed".into();
                st.error = Some(format!("spawn: {e}"));
                let _ = crate::setup::save_state(&st);
                format!("⌁ parsec: first-run setup failed to start ({e}) — run `parsec setup`")
            }
        }
    };
    match crate::setup::load_state() {
        None => Some(spawn_first_run("first-run setup started")),
        Some(st) => match st.phase.as_str() {
            "spawned" | "routing" if st.stale() => Some(spawn_first_run("setup resumed")),
            "spawned" | "routing" => {
                let pct = "starting".to_string();
                Some(format!(
                    "⌁ parsec: embedder download in progress ({pct}) — curation activates \
                     the session after it completes"
                ))
            }
            // Routed sessions get env at launch; reaching here with the env
            // unset means the pre-restart window — or the managed env was
            // removed from settings.json (a plugin update/reinstall makes
            // Claude Code rewrite it from memory). `env_written` only records
            // history, so verify against the file and re-assert the routing
            // (additive merge — a key the user set is never overwritten).
            // `parsec disable` remains the supported off-switch.
            "ready" if std::env::var("ANTHROPIC_BASE_URL").is_err() => {
                match crate::setup::ensure_routing(st.port) {
                    Ok(out) => {
                        let mut st2 = st.clone();
                        st2.env_written = out.routed;
                        st2.base_url_conflict = out.conflict.clone();
                        if st2 != st {
                            st2.updated_unix = unix_now();
                            let _ = crate::setup::save_state(&st2);
                        }
                        match (out.conflict, out.changed) {
                            (Some(url), _) => Some(format!(
                                "⌁ parsec: setup complete, but ANTHROPIC_BASE_URL was already \
                                 {url} — curation is NOT active. Remove it, then run \
                                 `parsec setup` (undo: `parsec disable`)"
                            )),
                            (None, true) => Some(
                                "⌁ parsec: routing was missing from Claude Code settings (a \
                                 plugin update can rewrite them) — restored. Restart Claude \
                                 Code to activate curation (undo: `parsec disable`)"
                                    .into(),
                            ),
                            (None, false) => Some(
                                "⌁ parsec: setup complete — restart Claude Code to activate \
                                 curation (undo: `parsec disable`)"
                                    .into(),
                            ),
                        }
                    }
                    Err(e) => Some(format!(
                        "⌁ parsec: curation routing is missing from Claude Code settings \
                         and could not be restored ({e}) — run `parsec setup`"
                    )),
                }
            }
            "ready" => None,
            "failed" => Some(format!(
                "⌁ parsec: setup failed ({}) — retry with `parsec setup` \
                 (log: ~/.parsec/setup.log)",
                st.error.as_deref().unwrap_or("unknown error")
            )),
            _ => None, // unsupported | disabled: terminal, silent
        },
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The Pro flip-on (DIRECTION.md §7: "plugin → proxy (manages)"): when this
/// session is ROUTED through a local parsec proxy (ANTHROPIC_BASE_URL points
/// at a loopback port) and nothing is listening there yet, spawn
/// `parsec proxy` detached so the session's first request doesn't hit a dead
/// port. The hook inherits the session env, so PARSEC_BRAIN_URL /
/// PARSEC_BRAIN_CONTRACT / PARSEC_EMBED_* configured in settings.json `env`
/// flow into the spawned proxy. What a hook CANNOT do is set
/// ANTHROPIC_BASE_URL itself — routing must exist at launch (settings env or
/// shell). Opt out with PARSEC_PROXY_AUTOSTART=0. Returns a user-visible
/// message when it acted (or failed — a dead routed port breaks the session,
/// which must never be silent).
fn maybe_autostart_proxy() -> Option<String> {
    if std::env::var("PARSEC_PROXY_AUTOSTART").ok().as_deref() == Some("0") {
        return None;
    }
    let base = std::env::var("ANTHROPIC_BASE_URL").ok()?;
    let port = local_proxy_port(&base)?;
    if port_listening(port) {
        return None; // already up (ours or the user's own) — never double-spawn
    }
    let log_path = crate::setup::parsec_home().join("proxy.log");
    // Spawns the SUPERVISOR (`parsec proxy`), which owns the port and keeps a
    // curating worker alive behind it. Own process group: it outlives this
    // hook AND the session — a local service, idle-cheap (~10MB per process),
    // reused by the next session. It no longer idle-exits, so this autostart
    // is now mostly a cold-start / post-reboot safety net rather than the
    // routine revival path.
    if let Err(e) = crate::setup::spawn_proxy_detached(port, &[]) {
        return Some(format!(
            "⌁ parsec: ANTHROPIC_BASE_URL routes through 127.0.0.1:{port} but the proxy \
             FAILED to start ({e}) — API requests will fail until you run `parsec proxy` \
             (log: {})",
            log_path.display()
        ));
    }
    for _ in 0..40 {
        if port_listening(port) {
            // Ask the SAME resolver the worker uses (env → release-baked URL,
            // plus the key/contract gates) — reading PARSEC_BRAIN_URL directly
            // reported "passthrough" on every baked build, which is every
            // shipped release: the worker curates, the banner denies it.
            let brain = crate::brain::BrainConfig::from_env().map(|c| c.url);
            return Some(match brain {
                Some(b) => format!(
                    "⌁ parsec proxy auto-started on 127.0.0.1:{port} (brain: {b}; \
                     log: {})",
                    log_path.display()
                ),
                None => format!(
                    "⌁ parsec proxy auto-started on 127.0.0.1:{port} in passthrough mode — \
                     set PARSEC_BRAIN_URL to enable curation (log: {})",
                    log_path.display()
                ),
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Some(format!(
        "⌁ parsec: proxy spawned for 127.0.0.1:{port} but never came up — API requests \
         will fail; check {}",
        log_path.display()
    ))
}

/// The port when `base` is a local parsec-proxy-shaped URL: plain http on
/// IPv4 loopback with an explicit port. Anything else (real API, remote
/// gateways, https, IPv6 — the proxy binds 127.0.0.1 only) is not ours to
/// manage.
pub(crate) fn local_proxy_port(base: &str) -> Option<u16> {
    let rest = base.trim().trim_end_matches('/').strip_prefix("http://")?;
    let (host, port) = rest.split_once(':')?;
    if !matches!(host, "127.0.0.1" | "localhost") {
        return None;
    }
    port.parse().ok()
}

pub(crate) fn port_listening(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

fn record_post(st: &mut SessionState, tool: &str, tool_input: &Value, cwd: &str) {
    match tool {
        "Read" => {
            if let Some((path, rng)) = read_tool_range(tool_input, cwd) {
                st.record_read(&path, rng);
            }
        }
        "Bash" => {
            if let Some(cmd) = tool_input.get("command").and_then(Value::as_str) {
                st.record_bash(cmd, cwd);
            }
        }
        // A completed edit means the file's recorded content is stale.
        "Edit" | "Write" | "MultiEdit" => {
            if let Some(p) = tool_input.get("file_path").and_then(Value::as_str) {
                st.evict_path(&crate::noreread::abs_path(p, cwd));
            }
        }
        "NotebookEdit" => {
            if let Some(p) = tool_input.get("notebook_path").and_then(Value::as_str) {
                st.evict_path(&crate::noreread::abs_path(p, cwd));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::local_proxy_port;

    #[test]
    fn only_loopback_http_urls_are_managed() {
        assert_eq!(local_proxy_port("http://127.0.0.1:8082"), Some(8082));
        assert_eq!(local_proxy_port("http://localhost:8082/"), Some(8082));
        // the proxy binds IPv4 loopback only — IPv6 base URLs are not managed
        assert_eq!(local_proxy_port("http://[::1]:9000"), None);
        assert_eq!(local_proxy_port("https://127.0.0.1:8082"), None); // https = not ours
        assert_eq!(local_proxy_port("http://api.anthropic.com"), None);
        assert_eq!(local_proxy_port("http://127.0.0.1"), None); // no explicit port
        assert_eq!(local_proxy_port("http://192.168.1.5:8082"), None); // not loopback
        assert_eq!(local_proxy_port("http://127.0.0.1:notaport"), None);
    }
}
