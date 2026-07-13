//! `dasein hook <event>` — Claude Code hook handler (free tier).
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
            let mut st = load_session(&session_id);
            record_post(&mut st, tool, &tool_input, &cwd);
            let _ = save_session(&session_id, &st);
        }
        "SessionStart" => {
            prune_sessions(7);
            if let Some(msg) = maybe_autostart_proxy() {
                println!(
                    "{}",
                    json!({ "systemMessage": msg, "suppressOutput": true })
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
/// (arms/dasein.py `_CONTINUE_STEER` + the DELIVER text) adapted to the
/// "stopped with no submittable work product while looping" case.
const STOP_BLOCK_REASON: &str = "Your work is not yet complete: there is no submittable edit on \
disk and your recent actions were repeating without producing new information. Do not stop yet — \
take a DIFFERENT concrete step toward the code change that resolves the task, make the edit, \
verify it, then stop.";

/// Stop-hook SUBMIT adjudicator (Track C). `DASEIN_ADJUDICATOR` =
/// - `advise` (default): record a JSONL row, print NOTHING;
/// - `block`: additionally print ONE `{"decision":"block"}` when the stop
///   looks premature (CONTINUE verdict, no submittable edit, mechanical
///   stall) within the per-session `DASEIN_ADJ_MAX_BLOCKS` budget (default
///   2 — the governor's validated coach+bank ceiling);
/// - `off`: skip entirely (no row).
///
/// Fail-open discipline: every internal error path returns silently (exit
/// 0); stdout carries ONLY the block-decision JSON, never anything else.
fn run_stop(payload: &Value, session_id: &str, cwd: &str) {
    let mode_env = std::env::var("DASEIN_ADJUDICATOR").unwrap_or_default();
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
        let max_blocks: u32 = std::env::var("DASEIN_ADJ_MAX_BLOCKS")
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

/// The Pro flip-on (DIRECTION.md §7: "plugin → proxy (manages)"): when this
/// session is ROUTED through a local dasein proxy (ANTHROPIC_BASE_URL points
/// at a loopback port) and nothing is listening there yet, spawn
/// `dasein proxy` detached so the session's first request doesn't hit a dead
/// port. The hook inherits the session env, so DASEIN_BRAIN_URL /
/// DASEIN_BRAIN_CONTRACT / DASEIN_EMBED_* configured in settings.json `env`
/// flow into the spawned proxy. What a hook CANNOT do is set
/// ANTHROPIC_BASE_URL itself — routing must exist at launch (settings env or
/// shell). Opt out with DASEIN_PROXY_AUTOSTART=0. Returns a user-visible
/// message when it acted (or failed — a dead routed port breaks the session,
/// which must never be silent).
fn maybe_autostart_proxy() -> Option<String> {
    if std::env::var("DASEIN_PROXY_AUTOSTART").ok().as_deref() == Some("0") {
        return None;
    }
    let base = std::env::var("ANTHROPIC_BASE_URL").ok()?;
    let port = local_proxy_port(&base)?;
    if port_listening(port) {
        return None; // already up (ours or the user's own) — never double-spawn
    }
    let exe = std::env::current_exe().ok()?;
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let log_dir = std::path::PathBuf::from(&home).join(".dasein");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("proxy.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("proxy")
        .env("DASEIN_PROXY_PORT", port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone().ok()?))
        .stderr(std::process::Stdio::from(log));
    // A MANAGED proxy also turns itself off: 30 min without traffic and it
    // exits (this hook revives it next session). A user-set value wins;
    // manual `dasein proxy` runs keep the run-forever default.
    if std::env::var("DASEIN_PROXY_IDLE_EXIT_S").is_err() {
        cmd.env("DASEIN_PROXY_IDLE_EXIT_S", "1800");
    }
    #[cfg(unix)]
    {
        // Own process group: the proxy outlives this hook AND the session —
        // it is a local service, idle-cheap (~10MB), reused by the next one.
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    }
    if let Err(e) = cmd.spawn() {
        return Some(format!(
            "⌁ dasein: ANTHROPIC_BASE_URL routes through 127.0.0.1:{port} but the proxy \
             FAILED to start ({e}) — API requests will fail until you run `dasein proxy` \
             (log: {})",
            log_path.display()
        ));
    }
    for _ in 0..40 {
        if port_listening(port) {
            let brain = std::env::var("DASEIN_BRAIN_URL").ok();
            return Some(match brain {
                Some(b) => format!(
                    "⌁ dasein proxy auto-started on 127.0.0.1:{port} (brain: {b}; \
                     log: {})",
                    log_path.display()
                ),
                None => format!(
                    "⌁ dasein proxy auto-started on 127.0.0.1:{port} in passthrough mode — \
                     set DASEIN_BRAIN_URL to enable curation (log: {})",
                    log_path.display()
                ),
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Some(format!(
        "⌁ dasein: proxy spawned for 127.0.0.1:{port} but never came up — API requests \
         will fail; check {}",
        log_path.display()
    ))
}

/// The port when `base` is a local dasein-proxy-shaped URL: plain http on
/// IPv4 loopback with an explicit port. Anything else (real API, remote
/// gateways, https, IPv6 — the proxy binds 127.0.0.1 only) is not ours to
/// manage.
fn local_proxy_port(base: &str) -> Option<u16> {
    let rest = base.trim().trim_end_matches('/').strip_prefix("http://")?;
    let (host, port) = rest.split_once(':')?;
    if !matches!(host, "127.0.0.1" | "localhost") {
        return None;
    }
    port.parse().ok()
}

fn port_listening(port: u16) -> bool {
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
