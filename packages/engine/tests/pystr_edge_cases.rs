//! Edge-case tests for `pystr` — the Python string-semantics layer.
//!
//! Each test pins a place where Python's behaviour differs from Rust's std,
//! since a silent divergence here shifts chunk boundaries and breaks the
//! byte-for-byte parity contract.

use parsec_engine::pystr::*;

// ---------------------------------------------------------------------------
// py_splitlines
// ---------------------------------------------------------------------------

/// Python splits on eleven boundaries, not just `\n`. `str::lines()` would
/// only handle two of these.
#[test]
fn test_py_splitlines_splits_on_every_python_boundary() {
    for brk in [
        '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
    ] {
        let s = format!("a{brk}b");
        assert_eq!(
            py_splitlines(&s),
            vec!["a", "b"],
            "U+{:04X} should be a line boundary",
            brk as u32
        );
    }
}

/// `\x1f` is whitespace to `strip()` but is *not* a line boundary — the one
/// member of the `\x1c..\x1f` run that `splitlines()` leaves alone.
#[test]
fn test_py_splitlines_does_not_split_on_unit_separator() {
    assert_eq!(py_splitlines("a\x1fb"), vec!["a\x1fb"]);
}

/// `\r\n` is one break, not two, so it must not produce an empty line between.
#[test]
fn test_py_splitlines_consumes_crlf_as_single_break() {
    assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
    // But a genuinely empty line between two CRLFs is preserved.
    assert_eq!(py_splitlines("a\r\n\r\nb"), vec!["a", "", "b"]);
    // Reversed order is two separate breaks.
    assert_eq!(py_splitlines("a\n\rb"), vec!["a", "", "b"]);
}

/// A trailing break yields no empty final element; a leading one does yield an
/// empty first element.
#[test]
fn test_py_splitlines_trailing_and_leading_breaks() {
    assert_eq!(py_splitlines("a\n"), vec!["a"]);
    assert_eq!(py_splitlines("a\r\n"), vec!["a"]);
    assert_eq!(py_splitlines("\na"), vec!["", "a"]);
    assert_eq!(py_splitlines("\n\n"), vec!["", ""]);
    assert_eq!(py_splitlines("\r"), vec![""]);
}

#[test]
fn test_py_splitlines_empty_and_no_break_inputs() {
    assert_eq!(py_splitlines(""), Vec::<&str>::new());
    assert_eq!(py_splitlines("no breaks here"), vec!["no breaks here"]);
}

/// Multi-byte content must survive slicing — a byte-indexed split would panic
/// or corrupt these.
#[test]
fn test_py_splitlines_preserves_multibyte_content() {
    assert_eq!(
        py_splitlines("héllo\n世界\r\n🎯"),
        vec!["héllo", "世界", "🎯"]
    );
}

/// Full-coverage style invariant: every line is a substring of the original,
/// in order, and only break characters are lost.
#[test]
fn test_py_splitlines_reconstructs_when_joined_on_single_breaks() {
    let s = "alpha\nbeta\ngamma\ndelta";
    assert_eq!(py_splitlines(s).join("\n"), s);
}

// ---------------------------------------------------------------------------
// py_is_space / py_strip / py_has_content
// ---------------------------------------------------------------------------

/// Python's whitespace set = Unicode White_Space plus the ASCII separators
/// `\x1c..\x1f`, which Rust's `char::is_whitespace` excludes.
#[test]
fn test_py_is_space_includes_ascii_separators() {
    for c in ['\x1c', '\x1d', '\x1e', '\x1f'] {
        assert!(py_is_space(c), "U+{:04X} is whitespace in Python", c as u32);
    }
}

#[test]
fn test_py_is_space_standard_and_unicode_whitespace() {
    for c in [' ', '\t', '\n', '\r', '\x0b', '\x0c', '\u{85}', '\u{a0}'] {
        assert!(py_is_space(c), "U+{:04X} should be whitespace", c as u32);
    }
}

/// Characters that look blank but are not whitespace to Python.
#[test]
fn test_py_is_space_rejects_non_whitespace() {
    for c in ['\0', '\u{200b}', 'a', '_'] {
        assert!(
            !py_is_space(c),
            "U+{:04X} is not whitespace in Python",
            c as u32
        );
    }
}

