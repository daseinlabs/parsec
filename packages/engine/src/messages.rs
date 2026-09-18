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

    #[test]
    fn actions_extracts_commands_and_queries() {
        let message = json!({
            "role": "assistant",
            "extra": {
                "actions": [
                    {
                        "command": "cargo test"
                    },
                    {
                        "query": "find rust files"
                    }
                ]
            }
        });

        assert_eq!(
            actions(&message),
            vec!["cargo test".to_string(), "find rust files".to_string()]
        );
    }

    #[test]
    fn steps_of_extracts_plain_string_observation() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {
                            "command": "cargo test"
                        }
                    ]
                }
            }),
            json!({
                "role": "tool",
                "content": "all tests passed"
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("cargo test".to_string(), "all tests passed".to_string())]
        );
    }

    #[test]
    fn steps_of_extracts_text_from_content_array() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {
                            "command": "read file"
                        }
                    ]
                }
            }),
            json!({
                "role": "tool",
                "content": [
                    {
                        "type": "text",
                        "text": "first part"
                    },
                    {
                        "type": "text",
                        "text": "second part"
                    }
                ]
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![(
                "read file".to_string(),
                "first part second part".to_string()
            )]
        );
    }

    #[test]
    fn assistant_chunks_of_extracts_plain_text() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "Here is the answer."
            }),
            json!({
                "role": "user",
                "content": "Thanks"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].text, "Here is the answer.");
        assert_eq!(chunks[0].step, 0);
    }

    #[test]
    fn assistant_chunks_of_skips_tool_use_blocks() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "I will inspect the file."
                    },
                    {
                        "type": "tool_use",
                        "id": "tool_1",
                        "name": "read_file",
                        "input": {
                            "path": "src/main.rs"
                        }
                    }
                ]
            }),
            json!({
                "role": "tool",
                "content": "file contents"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert!(chunks.is_empty());
    }

    #[test]
    fn message_helpers_handle_malformed_and_missing_fields() {
        let messages = vec![
            json!({}),
            json!({
                "role": 123,
                "content": null
            }),
            json!({
                "role": "assistant"
            }),
            json!({
                "role": "user",
                "content": {
                    "unexpected": "object"
                }
            }),
        ];

        assert_eq!(steps_of(&messages), vec![("".to_string(), "".to_string())]);

        assert!(assistant_chunks_of(&messages).is_empty());

        assert!(actions(&json!({})).is_empty());

        assert!(actions(&json!({
            "extra": {
                "actions": "not-an-array"
            }
        }))
        .is_empty());
    }
}
