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

    // --- text extraction / arrays ---

    #[test]
    fn content_text_single_string_block() {
        assert_eq!(content_text(&json!("hello world")), "hello world");
    }

    #[test]
    fn content_text_array_of_text_blocks_joins_with_space() {
        let c = json!([{"type": "text", "text": "hello"}, {"type": "text", "text": "world"}]);
        assert_eq!(content_text(&c), "hello world");
    }

    #[test]
    fn content_text_array_with_non_text_object_contributes_empty_segment() {
        // The tool_use block has no "text" key -> unwrap_or("") -> still
        // joined, producing a leading space (documents current behavior).
        let c = json!([
            {"type": "tool_use", "id": "1", "name": "bash", "input": {}},
            {"type": "text", "text": "world"}
        ]);
        assert_eq!(content_text(&c), " world");
    }

    #[test]
    fn content_text_array_skips_non_object_entries_entirely() {
        let c = json!([123, null, {"type": "text", "text": "x"}]);
        assert_eq!(content_text(&c), "x");
    }

    #[test]
    fn content_text_empty_array_is_empty_string() {
        assert_eq!(content_text(&json!([])), "");
    }

    #[test]
    fn content_text_bare_object_is_empty_string() {
        let c = json!({"type": "text", "text": "should not be read"});
        assert_eq!(content_text(&c), "");
    }

    #[test]
    fn content_text_unexpected_scalar_types_are_empty_string_not_panic() {
        assert_eq!(content_text(&json!(42)), "");
        assert_eq!(content_text(&json!(true)), "");
        assert_eq!(content_text(&Value::Null), "");
    }

    // --- tool_use (assistant_chunks_of) ---

    #[test]
    fn assistant_chunks_of_skips_pure_tool_use_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "1", "name": "bash", "input": {"command": "ls"}}]
            }),
            json!({"role": "tool", "content": "file1\nfile2"}),
        ];
        assert!(assistant_chunks_of(&messages).is_empty());
    }

    #[test]
    fn assistant_chunks_of_skips_message_when_any_part_is_non_text() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Let me check that."},
                    {"type": "tool_use", "id": "1", "name": "bash", "input": {}}
                ]
            }),
            json!({"role": "tool", "content": "ok"}),
        ];
        assert!(assistant_chunks_of(&messages).is_empty());
    }

    #[test]
    fn assistant_chunks_of_treats_missing_type_as_text() {
        let messages = vec![
            json!({"role": "assistant", "content": [{"text": "no type field here"}]}),
            json!({"role": "tool", "content": "ok"}),
        ];
        let chunks = assistant_chunks_of(&messages);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "no type field here");
    }

    // --- tool_result (steps_of) ---

    #[test]
    fn steps_of_tool_result_block_without_top_level_text_yields_empty_observation() {
        // Real Anthropic tool_result blocks nest their payload under
        // "content", not "text" -- content_text only looks for "text",
        // so this currently yields an empty observation string.
        let messages = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "cat file.py"}]}}),
            json!({
                "role": "tool",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "actual output"}]
            }),
        ];
        assert_eq!(
            steps_of(&messages),
            vec![("cat file.py".to_string(), "".to_string())]
        );
    }

    #[test]
    fn steps_of_tool_result_block_with_text_key_is_extracted() {
        let messages = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "grep foo"}]}}),
            json!({
                "role": "tool",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "text": "match found"}]
            }),
        ];
        assert_eq!(
            steps_of(&messages),
            vec![("grep foo".to_string(), "match found".to_string())]
        );
    }

    // --- malformed JSON, must not panic ---

    #[test]
    fn steps_of_handles_malformed_messages_without_panicking() {
        let messages = vec![
            json!("just a string, not an object"), // no "role" key -> ignored
            json!(42),                             // no "role" key -> ignored
            json!({"content": "no role field"}),   // missing "role" -> ignored
            json!({"role": "assistant", "extra": {"actions": "not an array"}}), // wrong type
            json!({"role": "user"}),               // no "content" key
        ];
        assert_eq!(steps_of(&messages), vec![("".to_string(), "".to_string())]);
    }

    #[test]
    fn assistant_chunks_of_handles_missing_content_field_without_panicking() {
        let messages = vec![
            json!({"role": "assistant"}), // no "content" key at all
            json!({"role": "tool", "content": "output"}),
        ];
        assert!(assistant_chunks_of(&messages).is_empty());
    }
}
