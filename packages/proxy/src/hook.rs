//! `dasein hook <event>` — Claude Code hook handler (free tier).
//!
//! Protocol (code.claude.com/docs/en/hooks): JSON on stdin; to deny a
//! PreToolUse call, print `hookSpecificOutput.permissionDecision: "deny"`
//! with a reason and exit 0. PostToolUse records what actually happened.
//! Hooks must NEVER fail the session: any internal error exits 0 silently
//! (fail-open, DIRECTION.md §8.3) — but is counted in the session file so a
//! silently dead hook is distinguishable from a quiet one.

use serde_json::{json, Value};

use crate::noreread::{
    load_session, prune_sessions, read_tool_range, save_session, Gate, SessionState,
};

pub fn run(event: &str) -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
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
        }
        _ => {}
    }
    Ok(())
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
