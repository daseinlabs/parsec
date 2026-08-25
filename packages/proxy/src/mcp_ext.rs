//! Trim tools for the plugin MCP server ("scout"). mapgen owns the stdio
//! loop but must not depend on this crate (dependency direction), so the
//! binary passes these in via `serve_stdio_with`.
//!
//! Same contract as the CLI: everything is a chars/4 ESTIMATE (labeled as
//! such in every tool description and summary — measurement honesty), the
//! server never generates directives itself, and a nothing-to-stage session
//! is a plain-text outcome, not an error.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::trim;
use parsec_mapgen::mcp::ExtraTool;

/// Parse the optional `level` argument (integer 1-5).
fn level_of(args: &Value) -> Result<Option<u8>, String> {
    match args.get("level") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|l| (1..=5).contains(l))
            .map(|l| Some(l as u8))
            .ok_or_else(|| format!("'level' must be an integer 1-5, got {v}")),
    }
}

pub fn tools() -> Vec<ExtraTool> {
    vec![
        ExtraTool {
            spec: json!({
                "name": "trim_stage",
                "description": "Stage a deterministic needed-set trim of the current session (step 1 of the /parsec:trim flow): computes which parts of the transcript were actually re-read, edited, or used later and stages them for one-shot injection after the context is cleared. Works on Claude Code and Codex sessions — by default it trims whichever transcript for this directory was written most recently. All token numbers are chars/4 ESTIMATES — never present them as measured. After staging, write the STANDING DIRECTIVES block from your own context and call trim_finalize.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "level": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 5,
                            "description": "Trim aggressiveness: 1 = low trimming (keep more) to 5 = very high. Omit for the default 3 — the only measured configuration."
                        },
                        "transcript": {
                            "type": "string",
                            "description": "Session transcript JSONL path (default: the newest transcript for the current project)"
                        },
                        "tool": {
                            "type": "string",
                            "enum": ["auto", "claude", "codex"],
                            "description": "Which harness's transcript to trim. Omit for auto — the freshest of the Claude Code and Codex transcripts for this directory, which is the session you are in."
                        }
                    }
                }
            }),
            handler: Box::new(|args| {
                let level = level_of(args)?;
                let transcript = args
                    .get("transcript")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                let tool = args.get("tool").and_then(Value::as_str);
                match trim::stage(transcript.as_deref(), None, None, "", level, tool) {
                    Ok(trim::StageOutcome::Staged(s)) => Ok(format!(
                        "staged (status: det, level {}, {} session): kept {}/{} chunks · {} of {} tokens (est., chars/4) at {}\nnext: extract the STANDING DIRECTIVES from your context and call trim_finalize, then tell the user to clear the context",
                        s.level,
                        s.source.tag(),
                        s.kept,
                        s.total_chunks,
                        trim::fmt_k(s.tokens_body_est),
                        trim::fmt_k(s.tokens_total_est),
                        s.path.display(),
                    )),
                    Ok(trim::StageOutcome::Nothing(msg)) => Ok(msg),
                    Err(e) => Err(e.to_string()),
                }
            }),
        },
        ExtraTool {
            spec: json!({
                "name": "trim_finalize",
                "description": "Attach a STANDING DIRECTIVES block to the trim staged by trim_stage and mark it ready (step 2 of the /parsec:trim flow). Directives are the durable rulings, constraints, and scope decisions you extract from your own in-context history — not a summary of the work. After this succeeds, tell the user to run /clear; never run it yourself.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "directives": {
                            "type": "string",
                            "description": "The pinned-constraints list, each line an imperative the agent must keep obeying (keep under ~3,000 tokens)"
                        }
                    },
                    "required": ["directives"]
                }
            }),
            handler: Box::new(|args| {
                let directives = args
                    .get("directives")
                    .and_then(Value::as_str)
                    .ok_or("missing 'directives'")?;
                match trim::finalize_directives(directives, None) {
                    Ok(f) => Ok(format!(
                        "ready: {} body + {} directives tokens (est.) staged at {}\ntell the user to run /clear now — the trim is injected into the next session (expires unused in {} min)",
                        trim::fmt_k(f.tokens_body_est),
                        trim::fmt_k(f.tokens_directives_est),
                        f.path.display(),
                        trim::ttl_secs() / 60,
                    )),
                    Err(e) => Err(e.to_string()),
                }
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_argument_parsing() {
        assert_eq!(level_of(&json!({})).unwrap(), None);
        assert_eq!(level_of(&json!({"level": null})).unwrap(), None);
        assert_eq!(level_of(&json!({"level": 4})).unwrap(), Some(4));
        assert!(level_of(&json!({"level": 0})).is_err());
        assert!(level_of(&json!({"level": 6})).is_err());
        assert!(level_of(&json!({"level": "3"})).is_err());
    }

    #[test]
    fn specs_are_wellformed() {
        for t in tools() {
            assert!(t.spec["name"].is_string());
            assert!(t.spec["description"].is_string());
            assert_eq!(t.spec["inputSchema"]["type"], "object");
        }
    }
}
