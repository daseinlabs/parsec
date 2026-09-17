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

    // -----------------------------------------------------------------------
    // helpers
    // -----------------------------------------------------------------------

    fn msg(role: &str, content: Value) -> Value {
        json!({"role": role, "content": content})
    }

    fn asst_msg(content: Value) -> Value {
        msg("assistant", content)
    }

    fn user_msg(content: Value) -> Value {
        msg("user", content)
    }

    // -----------------------------------------------------------------------
    // content_text (tested indirectly via steps_of / assistant_chunks_of)
    // -----------------------------------------------------------------------

    /// Plain string content block: steps_of extracts it as the observation text.
    #[test]
    fn test_content_text_plain_string() {
        let messages = vec![
            json!({"role": "assistant", "content": "do stuff", "extra": {"actions": [{"command": "ls"}]}}),
            user_msg(json!("hello from tool")),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].1, "hello from tool");
    }

    /// Array of content blocks with "text" fields: content_text joins them with " ".
    #[test]
    fn test_content_text_array_of_blocks() {
        let content = json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ]);
        let messages = vec![
            json!({"role": "assistant", "content": "x", "extra": {"actions": [{"command": "ls"}]}}),
            user_msg(content),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].1, "first second");
    }

    /// Content block objects without a "text" key contribute an empty string.
    #[test]
    fn test_content_text_array_missing_text_key_contributes_empty() {
        let content = json!([
            {"type": "text", "text": "has text"},
            {"type": "image_url"}  // no "text" key
        ]);
        let messages = vec![
            json!({"role": "assistant", "content": "x", "extra": {"actions": [{"command": "ls"}]}}),
            user_msg(content),
        ];
        let steps = steps_of(&messages);
        // "has text" + " " + "" = "has text "
        assert_eq!(steps[0].1, "has text ");
    }

    /// A Null content value produces an empty observation string, not a panic.
    #[test]
    fn test_content_text_null_content_is_empty_string() {
        let messages = vec![
            json!({"role": "assistant", "content": "x", "extra": {"actions": [{"command": "ls"}]}}),
            json!({"role": "user", "content": null}),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps[0].1, "");
    }

    /// A number as content (wrong type) produces an empty string, not a panic.
    #[test]
    fn test_content_text_wrong_type_number_is_empty() {
        let messages = vec![
            json!({"role": "assistant", "content": "x", "extra": {"actions": [{"command": "ls"}]}}),
            user_msg(json!(42)),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps[0].1, "");
    }

    // -----------------------------------------------------------------------
    // tool_use blocks in assistant_chunks_of
    // -----------------------------------------------------------------------

    /// An assistant message whose content is an array containing a tool_use
    /// block should be silently skipped by assistant_chunks_of (type != "text").
    #[test]
    fn test_assistant_chunks_of_skips_tool_use_content() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "1", "name": "bash", "input": {}}]
            }),
            user_msg(json!("obs")),
        ];
        let chunks = assistant_chunks_of(&messages);
        // tool_use type → the entire message's content fails the 'ok' check → no chunks
        assert!(
            chunks.is_empty(),
            "tool_use content must produce no assistant chunks, got {chunks:?}"
        );
    }

    /// A mixed array with a tool_use block causes the whole message to be skipped.
    #[test]
    fn test_assistant_chunks_of_mixed_text_and_tool_use_skipped() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "tool_use", "id": "1", "name": "bash", "input": {}}
                ]
            }),
            user_msg(json!("obs")),
        ];
        let chunks = assistant_chunks_of(&messages);
        assert!(
            chunks.is_empty(),
            "mixed text+tool_use must produce no assistant chunks"
        );
    }

    // -----------------------------------------------------------------------
    // tool_result in user messages — steps_of and assistant_chunks_of
    // -----------------------------------------------------------------------

    /// steps_of correctly pairs an assistant command with a tool-role observation.
    #[test]
    fn test_steps_of_tool_role_observation() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "x",
                "extra": {"actions": [{"command": "bash run.sh"}]}
            }),
            json!({"role": "tool", "content": "output from tool"}),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].0, "bash run.sh");
        assert_eq!(steps[0].1, "output from tool");
    }

    /// A tool_result block whose content is an array of text blocks.
    #[test]
    fn test_steps_of_tool_result_content_array() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "x",
                "extra": {"actions": [{"command": "ls"}]}
            }),
            user_msg(json!([
                {"type": "tool_result", "tool_use_id": "1",
                 "content": [{"type": "text", "text": "file1.rs"}]},
                {"type": "text", "text": "extra note"}
            ])),
        ];
        let steps = steps_of(&messages);
        // content_text on an array extracts the "text" field from each object;
        // {"type": "tool_result", ...} has no "text" key → ""
        // {"type": "text", "text": "extra note"} → "extra note"
        assert_eq!(steps[0].1, " extra note");
    }

    // -----------------------------------------------------------------------
    // steps_of — edge cases
    // -----------------------------------------------------------------------

    /// Empty message list → empty steps.
    #[test]
    fn test_steps_of_empty_messages() {
        assert!(steps_of(&[]).is_empty());
    }

    /// Only assistant messages, no following user/tool → no steps.
    #[test]
    fn test_steps_of_only_assistant_no_user() {
        let messages = vec![json!({"role": "assistant", "content": "x"})];
        assert!(steps_of(&messages).is_empty());
    }

    /// Only user messages → no steps (no preceding assistant).
    #[test]
    fn test_steps_of_only_user_messages() {
        let messages = vec![user_msg(json!("hello"))];
        assert!(steps_of(&messages).is_empty());
    }

    /// Multiple assistant-user pairs → correct step count.
    #[test]
    fn test_steps_of_multiple_turns() {
        let messages = vec![
            json!({"role": "assistant", "content": "x", "extra": {"actions": [{"command": "cmd1"}]}}),
            user_msg(json!("obs1")),
            json!({"role": "assistant", "content": "x", "extra": {"actions": [{"command": "cmd2"}]}}),
            user_msg(json!("obs2")),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].0, "cmd1");
        assert_eq!(steps[0].1, "obs1");
        assert_eq!(steps[1].0, "cmd2");
        assert_eq!(steps[1].1, "obs2");
    }

    /// Unknown role messages are ignored without panic.
    #[test]
    fn test_steps_of_unknown_role_ignored() {
        let messages = vec![
            json!({"role": "system", "content": "you are a helpful assistant"}),
            json!({"role": "assistant", "content": "x", "extra": {"actions": [{"command": "ls"}]}}),
            user_msg(json!("obs")),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps.len(), 1, "system role must be silently ignored");
    }

    /// Missing "role" field is treated as unknown → ignored.
    #[test]
    fn test_steps_of_missing_role_field_ignored() {
        let messages = vec![
            json!({"content": "no role here"}),
            json!({"role": "assistant", "content": "x", "extra": {"actions": [{"command": "ls"}]}}),
            user_msg(json!("obs")),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps.len(), 1);
    }

    /// "role" is a number (wrong type) → not a string, treated as unknown.
    #[test]
    fn test_steps_of_numeric_role_ignored() {
        let messages = vec![
            json!({"role": 42, "content": "weird"}),
            json!({"role": "assistant", "content": "x", "extra": {"actions": [{"command": "ls"}]}}),
            user_msg(json!("obs")),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps.len(), 1);
    }

    // -----------------------------------------------------------------------
    // assistant_chunks_of — edge cases
    // -----------------------------------------------------------------------

    /// Empty messages → empty chunks.
    #[test]
    fn test_assistant_chunks_of_empty_messages() {
        assert!(assistant_chunks_of(&[]).is_empty());
    }

    /// Whitespace-only content → py_has_content is false → no chunks.
    #[test]
    fn test_assistant_chunks_of_whitespace_only_content() {
        let messages = vec![asst_msg(json!("   \n\t\n   ")), user_msg(json!("obs"))];
        let chunks = assistant_chunks_of(&messages);
        assert!(
            chunks.is_empty(),
            "whitespace-only content must produce no assistant chunks"
        );
    }

    /// A plain text assistant message produces chunks with kind "asst".
    #[test]
    fn test_assistant_chunks_of_plain_text_produces_asst_chunks() {
        let messages = vec![
            asst_msg(json!("I am the assistant.\nThis is my reasoning.")),
            user_msg(json!("obs")),
        ];
        let chunks = assistant_chunks_of(&messages);
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert_eq!(c.kind, "asst");
        }
    }

    // -----------------------------------------------------------------------
    // truthy
    // -----------------------------------------------------------------------

    #[test]
    fn test_truthy_semantics() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(false)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!([])));
        assert!(!truthy(&json!({})));

        assert!(truthy(&json!(true)));
        assert!(truthy(&json!(1)));
        assert!(truthy(&json!("x")));
        assert!(truthy(&json!(["a"])));
        assert!(truthy(&json!({"a": 1})));
    }

    // -----------------------------------------------------------------------
    // actions
    // -----------------------------------------------------------------------

    #[test]
    fn test_actions_extracts_command_strings() {
        let m = json!({
            "role": "assistant",
            "extra": {"actions": [
                {"command": "ls -la"},
                {"command": "cat foo.rs"}
            ]}
        });
        assert_eq!(actions(&m), vec!["ls -la", "cat foo.rs"]);
    }

    #[test]
    fn test_actions_falls_back_to_query_when_command_empty() {
        let m = json!({
            "role": "assistant",
            "extra": {"actions": [
                {"command": "", "query": "search for something"}
            ]}
        });
        assert_eq!(actions(&m), vec!["search for something"]);
    }

    #[test]
    fn test_actions_missing_extra_returns_empty() {
        let m = json!({"role": "assistant", "content": "hi"});
        assert!(actions(&m).is_empty());
    }

    #[test]
    fn test_actions_extra_actions_not_array_returns_empty() {
        let m = json!({"role": "assistant", "extra": {"actions": "not an array"}});
        assert!(actions(&m).is_empty());
    }

    /// Extra unknown fields in the message object are silently ignored.
    #[test]
    fn test_steps_of_extra_unknown_fields_ignored() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "x",
                "extra": {"actions": [{"command": "ls"}]},
                "unknown_future_field": {"nested": true},
                "another_unknown": [1, 2, 3]
            }),
            user_msg(json!("obs")),
        ];
        let steps = steps_of(&messages);
        assert_eq!(steps.len(), 1, "extra fields must be silently ignored");
    }
}
