//! Codex session transcripts → the full-fidelity message list `/trim` labels.
//!
//! Claude Code writes one JSONL per session under
//! `~/.claude/projects/<project-key>/`; Codex writes
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`. Different tree,
//! different schema — `trim::discover_transcript` only ever looked in the
//! Claude tree, so `parsec trim` inside a Codex session either found nothing
//! or, worse, silently trimmed a STALE Claude transcript for the same
//! directory and staged it for the next Claude session.
//!
//! Rollout schema (captured from a real 0.147.0 rollout, not from docs): one
//! JSON object per line, `{timestamp, type, payload}`.
//!   * `type: "session_meta"` — payload carries `cwd`, `session_id`, `cli_version`.
//!   * `type: "response_item"` — payload IS a Responses API input item, the
//!     same shape the live proxy curates.
//!   * `event_msg` / `turn_context` / `world_state` — runtime telemetry, not
//!     conversation content; skipped.
//!
//! Because `response_item` payloads are ordinary Responses items, the
//! projection reuses [`crate::responses::internal_entry`] verbatim and only
//! reshapes the result into the `convert_agent` form `needed::det_trim`
//! consumes (assistant entries carry `reasoning_content`/`thinking_blocks`;
//! every entry carries `extra`). Same action vocabulary as the live curator,
//! by construction.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::setup::home_dir;

/// Newest-first candidates to inspect before giving up. A rollout tree is
/// one file per session and is never pruned, so an old install can hold
/// thousands; reading the head of every one to find a `cwd` match would turn
/// `parsec trim` into a disk scan.
const MAX_CANDIDATES: usize = 400;

fn sessions_root() -> PathBuf {
    home_dir().join(".codex").join("sessions")
}

/// Every `rollout-*.jsonl` under the sessions root, newest mtime first.
/// The tree is YYYY/MM/DD/, so a depth bound of 4 covers it without an
/// unbounded walk if something else ever lands in there.
fn rollouts_newest_first(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(std::time::SystemTime, PathBuf)>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                walk(&path, depth - 1, out);
            } else if path.extension().and_then(|x| x.to_str()) == Some("jsonl")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-"))
            {
                out.push((meta.modified().unwrap_or(std::time::UNIX_EPOCH), path));
            }
        }
    }
    let mut found = Vec::new();
    walk(root, 4, &mut found);
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// `session_meta.payload.cwd` of a rollout, read from its head. None when the
/// file does not open, has no meta line in its first few entries, or records
/// no cwd — all of which mean "cannot confirm this belongs to the project",
/// and an unconfirmed transcript is never trimmed.
fn rollout_cwd(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .take(8)
    {
        let Ok(e) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if e.get("type").and_then(Value::as_str) == Some("session_meta") {
            return e
                .pointer("/payload/cwd")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    None
}

/// The newest Codex rollout recorded for `cwd`, with its mtime.
pub fn discover_rollout(cwd: &str) -> Option<(PathBuf, std::time::SystemTime)> {
    for path in rollouts_newest_first(&sessions_root())
        .into_iter()
        .take(MAX_CANDIDATES)
    {
        if rollout_cwd(&path).as_deref() == Some(cwd) {
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            return Some((path, mtime));
        }
    }
    None
}

/// Uncapped, full-fidelity read of a Codex rollout. Unreadable file → `[]`
/// (fail-open), matching the Claude Code reader.
pub fn messages_from_rollout(path: &str) -> Vec<Value> {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    parse_rollout(std::io::BufReader::new(f).lines().map_while(Result::ok))
}

/// The per-line conversion. Pure over the transcript bytes, like its Claude
/// Code twin — the determinism seam a trim label is derived through.
pub fn parse_rollout<I, S>(lines: I) -> Vec<Value>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<Value> = Vec::new();
    for line in lines {
        let Ok(e) = serde_json::from_str::<Value>(line.as_ref()) else {
            continue;
        };
        if e.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = e.get("payload") else {
            continue;
        };
        let entry = crate::responses::internal_entry(payload);
        let content = entry
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match entry.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                // Reasoning rides this wire ENCRYPTED (`reasoning.encrypted_content`),
                // so there is no plaintext to put in reasoning_content — the
                // reasoning-chunk half of the label is simply absent on Codex,
                // and empty is the honest representation of that.
                let actions = entry
                    .pointer("/extra/actions")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                out.push(json!({
                    "role": "assistant",
                    "content": content,
                    "reasoning_content": "",
                    "thinking_blocks": [],
                    "extra": {"actions": actions},
                }));
            }
            Some("tool") => out.push(json!({"role": "tool", "content": content, "extra": {}})),
            Some("user") => out.push(json!({"role": "user", "content": content, "extra": {}})),
            // "system" (Codex's developer preamble) and "opaque" (reasoning,
            // item_reference, …) are instructions or ciphertext, not the
            // compactable window.
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines lifted from a real rollout, trimmed to the fields that matter.
    fn sample() -> Vec<String> {
        vec![
            r#"{"timestamp":"t","type":"session_meta","payload":{"session_id":"s","cwd":"/repo","cli_version":"0.147.0"}}"#,
            r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<skills_instructions>…"}]}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<recommended_plugins>…"}]}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the parser"}]}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reading it"}]}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"custom_tool_call","call_id":"c1","name":"exec","input":"const r = await tools.exec_command({cmd:\"sed -n '1,40p' src/parser.py\",\"workdir\":\"/repo\"});"}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":[{"type":"input_text","text":"1: def parse():\n2:     return None\n"}]}}"#,
            r#"{"timestamp":"t","type":"world_state","payload":{}}"#,
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[test]
    fn rollout_projects_to_the_convert_agent_shape() {
        let msgs = parse_rollout(sample());
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        // developer preamble and the non-response_item lines are gone; the
        // conversation itself is in order.
        assert_eq!(roles, ["user", "user", "assistant", "assistant", "tool"]);
        assert_eq!(msgs[1]["content"], "fix the parser");
        assert_eq!(msgs[4]["content"], "1: def parse():\n2:     return None\n");
        // Every assistant entry carries the convert_agent fields det_trim reads.
        for m in msgs.iter().filter(|m| m["role"] == "assistant") {
            assert!(m.get("reasoning_content").is_some());
            assert!(m.get("thinking_blocks").is_some());
            assert!(m.pointer("/extra/actions").is_some());
        }
        // …and the tool call's command survived the JS snippet.
        assert_eq!(
            msgs[3]["extra"]["actions"][0]["command"],
            "sed -n '1,40p' src/parser.py"
        );
    }

    #[test]
    fn garbage_and_foreign_lines_are_skipped_not_fatal() {
        let mut lines = sample();
        lines.insert(0, "not json at all".into());
        lines.push(r#"{"type":"response_item"}"#.into());
        lines.push(
            r#"{"type":"response_item","payload":{"type":"reasoning","encrypted_content":"gAAA"}}"#
                .into(),
        );
        assert_eq!(parse_rollout(lines).len(), 5);
    }

    #[test]
    fn missing_tree_yields_no_candidates_rather_than_an_error() {
        assert!(rollouts_newest_first(Path::new("/nonexistent/parsec/codex")).is_empty());
    }
}
