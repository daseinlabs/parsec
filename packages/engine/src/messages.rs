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
    fn actions_prefers_command_over_query() {
        let message = json!({
            "extra": {
                "actions": [
                    {
                        "command": "cat src/main.rs",
                        "query": "fallback query"
                    }
                ]
            }
        });

        assert_eq!(actions(&message), vec!["cat src/main.rs"]);
    }

    #[test]
    fn actions_uses_query_when_command_is_missing() {
        let message = json!({
            "extra": {
                "actions": [
                    {
                        "query": "search main"
                    }
                ]
            }
        });

        assert_eq!(actions(&message), vec!["search main"]);
    }

    #[test]
    fn actions_handles_missing_or_malformed_actions() {
        assert!(actions(&json!({})).is_empty());
        assert!(actions(&json!({"extra": {}})).is_empty());
        assert!(actions(&json!({"extra": {"actions": "not-an-array"}})).is_empty());
    }

    #[test]
    fn actions_handles_action_with_no_command_or_query() {
        let message = json!({
            "extra": {
                "actions": [{}]
            }
        });

        assert_eq!(actions(&message), vec![""]);
    }

    #[test]
    fn steps_of_pairs_assistant_action_with_following_user_content() {
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
                "content": "hello\nworld"
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("cat file.txt".to_string(), "hello\nworld".to_string())]
        );
    }

    #[test]
    fn steps_of_extracts_text_from_content_blocks() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {"command": "grep foo file.txt"}
                    ]
                }
            }),
            json!({
                "role": "tool",
                "content": [
                    {"type": "text", "text": "first"},
                    {"type": "text", "text": "second"}
                ]
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("grep foo file.txt".to_string(), "first second".to_string())]
        );
    }

    #[test]
    fn steps_of_ignores_messages_without_pending_assistant_action() {
        let messages = vec![
            json!({"role": "user", "content": "orphan"}),
            json!({"role": "system", "content": "system"}),
            json!({"role": "tool", "content": "tool"}),
        ];

        assert!(steps_of(&messages).is_empty());
    }

    #[test]
    fn steps_of_handles_malformed_content_without_panicking() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [{"command": "test"}]
                }
            }),
            json!({
                "role": "tool",
                "content": {
                    "unexpected": "object"
                }
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("test".to_string(), "".to_string())]
        );
    }

    #[test]
    fn truthy_matches_basic_json_truthiness() {
        assert!(!truthy(&Value::Null));
        assert!(!truthy(&json!(false)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!([])));
        assert!(!truthy(&json!({})));

        assert!(truthy(&json!(true)));
        assert!(truthy(&json!(1)));
        assert!(truthy(&json!("text")));
        assert!(truthy(&json!([1])));
        assert!(truthy(&json!({"key": "value"})));
    }

    #[test]
    fn blob_tokens_counts_reasoning_fields() {
        let message = json!({
            "reasoning_content": "abcdefgh"
        });

        assert_eq!(blob_tokens(&message), 2);
    }

    #[test]
    fn blob_tokens_counts_provider_specific_tool_fields() {
        let message = json!({
            "tool_calls": [
                {
                    "provider_specific_fields": {
                        "trace": "abcdefgh"
                    }
                }
            ]
        });

        assert!(blob_tokens(&message) > 0);
    }

    #[test]
    fn blob_tokens_ignores_empty_reasoning_values() {
        let message = json!({
            "reasoning_content": "",
            "thinking_blocks": [],
            "provider_specific_fields": {}
        });

        assert_eq!(blob_tokens(&message), 0);
    }

    #[test]
    fn reasoning_text_prefers_reasoning_content() {
        let message = json!({
            "reasoning_content": "this is the reasoning",
            "extra": {
                "actions": [
                    {"command": "ignored command"}
                ]
            }
        });

        assert_eq!(reasoning_text(&message), "this is the reasoning");
    }

    #[test]
    fn reasoning_text_falls_back_to_actions() {
        let message = json!({
            "extra": {
                "actions": [
                    {"command": "cat file.txt"},
                    {"query": "search foo"}
                ]
            }
        });

        assert_eq!(reasoning_text(&message), "cat file.txt ; search foo");
    }

    #[test]
    fn reasoning_text_defaults_to_reasoning() {
        assert_eq!(reasoning_text(&json!({})), "reasoning");
    }

    #[test]
    fn reasoning_chunk_returns_none_without_reasoning_blob() {
        let message = json!({
            "reasoning_content": ""
        });

        assert!(reasoning_chunk(&message, 3).is_none());
    }

    #[test]
    fn reasoning_chunk_sets_metadata_and_provider_eviction() {
        let message = json!({
            "reasoning_content": "some reasoning text"
        });

        let chunk = reasoning_chunk(&message, 7).unwrap();

        assert_eq!(chunk.text, "some reasoning text");
        assert_eq!(chunk.step, 7);
        assert_eq!(chunk.kind, "reasoning");
        assert_eq!(chunk.evict, "provider");
        assert!(chunk.tokens > 0);
    }

    #[test]
    fn reasoning_chunks_of_aligns_chunks_with_steps() {
        let messages = vec![
            json!({
                "role": "assistant",
                "reasoning_content": "first reasoning"
            }),
            json!({
                "role": "user",
                "content": "first response"
            }),
            json!({
                "role": "assistant",
                "reasoning_content": "second reasoning"
            }),
            json!({
                "role": "tool",
                "content": "second response"
            }),
        ];

        let chunks = reasoning_chunks_of(&messages);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].step, 0);
        assert_eq!(chunks[1].step, 1);
    }

    #[test]
    fn assistant_chunks_accept_plain_string_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "hello\nworld"
            }),
            json!({
                "role": "user",
                "content": "next"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello\nworld");
        assert_eq!(chunks[0].step, 0);
        assert_eq!(chunks[0].kind, "asst");
    }

    #[test]
    fn assistant_chunks_accepts_text_content_blocks() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "world"}
                ]
            }),
            json!({
                "role": "tool",
                "content": "done"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
    }

    #[test]
    fn assistant_chunks_skips_non_text_content_blocks() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "123"},
                    {"type": "text", "text": "visible"}
                ]
            }),
            json!({
                "role": "user",
                "content": "done"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert!(chunks.is_empty());
    }

    #[test]
    fn assistant_chunks_handles_malformed_content_without_panicking() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": 12345
            }),
            json!({
                "role": "user",
                "content": "done"
            }),
        ];

        assert!(assistant_chunks_of(&messages).is_empty());
    }

    #[test]
    fn assistant_chunks_skips_whitespace_only_text() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "   \n\t   "
            }),
            json!({
                "role": "user",
                "content": "done"
            }),
        ];

        assert!(assistant_chunks_of(&messages).is_empty());
    }
}
