//! Edge-case tests for `messages` — conversation-turn parsing and the
//! reasoning/assistant-text chunkers. Everything tested here is public;
//! `content_text` and `truthy` are private/`pub(crate)` and are exercised
//! indirectly through the public functions that call them.

use parsec_engine::messages::{
    actions, assistant_chunks_of, blob_tokens, reasoning_chunk, reasoning_chunks_of,
    reasoning_text, steps_of,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// actions() — malformed/missing extra.actions
// ---------------------------------------------------------------------------

#[test]
fn test_actions_prefers_command_over_query() {
    let m = json!({"extra": {"actions": [{"command": "cat file.py", "query": "ignored"}]}});
    assert_eq!(actions(&m), vec!["cat file.py".to_string()]);
}

#[test]
fn test_actions_falls_back_to_query_when_command_is_empty_or_missing() {
    let m = json!({"extra": {"actions": [
        {"command": "", "query": "grep foo"},
        {"query": "grep bar"}
    ]}});
    assert_eq!(
        actions(&m),
        vec!["grep foo".to_string(), "grep bar".to_string()]
    );
}

#[test]
fn test_actions_missing_command_and_query_yields_empty_string_entry() {
    let m = json!({"extra": {"actions": [{}]}});
    assert_eq!(actions(&m), vec!["".to_string()]);
}

#[test]
fn test_actions_missing_extra_or_actions_returns_empty_vec() {
    assert_eq!(actions(&json!({})), Vec::<String>::new());
    assert_eq!(actions(&json!({"extra": {}})), Vec::<String>::new());
    assert_eq!(
        actions(&json!({"extra": {"actions": "not an array"}})),
        Vec::<String>::new()
    );
}

/// A non-object element inside the actions array: `.get("command")` on a
/// non-object Value returns None rather than panicking, so it degrades to
/// an empty-string entry instead of crashing.
#[test]
fn test_actions_non_object_element_does_not_panic() {
    let m = json!({"extra": {"actions": [42, "raw string", null]}});
    assert_eq!(
        actions(&m),
        vec!["".to_string(), "".to_string(), "".to_string()]
    );
}

// ---------------------------------------------------------------------------
// steps_of() — text extraction (exercises content_text indirectly) and
// assistant/user pairing
// ---------------------------------------------------------------------------

#[test]
fn test_steps_of_pairs_assistant_action_with_following_observation() {
    let msgs = vec![
        json!({"role": "assistant", "extra": {"actions": [{"command": "cat a.py"}]}}),
        json!({"role": "tool", "content": "file contents here"}),
    ];
    assert_eq!(
        steps_of(&msgs),
        vec![("cat a.py".to_string(), "file contents here".to_string())]
    );
}

/// Content as an array of text blocks is joined with spaces.
#[test]
fn test_steps_of_extracts_text_from_array_content_blocks() {
    let msgs = vec![
        json!({"role": "assistant", "extra": {"actions": [{"command": "x"}]}}),
        json!({"role": "user", "content": [{"text": "part one"}, {"text": "part two"}]}),
    ];
    assert_eq!(steps_of(&msgs)[0].1, "part one part two");
}

/// A non-object element inside the content array is silently dropped by
/// `filter_map` (not turned into an empty-string placeholder) — different
/// from a present-but-textless object, which DOES contribute an empty slot
/// to the join. See the next test for that contrast.
#[test]
fn test_steps_of_content_array_skips_non_object_elements_without_a_gap() {
    let msgs = vec![
        json!({"role": "assistant", "extra": {"actions": [{"command": "x"}]}}),
        json!({"role": "user", "content": ["a raw string", {"text": "kept"}, 42]}),
    ];
    // Only one real text block -> no leading/trailing space from the
    // dropped non-object entries.
    assert_eq!(steps_of(&msgs)[0].1, "kept");
}

#[test]
fn test_steps_of_content_object_missing_text_key_contributes_empty_slot() {
    let msgs = vec![
        json!({"role": "assistant", "extra": {"actions": [{"command": "x"}]}}),
        json!({"role": "user", "content": [{"no_text_here": true}, {"text": "kept"}]}),
    ];
    // The textless object still occupies a slot in the join, unlike a
    // non-object element, which is dropped entirely (previous test).
    assert_eq!(steps_of(&msgs)[0].1, " kept");
}

/// Content missing, or of an unexpected scalar type (number, null, bool),
/// degrades to an empty string rather than panicking.
#[test]
fn test_steps_of_missing_or_scalar_content_yields_empty_text() {
    let msgs = vec![
        json!({"role": "assistant", "extra": {"actions": [{"command": "a"}]}}),
        json!({"role": "user"}), // no "content" key at all
        json!({"role": "assistant", "extra": {"actions": [{"command": "b"}]}}),
        json!({"role": "user", "content": 42}),
        json!({"role": "assistant", "extra": {"actions": [{"command": "c"}]}}),
        json!({"role": "user", "content": null}),
    ];
    let steps = steps_of(&msgs);
    assert_eq!(
        steps,
        vec![
            ("a".to_string(), "".to_string()),
            ("b".to_string(), "".to_string()),
            ("c".to_string(), "".to_string()),
        ]
    );
}

/// A lone user/tool message with no preceding assistant action is simply
/// skipped (no crash on `pending.take()` returning `None`).
#[test]
fn test_steps_of_user_message_with_no_pending_assistant_is_dropped() {
    let msgs = vec![json!({"role": "user", "content": "orphan"})];
    assert_eq!(steps_of(&msgs), Vec::new());
}

/// Two assistant messages in a row before any user/tool: the second
/// overwrites the pending action of the first, which is silently lost.
#[test]
fn test_steps_of_consecutive_assistant_messages_last_one_wins() {
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

/// A trailing assistant message with nothing after it never gets consumed.
#[test]
fn test_steps_of_trailing_assistant_with_no_response_is_dropped() {
    let msgs = vec![
        json!({"role": "user", "content": "first obs"}),
        json!({"role": "assistant", "extra": {"actions": [{"command": "x"}]}}),
        json!({"role": "assistant", "extra": {"actions": [{"command": "y"}]}}),
    ];
    assert_eq!(steps_of(&msgs), Vec::new());
}

#[test]
fn test_steps_of_empty_messages_list() {
    assert_eq!(steps_of(&[]), Vec::new());
}

#[test]
fn test_steps_of_ignores_unknown_roles() {
    let msgs = vec![
        json!({"role": "assistant", "extra": {"actions": [{"command": "x"}]}}),
        json!({"role": "system", "content": "should be ignored"}),
        json!({"role": "user", "content": "real obs"}),
    ];
    assert_eq!(
        steps_of(&msgs),
        vec![("x".to_string(), "real obs".to_string())]
    );
}

// ---------------------------------------------------------------------------
// blob_tokens() — truthiness branches and malformed shapes (exercises
// truthy() indirectly)
// ---------------------------------------------------------------------------

#[test]
fn test_blob_tokens_empty_message_is_zero() {
    assert_eq!(blob_tokens(&json!({})), 0);
}

/// tool_calls present but not an array degrades to zero contribution
/// instead of panicking on `.as_array()`.
#[test]
fn test_blob_tokens_tool_calls_wrong_type_does_not_panic() {
    assert_eq!(blob_tokens(&json!({"tool_calls": "not an array"})), 0);
    assert_eq!(blob_tokens(&json!({"tool_calls": {}})), 0);
}

#[test]
fn test_blob_tokens_falsy_provider_specific_fields_are_skipped() {
    // Empty object, null, and empty string are all falsy -> no contribution.
    for psf in [json!({}), json!(null), json!("")] {
        let m = json!({"tool_calls": [{"provider_specific_fields": psf}]});
        assert_eq!(blob_tokens(&m), 0);
    }
}

#[test]
fn test_blob_tokens_truthy_provider_specific_fields_counts_json_dumped_length() {
    let m = json!({"tool_calls": [{"provider_specific_fields": {"a": "bbbb"}}]});
    // py_json_dumps({"a": "bbbb"}) == {"a": "bbbb"} -> 14 chars -> 14/4 = 3
    assert_eq!(blob_tokens(&m), 3);
}

/// A string reasoning field is used directly (not JSON-quoted), while a
/// non-string truthy field is JSON-dumped first — the quotes and escaping
/// from dumping a string would otherwise inflate the length.
#[test]
fn test_blob_tokens_reasoning_content_string_is_not_json_quoted() {
    // "abcd" as a raw string -> 4 chars -> 4/4 = 1.
    // If it were json-dumped, it'd be `"abcd"` (6 chars) -> 6/4 = 1 too by
    // coincidence at this length, so use a length where it matters:
    let m = json!({"reasoning_content": "abcdefgh"}); // 8 chars -> 8/4 = 2
    assert_eq!(blob_tokens(&m), 2);
}

#[test]
fn test_blob_tokens_non_string_reasoning_field_is_json_dumped() {
    // thinking_blocks as an array -> py_json_dumps gives "[1, 2, 3]" (9 chars) -> 9/4 = 2.
    let m = json!({"thinking_blocks": [1, 2, 3]});
    assert_eq!(blob_tokens(&m), 2);
}

/// Zero (falsy number) and empty string are excluded; a nonzero number is
/// truthy but can still round down to zero tokens via integer division.
#[test]
fn test_blob_tokens_falsy_vs_truthy_numeric_reasoning_content() {
    assert_eq!(
        blob_tokens(&json!({"reasoning_content": 0})),
        0,
        "0 is falsy"
    );
    // 5 -> py_json_dumps(5) == "5" -> 1 char -> 1/4 == 0 by integer division,
    // even though 5 is truthy and DOES get processed.
    assert_eq!(blob_tokens(&json!({"reasoning_content": 5})), 0);
}

#[test]
fn test_blob_tokens_sums_across_tool_calls_and_reason_fields() {
    let m = json!({
        "tool_calls": [
            {"provider_specific_fields": {"x": "yyyy"}}, // "{"x": "yyyy"}" = 13 chars -> 3
        ],
        "reasoning_content": "12345678", // 8 chars -> 2
    });
    assert_eq!(blob_tokens(&m), 5);
}

// ---------------------------------------------------------------------------
// reasoning_text()
// ---------------------------------------------------------------------------

#[test]
fn test_reasoning_text_prefers_reasoning_content_when_present() {
    let m = json!({"reasoning_content": "thinking about it", "extra": {"actions": [{"command": "ignored"}]}});
    assert_eq!(reasoning_text(&m), "thinking about it");
}

/// Whitespace-only reasoning_content is treated as absent.
#[test]
fn test_reasoning_text_whitespace_only_reasoning_content_falls_back_to_actions() {
    let m = json!({"reasoning_content": "   ", "extra": {"actions": [{"command": "do a thing"}]}});
    assert_eq!(reasoning_text(&m), "do a thing");
}

#[test]
fn test_reasoning_text_joins_multiple_actions_with_semicolons() {
    let m = json!({"extra": {"actions": [{"command": "a"}, {"command": "b"}]}});
    assert_eq!(reasoning_text(&m), "a ; b");
}

/// No reasoning_content and no non-empty actions -> the "reasoning" sentinel.
#[test]
fn test_reasoning_text_defaults_to_sentinel_when_nothing_present() {
    assert_eq!(reasoning_text(&json!({})), "reasoning");
    assert_eq!(
        reasoning_text(&json!({"extra": {"actions": [{}]}})),
        "reasoning"
    );
}

#[test]
fn test_reasoning_text_truncates_to_2000_chars() {
    let long = "x".repeat(3000);
    let m = json!({"reasoning_content": long});
    assert_eq!(reasoning_text(&m).chars().count(), 2000);
}

/// Non-string reasoning_content (unexpected type) is ignored gracefully,
/// not treated as an error — falls through to the actions-based text.
#[test]
fn test_reasoning_text_non_string_reasoning_content_falls_back() {
    let m = json!({"reasoning_content": ["not", "a", "string"], "extra": {"actions": [{"command": "fallback"}]}});
    assert_eq!(reasoning_text(&m), "fallback");
}

// ---------------------------------------------------------------------------
// reasoning_chunk() / reasoning_chunks_of()
// ---------------------------------------------------------------------------

#[test]
fn test_reasoning_chunk_none_when_no_blob_tokens() {
    assert!(reasoning_chunk(&json!({}), 0).is_none());
}

#[test]
fn test_reasoning_chunk_some_carries_tokens_and_provider_eviction() {
    let m = json!({"reasoning_content": "12345678"}); // 2 tokens
    let c = reasoning_chunk(&m, 3).unwrap();
    assert_eq!(c.tokens, 2);
    assert_eq!(c.kind, "reasoning");
    assert_eq!(c.evict, "provider");
    assert_eq!(c.step, 3);
    assert_eq!(c.text, "12345678");
}

#[test]
fn test_reasoning_chunks_of_aligns_with_steps_of_indexing() {
    let msgs = vec![
        json!({"role": "assistant", "reasoning_content": "12345678"}),
        json!({"role": "user", "content": "obs1"}),
        json!({"role": "assistant"}), // no reasoning -> no chunk, but step still advances
        json!({"role": "user", "content": "obs2"}),
        json!({"role": "assistant", "reasoning_content": "abcdefgh"}),
        json!({"role": "user", "content": "obs3"}),
    ];
    let chunks = reasoning_chunks_of(&msgs);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].step, 0);
    assert_eq!(
        chunks[1].step, 2,
        "the reasoning-less middle step still consumed a step index"
    );
}

