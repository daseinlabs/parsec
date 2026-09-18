//! Comprehensive edge-case tests for `messages.rs`.
//!
//! Covers:
//!   - `steps_of`: assistant->user pairing, tool role, multi-turn, empty
//!   - `blob_tokens` / `truthy`: all JSON value kinds
//!   - `reasoning_text` / `reasoning_chunk` / `reasoning_chunks_of`
//!   - `assistant_chunks_of`: string, array-of-text, tool_use blocks skipped,
//!     no-content / whitespace, multi-step
//!   - Malformed / unexpected JSON structures must not panic

use parsec_engine::messages::{
    actions, assistant_chunks_of, blob_tokens, reasoning_chunk, reasoning_chunks_of,
    reasoning_text, steps_of,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn msg(role: &str, content: Value) -> Value {
    json!({ "role": role, "content": content })
}

fn asst(content: Value) -> Value {
    msg("assistant", content)
}

fn user(content: Value) -> Value {
    msg("user", content)
}

fn tool_msg(content: Value) -> Value {
    msg("tool", content)
}

// ---------------------------------------------------------------------------
// actions()
// ---------------------------------------------------------------------------

#[test]
fn test_actions_returns_command_from_extra_actions() {
    let m = json!({
        "role": "assistant",
        "content": "text",
        "extra": {
            "actions": [
                {"command": "grep foo bar.py"},
                {"command": "cat main.rs"}
            ]
        }
    });
    assert_eq!(actions(&m), vec!["grep foo bar.py", "cat main.rs"]);
}

#[test]
fn test_actions_returns_query_when_no_command() {
    let m = json!({
        "role": "assistant",
        "content": "text",
        "extra": {
            "actions": [{"query": "What is Rust?"}]
        }
    });
    assert_eq!(actions(&m), vec!["What is Rust?"]);
}

#[test]
fn test_actions_empty_command_falls_back_to_query() {
    let m = json!({
        "role": "assistant",
        "content": "text",
        "extra": {
            "actions": [{"command": "", "query": "fallback query"}]
        }
    });
    assert_eq!(actions(&m), vec!["fallback query"]);
}

#[test]
fn test_actions_no_extra_returns_empty() {
    let m = json!({"role": "assistant", "content": "text"});
    assert!(actions(&m).is_empty());
}

#[test]
fn test_actions_empty_actions_array() {
    let m = json!({
        "role": "assistant",
        "content": "text",
        "extra": {"actions": []}
    });
    assert!(actions(&m).is_empty());
}

#[test]
fn test_actions_malformed_extra_does_not_panic() {
    // extra.actions is a string, not an array -- must not panic
    let m = json!({
        "role": "assistant",
        "content": "text",
        "extra": {"actions": "not an array"}
    });
    assert!(actions(&m).is_empty());
}

// ---------------------------------------------------------------------------
// steps_of()
// ---------------------------------------------------------------------------

#[test]
fn test_steps_of_empty_messages() {
    assert!(steps_of(&[]).is_empty());
}

#[test]
fn test_steps_of_single_turn() {
    let messages = vec![
        json!({
            "role": "assistant",
            "content": "text",
            "extra": {"actions": [{"command": "cat file.py"}]}
        }),
        json!({"role": "user", "content": "file contents"}),
    ];
    let steps = steps_of(&messages);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].0, "cat file.py");
    assert_eq!(steps[0].1, "file contents");
}

#[test]
fn test_steps_of_multiple_turns() {
    let messages = vec![
        json!({
            "role": "assistant",
            "content": "text",
            "extra": {"actions": [{"command": "grep foo src/"}]}
        }),
        json!({"role": "user", "content": "grep result"}),
        json!({
            "role": "assistant",
            "content": "text",
            "extra": {"actions": [{"command": "cat main.rs"}]}
        }),
        json!({"role": "user", "content": "file content"}),
    ];
    let steps = steps_of(&messages);
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].0, "grep foo src/");
    assert_eq!(steps[0].1, "grep result");
    assert_eq!(steps[1].0, "cat main.rs");
    assert_eq!(steps[1].1, "file content");
}

#[test]
fn test_steps_of_tool_role_acts_like_user() {
    let messages = vec![
        json!({
            "role": "assistant",
            "content": "text",
            "extra": {"actions": [{"command": "ls"}]}
        }),
        json!({"role": "tool", "content": "file1\nfile2"}),
    ];
    let steps = steps_of(&messages);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].0, "ls");
    assert_eq!(steps[0].1, "file1\nfile2");
}

#[test]
fn test_steps_of_user_without_preceding_assistant_ignored() {
    let messages = vec![
        json!({"role": "user", "content": "hello"}),
        json!({"role": "user", "content": "world"}),
    ];
    assert!(steps_of(&messages).is_empty());
}

#[test]
fn test_steps_of_assistant_at_end_without_response_ignored() {
    let messages = vec![json!({
        "role": "assistant",
        "content": "text",
        "extra": {"actions": [{"command": "ls"}]}
    })];
    assert!(steps_of(&messages).is_empty());
}

