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

    fn msgs(v: Value) -> Vec<Value> {
        v.as_array().cloned().unwrap_or_default()
    }

    // --- actions ----------------------------------------------------------

    #[test]
    fn test_actions_prefers_command_and_falls_back_to_query() {
        let m = json!({"extra": {"actions": [
            {"command": "ls -la"},
            {"query": "foo"},
            {"command": "", "query": "bar"},
            {"command": "cat x", "query": "y"},
            {"other": 1},
        ]}});
        assert_eq!(actions(&m), vec!["ls -la", "foo", "bar", "cat x", ""]);
    }

    #[test]
    fn test_actions_missing_or_malformed_returns_empty_vec() {
        assert!(actions(&json!({})).is_empty());
        assert!(actions(&json!({"extra": {}})).is_empty());
        assert!(actions(&json!({"extra": {"actions": []}})).is_empty());
        assert!(actions(&json!({"extra": {"actions": "nope"}})).is_empty());
        // A non-object action item still contributes an empty string slot.
        assert_eq!(actions(&json!({"extra": {"actions": [42]}})), vec![""]);
    }

    // --- steps_of ---------------------------------------------------------

    #[test]
    fn test_steps_of_pairs_assistant_with_next_user_or_tool() {
        let ms = msgs(json!([
            {"role": "system", "content": "sys"},
            {"role": "assistant", "extra": {"actions": [{"command": "ls"}]}, "content": "thinking"},
            {"role": "user", "content": "obs one"},
            {"role": "assistant", "extra": {"actions": [{"command": "a"}, {"query": "b"}]}},
            {"role": "tool", "content": "tool out"},
        ]));
        assert_eq!(
            steps_of(&ms),
            vec![
                ("ls".to_string(), "obs one".to_string()),
                ("a ; b".to_string(), "tool out".to_string()),
            ]
        );
    }

    #[test]
    fn test_steps_of_drops_dangling_assistant_and_keeps_last_of_a_run() {
        let ms = msgs(json!([
            {"role": "assistant", "extra": {"actions": [{"command": "first"}]}},
            {"role": "assistant", "extra": {"actions": [{"command": "second"}]}},
            {"role": "user", "content": "obs"},
            {"role": "assistant", "extra": {"actions": [{"command": "dangling"}]}},
        ]));
        assert_eq!(
            steps_of(&ms),
            vec![("second".to_string(), "obs".to_string())]
        );
    }

    #[test]
    fn test_steps_of_content_string_blocks_and_missing() {
        let ms = msgs(json!([
            {"role": "assistant", "extra": {"actions": [{"command": "a"}]}},
            {"role": "user", "content": [{"type": "text", "text": "hello"}, {"type": "text", "text": "world"}]},
            {"role": "assistant", "extra": {"actions": [{"command": "b"}]}},
            {"role": "user"},
        ]));
        assert_eq!(
            steps_of(&ms),
            vec![
                ("a".to_string(), "hello world".to_string()),
                ("b".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn test_steps_of_ignores_user_without_pending_assistant() {
        let ms = msgs(json!([
            {"role": "user", "content": "orphan"},
            {"role": "assistant", "extra": {"actions": [{"command": "x"}]}},
            {"role": "user", "content": "obs"},
        ]));
        assert_eq!(steps_of(&ms), vec![("x".to_string(), "obs".to_string())]);
    }

    #[test]
    fn test_steps_of_text_blocks_join_with_spaces_even_when_blocks_lack_text() {
        let ms = msgs(json!([
            {"role": "assistant", "extra": {"actions": [{"command": "a"}]}},
            {"role": "user", "content": [{"type": "tool_result", "content": "z"}, {"type": "text", "text": "after"}]},
        ]));
        // The tool_result block contributes "" so the join starts with a space.
        assert_eq!(steps_of(&ms), vec![("a".to_string(), " after".to_string())]);
    }

    // --- assistant_chunks_of ---------------------------------------------

    #[test]
    fn test_assistant_chunks_of_plain_string_and_text_blocks() {
        let ms = msgs(json!([
            {"role": "assistant", "content": "one\ntwo"},
            {"role": "user", "content": "r"},
            {"role": "assistant", "content": [{"type": "text", "text": "x"}, {"text": "y"}]},
            {"role": "user", "content": "r"},
        ]));
        assert_eq!(
            assistant_chunks_of(&ms)
                .iter()
                .map(|c| (c.text.clone(), c.step, c.kind.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("one\ntwo".to_string(), 0, "asst".to_string()),
                ("x y".to_string(), 1, "asst".to_string()),
            ]
        );
    }

    #[test]
    fn test_assistant_chunks_of_skips_a_message_with_any_non_text_block() {
        // A single tool_use block makes the WHOLE message non-text.
        let ms = msgs(json!([
            {"role": "assistant", "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "t", "name": "n"}
            ]},
            {"role": "user", "content": "r"},
        ]));
        assert!(assistant_chunks_of(&ms).is_empty());
    }

    #[test]
    fn test_assistant_chunks_of_skips_blank_and_non_content() {
        let ms = msgs(json!([
            {"role": "assistant", "content": "   "},
            {"role": "user", "content": "r"},
            {"role": "assistant", "content": 42},
            {"role": "user", "content": "r"},
        ]));
        assert!(assistant_chunks_of(&ms).is_empty());
    }

    // --- reasoning --------------------------------------------------------

    #[test]
    fn test_reasoning_text_prefers_content_then_actions_then_placeholder() {
        assert_eq!(
            reasoning_text(&json!({"reasoning_content": "  think  "})),
            "  think  "
        );
        assert_eq!(
            reasoning_text(
                &json!({"reasoning_content": "   ", "extra": {"actions": [{"command": "cmd"}]}})
            ),
            "cmd"
        );
        assert_eq!(reasoning_text(&json!({})), "reasoning");
    }

    #[test]
    fn test_reasoning_text_truncates_to_2000_chars() {
        let m = json!({"reasoning_content": "a".repeat(2500)});
        assert_eq!(char_len(&reasoning_text(&m)), 2000);
    }

    #[test]
    fn test_reasoning_chunk_is_provider_evicted_only_with_blob() {
        assert!(reasoning_chunk(&json!({}), 0).is_none());
        let c = reasoning_chunk(&json!({"reasoning_content": "abcd"}), 7).unwrap();
        assert_eq!(c.kind, "reasoning");
        assert_eq!(c.evict, "provider");
        assert_eq!(c.step, 7);
        assert!(!c.text.is_empty());
    }

    #[test]
    fn test_reasoning_chunks_of_aligns_with_steps_and_skips_empty() {
        let ms = msgs(json!([
            {"role": "assistant", "reasoning_content": "aaaa"},
            {"role": "user", "content": "o"},
            {"role": "assistant"},
            {"role": "user", "content": "o"},
            {"role": "assistant", "reasoning_content": "cccc"},
            {"role": "user", "content": "o"},
        ]));
        assert_eq!(steps_of(&ms).len(), 3);
        // Step 1 carries no blob, so only steps 0 and 2 yield reasoning chunks.
        assert_eq!(
            reasoning_chunks_of(&ms)
                .iter()
                .map(|c| c.step)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
    }

    #[test]
    fn test_blob_tokens_sums_tool_calls_and_reason_fields() {
        assert_eq!(blob_tokens(&json!({})), 0);
        assert_eq!(
            blob_tokens(&json!({"reasoning_content": "abcde", "thinking_blocks": [1, 2, 3]})),
            3
        );
        assert_eq!(
            blob_tokens(&json!({"tool_calls": [{"provider_specific_fields": {"x": [1, 2]}}]})),
            3
        );
        // Falsy payloads contribute nothing.
        assert_eq!(blob_tokens(&json!({"reasoning_content": ""})), 0);
        assert_eq!(
            blob_tokens(&json!({"tool_calls": [{"provider_specific_fields": {}}]})),
            0
        );
    }

    #[test]
    fn test_truthy_matches_python_truthiness() {
        for v in [
            json!(null),
            json!(false),
            json!(0),
            json!(0.0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert!(!truthy(&v), "{v} should be falsy");
        }
        for v in [
            json!(true),
            json!(1),
            json!(-1),
            json!("x"),
            json!([0]),
            json!({"a": 0}),
        ] {
            assert!(truthy(&v), "{v} should be truthy");
        }
    }
}
