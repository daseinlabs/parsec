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
    fn content_text_extraction() {
        // Simple string content
        let s = json!("Hello, world!");
        assert_eq!(content_text(&s), "Hello, world!");

        // Array of content blocks with text
        let arr = json!([
            {"type": "text", "text": "First part."},
            {"type": "text", "text": "Second part."}
        ]);
        assert_eq!(content_text(&arr), "First part. Second part.");

        // Content blocks with missing text field defaults to empty string
        let missing_text = json!([
            {"type": "text", "text": "Has text"},
            {"type": "text"}
        ]);
        assert_eq!(content_text(&missing_text), "Has text ");

        // Non-object array elements are ignored
        let mixed = json!([
            {"type": "text", "text": "Valid"},
            "raw string in array",
            123
        ]);
        assert_eq!(content_text(&mixed), "Valid");

        // Non-string, non-array values return empty string
        assert_eq!(content_text(&json!(null)), "");
        assert_eq!(content_text(&json!(42)), "");
        assert_eq!(content_text(&json!({"not": "array"})), "");
    }

    #[test]
    fn actions_extraction() {
        // Actions with command field
        let m = json!({
            "extra": {
                "actions": [
                    {"command": "git status"},
                    {"command": "cargo test"}
                ]
            }
        });
        assert_eq!(actions(&m), vec!["git status", "cargo test"]);

        // Actions falling back to query field when command is missing or empty
        let m_query = json!({
            "extra": {
                "actions": [
                    {"query": "rust documentation"},
                    {"command": "", "query": "fallback search"}
                ]
            }
        });
        assert_eq!(actions(&m_query), vec!["rust documentation", "fallback search"]);

        // Actions with empty or missing fields
        let m_empty = json!({
            "extra": {
                "actions": [
                    {},
                    {"other_key": "val"}
                ]
            }
        });
        assert_eq!(actions(&m_empty), vec!["", ""]);

        // Malformed or missing extra actions returns empty vec
        assert!(actions(&json!({})).is_empty());
        assert!(actions(&json!({"extra": null})).is_empty());
        assert!(actions(&json!({"extra": {"actions": "not an array"}})).is_empty());
    }

    #[test]
    fn steps_of_turn_pairing() {
        let messages = vec![
            json!({
                "role": "assistant",
                "extra": {"actions": [{"command": "echo 1"}, {"command": "echo 2"}]}
            }),
            json!({
                "role": "tool",
                "content": "output from echo"
            }),
            json!({
                "role": "assistant",
                "extra": {"actions": [{"command": "git diff"}]}
            }),
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "diff result"}]
            }),
        ];

        let steps = steps_of(&messages);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], ("echo 1 ; echo 2".to_string(), "output from echo".to_string()));
        assert_eq!(steps[1], ("git diff".to_string(), "diff result".to_string()));

        // Consecutive assistant turns: last assistant turn overwrites pending
        let overwritten = vec![
            json!({"role": "assistant", "extra": {"actions": [{"command": "first"}]}}),
            json!({"role": "assistant", "extra": {"actions": [{"command": "second"}]}}),
            json!({"role": "tool", "content": "result"}),
        ];
        let steps_overwritten = steps_of(&overwritten);
        assert_eq!(steps_overwritten.len(), 1);
        assert_eq!(steps_overwritten[0].0, "second");

        // Consecutive user/tool turns without assistant are ignored
        let orphan_obs = vec![
            json!({"role": "user", "content": "orphan 1"}),
            json!({"role": "tool", "content": "orphan 2"}),
        ];
        assert!(steps_of(&orphan_obs).is_empty());

        // System messages are ignored
        let system_msg = vec![
            json!({"role": "system", "content": "system prompt"}),
            json!({"role": "assistant", "extra": {"actions": [{"command": "ls"}]}}),
            json!({"role": "system", "content": "another system message"}),
            json!({"role": "tool", "content": "file.txt"}),
        ];
        let steps_sys = steps_of(&system_msg);
        assert_eq!(steps_sys.len(), 1);
        assert_eq!(steps_sys[0], ("ls".to_string(), "file.txt".to_string()));
    }

    #[test]
    fn truthy_python_semantics() {
        // Falsy in Python
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(false)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!(0.0)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!([])));
        assert!(!truthy(&json!({})));

        // Truthy in Python
        assert!(truthy(&json!(true)));
        assert!(truthy(&json!(1)));
        assert!(truthy(&json!(-1)));
        assert!(truthy(&json!(0.001)));
        assert!(truthy(&json!("non-empty")));
        assert!(truthy(&json!("0"))); // Non-empty string is truthy in Python!
        assert!(truthy(&json!([null])));
        assert!(truthy(&json!({"key": "val"})));
    }

    #[test]
    fn blob_tokens_and_reasoning_chunk() {
        // Message with reasoning_content string
        let m = json!({
            "reasoning_content": "Detailed step-by-step reasoning that should be counted."
        });
        let tok = blob_tokens(&m);
        assert!(tok > 0);
        let rc = reasoning_chunk(&m, 0);
        assert!(rc.is_some());
        let chunk = rc.unwrap();
        assert_eq!(chunk.kind, "reasoning");
        assert_eq!(chunk.evict, "provider");
        assert_eq!(chunk.tokens, tok);

        // Message with tool_calls provider_specific_fields
        let m_tool = json!({
            "tool_calls": [
                {
                    "id": "call_1",
                    "provider_specific_fields": {"metadata": "extra"}
                }
            ]
        });
        assert!(blob_tokens(&m_tool) > 0);

        // Message without reasoning or provider fields has 0 tokens and no chunk
        let m_empty = json!({"role": "assistant", "content": "Hello"});
        assert_eq!(blob_tokens(&m_empty), 0);
        assert!(reasoning_chunk(&m_empty, 0).is_none());
    }

    #[test]
    fn assistant_chunks_of_content_types() {
        // Plain string content produces assistant chunks
        let msgs_plain = vec![
            json!({"role": "assistant", "content": "Here is the plan."}),
            json!({"role": "user", "content": "Proceed."})
        ];
        let chunks = assistant_chunks_of(&msgs_plain);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "asst");
        assert_eq!(chunks[0].text, "Here is the plan.");

        // Array with text blocks produces chunks
        let msgs_blocks = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Step 1: Read files."},
                    {"type": "text", "text": "Step 2: Run tests."}
                ]
            }),
            json!({"role": "user", "content": "Done"})
        ];
        let chunks_blocks = assistant_chunks_of(&msgs_blocks);
        assert_eq!(chunks_blocks.len(), 1);
        assert_eq!(chunks_blocks[0].text, "Step 1: Read files. Step 2: Run tests.");

        // Non-text blocks (e.g. tool_use) are excluded
        let msgs_tool_use = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash"}
                ]
            }),
            json!({"role": "tool", "content": "result"})
        ];
        assert!(assistant_chunks_of(&msgs_tool_use).is_empty());
    }

    #[test]
    fn malformed_json_graceful_handling() {
        // Completely invalid message structures must not panic
        let malformed = vec![
            json!(null),
            json!(42),
            json!("a bare string"),
            json!([]),
            json!({"role": null, "content": 123}),
            json!({"role": "assistant", "content": null, "extra": "invalid"}),
            json!({"role": "user", "content": {"unexpected": "object"}}),
        ];

        let _ = steps_of(&malformed);
        let _ = reasoning_chunks_of(&malformed);
        let _ = assistant_chunks_of(&malformed);
    }
}