#[test]
fn test_steps_of_content_array_of_text_blocks() {
    let messages = vec![
        json!({
            "role": "assistant",
            "content": "text",
            "extra": {"actions": [{"command": "ls"}]}
        }),
        json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "world"}
            ]
        }),
    ];
    let steps = steps_of(&messages);
    assert_eq!(steps.len(), 1);
    assert!(
        steps[0].1.contains("hello"),
        "content should contain 'hello'"
    );
    assert!(
        steps[0].1.contains("world"),
        "content should contain 'world'"
    );
}

#[test]
fn test_steps_of_content_null_becomes_empty_string() {
    let messages = vec![
        json!({
            "role": "assistant",
            "content": "text",
            "extra": {"actions": [{"command": "ls"}]}
        }),
        json!({"role": "user"}), // no content key
    ];
    let steps = steps_of(&messages);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].1, "");
}

#[test]
fn test_steps_of_unknown_role_ignored() {
    let messages = vec![
        json!({"role": "system", "content": "system prompt"}),
        json!({
            "role": "assistant",
            "content": "text",
            "extra": {"actions": [{"command": "ls"}]}
        }),
        json!({"role": "user", "content": "ok"}),
    ];
    let steps = steps_of(&messages);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].0, "ls");
}

// ---------------------------------------------------------------------------
// blob_tokens()
// ---------------------------------------------------------------------------

#[test]
fn test_blob_tokens_no_reasoning_fields_is_zero() {
    let m = json!({"role": "assistant", "content": "hello"});
    assert_eq!(blob_tokens(&m), 0);
}

#[test]
fn test_blob_tokens_reasoning_content_string() {
    let content = "a".repeat(400);
    let m = json!({"role": "assistant", "reasoning_content": content});
    assert_eq!(blob_tokens(&m), 100);
}

#[test]
fn test_blob_tokens_empty_string_fields_not_counted() {
    let m = json!({"role": "assistant", "reasoning_content": ""});
    assert_eq!(blob_tokens(&m), 0);
}

#[test]
fn test_blob_tokens_null_fields_not_counted() {
    let m = json!({"role": "assistant", "reasoning_content": null});
    assert_eq!(blob_tokens(&m), 0);
}

#[test]
fn test_blob_tokens_empty_array_field_not_counted() {
    let m = json!({"role": "assistant", "thinking_blocks": []});
    assert_eq!(blob_tokens(&m), 0);
}

#[test]
fn test_blob_tokens_non_empty_array_field_counted() {
    let val = json!({"text": "a".repeat(400)});
    let m = json!({"role": "assistant", "thinking_blocks": [val]});
    assert!(blob_tokens(&m) > 0);
}

#[test]
fn test_blob_tokens_does_not_panic_on_unexpected_types() {
    let m = json!({
        "role": "assistant",
        "reasoning_content": 42,
        "thinking_blocks": "not an array",
        "provider_specific_fields": true
    });
    let _ = blob_tokens(&m);
}

// ---------------------------------------------------------------------------
// reasoning_text()
// ---------------------------------------------------------------------------

#[test]
fn test_reasoning_text_uses_reasoning_content_field() {
    let m = json!({"role": "assistant", "reasoning_content": "My reasoning here"});
    assert_eq!(reasoning_text(&m), "My reasoning here");
}

#[test]
fn test_reasoning_text_truncates_to_2000_chars() {
    let long = "x".repeat(3000);
    let m = json!({"role": "assistant", "reasoning_content": long});
    let t = reasoning_text(&m);
    assert_eq!(t.chars().count(), 2000);
}

#[test]
fn test_reasoning_text_falls_back_to_actions_when_no_reasoning_content() {
    let m = json!({
        "role": "assistant",
        "content": "text",
        "extra": {"actions": [{"command": "ls -la"}]}
    });
    let t = reasoning_text(&m);
    assert!(t.contains("ls -la"), "should contain action command");
}

#[test]
fn test_reasoning_text_empty_actions_returns_reasoning_literal() {
    let m = json!({
        "role": "assistant",
        "content": "text",
        "extra": {"actions": [{"command": ""}]}
    });
    assert_eq!(reasoning_text(&m), "reasoning");
}

#[test]
fn test_reasoning_text_whitespace_reasoning_content_falls_back() {
    let m = json!({"role": "assistant", "reasoning_content": "   "});
    assert_eq!(reasoning_text(&m), "reasoning");
}

// ---------------------------------------------------------------------------
// reasoning_chunk()
// ---------------------------------------------------------------------------

#[test]
fn test_reasoning_chunk_none_when_no_blob() {
    let m = json!({"role": "assistant", "content": "text"});
    assert!(reasoning_chunk(&m, 0).is_none());
}

