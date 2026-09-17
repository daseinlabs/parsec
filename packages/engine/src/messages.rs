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
                let obj = p.as_object()?;
                let block_type = obj.get("type").and_then(Value::as_str);
                if block_type
                    .is_some_and(|t| t != "text" && t != "output_text" && t != "tool_result")
                {
                    return None;
                }

                if block_type.is_none()
                    || block_type == Some("text")
                    || block_type == Some("output_text")
                {
                    if let Some(text) = obj.get("text").and_then(Value::as_str) {
                        return Some(text.to_string());
                    }
                }

                if let Some(content) = obj.get("content") {
                    let text = content_text(content);
                    if !text.is_empty() {
                        return Some(text);
                    }
                }

                None
            })
            .collect::<Vec<_>>()
            .join(" "),

        Value::Object(obj) => {
            let is_text_block = obj.get("type").and_then(Value::as_str) == Some("text");

            if is_text_block {
                if let Some(text) = obj.get("text").and_then(Value::as_str) {
                    return text.to_string();
                }
            }

            if let Some(content) = obj.get("content") {
                return content_text(content);
            }

            String::new()
        }
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

    #[test]
    fn content_text_extracts_single_string() {
        let content = json!("Hello, world!");

        assert_eq!(content_text(&content), "Hello, world!");
    }

    #[test]
    fn content_text_extracts_text_blocks() {
        let content = json!([
            {"type": "text", "text": "Hello"},
            {"type": "text", "text": "world"}
        ]);

        assert_eq!(content_text(&content), "Hello world");
    }

    #[test]
    fn content_text_ignores_missing_text_field() {
        let content = json!([
            {"type": "text"},
            {"type": "text", "text": "Valid"}
        ]);

        assert_eq!(content_text(&content), "Valid");
    }

    #[test]
    fn content_text_handles_malformed_content() {
        assert_eq!(content_text(&json!(null)), "");
        assert_eq!(content_text(&json!(123)), "");
        assert_eq!(content_text(&json!({"text": "object"})), "");
    }

    #[test]
    fn actions_extracts_command_and_query() {
        let message = json!({
            "extra": {
                "actions": [
                    {"command": "ls -la"},
                    {"query": "search term"}
                ]
            }
        });

        assert_eq!(
            actions(&message),
            vec!["ls -la".to_string(), "search term".to_string()]
        );
    }

    #[test]
    fn actions_handles_malformed_actions() {
        let message = json!({
            "extra": {
                "actions": "not-an-array"
            }
        });

        assert!(actions(&message).is_empty());
    }

    #[test]
    fn steps_of_handles_missing_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [{"command": "echo hello"}]
                }
            }),
            json!({
                "role": "user"
            }),
        ];

        let result = steps_of(&messages);

        assert_eq!(result, vec![("echo hello".to_string(), "".to_string())]);
    }

    #[test]
    fn truthy_handles_different_json_types() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(false)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!([])));

        assert!(truthy(&json!(true)));
        assert!(truthy(&json!(1)));
        assert!(truthy(&json!("text")));
        assert!(truthy(&json!([1])));
    }

    #[test]
    fn content_text_handles_tool_result_string() {
        let content = json!("Tool execution completed");
        assert_eq!(content_text(&content), "Tool execution completed");
    }

    #[test]
    fn content_text_handles_tool_result_blocks() {
        let content = json!([
            {
                "type": "tool_result",
                "content": "Result from tool"
            }
        ]);

        // Current implementation only extracts `text`.
        // This test documents the expected behavior to implement.
        assert_eq!(content_text(&content), "Result from tool");
    }
}
