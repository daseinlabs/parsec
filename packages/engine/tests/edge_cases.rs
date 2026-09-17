//! Comprehensive edge-case unit tests for pystr, chunking, and messages modules.
//!
//! These tests focus on correctness at the boundaries that matter for
//! byte-for-byte Python parity: all 11 line-break characters, Python's
//! whitespace superset, char-vs-byte slicing, JSON serialisation, grep-line
//! parsing, the full-coverage invariant, and message content extraction.

use parsec_engine::chunking::{
    chunk_assistant, chunk_observation, parse_grep_candidate, Chunk, ChunkMode, DEFAULT_WIN,
};
use parsec_engine::messages;
use parsec_engine::pystr::{
    char_len, char_prefix, py_has_content, py_is_space, py_json_dumps_opts, py_split_ws,
    py_splitlines, py_strip,
};
use serde_json::{json, Value};

// -----------------------------------------------------------------------------
// Helper: assert the full-coverage invariant for chunk_observation.
// Every character of `obs` must appear in exactly one chunk, in original order.
// -----------------------------------------------------------------------------
fn assert_full_coverage(obs: &str, chunks: &[Chunk]) {
    let reconstructed = chunks
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let expected = parsec_engine::pystr::py_splitlines(obs).join("\n");
    assert_eq!(
        reconstructed, expected,
        "full-coverage invariant violated for obs: {obs:?}"
    );
}

// =============================================================================
// Area 1 - pystr.rs
// =============================================================================

// -- py_splitlines --

#[test]
fn test_py_splitlines_empty_string() {
    assert_eq!(py_splitlines(""), Vec::<&str>::new());
}

#[test]
fn test_py_splitlines_no_linebreaks() {
    assert_eq!(py_splitlines("hello world"), vec!["hello world"]);
}

#[test]
fn test_py_splitlines_single_newline_only() {
    assert_eq!(py_splitlines("\n"), vec![""]);
}

