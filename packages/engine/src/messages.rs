//! Port of chunking.py's message-level layer: steps_of, reasoning chunks,
//! assistant-text chunks. Operates on harness message dicts (serde_json
//! Values with preserve_order, so json.dumps-length parity holds).

use serde_json::Value;

use crate::chunking::{chunk_assistant, Chunk, DEFAULT_WIN};
use crate::pystr::*;

/// Provider/reasoning fields that get re-fed every call — the eviction target
/// for 'provider' chunks.
const REASON_FIELDS: [&str; 3] = [
    "provider_specific_fields",
    "thinking_blocks",
    "reasoning_content",
];

/// Action strings of an assistant message (`extra.actions[].command|query`).
pub fn actions(m: &Value) -> Vec<String> {
    m.get("extra")
        .and_then(|e| e.get("actions"))
        .and_then(|a| a.as_array())
        .map(|acts| {
            acts.iter()
                .map(|a| {
                    a.get("command")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| a.get("query").and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn content_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                p.as_object()
                    .map(|o| o.get("text").and_then(Value::as_str).unwrap_or(""))
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// chunking.steps_of: ordered (command, observation_text) per agent step.
pub fn steps_of(messages: &[Value]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for m in messages {
        match m.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                pending = Some(actions(m).join(" ; "));
            }
            Some("user") | Some("tool") => {
                if let Some(cmd) = pending.take() {
                    let txt = m.get("content").map(content_text).unwrap_or_default();
                    out.push((cmd, txt));
                }
            }
            _ => {}
        }
    }
    out
}

/// chunking.blob_tokens: token weight of a step's re-fed reasoning payload.
pub fn blob_tokens(m: &Value) -> i64 {
    let mut tot: i64 = 0;
    if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
        for tc in tcs {
            if let Some(psf) = tc.get("provider_specific_fields") {
                if truthy(psf) {
                    tot += char_len(&py_json_dumps(psf)) as i64 / 4;
                }
            }
        }
    }
    for k in REASON_FIELDS {
        if let Some(v) = m.get(k) {
            if truthy(v) {
                let s = match v {
                    Value::String(s) => s.clone(),
                    other => py_json_dumps(other),
                };
                tot += char_len(&s) as i64 / 4;
            }
        }
    }
    tot
}

/// Python truthiness for the `if psf:` / `if v:` guards.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// chunking.reasoning_text: embed text for the THINKING object.
pub fn reasoning_text(m: &Value) -> String {
    if let Some(rc) = m.get("reasoning_content").and_then(Value::as_str) {
        if py_has_content(rc) {
            return char_prefix(rc, 2000).to_string();
        }
    }
    let acts = actions(m);
    let t: String = acts
        .iter()
        .filter(|a| !a.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ; ");
    let t = char_prefix(&t, 2000).to_string();
    if t.is_empty() {
        "reasoning".to_string()
    } else {
        t
    }
}

/// chunking.reasoning_chunk: one scorable chunk for a step's re-fed reasoning
/// payload; None when the step carries no reasoning blob.
pub fn reasoning_chunk(m: &Value, step: i64) -> Option<Chunk> {
    let tok = blob_tokens(m);
    if tok <= 0 {
        return None;
    }
    let mut c = Chunk::new(reasoning_text(m), None, None, None, step, "reasoning").with_tokens(tok);
    c.evict = "provider".into();
    Some(c)
}

/// chunking.reasoning_chunks_of, aligned to steps_of()'s step index.
pub fn reasoning_chunks_of(messages: &[Value]) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut pending: Option<&Value> = None;
    let mut step: i64 = 0;
    for m in messages {
        match m.get("role").and_then(Value::as_str) {
            Some("assistant") => pending = Some(m),
            Some("user") | Some("tool") => {
                if let Some(p) = pending.take() {
                    if let Some(rc) = reasoning_chunk(p, step) {
                        out.push(rc);
                    }
                    step += 1;
                }
            }
            _ => {}
        }
    }
    out
}

/// chunking.assistant_chunks_of: per-step assistant text chunks; skips
/// messages whose content isn't plain text.
pub fn assistant_chunks_of(messages: &[Value]) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut pending: Option<&Value> = None;
    let mut step: i64 = 0;
    for m in messages {
        match m.get("role").and_then(Value::as_str) {
            Some("assistant") => pending = Some(m),
            Some("user") | Some("tool") => {
                if let Some(p) = pending.take() {
                    let c = p.get("content").unwrap_or(&Value::Null);
                    let ok = c.is_string()
                        || c.as_array().is_some_and(|parts| {
                            parts.iter().all(|q| {
                                q.as_object().is_some_and(|o| match o.get("type") {
                                    None => true, // key absent -> Python's .get default "text"
                                    Some(Value::String(s)) => s == "text",
                                    Some(_) => false,
                                })
                            })
                        });
                    if ok {
                        let t = content_text(c);
                        if py_has_content(&t) {
                            out.extend(chunk_assistant(&t, step, DEFAULT_WIN));
                        }
                    }
                    step += 1;
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── truthy ───────────────────────────────────────────────────────────────

    #[test]
    fn test_truthy_all_falsy_values() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(false)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!(0.0)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!([])));
        assert!(!truthy(&json!({})));
    }

    #[test]
    fn test_truthy_all_truthy_values() {
        assert!(truthy(&json!(true)));
        assert!(truthy(&json!(1)));
        assert!(truthy(&json!(-1)));
        assert!(truthy(&json!(0.1)));
        assert!(truthy(&json!("x")));
        assert!(truthy(&json!([1])));
        assert!(truthy(&json!({"k": "v"})));
    }

    // ── steps_of ─────────────────────────────────────────────────────────────

    #[test]
    fn test_steps_of_basic_conversation() {
        let msgs = vec![
            json!({"role": "assistant", "content": "thinking",
                   "extra": {"actions": [{"command": "ls -la"}]}}),
            json!({"role": "user", "content": "file1.txt\nfile2.txt"}),
        ];
        let steps = steps_of(&msgs);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].0, "ls -la");
        assert_eq!(steps[0].1, "file1.txt\nfile2.txt");
    }

    #[test]
    fn test_steps_of_multiple_steps() {
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "step1"}]}}),
            json!({"role": "user", "content": "obs1"}),
            json!({"role": "assistant", "extra": {"actions": [{"command": "step2"}]}}),
            json!({"role": "user", "content": "obs2"}),
        ];
        let steps = steps_of(&msgs);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], ("step1".to_string(), "obs1".to_string()));
        assert_eq!(steps[1], ("step2".to_string(), "obs2".to_string()));
    }

    /// No assistant message -> empty steps.
    #[test]
    fn test_steps_of_no_assistant() {
        let msgs = vec![json!({"role": "user", "content": "hello"})];
        assert!(steps_of(&msgs).is_empty());
    }

    /// Trailing assistant with no following user/tool is not emitted.
    #[test]
    fn test_steps_of_dangling_assistant_not_emitted() {
        let msgs = vec![json!({"role": "assistant", "extra": {"actions": [{"command": "cmd"}]}})];
        assert!(steps_of(&msgs).is_empty());
    }

    /// role: "tool" (Anthropic tool result) also triggers step emission.
    #[test]
    fn test_steps_of_tool_role_triggers_step() {
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "read file"}]}}),
            json!({"role": "tool", "content": "file contents here"}),
        ];
        let steps = steps_of(&msgs);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].1, "file contents here");
    }

    /// Unknown roles are silently skipped.
    #[test]
    fn test_steps_of_unknown_role_ignored() {
        let msgs = vec![
            json!({"role": "system", "content": "system prompt"}),
            json!({"role": "assistant", "extra": {"actions": [{"command": "cmd"}]}}),
            json!({"role": "function", "content": "ignored"}),
            json!({"role": "user", "content": "real obs"}),
        ];
        let steps = steps_of(&msgs);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].1, "real obs");
    }

    /// content as an array of text blocks -> joined with space.
    #[test]
    fn test_steps_of_content_array_of_text_blocks() {
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "cmd"}]}}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "world"}
            ]}),
        ];
        let steps = steps_of(&msgs);
        assert_eq!(steps[0].1, "hello world");
    }

    /// Missing content field -> empty observation string.
    #[test]
    fn test_steps_of_missing_content_is_empty() {
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "cmd"}]}}),
            json!({"role": "user"}),
        ];
        let steps = steps_of(&msgs);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].1, "");
    }

    // ── blob_tokens ───────────────────────────────────────────────────────────

    /// Message with no reasoning fields -> 0 tokens.
    #[test]
    fn test_blob_tokens_no_reasoning() {
        let m = json!({"role": "assistant", "content": "plain text"});
        assert_eq!(blob_tokens(&m), 0);
    }

    /// reasoning_content as string: char_len / 4.
    #[test]
    fn test_blob_tokens_reasoning_content_string() {
        let m = json!({"role": "assistant", "reasoning_content": "aaaa"});
        assert_eq!(blob_tokens(&m), 1);
        let m = json!({"role": "assistant", "reasoning_content": "aaaaaaaa"});
        assert_eq!(blob_tokens(&m), 2);
    }

    /// Empty string reasoning_content is falsy -> contributes 0.
    #[test]
    fn test_blob_tokens_empty_reasoning_content() {
        let m = json!({"role": "assistant", "reasoning_content": ""});
        assert_eq!(blob_tokens(&m), 0);
    }

    /// provider_specific_fields in tool_calls contributes tokens.
    #[test]
    fn test_blob_tokens_provider_specific_fields() {
        let m = json!({"tool_calls": [{"provider_specific_fields": {"k": "v"}}]});
        let tok = blob_tokens(&m);
        assert!(tok > 0, "provider_specific_fields should contribute tokens");
    }

    // ── reasoning_text ────────────────────────────────────────────────────────

    #[test]
    fn test_reasoning_text_from_reasoning_content() {
        let m = json!({"reasoning_content": "deep thought"});
        assert_eq!(reasoning_text(&m), "deep thought");
    }

    /// Long reasoning_content is truncated at 2000 chars.
    #[test]
    fn test_reasoning_text_truncated_at_2000() {
        let long = "x".repeat(3000);
        let m = json!({"reasoning_content": long});
        let result = reasoning_text(&m);
        assert_eq!(result.chars().count(), 2000);
    }

    /// Falls back to action commands when no reasoning_content.
    #[test]
    fn test_reasoning_text_fallback_to_actions() {
        let m = json!({
            "extra": {"actions": [{"command": "ls -la"}, {"command": "cat file.rs"}]}
        });
        let result = reasoning_text(&m);
        assert!(result.contains("ls -la"));
        assert!(result.contains("cat file.rs"));
    }

    /// Falls back to "reasoning" when no reasoning_content and no actions.
    #[test]
    fn test_reasoning_text_fallback_default() {
        let m = json!({"role": "assistant"});
        assert_eq!(reasoning_text(&m), "reasoning");
    }

    // ── assistant_chunks_of ───────────────────────────────────────────────────

    #[test]
    fn test_assistant_chunks_of_plain_string_content() {
        let msgs = vec![
            json!({"role": "assistant", "content": "Here is my answer to your question."}),
            json!({"role": "user", "content": "ok"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.kind == "asst"));
    }

    /// tool_use content blocks cause the whole message to be skipped.
    #[test]
    fn test_assistant_chunks_of_tool_use_content_skipped() {
        let msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}
            ]}),
            json!({"role": "user", "content": "result"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        assert_eq!(
            chunks.len(),
            0,
            "tool_use blocks should produce no asst chunks"
        );
    }

    /// Whitespace-only assistant text produces no chunks.
    #[test]
    fn test_assistant_chunks_of_whitespace_only_skipped() {
        let msgs = vec![
            json!({"role": "assistant", "content": "   \n  \n  "}),
            json!({"role": "user", "content": "ok"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        assert_eq!(chunks.len(), 0);
    }

    /// Step indices advance correctly across multiple assistant turns.
    #[test]
    fn test_assistant_chunks_of_step_index_correct() {
        let msgs = vec![
            json!({"role": "assistant", "content": "first answer"}),
            json!({"role": "user", "content": "obs1"}),
            json!({"role": "assistant", "content": "second answer"}),
            json!({"role": "user", "content": "obs2"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        assert!(chunks.iter().any(|c| c.step == 0));
        assert!(chunks.iter().any(|c| c.step == 1));
    }

    /// Array of text blocks (all type="text") produces chunks.
    #[test]
    fn test_assistant_chunks_of_array_of_text_blocks() {
        let msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "block one"},
                {"type": "text", "text": "block two"}
            ]}),
            json!({"role": "user", "content": "ok"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        assert!(!chunks.is_empty(), "text blocks should produce asst chunks");
    }

    /// A block without "type" key is treated as text (Python's .get default).
    #[test]
    fn test_assistant_chunks_of_missing_type_key_treated_as_text() {
        let msgs = vec![
            json!({"role": "assistant", "content": [{"text": "no type key"}]}),
            json!({"role": "user", "content": "ok"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        assert!(!chunks.is_empty());
    }

    // ── actions ───────────────────────────────────────────────────────────────

    #[test]
    fn test_actions_extracts_commands() {
        let m = json!({
            "extra": {"actions": [{"command": "ls -la"}, {"command": "cat file.rs"}]}
        });
        assert_eq!(actions(&m), vec!["ls -la", "cat file.rs"]);
    }

    /// Empty command falls back to query field.
    #[test]
    fn test_actions_falls_back_to_query() {
        let m = json!({"extra": {"actions": [{"command": "", "query": "search term"}]}});
        assert_eq!(actions(&m), vec!["search term"]);
    }

    /// No extra field -> empty vec.
    #[test]
    fn test_actions_no_extra_field() {
        let m = json!({"role": "assistant"});
        assert!(actions(&m).is_empty());
    }

    /// Empty actions array -> empty vec.
    #[test]
    fn test_actions_empty_array() {
        let m = json!({"extra": {"actions": []}});
        assert!(actions(&m).is_empty());
    }
}
