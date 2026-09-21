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

    // --- content_text: text extraction + malformed-block robustness ------

    #[test]
    fn content_text_plain_string() {
        assert_eq!(content_text(&json!("hello")), "hello");
    }

    #[test]
    fn content_text_array_of_text_blocks_joins_with_space() {
        let c = json!([{"type": "text", "text": "hi"}, {"type": "text", "text": "there"}]);
        assert_eq!(content_text(&c), "hi there");
    }

    #[test]
    fn content_text_block_missing_text_field_yields_empty_segment_not_panic() {
        // A tool_use-shaped block has no "text" key at all; content_text
        // must degrade to "" for that block rather than panicking.
        let c = json!([{"type": "text", "text": "before"}, {"type": "tool_use", "id": "1", "input": {}}]);
        assert_eq!(content_text(&c), "before ");
    }

    #[test]
    fn content_text_array_with_non_object_entries_skips_them_gracefully() {
        // A malformed content array mixing a bare number/null in with proper
        // blocks must not panic; non-object entries are simply dropped.
        let c = json!([{"type": "text", "text": "ok"}, 42, null, "bare-string"]);
        assert_eq!(content_text(&c), "ok");
    }

    #[test]
    fn content_text_non_string_non_array_yields_empty_string() {
        assert_eq!(content_text(&json!(null)), "");
        assert_eq!(content_text(&json!(42)), "");
        assert_eq!(content_text(&json!({"unexpected": "shape"})), "");
    }

    // --- actions() --------------------------------------------------------

    #[test]
    fn actions_missing_extra_field_returns_empty_vec() {
        assert_eq!(actions(&json!({"role": "assistant"})), Vec::<String>::new());
    }

    #[test]
    fn actions_falls_back_to_query_when_command_empty() {
        let m = json!({"extra": {"actions": [{"command": "", "query": "search term"}]}});
        assert_eq!(actions(&m), vec!["search term".to_string()]);
    }

    #[test]
    fn actions_neither_command_nor_query_yields_empty_string_entry() {
        let m = json!({"extra": {"actions": [{"other_field": true}]}});
        assert_eq!(actions(&m), vec!["".to_string()]);
    }

    #[test]
    fn actions_prefers_nonempty_command_over_query() {
        let m = json!({"extra": {"actions": [{"command": "ls -la", "query": "ignored"}]}});
        assert_eq!(actions(&m), vec!["ls -la".to_string()]);
    }

    // --- truthy(): every Value variant -------------------------------------

    #[test]
    fn truthy_matches_python_bool_semantics_per_variant() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(false)));
        assert!(truthy(&json!(true)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!(0.0)));
        assert!(truthy(&json!(0.0001)));
        assert!(truthy(&json!(-1)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!("x")));
        assert!(!truthy(&json!([])));
        assert!(truthy(&json!([false])));
        assert!(!truthy(&json!({})));
        assert!(truthy(&json!({"k": null})));
    }

    // --- steps_of() --------------------------------------------------------

    #[test]
    fn steps_of_pairs_assistant_action_with_following_user_text() {
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "cat a.py"}]}}),
            json!({"role": "user", "content": "file contents"}),
        ];
        assert_eq!(
            steps_of(&msgs),
            vec![("cat a.py".to_string(), "file contents".to_string())]
        );
    }

    #[test]
    fn steps_of_treats_tool_role_same_as_user() {
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "ls"}]}}),
            json!({"role": "tool", "content": "a.py b.py"}),
        ];
        assert_eq!(
            steps_of(&msgs),
            vec![("ls".to_string(), "a.py b.py".to_string())]
        );
    }

    #[test]
    fn steps_of_consecutive_assistants_overwrite_pending_and_lose_the_first() {
        // Documents current behavior: with no observation between two
        // assistant turns, `pending` is simply overwritten, so the first
        // assistant's action is dropped rather than queued.
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "first"}]}}),
            json!({"role": "assistant", "extra": {"actions": [{"command": "second"}]}}),
            json!({"role": "user", "content": "obs"}),
        ];
        assert_eq!(
            steps_of(&msgs),
            vec![("second".to_string(), "obs".to_string())]
        );
    }

    #[test]
    fn steps_of_trailing_assistant_with_no_observation_is_dropped() {
        let msgs =
            vec![json!({"role": "assistant", "extra": {"actions": [{"command": "orphan"}]}})];
        assert_eq!(steps_of(&msgs), Vec::new());
    }

    #[test]
    fn steps_of_ignores_unknown_roles() {
        let msgs = vec![
            json!({"role": "system", "content": "ignored"}),
            json!({"role": "assistant", "extra": {"actions": [{"command": "x"}]}}),
            json!({"role": "user", "content": "y"}),
        ];
        assert_eq!(steps_of(&msgs), vec![("x".to_string(), "y".to_string())]);
    }

    // --- blob_tokens() ------------------------------------------------------

    #[test]
    fn blob_tokens_ignores_falsy_provider_specific_fields() {
        let m = json!({"tool_calls": [{"provider_specific_fields": {}}]});
        assert_eq!(blob_tokens(&m), 0);
    }

    #[test]
    fn blob_tokens_sums_truthy_provider_specific_fields_across_tool_calls() {
        let m = json!({"tool_calls": [
            {"provider_specific_fields": {"a": "12345678"}},
            {"provider_specific_fields": {"b": "1234"}},
        ]});
        // py_json_dumps({"a": "12345678"}) == {"a": "12345678"} (17 chars) -> 17/4 = 4
        // py_json_dumps({"b": "1234"}) == {"b": "1234"} (13 chars) -> 13/4 = 3
        assert_eq!(blob_tokens(&m), 4 + 3);
    }

    #[test]
    fn blob_tokens_reasoning_field_non_string_uses_json_dumps() {
        let m = json!({"reasoning_content": {"steps": ["a", "b"]}});
        let expected = char_len(&py_json_dumps(&json!({"steps": ["a", "b"]}))) as i64 / 4;
        assert_eq!(blob_tokens(&m), expected);
        assert!(expected > 0);
    }

    #[test]
    fn blob_tokens_no_reasoning_fields_present_is_zero() {
        assert_eq!(blob_tokens(&json!({"role": "assistant"})), 0);
    }

    // --- reasoning_text() ----------------------------------------------------

    #[test]
    fn reasoning_text_prefers_reasoning_content_and_truncates_at_2000_chars() {
        let long = "x".repeat(2500);
        let m = json!({"reasoning_content": long});
        let got = reasoning_text(&m);
        assert_eq!(char_len(&got), 2000);
    }

    #[test]
    fn reasoning_text_falls_back_to_joined_actions_when_no_reasoning_content() {
        let m = json!({"extra": {"actions": [{"command": "grep foo"}, {"command": "cat bar"}]}});
        assert_eq!(reasoning_text(&m), "grep foo ; cat bar");
    }

    #[test]
    fn reasoning_text_falls_back_to_literal_when_nothing_available() {
        assert_eq!(reasoning_text(&json!({})), "reasoning");
    }

    // --- reasoning_chunk() / reasoning_chunks_of() ----------------------------

    #[test]
    fn reasoning_chunk_none_when_no_reasoning_payload() {
        assert_eq!(reasoning_chunk(&json!({}), 0), None);
    }

    #[test]
    fn reasoning_chunk_some_with_provider_evict_when_payload_present() {
        let m = json!({"reasoning_content": "some reasoning text here"});
        let c = reasoning_chunk(&m, 3).expect("expected a chunk");
        assert_eq!(c.evict, "provider");
        assert_eq!(c.step, 3);
        assert_eq!(c.kind, "reasoning");
        assert!(c.tokens > 0);
    }

    #[test]
    fn reasoning_chunks_of_steps_align_with_steps_of() {
        let msgs = vec![
            json!({"role": "assistant", "reasoning_content": "thinking hard about this"}),
            json!({"role": "user", "content": "obs1"}),
            json!({"role": "assistant"}), // no reasoning payload -> no chunk, but step still advances
            json!({"role": "user", "content": "obs2"}),
        ];
        let chunks = reasoning_chunks_of(&msgs);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].step, 0);
    }

    // --- assistant_chunks_of(): malformed-content robustness -----------------

    #[test]
    fn assistant_chunks_of_plain_string_content_is_chunked() {
        let msgs = vec![
            json!({"role": "assistant", "content": "Here is my plan.\nStep two."}),
            json!({"role": "user", "content": "ok"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "asst");
        assert_eq!(chunks[0].step, 0);
    }

    #[test]
    fn assistant_chunks_of_array_blocks_with_missing_type_default_to_text() {
        // Python's `.get("type", "text")` default means a block with no
        // "type" key at all is still treated as text.
        let msgs = vec![
            json!({"role": "assistant", "content": [{"text": "no type key here"}]}),
            json!({"role": "user", "content": "ok"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn assistant_chunks_of_skips_message_containing_a_tool_use_block_without_panicking() {
        let msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "calling a tool"},
                {"type": "tool_use", "id": "1", "input": {}},
            ]}),
            json!({"role": "user", "content": "ok"}),
            json!({"role": "assistant", "content": "second turn, plain text"}),
            json!({"role": "user", "content": "ok2"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        // Only the second (pure-text) assistant turn produces a chunk; the
        // first is skipped but must not panic and must not corrupt the step
        // counter for the turn that follows it.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].step, 1);
    }

    #[test]
    fn assistant_chunks_of_missing_content_field_is_skipped_without_panic() {
        let msgs = vec![
            json!({"role": "assistant"}),
            json!({"role": "user", "content": "ok"}),
        ];
        assert_eq!(assistant_chunks_of(&msgs), Vec::new());
    }

    #[test]
    fn assistant_chunks_of_array_with_non_object_entry_is_skipped_without_panic() {
        let msgs = vec![
            json!({"role": "assistant", "content": ["not", "an", "object", "list"]}),
            json!({"role": "user", "content": "ok"}),
        ];
        assert_eq!(assistant_chunks_of(&msgs), Vec::new());
    }
}