#[test]
fn test_py_splitlines_lf() {
    assert_eq!(py_splitlines("a\nb"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_cr() {
    assert_eq!(py_splitlines("a\rb"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_crlf_consumed_as_one() {
    assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_crlf_does_not_create_empty_middle() {
    let result = py_splitlines("a\r\nb\r\nc");
    assert_eq!(result, vec!["a", "b", "c"]);
}

#[test]
fn test_py_splitlines_cr_not_followed_by_lf() {
    assert_eq!(py_splitlines("a\rb\nc"), vec!["a", "b", "c"]);
}

#[test]
fn test_py_splitlines_vertical_tab_0x0b() {
    assert_eq!(py_splitlines("a\x0bb"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_form_feed_0x0c() {
    assert_eq!(py_splitlines("a\x0cb"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_file_separator_0x1c() {
    assert_eq!(py_splitlines("a\x1cb"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_group_separator_0x1d() {
    assert_eq!(py_splitlines("a\x1db"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_record_separator_0x1e() {
    assert_eq!(py_splitlines("a\x1eb"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_next_line_u85() {
    assert_eq!(py_splitlines("a\u{85}b"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_line_separator_u2028() {
    assert_eq!(py_splitlines("a\u{2028}b"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_paragraph_separator_u2029() {
    assert_eq!(py_splitlines("a\u{2029}b"), vec!["a", "b"]);
}

#[test]
fn test_py_splitlines_all_ten_non_crlf_break_chars() {
    let breaks = [
        '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
    ];
    for brk in breaks {
        let s = format!("before{brk}after");
        let parts = py_splitlines(&s);
        assert_eq!(
            parts,
            vec!["before", "after"],
            "Failed for break char U+{:04X}",
            brk as u32
        );
    }
    // \r\n together counts as one
    assert_eq!(py_splitlines("before\r\nafter"), vec!["before", "after"]);
}

#[test]
fn test_py_splitlines_consecutive_newlines_produce_empty_lines() {
    assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
}

#[test]
fn test_py_splitlines_leading_newline() {
    assert_eq!(py_splitlines("\nabc"), vec!["", "abc"]);
}

#[test]
fn test_py_splitlines_trailing_newline_no_empty_element() {
    assert_eq!(py_splitlines("abc\n"), vec!["abc"]);
}

#[test]
fn test_py_splitlines_only_newlines() {
    assert_eq!(py_splitlines("\n\n"), vec!["", ""]);
}

#[test]
fn test_py_splitlines_mixed_breaks_content_preserved() {
    let s = "alpha\nbeta\r\ngamma\x0bdelta";
    let parts = py_splitlines(s);
    let reconstructed = parts.join("\n");
    assert_eq!(reconstructed, "alpha\nbeta\ngamma\ndelta");
}

#[test]
fn test_py_splitlines_unicode_content_between_breaks() {
    let s = "日本語\nEmoji: 🦀\nEnd";
    let parts = py_splitlines(s);
    assert_eq!(parts, vec!["日本語", "Emoji: 🦀", "End"]);
}

// -- py_strip and py_is_space --

#[test]
fn test_py_is_space_standard_unicode_whitespace() {
    for c in [' ', '\t', '\n', '\r', '\x0c', '\x0b'] {
        assert!(
            py_is_space(c),
            "expected py_is_space for U+{:04X}",
            c as u32
        );
    }
}

#[test]
fn test_py_is_space_c0_separators_0x1c_to_0x1f() {
    for c in ['\x1c', '\x1d', '\x1e', '\x1f'] {
        assert!(
            py_is_space(c),
            "expected py_is_space for U+{:04X}",
            c as u32
        );
    }
}

#[test]
fn test_py_is_space_not_space_for_printable_ascii() {
    for c in ['a', 'Z', '0', '_', '-', '.'] {
        assert!(
            !py_is_space(c),
            "U+{:04X} should NOT be a py space",
            c as u32
        );
    }
}

#[test]
fn test_py_strip_empty_string() {
    assert_eq!(py_strip(""), "");
}

#[test]
fn test_py_strip_no_whitespace() {
    assert_eq!(py_strip("hello"), "hello");
}

#[test]
fn test_py_strip_leading_and_trailing_spaces() {
    assert_eq!(py_strip("  hello  "), "hello");
}

#[test]
fn test_py_strip_only_whitespace_returns_empty() {
    assert_eq!(py_strip("   \t\n   "), "");
}

#[test]
fn test_py_strip_c0_separators() {
    assert_eq!(py_strip("\x1c\x1fhello\x1d\x1e"), "hello");
}

#[test]
fn test_py_strip_unicode_no_break_space() {
    assert_eq!(py_strip("\u{00A0}hello\u{00A0}"), "hello");
}

#[test]
fn test_py_has_content_non_empty() {
    assert!(py_has_content("hello"));
    assert!(py_has_content("  x  "));
}

#[test]
fn test_py_has_content_whitespace_only_is_false() {
    assert!(!py_has_content(""));
    assert!(!py_has_content("   "));
    assert!(!py_has_content("\t\n"));
    assert!(!py_has_content("\x1c\x1f"));
}

// -- char_prefix and char_len --

#[test]
fn test_char_prefix_ascii_same_as_bytes() {
    assert_eq!(char_prefix("hello", 3), "hel");
}

#[test]
fn test_char_prefix_multibyte_utf8_accent() {
    assert_eq!(char_prefix("héllo", 2), "hé");
    assert_eq!(char_prefix("héllo", 3), "hél");
}

#[test]
fn test_char_prefix_japanese_characters() {
    assert_eq!(char_prefix("日本語テスト", 3), "日本語");
}

#[test]
fn test_char_prefix_emoji_astral_codepoint() {
    let s = "🦀rust";
    assert_eq!(char_prefix(s, 1), "🦀");
    assert_eq!(char_prefix(s, 2), "🦀r");
}

#[test]
fn test_char_prefix_n_exceeds_length_returns_whole_string() {
    assert_eq!(char_prefix("hi", 10), "hi");
    assert_eq!(char_prefix("", 5), "");
}

#[test]
fn test_char_prefix_zero_returns_empty() {
    assert_eq!(char_prefix("hello", 0), "");
}

#[test]
fn test_char_len_ascii() {
    assert_eq!(char_len("hello"), 5);
}

#[test]
fn test_char_len_multibyte_accent() {
    assert_eq!(char_len("héllo"), 5);
}

#[test]
fn test_char_len_cjk_characters() {
    assert_eq!(char_len("日本語"), 3);
}

#[test]
fn test_char_len_emoji_astral() {
    assert_eq!(char_len("🦀🦀"), 2);
}

#[test]
fn test_char_len_empty() {
    assert_eq!(char_len(""), 0);
}

#[test]
fn test_char_len_lte_byte_len_invariant() {
    let cases = ["", "hello", "héllo", "日本語", "🦀"];
    for s in cases {
        assert!(
            char_len(s) <= s.len(),
            "char_len({s:?})={} > byte_len={}",
            char_len(s),
            s.len()
        );
    }
}

// -- py_json_dumps_opts --

#[test]
fn test_py_json_dumps_opts_sort_keys_false_insertion_order() {
    let v: Value = serde_json::from_str(r#"{"b": 1, "a": 2}"#).unwrap();
    let out = py_json_dumps_opts(&v, false, true);
    assert!(
        out.find("\"b\"").unwrap() < out.find("\"a\"").unwrap(),
        "sort_keys=false must preserve insertion order; got: {out}"
    );
}

#[test]
fn test_py_json_dumps_opts_sort_keys_true_alphabetical() {
    let v: Value = serde_json::from_str(r#"{"b": 1, "a": 2}"#).unwrap();
    let out = py_json_dumps_opts(&v, true, true);
    assert!(
        out.find("\"a\"").unwrap() < out.find("\"b\"").unwrap(),
        "sort_keys=true must sort alphabetically; got: {out}"
    );
}

#[test]
fn test_py_json_dumps_opts_ensure_ascii_true_escapes_non_ascii() {
    let v = json!("café");
    let out = py_json_dumps_opts(&v, false, true);
    assert_eq!(out, r#""caf\u00e9""#);
}

#[test]
fn test_py_json_dumps_opts_ensure_ascii_false_raw_utf8() {
    let v = json!("café");
    let out = py_json_dumps_opts(&v, false, false);
    assert_eq!(out, "\"café\"");
}

#[test]
fn test_py_json_dumps_opts_astral_char_surrogate_pair() {
    let v = json!("🦀");
    let out = py_json_dumps_opts(&v, false, true);
    assert_eq!(out, r#""\ud83e\udd80""#);
}

#[test]
fn test_py_json_dumps_opts_astral_char_raw_when_not_ascii() {
    let v = json!("🦀");
    let out = py_json_dumps_opts(&v, false, false);
    assert_eq!(out, "\"🦀\"");
}

#[test]
fn test_py_json_dumps_opts_null() {
    assert_eq!(py_json_dumps_opts(&Value::Null, false, true), "null");
}

#[test]
fn test_py_json_dumps_opts_bool_values() {
    assert_eq!(py_json_dumps_opts(&json!(true), false, true), "true");
    assert_eq!(py_json_dumps_opts(&json!(false), false, true), "false");
}

#[test]
fn test_py_json_dumps_opts_integer() {
    assert_eq!(py_json_dumps_opts(&json!(42), false, true), "42");
}

#[test]
fn test_py_json_dumps_opts_array_with_mixed_types() {
    let v = json!([1, "two", null, true]);
    let out = py_json_dumps_opts(&v, false, true);
    assert_eq!(out, r#"[1, "two", null, true]"#);
}

#[test]
fn test_py_json_dumps_opts_nested_object_sorted() {
    let v: Value = serde_json::from_str(r#"{"z": {"b": 2, "a": 1}, "a": [3, 4]}"#).unwrap();
    let out = py_json_dumps_opts(&v, true, true);
    assert_eq!(out, r#"{"a": [3, 4], "z": {"a": 1, "b": 2}}"#);
}

#[test]
fn test_py_json_dumps_opts_control_characters_escaped() {
    let v = json!("\x01\x02");
    let out = py_json_dumps_opts(&v, false, true);
    assert_eq!(out, r#""\u0001\u0002""#);
}

#[test]
fn test_py_json_dumps_opts_backslash_and_quote_escaped() {
    let v = json!("say \"hello\" \\world");
    let out = py_json_dumps_opts(&v, false, true);
    assert_eq!(out, r#""say \"hello\" \\world""#);
}

#[test]
fn test_py_json_dumps_opts_newline_tab_escaped() {
    let v = json!("a\nb\tc");
    let out = py_json_dumps_opts(&v, false, true);
    assert_eq!(out, r#""a\nb\tc""#);
}

#[test]
fn test_py_json_dumps_opts_empty_object_and_array() {
    assert_eq!(py_json_dumps_opts(&json!({}), false, true), "{}");
    assert_eq!(py_json_dumps_opts(&json!([]), false, true), "[]");
}

// -- py_split_ws --

#[test]
fn test_py_split_ws_basic() {
    assert_eq!(py_split_ws("  hello   world  "), vec!["hello", "world"]);
}

#[test]
fn test_py_split_ws_empty_string() {
    assert_eq!(py_split_ws(""), Vec::<&str>::new());
}

#[test]
fn test_py_split_ws_only_whitespace() {
    assert_eq!(py_split_ws("   "), Vec::<&str>::new());
}

#[test]
fn test_py_split_ws_c0_separators_act_as_delimiters() {
    assert_eq!(py_split_ws("a\x1cb"), vec!["a", "b"]);
}

// =============================================================================
// Area 2 - chunking.rs
// =============================================================================

// -- parse_grep_candidate --

#[test]
fn test_parse_grep_candidate_standard_format() {
    let result = parse_grep_candidate("path/to/file.rs:42:matching line content");
    assert_eq!(result, Some(("file.rs".to_string(), Some(42))));
}

#[test]
fn test_parse_grep_candidate_returns_basename() {
    let result = parse_grep_candidate("deep/nested/path/utils.py:10:some code");
    assert_eq!(result, Some(("utils.py".to_string(), Some(10))));
}

#[test]
fn test_parse_grep_candidate_non_numeric_second_field_gives_none_line() {
    let result = parse_grep_candidate("src/main.rs:not a number here");
    assert_eq!(result, Some(("main.rs".to_string(), None)));
}

#[test]
fn test_parse_grep_candidate_path_only_no_colon() {
    let result = parse_grep_candidate("src/lib.rs");
    assert_eq!(result, Some(("lib.rs".to_string(), None)));
}

#[test]
fn test_parse_grep_candidate_multiple_colons_in_content() {
    let result = parse_grep_candidate("file.rs:7:url: http://example.com:8080");
    assert_eq!(result, Some(("file.rs".to_string(), Some(7))));
}

#[test]
fn test_parse_grep_candidate_empty_line_returns_none() {
    assert_eq!(parse_grep_candidate(""), None);
    assert_eq!(parse_grep_candidate("   "), None);
}

#[test]
fn test_parse_grep_candidate_reranked_prefix_returns_none() {
    assert_eq!(parse_grep_candidate("[reranked 5 results]"), None);
}

#[test]
fn test_parse_grep_candidate_non_path_token_returns_none() {
    assert_eq!(parse_grep_candidate("justwords"), None);
}

#[test]
fn test_parse_grep_candidate_line_number_saturation() {
    let result = parse_grep_candidate("a.py:99999999999999999999:content");
    assert_eq!(result, Some(("a.py".to_string(), Some(i64::MAX))));
}

#[test]
fn test_parse_grep_candidate_line_number_zero() {
    let result = parse_grep_candidate("main.go:0:package main");
    assert_eq!(result, Some(("main.go".to_string(), Some(0))));
}

#[test]
fn test_parse_grep_candidate_whitespace_trimmed() {
    let result = parse_grep_candidate("  file.rs:5:code  ");
    assert_eq!(result, Some(("file.rs".to_string(), Some(5))));
}

#[test]
fn test_parse_grep_candidate_empty_text_after_line_number() {
    // `file.rs:10:` — a match line with empty text still yields the coordinate.
    let result = parse_grep_candidate("file.rs:10:");
    assert_eq!(result, Some(("file.rs".to_string(), Some(10))));
}

#[test]
fn test_parse_grep_candidate_windows_backslash_path_not_a_candidate() {
    // Known limitation (documents current behaviour, no panic): backslash
    // separators are not recognised, so `C:\project\file.rs:10:code` parses
    // to None instead of (file.rs, 10). The forward-slash drive form falls
    // through to the path-only branch on the whole line (basename splits on
    // '/' only), yielding the trailing segment with no line number.
    assert_eq!(parse_grep_candidate("C:\\project\\file.rs:10:code"), None);
    assert_eq!(
        parse_grep_candidate("C:/project/file.rs:10:code"),
        Some(("file.rs:10:code".to_string(), None))
    );
}

// -- chunk_observation helpers --

fn chunk_obs(cmd: &str, obs: &str) -> Vec<Chunk> {
    chunk_observation(cmd, obs, 0, DEFAULT_WIN, None, ChunkMode::Fixed)
}

#[test]
fn test_chunk_observation_empty_obs_returns_one_chunk() {
    let chunks = chunk_obs("echo hello", "");
    assert!(
        !chunks.is_empty(),
        "should produce at least one chunk for empty obs"
    );
}

#[test]
fn test_chunk_observation_whitespace_only_obs() {
    let chunks = chunk_obs("ls", "   \n\n   ");
    assert!(!chunks.is_empty());
}

#[test]
fn test_chunk_observation_short_output_single_other_chunk() {
    let obs = "line1\nline2\nline3";
    let chunks = chunk_obs("python3 script.py", obs);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, "other");
}

#[test]
fn test_chunk_observation_long_output_windowed() {
    let obs = (0..100)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk_obs("make test", &obs);
    assert_eq!(chunks.len(), 3, "100 lines / win=40 should yield 3 chunks");
}

#[test]
fn test_chunk_observation_grep_cmd_produces_grep_chunks() {
    let obs = "src/main.rs:10:fn main() {\nsrc/lib.rs:20:pub fn helper() {";
    let chunks = chunk_observation("grep -r fn .", obs, 0, DEFAULT_WIN, None, ChunkMode::Fixed);
    assert!(
        chunks.iter().any(|c| c.kind == "grep"),
        "grep command should produce grep chunks"
    );
}

#[test]
fn test_chunk_observation_grep_chunk_has_file_and_line() {
    let obs = "utils.rs:42:let x = 1;";
    let chunks = chunk_observation("grep let", obs, 0, DEFAULT_WIN, None, ChunkMode::Fixed);
    let grep_chunk = chunks
        .iter()
        .find(|c| c.kind == "grep")
        .expect("grep chunk");
    assert_eq!(grep_chunk.file.as_deref(), Some("utils.rs"));
    assert_eq!(grep_chunk.lo, Some(42));
}

#[test]
fn test_chunk_observation_full_coverage_short() {
    let obs = "alpha\nbeta\ngamma";
    let chunks = chunk_obs("cat file.txt", obs);
    assert_full_coverage(obs, &chunks);
}

#[test]
fn test_chunk_observation_full_coverage_large() {
    let obs = (0..200)
        .map(|i| format!("content line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk_obs("python3 run.py", &obs);
    assert_full_coverage(&obs, &chunks);
}

#[test]
fn test_chunk_observation_full_coverage_unicode() {
    let obs = "日本語テキスト\n🦀 Rust is great!\nEnd";
    let chunks = chunk_obs("cat notes.txt", obs);
    assert_full_coverage(obs, &chunks);
}

#[test]
fn test_chunk_observation_cmd_capped_at_300_chars() {
    let long_cmd = "x".repeat(500);
    let chunks = chunk_obs(&long_cmd, "output");
    for c in &chunks {
        assert!(
            char_len(&c.cmd) <= 300,
            "cmd must be capped at 300 chars, got {}",
            char_len(&c.cmd)
        );
    }
}

#[test]
fn test_chunk_observation_head_capped_at_240_chars() {
    let obs = "x".repeat(500);
    let chunks = chunk_obs("ls -la", &obs);
    for c in &chunks {
        assert!(
            char_len(&c.head) <= 240,
            "head must be capped at 240 chars, got {}",
            char_len(&c.head)
        );
    }
}

#[test]
fn test_chunk_observation_all_chunks_share_cmd_and_head() {
    let obs = (0..100)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk_obs("python3 test.py", &obs);
    let first_cmd = chunks[0].cmd.clone();
    let first_head = chunks[0].head.clone();
    for c in &chunks {
        assert_eq!(c.cmd, first_cmd);
        assert_eq!(c.head, first_head);
    }
}

#[test]
fn test_chunk_observation_read_cmd_produces_read_chunks() {
    let obs = "fn main() {\n    println!(\"hello\");\n}";
    let chunks = chunk_observation(
        "cat src/main.rs",
        obs,
        0,
        DEFAULT_WIN,
        None,
        ChunkMode::Fixed,
    );
    assert!(
        chunks.iter().all(|c| c.kind == "read"),
        "cat command should produce read chunks"
    );
}

#[test]
fn test_chunk_observation_sed_base_line_number() {
    let obs = (0..10)
        .map(|i| format!("code line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk_observation(
        "sed -n '10,20p' main.rs",
        &obs,
        0,
        DEFAULT_WIN,
        None,
        ChunkMode::Fixed,
    );
    assert_eq!(chunks[0].lo, Some(10), "sed base should set lo to 10");
}

#[test]
fn test_chunk_observation_sed_inverted_range_keeps_first_number() {
    // `sed -n '20,10p'` is an inverted range; the parser takes the first
    // number as the base (documents current behaviour, no panic).
    let obs = (0..10)
        .map(|i| format!("code line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk_observation(
        "sed -n '20,10p' main.rs",
        &obs,
        0,
        DEFAULT_WIN,
        None,
        ChunkMode::Fixed,
    );
    assert_eq!(chunks[0].lo, Some(20));
}

#[test]
fn test_chunk_observation_sed_non_numeric_range_falls_back_to_one() {
    // `sed -n 'a,bp'` matches no range pattern, so the base falls back to 1.
    let obs = (0..10)
        .map(|i| format!("code line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk_observation(
        "sed -n 'a,bp' main.rs",
        &obs,
        0,
        DEFAULT_WIN,
        None,
        ChunkMode::Fixed,
    );
    assert_eq!(chunks[0].lo, Some(1));
}

#[test]
fn test_chunk_observation_sed_unusual_spacing_still_parses_range() {
    // Extra spacing around `-n` does not matter: the range pattern is
    // searched anywhere in the command string.
    let obs = (0..10)
        .map(|i| format!("code line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk_observation(
        "sed  -n   '30,40p'  main.rs",
        &obs,
        0,
        DEFAULT_WIN,
        None,
        ChunkMode::Fixed,
    );
    assert_eq!(chunks[0].lo, Some(30));
}

#[test]
fn test_chunk_observation_small_window_more_chunks() {
    let obs = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks_small = chunk_observation("make", &obs, 0, 5, None, ChunkMode::Fixed);
    let chunks_large = chunk_observation("make", &obs, 0, 20, None, ChunkMode::Fixed);
    assert!(
        chunks_small.len() >= chunks_large.len(),
        "smaller window should produce >= chunks"
    );
}

#[test]
fn test_chunk_observation_rc_parsed_from_returncode_tag() {
    let obs = "<returncode>0</returncode>\nsome output";
    let chunks = chunk_obs("./run.sh", obs);
    for c in &chunks {
        assert_eq!(c.rc, Some(0));
    }
}

#[test]
fn test_chunk_observation_rc_parsed_from_returncode_equals() {
    let obs = "returncode=1\nerror output";
    let chunks = chunk_obs("cargo build", obs);
    for c in &chunks {
        assert_eq!(c.rc, Some(1));
    }
}

#[test]
fn test_chunk_observation_no_rc_when_absent() {
    let obs = "clean output with no returncode";
    let chunks = chunk_obs("echo hi", obs);
    for c in &chunks {
        assert_eq!(c.rc, None);
    }
}

// -- chunk_assistant --

#[test]
fn test_chunk_assistant_empty_returns_no_chunks() {
    let chunks = chunk_assistant("", 0, DEFAULT_WIN);
    assert!(chunks.is_empty());
}

#[test]
fn test_chunk_assistant_whitespace_only_returns_no_chunks() {
    let chunks = chunk_assistant("   \n\n   ", 0, DEFAULT_WIN);
    assert!(chunks.is_empty());
}

#[test]
fn test_chunk_assistant_short_text_single_chunk() {
    let chunks = chunk_assistant("Let me help you with that.", 1, DEFAULT_WIN);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, "asst");
    assert_eq!(chunks[0].step, 1);
}

#[test]
fn test_chunk_assistant_long_text_windowed() {
    let txt = (0..100)
        .map(|i| format!("Reasoning line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk_assistant(&txt, 0, DEFAULT_WIN);
    assert_eq!(chunks.len(), 3);
}

#[test]
fn test_chunk_assistant_all_chunks_are_asst_kind() {
    let chunks = chunk_assistant("Line one\nLine two\nLine three", 0, DEFAULT_WIN);
    for c in &chunks {
        assert_eq!(c.kind, "asst");
    }
}

#[test]
fn test_chunk_assistant_full_coverage_invariant() {
    let txt = "First thought.\nSecond thought.\nThird thought.";
    let chunks = chunk_assistant(txt, 0, DEFAULT_WIN);
    let all_text = chunks
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for line in py_splitlines(txt) {
        assert!(all_text.contains(line), "line {line:?} missing from chunks");
    }
}

// =============================================================================
// Area 3 - messages.rs
// =============================================================================

#[test]
fn test_steps_of_empty_messages_returns_empty() {
    let steps = messages::steps_of(&[]);
    assert!(steps.is_empty());
}

#[test]
fn test_steps_of_single_assistant_with_string_content() {
    let msgs = vec![
        json!({
            "role": "assistant",
            "content": "some response",
            "extra": {"actions": [{"command": "ls -la"}]}
        }),
        json!({"role": "user", "content": "observation text"}),
    ];
    let steps = messages::steps_of(&msgs);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].0, "ls -la");
    assert_eq!(steps[0].1, "observation text");
}

#[test]
fn test_steps_of_content_as_array_of_text_blocks() {
    let msgs = vec![
        json!({"role": "assistant", "extra": {"actions": [{"command": "grep foo"}]}}),
        json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "first part"},
                {"type": "text", "text": "second part"}
            ]
        }),
    ];
    let steps = messages::steps_of(&msgs);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].1, "first part second part");
}

#[test]
fn test_steps_of_no_trailing_user_creates_no_step() {
    let msgs = vec![json!({
        "role": "assistant",
        "extra": {"actions": [{"command": "ls"}]}
    })];
    let steps = messages::steps_of(&msgs);
    assert!(steps.is_empty());
}

#[test]
fn test_steps_of_tool_role_triggers_step() {
    let msgs = vec![
        json!({"role": "assistant", "extra": {"actions": [{"command": "cat file"}]}}),
        json!({"role": "tool", "content": "file contents here"}),
    ];
    let steps = messages::steps_of(&msgs);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].1, "file contents here");
}

#[test]
fn test_steps_of_multiple_actions_joined_with_semicolon() {
    let msgs = vec![
        json!({
            "role": "assistant",
            "extra": {"actions": [{"command": "ls"}, {"command": "pwd"}]}
        }),
        json!({"role": "user", "content": "output"}),
    ];
    let steps = messages::steps_of(&msgs);
    assert_eq!(steps[0].0, "ls ; pwd");
}

#[test]
fn test_steps_of_falls_back_to_query_when_command_empty() {
    let msgs = vec![
        json!({
            "role": "assistant",
            "extra": {"actions": [{"command": "", "query": "search term"}]}
        }),
        json!({"role": "user", "content": "result"}),
    ];
    let steps = messages::steps_of(&msgs);
    assert_eq!(steps[0].0, "search term");
}

#[test]
fn test_steps_of_malformed_no_role_field_skipped() {
    let msgs = vec![
        json!({"content": "no role"}),
        json!({"role": "assistant", "extra": {"actions": [{"command": "ls"}]}}),
        json!({"role": "user", "content": "obs"}),
    ];
    let steps = messages::steps_of(&msgs);
    assert_eq!(steps.len(), 1);
}

#[test]
fn test_steps_of_malformed_integer_role_skipped() {
    let msgs = vec![
        json!({"role": 42, "content": "integer role"}),
        json!({"role": "assistant", "extra": {"actions": [{"command": "cmd"}]}}),
        json!({"role": "user", "content": "obs"}),
    ];
    let steps = messages::steps_of(&msgs);
    assert_eq!(steps.len(), 1);
}

#[test]
fn test_steps_of_no_extra_actions_yields_empty_command() {
    let msgs = vec![
        json!({"role": "assistant", "content": "hello"}),
        json!({"role": "user", "content": "world"}),
    ];
    let steps = messages::steps_of(&msgs);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].0, "");
}

#[test]
fn test_steps_of_numeric_content_yields_empty_obs_without_panic() {
    // Malformed `content` with an unexpected type must not panic;
    // non-string/non-array content extracts as an empty observation.
    let msgs = vec![
        json!({"role": "assistant", "extra": {"actions": [{"command": "ls"}]}}),
        json!({"role": "user", "content": 42}),
    ];
    let steps = messages::steps_of(&msgs);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0], ("ls".to_string(), String::new()));
}

#[test]
fn test_reasoning_chunks_of_no_reasoning_empty() {
    let msgs = vec![
        json!({"role": "assistant", "content": "plain text"}),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = messages::reasoning_chunks_of(&msgs);
    assert!(chunks.is_empty());
}

#[test]
fn test_reasoning_chunks_of_with_thinking_blocks() {
    let msgs = vec![
        json!({
            "role": "assistant",
            "thinking_blocks": "This is my reasoning process",
            "extra": {"actions": [{"command": "ls"}]}
        }),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = messages::reasoning_chunks_of(&msgs);
    assert!(!chunks.is_empty());
    assert!(chunks.iter().all(|c| c.kind == "reasoning"));
}

#[test]
fn test_reasoning_chunks_of_evict_is_provider() {
    let msgs = vec![
        json!({"role": "assistant", "reasoning_content": "some reasoning"}),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = messages::reasoning_chunks_of(&msgs);
    if !chunks.is_empty() {
        assert_eq!(chunks[0].evict, "provider");
    }
}

#[test]
fn test_assistant_chunks_of_plain_string_content() {
    let long_text = (0..50)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let msgs = vec![
        json!({"role": "assistant", "content": long_text}),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = messages::assistant_chunks_of(&msgs);
    assert!(!chunks.is_empty());
    assert!(chunks.iter().all(|c| c.kind == "asst"));
}

#[test]
fn test_assistant_chunks_of_whitespace_only_no_chunks() {
    let msgs = vec![
        json!({"role": "assistant", "content": "   \n\n   "}),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = messages::assistant_chunks_of(&msgs);
    assert!(chunks.is_empty());
}

#[test]
fn test_assistant_chunks_of_tool_use_content_skipped() {
    let msgs = vec![
        json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "name": "bash", "input": {}}]
        }),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = messages::assistant_chunks_of(&msgs);
    assert!(
        chunks.is_empty(),
        "tool_use content should produce no assistant chunks"
    );
}

#[test]
fn test_assistant_chunks_of_mixed_text_and_tool_use_skipped() {
    let msgs = vec![
        json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "some text"},
                {"type": "tool_use", "name": "bash", "input": {}}
            ]
        }),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = messages::assistant_chunks_of(&msgs);
    assert!(
        chunks.is_empty(),
        "mixed content with tool_use should be skipped"
    );
}

#[test]
fn test_assistant_chunks_of_null_content_no_chunks() {
    let msgs = vec![
        json!({"role": "assistant", "content": null}),
        json!({"role": "user", "content": "obs"}),
    ];
    let chunks = messages::assistant_chunks_of(&msgs);
    assert!(chunks.is_empty());
}

#[test]
fn test_assistant_chunks_of_step_increments_per_turn() {
    let text = "Hello world from step";
    let msgs = vec![
        json!({"role": "assistant", "content": text}),
        json!({"role": "user", "content": "obs1"}),
        json!({"role": "assistant", "content": text}),
        json!({"role": "user", "content": "obs2"}),
    ];
    let chunks = messages::assistant_chunks_of(&msgs);
    let steps: Vec<i64> = chunks.iter().map(|c| c.step).collect();
    assert!(steps.contains(&0), "first turn should have step=0");
    assert!(steps.contains(&1), "second turn should have step=1");
}

#[test]
fn test_blob_tokens_no_reasoning_fields_is_zero() {
    let m = json!({"role": "assistant", "content": "text"});
    assert_eq!(messages::blob_tokens(&m), 0);
}

#[test]
fn test_blob_tokens_empty_reasoning_content_is_zero() {
    let m = json!({"role": "assistant", "reasoning_content": ""});
    assert_eq!(messages::blob_tokens(&m), 0);
}

#[test]
fn test_blob_tokens_non_empty_reasoning_content_positive() {
    let content = "a".repeat(400);
    let m = json!({"role": "assistant", "reasoning_content": content});
    assert!(messages::blob_tokens(&m) > 0);
}

#[test]
fn test_blob_tokens_null_reasoning_content_is_zero() {
    let m = json!({"role": "assistant", "reasoning_content": null});
    assert_eq!(messages::blob_tokens(&m), 0);
}