// ---------------------------------------------------------------------------
// assistant_chunks_of()
// ---------------------------------------------------------------------------

#[test]
fn test_assistant_chunks_of_string_content() {
    let msgs = vec![
        json!({"role": "assistant", "content": "hello there"}),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = assistant_chunks_of(&msgs);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "hello there");
    assert_eq!(chunks[0].kind, "asst");
}

/// An array of blocks with no "type" key at all defaults to text (matching
/// Python's `dict.get("type", "text")`), per the source comment.
#[test]
fn test_assistant_chunks_of_array_content_with_missing_type_defaults_to_text() {
    let msgs = vec![
        json!({"role": "assistant", "content": [{"text": "no type field"}]}),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = assistant_chunks_of(&msgs);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "no type field");
}

/// A block explicitly typed as something other than "text" (e.g. a
/// tool_use block) rejects the WHOLE message, not just that one block.
#[test]
fn test_assistant_chunks_of_non_text_block_type_rejects_whole_message() {
    let msgs = vec![
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "kept"},
            {"type": "tool_use", "text": "ignored anyway"}
        ]}),
        json!({"role": "user", "content": "obs"}),
    ];
    assert_eq!(assistant_chunks_of(&msgs), Vec::new());
}

/// A "type" field present but not a string (unexpected shape) also rejects
/// the message rather than panicking on the type check.
#[test]
fn test_assistant_chunks_of_non_string_type_field_rejects_message_without_panicking() {
    let msgs = vec![
        json!({"role": "assistant", "content": [{"type": 5, "text": "x"}]}),
        json!({"role": "user", "content": "obs"}),
    ];
    assert_eq!(assistant_chunks_of(&msgs), Vec::new());
}

