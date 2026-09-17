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
            Some("assistant") => {
                pending = Some(m);
            }

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
            Some("assistant") => {
                pending = Some(m);
            }

            Some("user") | Some("tool") => {
                if let Some(p) = pending.take() {
                    let c = p.get("content").unwrap_or(&Value::Null);

                    let ok = c.is_string()
                        || c.as_array().is_some_and(|parts| {
                            parts.iter().all(|q| {
                                q.as_object().is_some_and(|o| match o.get("type") {
                                    None => true,
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

    // ============================================================
    // actions()
    // ============================================================

    #[test]
    fn actions_extracts_command_from_assistant_message() {
        let message = json!({
            "role": "assistant",
            "extra": {
                "actions": [
                    {
                        "command": "cat src/main.rs"
                    }
                ]
            }
        });

        assert_eq!(actions(&message), vec!["cat src/main.rs"]);
    }

    #[test]
    fn actions_uses_query_when_command_is_missing() {
        let message = json!({
            "role": "assistant",
            "extra": {
                "actions": [
                    {
                        "query": "search for Rust files"
                    }
                ]
            }
        });

        assert_eq!(actions(&message), vec!["search for Rust files"]);
    }

    #[test]
    fn actions_prefers_command_over_query() {
        let message = json!({
            "role": "assistant",
            "extra": {
                "actions": [
                    {
                        "command": "grep hello file.rs",
                        "query": "ignored query"
                    }
                ]
            }
        });

        assert_eq!(actions(&message), vec!["grep hello file.rs"]);
    }

    #[test]
    fn actions_handles_multiple_actions() {
        let message = json!({
            "role": "assistant",
            "extra": {
                "actions": [
                    {
                        "command": "cat a.rs"
                    },
                    {
                        "query": "find TODO"
                    },
                    {
                        "command": "cargo test"
                    }
                ]
            }
        });

        assert_eq!(
            actions(&message),
            vec!["cat a.rs", "find TODO", "cargo test"]
        );
    }

    #[test]
    fn actions_handles_missing_or_malformed_data() {
        assert_eq!(actions(&json!({})), Vec::<String>::new());

        assert_eq!(
            actions(&json!({
                "extra": {}
            })),
            Vec::<String>::new()
        );

        assert_eq!(
            actions(&json!({
                "extra": {
                    "actions": "not an array"
                }
            })),
            Vec::<String>::new()
        );

        assert_eq!(
            actions(&json!({
                "extra": {
                    "actions": [
                        "not an object"
                    ]
                }
            })),
            vec!["".to_string()]
        );
    }

    // ============================================================
    // steps_of()
    // ============================================================

    #[test]
    fn steps_of_pairs_assistant_action_with_following_user_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {
                            "command": "cat file.rs"
                        }
                    ]
                }
            }),
            json!({
                "role": "user",
                "content": "line one\nline two"
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("cat file.rs".to_string(), "line one\nline two".to_string())]
        );
    }

    #[test]
    fn steps_of_joins_multiple_actions() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {
                            "command": "cat a.rs"
                        },
                        {
                            "command": "cat b.rs"
                        }
                    ]
                }
            }),
            json!({
                "role": "tool",
                "content": "result"
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("cat a.rs ; cat b.rs".to_string(), "result".to_string())]
        );
    }

    #[test]
    fn steps_of_supports_content_block_arrays() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {
                            "command": "tool"
                        }
                    ]
                }
            }),
            json!({
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "first"
                    },
                    {
                        "type": "text",
                        "text": "second"
                    }
                ]
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("tool".to_string(), "first second".to_string())]
        );
    }

    #[test]
    fn steps_of_handles_missing_content_without_panicking() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {
                            "command": "tool"
                        }
                    ]
                }
            }),
            json!({
                "role": "user"
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("tool".to_string(), "".to_string())]
        );
    }

    #[test]
    fn steps_of_ignores_unrelated_roles() {
        let messages = vec![
            json!({
                "role": "system",
                "content": "system message"
            }),
            json!({
                "role": "assistant",
                "extra": {
                    "actions": [
                        {
                            "command": "echo hello"
                        }
                    ]
                }
            }),
            json!({
                "role": "system",
                "content": "ignored"
            }),
            json!({
                "role": "tool",
                "content": "hello"
            }),
        ];

        assert_eq!(
            steps_of(&messages),
            vec![("echo hello".to_string(), "hello".to_string())]
        );
    }

    // ============================================================
    // truthy()
    // ============================================================

    #[test]
    fn truthy_matches_expected_json_values() {
        assert!(!truthy(&Value::Null));

        assert!(!truthy(&json!(false)));
        assert!(truthy(&json!(true)));

        assert!(!truthy(&json!(0)));
        assert!(truthy(&json!(1)));

        assert!(!truthy(&json!("")));
        assert!(truthy(&json!("hello")));

        assert!(!truthy(&json!([])));
        assert!(truthy(&json!([1])));

        assert!(!truthy(&json!({})));
        assert!(truthy(&json!({"key": "value"})));
    }

    // ============================================================
    // blob_tokens()
    // ============================================================

    #[test]
    fn blob_tokens_returns_zero_when_reasoning_fields_are_absent() {
        let message = json!({
            "role": "assistant"
        });

        assert_eq!(blob_tokens(&message), 0);
    }

    #[test]
    fn blob_tokens_counts_reasoning_content() {
        let message = json!({
            "role": "assistant",
            "reasoning_content": "abcdefgh"
        });

        assert_eq!(blob_tokens(&message), 2);
    }

    #[test]
    fn blob_tokens_ignores_empty_reasoning_values() {
        let message = json!({
            "role": "assistant",
            "reasoning_content": "",
            "thinking_blocks": [],
            "provider_specific_fields": {}
        });

        assert_eq!(blob_tokens(&message), 0);
    }

    #[test]
    fn blob_tokens_counts_tool_call_provider_fields() {
        let message = json!({
            "role": "assistant",
            "tool_calls": [
                {
                    "provider_specific_fields": {
                        "trace": "abcdefgh"
                    }
                }
            ]
        });

        let tokens = blob_tokens(&message);

        assert!(tokens > 0);
    }

    #[test]
    fn blob_tokens_handles_malformed_tool_calls_without_panicking() {
        let message = json!({
            "role": "assistant",
            "tool_calls": [
                "invalid",
                null,
                42,
                {
                    "provider_specific_fields": null
                }
            ]
        });

        assert_eq!(blob_tokens(&message), 0);
    }

    // ============================================================
    // reasoning_text()
    // ============================================================

    #[test]
    fn reasoning_text_prefers_reasoning_content() {
        let message = json!({
            "role": "assistant",
            "reasoning_content": "thinking about the solution",
            "extra": {
                "actions": [
                    {
                        "command": "ignored command"
                    }
                ]
            }
        });

        assert_eq!(reasoning_text(&message), "thinking about the solution");
    }

    #[test]
    fn reasoning_text_falls_back_to_actions() {
        let message = json!({
            "role": "assistant",
            "extra": {
                "actions": [
                    {
                        "command": "cat file.rs"
                    },
                    {
                        "query": "find TODO"
                    }
                ]
            }
        });

        assert_eq!(reasoning_text(&message), "cat file.rs ; find TODO");
    }

    #[test]
    fn reasoning_text_returns_default_when_no_reasoning_exists() {
        let message = json!({
            "role": "assistant"
        });

        assert_eq!(reasoning_text(&message), "reasoning");
    }

    #[test]
    fn reasoning_text_ignores_whitespace_only_reasoning_content() {
        let message = json!({
            "role": "assistant",
            "reasoning_content": "   \n\t  ",
            "extra": {
                "actions": [
                    {
                        "command": "fallback"
                    }
                ]
            }
        });

        assert_eq!(reasoning_text(&message), "fallback");
    }

    #[test]
    fn reasoning_text_limits_reasoning_content_to_2000_characters() {
        let long_text = "a".repeat(2500);

        let message = json!({
            "role": "assistant",
            "reasoning_content": long_text
        });

        let result = reasoning_text(&message);

        assert_eq!(char_len(&result), 2000);
    }

    // ============================================================
    // reasoning_chunk()
    // ============================================================

    #[test]
    fn reasoning_chunk_returns_none_without_reasoning_blob() {
        let message = json!({
            "role": "assistant"
        });

        assert!(reasoning_chunk(&message, 3).is_none());
    }

    #[test]
    fn reasoning_chunk_creates_provider_chunk() {
        let message = json!({
            "role": "assistant",
            "reasoning_content": "abcdefgh"
        });

        let chunk = reasoning_chunk(&message, 5).unwrap();

        assert_eq!(chunk.text, "abcdefgh");

        assert_eq!(chunk.kind, "reasoning");

        assert_eq!(chunk.step, 5);

        assert_eq!(chunk.evict, "provider");

        assert!(chunk.tokens > 0);
    }

    // ============================================================
    // reasoning_chunks_of()
    // ============================================================

    #[test]
    fn reasoning_chunks_of_aligns_chunks_with_steps() {
        let messages = vec![
            json!({
                "role": "assistant",
                "reasoning_content": "abcdefgh"
            }),
            json!({
                "role": "user",
                "content": "result"
            }),
            json!({
                "role": "assistant",
                "reasoning_content": "ijklmnop"
            }),
            json!({
                "role": "tool",
                "content": "result"
            }),
        ];

        let chunks = reasoning_chunks_of(&messages);

        assert_eq!(chunks.len(), 2);

        assert_eq!(chunks[0].step, 0);

        assert_eq!(chunks[1].step, 1);
    }

    #[test]
    fn reasoning_chunks_of_skips_steps_without_reasoning_blob() {
        let messages = vec![
            json!({
                "role": "assistant"
            }),
            json!({
                "role": "user",
                "content": "result"
            }),
            json!({
                "role": "assistant",
                "reasoning_content": "abcdefgh"
            }),
            json!({
                "role": "user",
                "content": "result"
            }),
        ];

        let chunks = reasoning_chunks_of(&messages);

        assert_eq!(chunks.len(), 1);

        assert_eq!(chunks[0].step, 1);
    }

    // ============================================================
    // assistant_chunks_of()
    // ============================================================

    #[test]
    fn assistant_chunks_of_extracts_string_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "Hello from assistant"
            }),
            json!({
                "role": "user",
                "content": "response"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert_eq!(chunks.len(), 1);

        assert_eq!(chunks[0].text, "Hello from assistant");

        assert_eq!(chunks[0].kind, "asst");

        assert_eq!(chunks[0].step, 0);
    }

    #[test]
    fn assistant_chunks_of_extracts_text_content_blocks() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "Hello"
                    },
                    {
                        "type": "text",
                        "text": "world"
                    }
                ]
            }),
            json!({
                "role": "tool",
                "content": "done"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert_eq!(chunks.len(), 1);

        assert_eq!(chunks[0].text, "Hello world");
    }

    #[test]
    fn assistant_chunks_of_accepts_content_block_without_type() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {
                        "text": "Hello"
                    }
                ]
            }),
            json!({
                "role": "user",
                "content": "done"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert_eq!(chunks.len(), 1);

        assert_eq!(chunks[0].text, "Hello");
    }

    #[test]
    fn assistant_chunks_of_skips_non_text_content_blocks() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "123",
                        "name": "search"
                    }
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
    fn assistant_chunks_of_skips_tool_result_blocks() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_result",
                        "content": "tool output"
                    }
                ]
            }),
            json!({
                "role": "tool",
                "content": "done"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert!(chunks.is_empty());
    }

    #[test]
    fn assistant_chunks_of_skips_malformed_content() {
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

        let chunks = assistant_chunks_of(&messages);

        assert!(chunks.is_empty());
    }

    #[test]
    fn assistant_chunks_of_skips_missing_content() {
        let messages = vec![
            json!({
                "role": "assistant"
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
    fn assistant_chunks_of_skips_whitespace_only_content() {
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

        let chunks = assistant_chunks_of(&messages);

        assert!(chunks.is_empty());
    }

    #[test]
    fn assistant_chunks_of_handles_multiple_steps() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "first response"
            }),
            json!({
                "role": "user",
                "content": "first result"
            }),
            json!({
                "role": "assistant",
                "content": "second response"
            }),
            json!({
                "role": "tool",
                "content": "second result"
            }),
        ];

        let chunks = assistant_chunks_of(&messages);

        assert_eq!(chunks.len(), 2);

        assert_eq!(chunks[0].text, "first response");

        assert_eq!(chunks[1].text, "second response");

        assert_eq!(chunks[0].step, 0);

        assert_eq!(chunks[1].step, 1);
    }
}