#[test]
fn test_py_strip_removes_separators_rust_trim_would_keep() {
    assert_eq!(py_strip("\x1c\x1d\x1e\x1fvalue\x1f"), "value");
    assert_eq!(py_strip("  spaced  "), "spaced");
    assert_eq!(py_strip("\t\n mixed \r\n"), "mixed");
}

#[test]
fn test_py_strip_only_touches_the_ends() {
    assert_eq!(py_strip("  a  b  "), "a  b");
}

#[test]
fn test_py_strip_empty_and_all_whitespace() {
    assert_eq!(py_strip(""), "");
    assert_eq!(py_strip("   \t\n\x1c "), "");
    assert_eq!(py_strip("nowhitespace"), "nowhitespace");
}

#[test]
fn test_py_has_content_matches_strip_truthiness() {
    assert!(!py_has_content(""));
    assert!(!py_has_content(" \t\n\x1c\x1f "));
    assert!(py_has_content("  x  "));
    assert!(
        py_has_content("\u{200b}"),
        "ZWSP is content, not whitespace"
    );
}

// ---------------------------------------------------------------------------
// py_split_ws
// ---------------------------------------------------------------------------

#[test]
fn test_py_split_ws_collapses_runs_and_drops_empties() {
    assert_eq!(py_split_ws("  a   b  "), vec!["a", "b"]);
    assert_eq!(py_split_ws("a\n\tb\x1cc"), vec!["a", "b", "c"]);
    assert_eq!(py_split_ws(""), Vec::<&str>::new());
    assert_eq!(py_split_ws("     "), Vec::<&str>::new());
    assert_eq!(py_split_ws("single"), vec!["single"]);
}

// ---------------------------------------------------------------------------
// char_prefix / char_len
// ---------------------------------------------------------------------------

/// `s[:n]` in Python counts characters. Byte slicing would cut "é" in half.
#[test]
fn test_char_prefix_counts_chars_not_bytes() {
    assert_eq!(char_prefix("héllo", 2), "hé");
    assert_eq!(char_prefix("日本語テキスト", 3), "日本語");
    assert_eq!(char_prefix("🎯🎯🎯", 1), "🎯");
}

#[test]
fn test_char_prefix_boundary_lengths() {
    assert_eq!(char_prefix("abc", 0), "");
    assert_eq!(char_prefix("", 5), "");
    // n exactly at, and beyond, the char length returns the whole string.
    assert_eq!(char_prefix("héllo", 5), "héllo");
    assert_eq!(char_prefix("héllo", 999), "héllo");
}

#[test]
fn test_char_prefix_at_own_length_is_identity() {
    for s in ["", "ascii", "héllo", "日本語", "🎯 mixed é text"] {
        assert_eq!(char_prefix(s, char_len(s)), s, "identity for {s:?}");
    }
}

#[test]
fn test_char_len_counts_code_points() {
    assert_eq!(char_len(""), 0);
    assert_eq!(char_len("abc"), 3);
    assert_eq!(char_len("héllo"), 5, "6 bytes, 5 chars");
    assert_eq!(char_len("日本語"), 3, "9 bytes, 3 chars");
    assert_eq!(char_len("🎯"), 1, "4 bytes, 1 char");
    // A combining sequence is two code points, matching Python's len().
    assert_eq!(char_len("e\u{301}"), 2);
}

#[test]
fn test_char_len_never_exceeds_byte_len() {
    for s in ["", "ascii", "héllo", "日本語", "🎯🎯"] {
        assert!(char_len(s) <= s.len(), "invariant violated for {s:?}");
    }
}

// ---------------------------------------------------------------------------
// py_json_dumps / py_json_dumps_opts
// ---------------------------------------------------------------------------

fn json(src: &str) -> serde_json::Value {
    serde_json::from_str(src).expect("test fixture parses")
}