/// A non-object element inside the content array is stricter here than in
/// `steps_of`/`content_text`: the gate rejects the whole message outright
/// rather than silently skipping that one element.
#[test]
fn test_assistant_chunks_of_non_object_array_element_rejects_whole_message() {
    let msgs = vec![
        json!({"role": "assistant", "content": ["a raw string", {"type": "text", "text": "b"}]}),
        json!({"role": "user", "content": "obs"}),
    ];
    assert_eq!(assistant_chunks_of(&msgs), Vec::new());
}

/// Missing content, or content of an unexpected scalar type, is rejected
/// gracefully (no chunks, no panic) but the step index still advances.
#[test]
fn test_assistant_chunks_of_missing_or_scalar_content_is_skipped_gracefully() {
    let msgs = vec![
        json!({"role": "assistant"}), // no "content" key
        json!({"role": "user", "content": "obs1"}),
        json!({"role": "assistant", "content": 42}),
        json!({"role": "user", "content": "obs2"}),
        json!({"role": "assistant", "content": "real text"}),
        json!({"role": "user", "content": "obs3"}),
    ];
    let chunks = assistant_chunks_of(&msgs);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].step, 2,
        "the two skipped steps still advanced the counter"
    );
}

/// Whitespace-only assistant text produces no chunks (has_content gate),
/// but doesn't crash or desync the step counter.
#[test]
fn test_assistant_chunks_of_whitespace_only_text_is_skipped() {
    let msgs = vec![
        json!({"role": "assistant", "content": "   \n  "}),
        json!({"role": "user", "content": "obs"}),
    ];
    assert_eq!(assistant_chunks_of(&msgs), Vec::new());
}

#[test]
fn test_assistant_chunks_of_empty_messages_list() {
    assert_eq!(assistant_chunks_of(&[]), Vec::new());
}
