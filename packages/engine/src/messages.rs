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
    fn content_text_handles_string_content() {
        let value = json!("hello world");

        assert_eq!(content_text(&value), "hello world");
    }

    #[test]
    fn content_text_extracts_text_from_array_content() {
        let value = json!([
            {"type": "text", "text": "hello"},
            {"type": "text", "text": "world"}
        ]);

        assert_eq!(content_text(&value), "hello world");
    }

    #[test]
    fn content_text_handles_missing_or_non_string_text() {
        let value = json!([
            {"type": "text"},
            {"type": "text", "text": 123},
            {"type": "image", "text": "ignored"}
        ]);

        assert_eq!(content_text(&value), "  ignored");
    }

    #[test]
    fn content_text_handles_unsupported_content_gracefully() {
        assert_eq!(content_text(&Value::Null), "");
        assert_eq!(content_text(&json!(123)), "");
        assert_eq!(content_text(&json!({"text": "hello"})), "");
    }

    #[test]
    fn actions_prefers_command_and_falls_back_to_query() {
        let message = json!({
            "extra": {
                "actions": [
                    {"command": "grep foo"},
                    {"query": "search bar"},
                    {"command": "", "query": "fallback"}
                ]
            }
        });

        assert_eq!(
            actions(&message),
            vec![
                "grep foo".to_string(),
                "search bar".to_string(),
                "fallback".to_string()
            ]
        );
    }

    #[test]
    fn actions_handles_missing_or_malformed_data() {
        assert!(actions(&json!({})).is_empty());
        assert!(actions(&json!({"extra": {}})).is_empty());
        assert!(actions(&json!({"extra": {"actions": "invalid"}})).is_empty());

        let message = json!({
            "extra": {
                "actions": [
                    {"command": 123},
                    {"query": 456},
                    {}
                ]
            }
        });

        assert_eq!(
            actions(&message),
            vec!["".to_string(), "".to_string(), "".to_string()]
        );
    }

    #[test]
    fn steps_of_pairs_assistant_actions_with_user_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {"command": "cat file.txt"}
                    ]
                }
            }),
            json!({
                "role": "user",
                "content": "file contents"
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("cat file.txt".to_string(), "file contents".to_string())]
        );
    }

    #[test]
    fn steps_of_extracts_array_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {"command": "read file"}
                    ]
                }
            }),
            json!({
                "role": "tool",
                "content": [
                    {"type": "text", "text": "line one"},
                    {"type": "text", "text": "line two"}
                ]
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("read file".to_string(), "line one line two".to_string())]
        );
    }

    #[test]
    fn steps_of_ignores_unpaired_messages() {
        let messages = vec![
            json!({"role": "user", "content": "orphan user"}),
            json!({"role": "assistant"}),
            json!({"role": "system", "content": "system message"}),
        ];

        assert!(steps_of(&messages).is_empty());
    }

    #[test]
    fn reasoning_text_prefers_reasoning_content() {
        let message = json!({
            "reasoning_content": "thinking about the solution",
            "extra": {
                "actions": [
                    {"command": "ignored command"}
                ]
            }
        });

        assert_eq!(reasoning_text(&message), "thinking about the solution");
    }

    #[test]
    fn reasoning_text_falls_back_to_actions() {
        let message = json!({
            "extra": {
                "actions": [
                    {"command": "first"},
                    {"query": "second"}
                ]
            }
        });

        assert_eq!(reasoning_text(&message), "first ; second");
    }

    #[test]
    fn reasoning_text_defaults_when_no_content_exists() {
        assert_eq!(reasoning_text(&json!({})), "reasoning");
        assert_eq!(
            reasoning_text(&json!({"reasoning_content": "   "})),
            "reasoning"
        );
    }

    #[test]
    fn reasoning_chunk_is_none_without_reasoning_blob() {
        assert!(reasoning_chunk(&json!({}), 0).is_none());
    }

    #[test]
    fn reasoning_chunk_uses_provider_eviction() {
        let message = json!({
            "reasoning_content": "This is reasoning content",
        });

        let chunk = reasoning_chunk(&message, 4).unwrap();

        assert_eq!(chunk.step, 4);
        assert_eq!(chunk.kind, "reasoning");
        assert_eq!(chunk.evict, "provider");
        assert_eq!(chunk.text, "This is reasoning content");
        assert!(chunk.tokens > 0);
    }

    #[test]
    fn assistant_chunks_of_accepts_string_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "hello\nworld"
            }),
            json!({
                "role": "tool",
                "content": "result"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello\nworld");
        assert_eq!(chunks[0].kind, "asst");
        assert_eq!(chunks[0].step, 0);
    }

    #[test]
    fn assistant_chunks_of_accepts_text_content_blocks() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "world"}
                ]
            }),
            json!({
                "role": "user",
                "content": "next"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
    }

    #[test]
    fn assistant_chunks_of_rejects_non_text_content_blocks() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image", "url": "image.png"}
                ]
            }),
            json!({
                "role": "tool",
                "content": "result"
            }),
        ];

        assert!(assistant_chunks_of(&messages).is_empty());
    }

    #[test]
    fn assistant_chunks_of_handles_missing_and_empty_content() {
        let messages = vec![
            json!({"role": "assistant"}),
            json!({"role": "tool", "content": "result"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "user", "content": "result"}),
        ];

        assert!(assistant_chunks_of(&messages).is_empty());
    }

    #[test]
    fn blob_tokens_handles_tool_provider_fields_and_reasoning_fields() {
        let message = json!({
            "tool_calls": [
                {
                    "provider_specific_fields": {
                        "foo": "bar"
                    }
                }
            ],
            "reasoning_content": "thinking"
        });

        assert!(blob_tokens(&message) > 0);
    }

    #[test]
    fn malformed_json_values_do_not_panic() {
        let values = vec![
            Value::Null,
            json!(true),
            json!(123),
            json!("text"),
            json!([]),
            json!({}),
            json!({"role": 123}),
            json!({"extra": null}),
            json!({"tool_calls": "invalid"}),
            json!({"reasoning_content": 123}),
        ];

        for value in values {
            let _ = actions(&value);
            let _ = steps_of(std::slice::from_ref(&value));
            let _ = blob_tokens(&value);
            let _ = reasoning_text(&value);
            let _ = reasoning_chunk(&value, 0);
            let _ = assistant_chunks_of(std::slice::from_ref(&value));
        }
    }
}