/// Python's default separators are ", " and ": " — serde_json emits neither.
#[test]
fn test_py_json_dumps_uses_python_separators() {
    let v = json(r#"{"a": 1, "b": [1, 2]}"#);
    assert_eq!(py_json_dumps(&v), r#"{"a": 1, "b": [1, 2]}"#);
}

#[test]
fn test_py_json_dumps_opts_sort_keys() {
    let v = json(r#"{"b": 1, "a": 2, "C": 3}"#);
    // Insertion order is preserved when sort_keys is off.
    assert_eq!(
        py_json_dumps_opts(&v, false, true),
        r#"{"b": 1, "a": 2, "C": 3}"#
    );
    // Sorted by code point: uppercase "C" sorts before lowercase.
    assert_eq!(
        py_json_dumps_opts(&v, true, true),
        r#"{"C": 3, "a": 2, "b": 1}"#
    );
}

#[test]
fn test_py_json_dumps_opts_sort_keys_is_recursive() {
    let v = json(r#"{"z": {"y": 1, "x": 2}}"#);
    assert_eq!(
        py_json_dumps_opts(&v, true, true),
        r#"{"z": {"x": 2, "y": 1}}"#
    );
}

#[test]
fn test_py_json_dumps_opts_ensure_ascii_escaping() {
    let v = json(r#"{"k": "é"}"#);
    assert_eq!(py_json_dumps_opts(&v, false, true), r#"{"k": "\u00e9"}"#);
    assert_eq!(py_json_dumps_opts(&v, false, false), "{\"k\": \"é\"}");
}

/// Astral-plane characters become surrogate pairs under ensure_ascii, the way
/// CPython emits them.
#[test]
fn test_py_json_dumps_opts_astral_becomes_surrogate_pair() {
    let v = json(r#""🎯""#);
    assert_eq!(py_json_dumps_opts(&v, false, true), r#""\ud83c\udfaf""#);
    assert_eq!(py_json_dumps_opts(&v, false, false), "\"🎯\"");
}

/// ensure_ascii=false still escapes control characters and structural chars.
#[test]
fn test_py_json_dumps_opts_escapes_controls_regardless_of_ensure_ascii() {
    let v = serde_json::Value::String("a\"b\\c\nd\te\x08f\x0cg\x01h".to_string());
    let want = r#""a\"b\\c\nd\te\bf\fg\u0001h""#;
    assert_eq!(py_json_dumps_opts(&v, false, true), want);
    assert_eq!(py_json_dumps_opts(&v, false, false), want);
}

#[test]
fn test_py_json_dumps_handles_empty_and_scalar_values() {
    assert_eq!(py_json_dumps(&json("{}")), "{}");
    assert_eq!(py_json_dumps(&json("[]")), "[]");
    assert_eq!(py_json_dumps(&json("null")), "null");
    assert_eq!(py_json_dumps(&json("true")), "true");
    assert_eq!(py_json_dumps(&json("false")), "false");
    assert_eq!(py_json_dumps(&json(r#""""#)), r#""""#);
}

#[test]
fn test_py_json_dumps_nested_structures_keep_separators() {
    let v = json(r#"{"a": [{"b": null}, []], "c": {}}"#);
    assert_eq!(py_json_dumps(&v), r#"{"a": [{"b": null}, []], "c": {}}"#);
}

// ---------------------------------------------------------------------------
// py_float_repr
// ---------------------------------------------------------------------------

/// The positional/scientific switch happens at exponent -4 and 16.
#[test]
fn test_py_float_repr_notation_switch_boundaries() {
    for (f, want) in [
        (0.0001, "0.0001"),
        (0.00001, "1e-05"),
        (1e15, "1000000000000000.0"),
        (1e16, "1e+16"),
    ] {
        assert_eq!(py_float_repr(f), want, "repr({f})");
    }
}

/// CPython pads the exponent to at least two digits and always signs it.
#[test]
fn test_py_float_repr_exponent_is_signed_and_padded() {
    assert_eq!(py_float_repr(1e-7), "1e-07");
    assert_eq!(py_float_repr(1e100), "1e+100");
    assert_eq!(py_float_repr(-2.5e-7), "-2.5e-07");
}

/// Integral floats keep a ".0" suffix, and negative zero keeps its sign.
#[test]
fn test_py_float_repr_integral_and_negative_zero() {
    assert_eq!(py_float_repr(1.0), "1.0");
    assert_eq!(py_float_repr(-3.0), "-3.0");
    assert_eq!(py_float_repr(0.0), "0.0");
    assert_eq!(py_float_repr(-0.0), "-0.0");
}

#[test]
fn test_py_json_dumps_integers_are_not_floatified() {
    assert_eq!(py_json_dumps(&json("[0, -1, 42]")), "[0, -1, 42]");
}