#[test]
fn test_reasoning_chunk_some_when_blob_present() {
    let content = "a".repeat(400);
    let m = json!({"role": "assistant", "reasoning_content": content});
    let c = reasoning_chunk(&m, 3).unwrap();
    assert_eq!(c.kind, "reasoning");
    assert_eq!(c.step, 3);
    assert_eq!(c.evict, "provider");
    assert!(c.tokens > 0);
}

// ---------------------------------------------------------------------------
// reasoning_chunks_of()
// ---------------------------------------------------------------------------

#[test]
fn test_reasoning_chunks_of_empty_messages() {
    assert!(reasoning_chunks_of(&[]).is_empty());
}

#[test]
fn test_reasoning_chunks_of_no_reasoning_payloads() {
    let messages = vec![
        json!({"role": "assistant", "content": "text", "extra": {"actions": []}}),
        json!({"role": "user", "content": "ok"}),
    ];
    assert!(reasoning_chunks_of(&messages).is_empty());
}

#[test]
fn test_reasoning_chunks_of_single_step_with_reasoning() {
    let content = "b".repeat(400);
    let messages = vec![
        json!({"role": "assistant", "reasoning_content": content}),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = reasoning_chunks_of(&messages);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, "reasoning");
    assert_eq!(chunks[0].step, 0);
}

#[test]
fn test_reasoning_chunks_of_step_index_increments() {
    let content = "c".repeat(400);
    let messages = vec![
        json!({"role": "assistant", "reasoning_content": content.clone()}),
        json!({"role": "user", "content": "ok"}),
        json!({"role": "assistant", "reasoning_content": content}),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = reasoning_chunks_of(&messages);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].step, 0);
    assert_eq!(chunks[1].step, 1);
}

// ---------------------------------------------------------------------------
// assistant_chunks_of()
// ---------------------------------------------------------------------------

#[test]
fn test_assistant_chunks_of_empty_messages() {
    assert!(assistant_chunks_of(&[]).is_empty());
}

#[test]
fn test_assistant_chunks_of_string_content() {
    let messages = vec![
        json!({"role": "assistant", "content": "Hello from the assistant"}),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = assistant_chunks_of(&messages);
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].kind, "asst");
    assert!(chunks[0].text.contains("Hello from the assistant"));
}

#[test]
fn test_assistant_chunks_of_array_of_text_blocks() {
    let messages = vec![
        json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Part one."},
                {"type": "text", "text": "Part two."}
            ]
        }),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = assistant_chunks_of(&messages);
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].kind, "asst");
}

#[test]
fn test_assistant_chunks_of_tool_use_blocks_are_skipped() {
    let messages = vec![
        json!({
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "t1", "name": "grep", "input": {}}
            ]
        }),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = assistant_chunks_of(&messages);
    assert!(
        chunks.is_empty(),
        "tool_use content should produce no asst chunks"
    );
}

#[test]
fn test_assistant_chunks_of_null_content_produces_no_chunks() {
    let messages = vec![
        json!({"role": "assistant", "content": null}),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = assistant_chunks_of(&messages);
    assert!(chunks.is_empty());
}

#[test]
fn test_assistant_chunks_of_whitespace_only_produces_no_chunks() {
    let messages = vec![
        json!({"role": "assistant", "content": "   \n\t  "}),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = assistant_chunks_of(&messages);
    assert!(
        chunks.is_empty(),
        "whitespace-only content should produce no asst chunks"
    );
}

#[test]
fn test_assistant_chunks_of_step_index_increments() {
    let messages = vec![
        json!({"role": "assistant", "content": "Step zero content"}),
        json!({"role": "user", "content": "ok"}),
        json!({"role": "assistant", "content": "Step one content"}),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = assistant_chunks_of(&messages);
    assert!(chunks.iter().any(|c| c.step == 0));
    assert!(chunks.iter().any(|c| c.step == 1));
}

#[test]
fn test_assistant_chunks_of_no_user_after_assistant_not_counted() {
    let messages = vec![json!({"role": "assistant", "content": "content"})];
    let chunks = assistant_chunks_of(&messages);
    assert!(chunks.is_empty());
}

#[test]
fn test_assistant_chunks_of_unicode_content() {
    let messages = vec![
        json!({"role": "assistant", "content": "Japanese: \u{65e5}\u{672c}\u{8a9e} with English"}),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = assistant_chunks_of(&messages);
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].kind, "asst");
}

#[test]
fn test_assistant_chunks_of_mixed_text_and_tool_use_skipped() {
    let messages = vec![
        json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "thinking..."},
                {"type": "tool_use", "id": "t1", "name": "ls", "input": {}}
            ]
        }),
        json!({"role": "user", "content": "ok"}),
    ];
    let chunks = assistant_chunks_of(&messages);
    assert!(
        chunks.is_empty(),
        "mixed text+tool_use should be skipped entirely"
    );
}

#[test]
fn test_assistant_chunks_of_does_not_panic_on_unexpected_json() {
    let messages = vec![
        json!({"role": "assistant", "content": 42}),
        json!({"role": "user", "content": "ok"}),
    ];
    let _ = assistant_chunks_of(&messages);
}
