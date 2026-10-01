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

    fn assistant(actions: &[&str]) -> Value {
        let acts: Vec<Value> = actions.iter().map(|c| json!({"command": c})).collect();
        json!({"role": "assistant", "content": "", "extra": {"actions": acts}})
    }

    fn step(cmd: &str, obs: &str) -> (String, String) {
        (cmd.to_string(), obs.to_string())
    }

    #[test]
    fn content_text_of_plain_string() {
        assert_eq!(content_text(&json!("hello\nworld")), "hello\nworld");
        assert_eq!(content_text(&json!("")), "");
    }

    #[test]
    fn content_text_joins_text_blocks_with_spaces() {
        let c = json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ]);
        assert_eq!(content_text(&c), "first second");
        assert_eq!(content_text(&json!([])), "");
    }

    #[test]
    fn content_text_blocks_without_text_still_add_a_separator() {
        // Records current behaviour: every object block joins, even empty ones.
        let c = json!([{"type": "image", "source": {}}, {"text": "a"}, {"type": "text"}]);
        assert_eq!(content_text(&c), " a ");
    }

    #[test]
    fn content_text_skips_non_object_parts_and_non_string_text() {
        let c = json!(["raw", 5, null, [{"text": "nested"}], {"text": 42}, {"text": "ok"}]);
        assert_eq!(content_text(&c), " ok");
    }

    #[test]
    fn content_text_of_other_json_types_is_empty() {
        for c in [
            json!(null),
            json!(true),
            json!(3.5),
            json!({"text": "not a list"}),
        ] {
            assert_eq!(content_text(&c), "", "{c}");
        }
    }

    #[test]
    fn content_text_does_not_look_inside_tool_blocks() {
        // Records current behaviour: only top-level "text" fields are read, so
        // tool_use input and tool_result content add nothing but separators.
        let c = json!([
            {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}},
            {"type": "tool_result", "tool_use_id": "t1", "content": "file.txt"},
            {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "x"}]}
        ]);
        assert_eq!(content_text(&c), "  ");
    }

    #[test]
    fn actions_prefers_command_then_query() {
        let m = json!({"extra": {"actions": [
            {"command": "ls", "query": "ignored"},
            {"command": "", "query": "search terms"},
            {"query": "only query"},
            {"command": 7, "query": "non-string command"},
            {},
            "not an object",
            {"command": null}
        ]}});
        assert_eq!(
            actions(&m),
            vec![
                "ls",
                "search terms",
                "only query",
                "non-string command",
                "",
                "",
                ""
            ]
        );
    }

    #[test]
    fn actions_missing_or_malformed_containers_give_empty() {
        for m in [
            json!({}),
            json!({"extra": null}),
            json!({"extra": {}}),
            json!({"extra": {"actions": {"command": "ls"}}}),
            json!({"extra": "actions"}),
            json!("assistant"),
            json!(null),
        ] {
            assert!(actions(&m).is_empty(), "{m}");
        }
    }

    #[test]
    fn steps_of_pairs_each_assistant_with_next_user_or_tool() {
        let msgs = [
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            assistant(&["ls", "pwd"]),
            json!({"role": "tool", "content": "a.py"}),
            assistant(&["cat a.py"]),
            json!({"role": "user", "content": [{"type": "text", "text": "print(1)"}]}),
        ];
        assert_eq!(
            steps_of(&msgs),
            vec![step("ls ; pwd", "a.py"), step("cat a.py", "print(1)")]
        );
    }

    #[test]
    fn steps_of_consecutive_assistants_keep_only_the_latest() {
        let msgs = [
            assistant(&["first"]),
            json!({"role": "system", "content": "between"}),
            assistant(&["second"]),
            json!({"role": "user", "content": "out"}),
            json!({"role": "user", "content": "no pending assistant"}),
        ];
        assert_eq!(steps_of(&msgs), vec![step("second", "out")]);
    }

    #[test]
    fn steps_of_drops_trailing_assistant_without_observation() {
        assert!(steps_of(&[assistant(&["ls"])]).is_empty());
        assert!(steps_of(&[]).is_empty());
    }

    #[test]
    fn steps_of_missing_or_null_content_is_empty_observation() {
        let msgs = [
            assistant(&[]),
            json!({"role": "tool"}),
            assistant(&["x"]),
            json!({"role": "user", "content": null}),
        ];
        assert_eq!(steps_of(&msgs), vec![step("", ""), step("x", "")]);
    }

    #[test]
    fn steps_of_ignores_malformed_messages_without_panicking() {
        let msgs = [
            json!(null),
            json!("assistant"),
            json!([1, 2]),
            json!({"role": 5}),
            json!({"role": "ASSISTANT"}),
            json!({"content": "no role"}),
            assistant(&["ok"]),
            json!({"role": "tool", "content": {"unexpected": "object"}}),
        ];
        assert_eq!(steps_of(&msgs), vec![step("ok", "")]);
    }

    #[test]
    fn anthropic_tool_use_and_tool_result_blocks_yield_empty_step() {
        // Records current behaviour: commands come from extra.actions, not
        // tool_use blocks, and a tool_result's payload sits under "content",
        // not "text", so Anthropic-format turns give an empty step.
        let msgs = [
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "a.py\nb.py"}
            ]}),
        ];
        assert_eq!(steps_of(&msgs), vec![step("", "")]);
        assert!(assistant_chunks_of(&msgs).is_empty());
    }

    #[test]
    fn truthy_follows_python_rules() {
        let falsy = [
            json!(null),
            json!(false),
            json!(0),
            json!(0.0),
            json!(-0.0),
            json!(""),
            json!([]),
            json!({}),
        ];
        for v in falsy {
            assert!(!truthy(&v), "{v} should be falsy");
        }
        let truthy_values = [
            json!(true),
            json!(1),
            json!(-1),
            json!(0.1),
            json!(" "),
            json!([null]),
            json!({"a": null}),
        ];
        for v in truthy_values {
            assert!(truthy(&v), "{v} should be truthy");
        }
    }

    #[test]
    fn blob_tokens_sums_provider_fields_and_reasoning_fields() {
        let m = json!({
            "tool_calls": [
                {"provider_specific_fields": {"sig": "abcd"}},
                {"provider_specific_fields": {}},
                {"provider_specific_fields": null},
                {"id": "no psf"}
            ],
            "reasoning_content": "x".repeat(40),
            "thinking_blocks": [{"t": "é"}],
            "provider_specific_fields": "abcdefgh"
        });
        // Objects are measured as their Python json.dumps form (ASCII-escaped).
        let want = char_len(r#"{"sig": "abcd"}"#) as i64 / 4
            + 40 / 4
            + char_len(r#"[{"t": "\u00e9"}]"#) as i64 / 4
            + 8 / 4;
        assert_eq!(blob_tokens(&m), want);
    }

    #[test]
    fn blob_tokens_counts_chars_not_bytes_and_floors() {
        assert_eq!(blob_tokens(&json!({"reasoning_content": "é".repeat(8)})), 2);
        assert_eq!(blob_tokens(&json!({"reasoning_content": "abc"})), 0);
        assert_eq!(blob_tokens(&json!({"reasoning_content": "abcdefg"})), 1);
    }

    #[test]
    fn blob_tokens_ignores_falsy_and_malformed_fields() {
        for m in [
            json!({}),
            json!({"reasoning_content": ""}),
            json!({"thinking_blocks": []}),
            json!({"provider_specific_fields": {}}),
            json!({"reasoning_content": 0}),
            json!({"tool_calls": {"provider_specific_fields": {"a": "bbbbbbbbbbbbbbbbbbbb"}}}),
            json!({"tool_calls": ["not an object", 3]}),
            json!(null),
        ] {
            assert_eq!(blob_tokens(&m), 0, "{m}");
        }
    }

    #[test]
    fn blob_tokens_serialises_non_string_reasoning_fields() {
        // true -> "true" (4 chars) -> 1 token.
        assert_eq!(blob_tokens(&json!({"reasoning_content": true})), 1);
    }

    #[test]
    fn reasoning_text_prefers_reasoning_content_then_actions() {
        let m = json!({"reasoning_content": "think", "extra": {"actions": [{"command": "ls"}]}});
        assert_eq!(reasoning_text(&m), "think");
        let m = json!({
            "reasoning_content": " \n\u{1f}",
            "extra": {"actions": [{"command": "ls"}, {"command": ""}, {"query": "q"}]}
        });
        assert_eq!(reasoning_text(&m), "ls ; q");
        let m = json!({"reasoning_content": ["not a string"], "extra": {"actions": [{"command": "pwd"}]}});
        assert_eq!(reasoning_text(&m), "pwd");
    }

    #[test]
    fn reasoning_text_defaults_to_placeholder() {
        assert_eq!(reasoning_text(&json!({})), "reasoning");
        assert_eq!(
            reasoning_text(&json!({"extra": {"actions": [{"command": ""}, {}]}})),
            "reasoning"
        );
    }

    #[test]
    fn reasoning_text_is_capped_at_2000_chars() {
        let m = json!({"reasoning_content": "日".repeat(2500)});
        assert_eq!(reasoning_text(&m), "日".repeat(2000));
        let long = "é".repeat(1500);
        let m = json!({"extra": {"actions": [{"command": long}, {"command": long}]}});
        assert_eq!(char_len(&reasoning_text(&m)), 2000);
    }

    #[test]
    fn reasoning_chunk_uses_blob_tokens_and_provider_eviction() {
        let m = json!({"reasoning_content": "x".repeat(400)});
        let c = reasoning_chunk(&m, 4).expect("has a reasoning blob");
        assert_eq!(c.kind, "reasoning");
        assert_eq!(c.evict, "provider");
        assert_eq!(c.step, 4);
        assert_eq!(c.tokens, 100);
        assert_eq!(c.text, "x".repeat(400));
    }

    #[test]
    fn reasoning_chunk_is_none_when_blob_rounds_to_zero_tokens() {
        assert!(reasoning_chunk(&json!({"reasoning_content": "abc"}), 0).is_none());
        assert!(reasoning_chunk(&json!({}), 0).is_none());
    }

    #[test]
    fn reasoning_chunks_of_step_index_matches_steps_of() {
        let with_blob = json!({"role": "assistant", "reasoning_content": "y".repeat(80)});
        let msgs = [
            json!({"role": "assistant", "content": "no blob"}),
            json!({"role": "user", "content": "o0"}),
            json!({"role": "user", "content": "unpaired"}),
            with_blob.clone(),
            json!({"role": "tool", "content": "o1"}),
            with_blob,
        ];
        let chunks = reasoning_chunks_of(&msgs);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].step, 1);
        assert_eq!(steps_of(&msgs).len(), 2);
    }

    #[test]
    fn assistant_chunks_of_accepts_strings_and_text_only_blocks() {
        let msgs = [
            json!({"role": "assistant", "content": "plain"}),
            json!({"role": "user", "content": "o"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "a"}, {"text": "b"}]}),
            json!({"role": "tool", "content": "o"}),
        ];
        let got: Vec<_> = assistant_chunks_of(&msgs)
            .into_iter()
            .map(|c| (c.step, c.text, c.kind))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, "plain".to_string(), "asst".to_string()),
                (1, "a b".to_string(), "asst".to_string())
            ]
        );
    }

    #[test]
    fn assistant_chunks_of_skips_non_text_content_but_still_advances_step() {
        let msgs = [
            json!({"role": "assistant", "content": [{"type": "text", "text": "x"}, {"type": "tool_use"}]}),
            json!({"role": "user", "content": "o"}),
            json!({"role": "assistant", "content": [{"type": 1, "text": "x"}]}),
            json!({"role": "user", "content": "o"}),
            json!({"role": "assistant", "content": ["x"]}),
            json!({"role": "user", "content": "o"}),
            json!({"role": "assistant"}),
            json!({"role": "user", "content": "o"}),
            json!({"role": "assistant", "content": {"text": "x"}}),
            json!({"role": "user", "content": "o"}),
            json!({"role": "assistant", "content": "kept"}),
            json!({"role": "user", "content": "o"}),
        ];
        let chunks = assistant_chunks_of(&msgs);
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].step, chunks[0].text.as_str()), (5, "kept"));
    }

    #[test]
    fn assistant_chunks_of_blank_text_and_empty_block_list_give_nothing() {
        let msgs = [
            json!({"role": "assistant", "content": " \n\t"}),
            json!({"role": "user", "content": "o"}),
            json!({"role": "assistant", "content": []}),
            json!({"role": "user", "content": "o"}),
        ];
        assert!(assistant_chunks_of(&msgs).is_empty());
    }
}
