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

// ============================================================================
// Unit tests
//   1. Text content extraction (single string blocks vs. arrays of content
//      blocks, including mixed/missing "type" fields).
//   2. Tool call (tool_use) / tool response (tool_result) content parsing,
//      via `actions()` and `blob_tokens()`.
//   3. Malformed / unexpected JSON shapes (wrong types, missing fields,
//      nulls) being handled gracefully without panicking.
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- content_text: string content -----------------------------------

    #[test]
    fn test_content_text_plain_string() {
        let v = json!("hello world");
        assert_eq!(content_text(&v), "hello world");
    }

    #[test]
    fn test_content_text_empty_string() {
        let v = json!("");
        assert_eq!(content_text(&v), "");
    }

    // -- content_text: array-of-blocks content ---------------------------

    #[test]
    fn test_content_text_array_of_text_blocks() {
        let v = json!([
            {"type": "text", "text": "part one"},
            {"type": "text", "text": "part two"}
        ]);
        assert_eq!(content_text(&v), "part one part two");
    }

    #[test]
    fn test_content_text_array_missing_text_field() {
        // Blocks without a "text" key contribute an empty string, joined
        // with the surrounding spaces (mirrors the Python `.get("text", "")`
        // semantics ported here as `unwrap_or("")`).
        let v = json!([
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "id": "abc"},
            {"type": "text", "text": "world"}
        ]);
        assert_eq!(content_text(&v), "hello  world");
    }

    #[test]
    fn test_content_text_array_with_non_object_entries() {
        // Non-object array entries (string, number, null) must not panic.
        // `.as_object()` returns None for them, and `filter_map` drops a
        // None entirely rather than mapping it to an empty string, so they
        // contribute nothing at all to the join — not even a separator.
        let v = json!(["not an object", 42, null, {"text": "ok"}]);
        assert_eq!(content_text(&v), "ok");
    }

    #[test]
    fn test_content_text_unexpected_type_returns_empty() {
        // Numbers, bools, objects (not arrays), and null are all
        // "unexpected" shapes for `content` and should degrade to "".
        assert_eq!(content_text(&json!(123)), "");
        assert_eq!(content_text(&json!(true)), "");
        assert_eq!(content_text(&json!(null)), "");
        assert_eq!(content_text(&json!({"not": "an array"})), "");
    }

    // -- actions(): tool_use / tool call style extraction ----------------

    #[test]
    fn test_actions_extracts_command_field() {
        let m = json!({
            "role": "assistant",
            "extra": {"actions": [{"command": "ls -la"}]}
        });
        assert_eq!(actions(&m), vec!["ls -la".to_string()]);
    }

    #[test]
    fn test_actions_falls_back_to_query_when_command_empty_or_absent() {
        let m = json!({
            "role": "assistant",
            "extra": {"actions": [
                {"command": "", "query": "search term"},
                {"query": "another query"},
                {"command": "real command"}
            ]}
        });
        assert_eq!(
            actions(&m),
            vec![
                "search term".to_string(),
                "another query".to_string(),
                "real command".to_string(),
            ]
        );
    }

    #[test]
    fn test_actions_defaults_to_empty_string_when_neither_field_present() {
        let m = json!({
            "role": "assistant",
            "extra": {"actions": [{"other_field": "irrelevant"}]}
        });
        assert_eq!(actions(&m), vec!["".to_string()]);
    }

    #[test]
    fn test_actions_missing_extra_returns_empty_vec() {
        let m = json!({"role": "assistant", "content": "no extra field here"});
        assert!(actions(&m).is_empty());
    }

    #[test]
    fn test_actions_extra_actions_wrong_type_returns_empty_vec() {
        // `extra.actions` present but not an array — must not panic.
        let m = json!({"role": "assistant", "extra": {"actions": "not-an-array"}});
        assert!(actions(&m).is_empty());
    }

    #[test]
    fn test_actions_entries_wrong_type_are_tolerated() {
        // Individual entries in `actions` that aren't objects should not
        // panic; `.get()` on a non-object Value returns None, so both
        // command/query lookups fall back to "".
        let m = json!({
            "role": "assistant",
            "extra": {"actions": ["not-an-object", 5, null]}
        });
        assert_eq!(
            actions(&m),
            vec!["".to_string(), "".to_string(), "".to_string()]
        );
    }

    // -- blob_tokens(): tool_calls / provider_specific_fields parsing ----

    #[test]
    fn test_blob_tokens_zero_when_no_relevant_fields() {
        let m = json!({"role": "assistant", "content": "just text"});
        assert_eq!(blob_tokens(&m), 0);
    }

    #[test]
    fn test_blob_tokens_counts_tool_call_provider_specific_fields() {
        let m = json!({
            "role": "assistant",
            "tool_calls": [
                {"provider_specific_fields": {"foo": "bar"}}
            ]
        });
        assert!(blob_tokens(&m) > 0);
    }

    #[test]
    fn test_blob_tokens_ignores_falsy_provider_specific_fields() {
        let m = json!({
            "role": "assistant",
            "tool_calls": [
                {"provider_specific_fields": {}},
                {"provider_specific_fields": null},
                {"provider_specific_fields": ""}
            ]
        });
        assert_eq!(blob_tokens(&m), 0);
    }

    #[test]
    fn test_blob_tokens_tool_calls_wrong_type_does_not_panic() {
        // `tool_calls` present but not an array of objects.
        let m = json!({"role": "assistant", "tool_calls": "unexpected-string"});
        assert_eq!(blob_tokens(&m), 0);

        let m2 = json!({"role": "assistant", "tool_calls": [1, "x", null, true]});
        assert_eq!(blob_tokens(&m2), 0);
    }

    #[test]
    fn test_blob_tokens_sums_reasoning_fields() {
        let m = json!({
            "role": "assistant",
            "reasoning_content": "some reasoning text here",
            "thinking_blocks": [{"type": "thinking", "text": "block"}],
        });
        assert!(blob_tokens(&m) > 0);
    }

    // -- truthy(): Python-truthiness parity, including edge shapes -------

    #[test]
    fn test_truthy_matches_python_semantics() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(false)));
        assert!(truthy(&json!(true)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!(0.0)));
        assert!(truthy(&json!(1)));
        assert!(truthy(&json!(-1)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!("x")));
        assert!(!truthy(&json!([])));
        assert!(truthy(&json!([1])));
        assert!(!truthy(&json!({})));
        assert!(truthy(&json!({"a": 1})));
    }

    // -- steps_of(): malformed / missing content handled gracefully ------

    #[test]
    fn test_steps_of_pairs_assistant_actions_with_next_observation() {
        let messages = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "cmd1"}]}}),
            json!({"role": "user", "content": "obs1"}),
            json!({"role": "assistant", "extra": {"actions": [{"query": "q2"}]}}),
            json!({"role": "tool", "content": [{"type": "text", "text": "obs2"}]}),
        ];
        let steps = steps_of(&messages);
        assert_eq!(
            steps,
            vec![
                ("cmd1".to_string(), "obs1".to_string()),
                ("q2".to_string(), "obs2".to_string()),
            ]
        );
    }

    #[test]
    fn test_steps_of_missing_content_field_defaults_to_empty_string() {
        let messages = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "cmd"}]}}),
            json!({"role": "user"}), // no "content" key at all
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps, vec![("cmd".to_string(), "".to_string())]);
    }

    #[test]
    fn test_steps_of_ignores_messages_with_missing_or_unexpected_role() {
        let messages = vec![
            json!({"no_role_field": true}),
            json!({"role": 123}), // role is not a string
            json!({"role": "system", "content": "sys prompt"}),
            json!({"role": "assistant", "extra": {"actions": [{"command": "cmd"}]}}),
            json!({"role": "user", "content": "obs"}),
        ];
        // None of the malformed/system messages should panic or produce
        // spurious steps; only the assistant/user pair yields a step.
        let steps = steps_of(&messages);
        assert_eq!(steps, vec![("cmd".to_string(), "obs".to_string())]);
    }

    #[test]
    fn test_steps_of_dangling_assistant_action_without_observation_is_dropped() {
        let messages = vec![json!({
            "role": "assistant",
            "extra": {"actions": [{"command": "cmd"}]}
        })];
        assert!(steps_of(&messages).is_empty());
    }

    #[test]
    fn test_steps_of_empty_input_returns_empty() {
        let messages: Vec<Value> = vec![];
        assert!(steps_of(&messages).is_empty());
    }

    // -- reasoning_chunk / reasoning_chunks_of ----------------------------

    #[test]
    fn test_reasoning_chunk_none_when_no_blob() {
        let m = json!({"role": "assistant", "content": "plain text only"});
        assert!(reasoning_chunk(&m, 0).is_none());
    }

    #[test]
    fn test_reasoning_chunk_some_when_blob_present() {
        let m = json!({
            "role": "assistant",
            "reasoning_content": "a chunk of reasoning text"
        });
        let c = reasoning_chunk(&m, 3).expect("blob present so a chunk must be produced");
        assert_eq!(c.step, 3);
        assert_eq!(c.kind, "reasoning");
        assert_eq!(c.evict, "provider");
        assert!(c.tokens > 0);
        assert_eq!(c.text, "a chunk of reasoning text");
    }

    #[test]
    fn test_reasoning_chunks_of_skips_malformed_messages_but_keeps_valid_ones() {
        // Mix of a valid reasoning-bearing assistant message with several
        // malformed/edge-case messages around it. The malformed messages
        // must not panic AND must not contribute spurious chunks; only the
        // one valid reasoning blob should surface, at step 0 (its position
        // among assistant/user-or-tool pairs).
        let messages = vec![
            json!({"role": "assistant", "reasoning_content": "reasoning here"}),
            json!({"role": "user", "content": null}),
            json!(null), // an entirely malformed top-level message
            json!("not-even-an-object"),
        ];
        let chunks = reasoning_chunks_of(&messages);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].step, 0);
        assert_eq!(chunks[0].kind, "reasoning");
        assert_eq!(chunks[0].text, "reasoning here");
    }

    #[test]
    fn test_reasoning_chunks_of_all_malformed_messages_yields_no_chunks_without_panicking() {
        // Every element is a shape with no recognizable "role"/assistant
        // pairing; the function must return an empty vec, not panic.
        let messages = vec![json!(null), json!("not-even-an-object"), json!(42)];
        let chunks = reasoning_chunks_of(&messages);
        assert!(chunks.is_empty());
    }

    // -- assistant_chunks_of: content shape validation --------------------

    #[test]
    fn test_assistant_chunks_of_skips_non_text_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "1", "input": {}}]
            }),
            json!({"role": "user", "content": "obs"}),
        ];
        // Non-text content blocks mean this assistant message is skipped
        // entirely, so no chunks are produced.
        assert!(assistant_chunks_of(&messages).is_empty());
    }

    #[test]
    fn test_assistant_chunks_of_accepts_blocks_missing_type_field() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{"text": "no type key, defaults to text"}]
            }),
            json!({"role": "user", "content": "obs"}),
        ];
        // Blocks missing a "type" key are treated as implicit "text",
        // matching Python's `.get("type", "text")` default.
        assert!(!assistant_chunks_of(&messages).is_empty());
    }

    #[test]
    fn test_assistant_chunks_of_handles_missing_content_gracefully() {
        let messages = vec![
            json!({"role": "assistant"}), // no "content" key
            json!({"role": "user", "content": "obs"}),
        ];
        // Missing content defaults to Value::Null, which is neither a
        // string nor an array — message should be skipped, not panic.
        assert!(assistant_chunks_of(&messages).is_empty());
    }
}