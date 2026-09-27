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
    fn actions_prefers_command_falls_back_to_query() {
        let m = json!({
            "extra": {
                "actions": [
                    {"command": "cat f.py", "query": "ignored"},
                    {"query": "search term"},
                    {}
                ]
            }
        });
        assert_eq!(
            actions(&m),
            vec![
                "cat f.py".to_string(),
                "search term".to_string(),
                "".to_string()
            ]
        );
    }

    #[test]
    fn actions_missing_extra_or_actions_returns_empty() {
        assert_eq!(actions(&json!({})), Vec::<String>::new());
        assert_eq!(actions(&json!({"extra": {}})), Vec::<String>::new());
        assert_eq!(
            actions(&json!({"extra": {"actions": "not-an-array"}})),
            Vec::<String>::new()
        );
    }

    #[test]
    fn actions_empty_command_string_falls_back_to_query() {
        let m = json!({"extra": {"actions": [{"command": "", "query": "q"}]}});
        assert_eq!(actions(&m), vec!["q".to_string()]);
    }

    #[test]
    fn steps_of_pairs_assistant_with_next_user_or_tool() {
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "ls"}]}}),
            json!({"role": "user", "content": "file1\nfile2"}),
            json!({"role": "assistant", "extra": {"actions": [{"command": "cat f"}]}}),
            json!({"role": "tool", "content": "contents"}),
        ];
        let steps = steps_of(&msgs);
        assert_eq!(
            steps,
            vec![
                ("ls".to_string(), "file1\nfile2".to_string()),
                ("cat f".to_string(), "contents".to_string()),
            ]
        );
    }

    #[test]
    fn steps_of_drops_unpaired_trailing_assistant() {
        let msgs = vec![json!({"role": "assistant", "extra": {"actions": []}})];
        assert_eq!(steps_of(&msgs), Vec::new());
    }

    #[test]
    fn steps_of_content_as_array_of_text_blocks() {
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "x"}]}}),
            json!({"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}),
        ];
        assert_eq!(steps_of(&msgs), vec![("x".to_string(), "a b".to_string())]);
    }

    #[test]
    fn steps_of_missing_content_field_does_not_panic() {
        let msgs = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "x"}]}}),
            json!({"role": "user"}),
        ];
        assert_eq!(steps_of(&msgs), vec![("x".to_string(), "".to_string())]);
    }

    #[test]
    fn truthy_matches_python_semantics() {
        assert!(!truthy(&Value::Null));
        assert!(!truthy(&json!(false)));
        assert!(truthy(&json!(true)));
        assert!(!truthy(&json!(0)));
        assert!(truthy(&json!(0.0001)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!("x")));
        assert!(!truthy(&json!([])));
        assert!(truthy(&json!([1])));
        assert!(!truthy(&json!({})));
        assert!(truthy(&json!({"a": 1})));
    }

    #[test]
    fn blob_tokens_sums_reasoning_fields_and_tool_calls() {
        let m = json!({
            "reasoning_content": "abcd",
            "tool_calls": [
                {"provider_specific_fields": {"k": "v"}},
                {"provider_specific_fields": null},
            ]
        });
        // {"k": "v"} is 10 chars -> 10/4=2 (integer division); "abcd" is 4 -> 1.
        assert_eq!(blob_tokens(&m), 3);
    }

    #[test]
    fn blob_tokens_zero_when_all_fields_falsy() {
        let m = json!({
            "reasoning_content": "",
            "thinking_blocks": null,
            "tool_calls": [{"provider_specific_fields": {}}]
        });
        assert_eq!(blob_tokens(&m), 0);
    }

    #[test]
    fn blob_tokens_handles_missing_or_malformed_tool_calls_gracefully() {
        assert_eq!(blob_tokens(&json!({})), 0);
        assert_eq!(blob_tokens(&json!({"tool_calls": "not-array"})), 0);
    }

    #[test]
    fn reasoning_text_prefers_reasoning_content() {
        let m = json!({"reasoning_content": "the actual reasoning"});
        assert_eq!(reasoning_text(&m), "the actual reasoning");
    }

    #[test]
    fn reasoning_text_falls_back_to_actions_then_default() {
        let m = json!({"extra": {"actions": [{"command": "ls"}, {"command": "cat f"}]}});
        assert_eq!(reasoning_text(&m), "ls ; cat f");

        let empty = json!({});
        assert_eq!(reasoning_text(&empty), "reasoning");
    }

    #[test]
    fn reasoning_text_truncates_to_2000_chars() {
        let long = "x".repeat(3000);
        let m = json!({"reasoning_content": long});
        assert_eq!(reasoning_text(&m).chars().count(), 2000);
    }

    #[test]
    fn reasoning_chunk_none_when_no_blob() {
        assert!(reasoning_chunk(&json!({}), 0).is_none());
    }

    #[test]
    fn reasoning_chunk_some_with_provider_eviction() {
        let m = json!({"reasoning_content": "abcd"});
        let c = reasoning_chunk(&m, 2).unwrap();
        assert_eq!(c.evict, "provider");
        assert_eq!(c.kind, "reasoning");
        assert_eq!(c.step, 2);
    }

    #[test]
    fn assistant_chunks_of_skips_non_text_content() {
        let msgs = vec![
            json!({"role": "assistant", "content": [{"type": "image", "url": "x"}]}),
            json!({"role": "user", "content": "reply"}),
            json!({"role": "assistant", "content": "hello world"}),
            json!({"role": "user", "content": "reply2"}),
        ];
        let out = assistant_chunks_of(&msgs);
        // First assistant message had non-text content -> skipped entirely.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "hello world");
    }

    #[test]
    fn assistant_chunks_of_missing_type_key_defaults_to_text() {
        let msgs = vec![
            json!({"role": "assistant", "content": [{"text": "no type key"}]}),
            json!({"role": "user", "content": "ok"}),
        ];
        let out = assistant_chunks_of(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "no type key");
    }

    #[test]
    fn assistant_chunks_of_empty_text_produces_no_chunks() {
        let msgs = vec![
            json!({"role": "assistant", "content": "   "}),
            json!({"role": "user", "content": "ok"}),
        ];
        assert_eq!(assistant_chunks_of(&msgs).len(), 0);
    }

    #[test]
    fn assistant_chunks_of_malformed_content_does_not_panic() {
        let msgs = vec![
            json!({"role": "assistant", "content": 12345}),
            json!({"role": "user", "content": "ok"}),
            json!({"role": "assistant"}), // missing content entirely
            json!({"role": "tool", "content": "ok2"}),
        ];
        // Must not panic; numeric/missing content simply yields no chunks.
        assert_eq!(assistant_chunks_of(&msgs).len(), 0);
    }

    #[test]
    fn reasoning_chunks_of_aligns_step_index_with_steps_of() {
        let msgs = vec![
            json!({"role": "assistant", "reasoning_content": "reasoning block one"}),
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant"}), // no reasoning blob -> no chunk, step still advances
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "reasoning_content": "reasoning block three"}),
            json!({"role": "user", "content": "u3"}),
        ];
        let chunks = reasoning_chunks_of(&msgs);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].step, 0);
        assert_eq!(chunks[1].step, 2);
    }
}
