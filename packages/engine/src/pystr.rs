//! Python string semantics, reproduced exactly.
//!
//! The parity contract (DIRECTION.md §7b) is byte-for-byte against the Python
//! reference, and Python's string ops differ from Rust's std in ways that
//! silently shift chunk boundaries: `str.splitlines()` splits on eleven line
//! boundaries (not just \n / \r\n), `str.strip()`'s whitespace set includes
//! \x1c..\x1f, and slices like `obs[:400]` count chars, not bytes. Every port
//! in this crate goes through these helpers instead of std equivalents.

/// Characters `str.splitlines()` treats as line boundaries.
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\x0b'
            | '\x0c'
            | '\x1c'
            | '\x1d'
            | '\x1e'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// `str.splitlines()`: no trailing empty element, \r\n consumed as one break.
pub fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if is_line_break(c) {
            out.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(j, '\n')) = iter.peek() {
                    iter.next();
                    end = j + 1;
                }
            }
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `str.isspace()` per char: Unicode White_Space plus \x1c..\x1f.
pub fn py_is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\x1c'..='\x1f')
}

/// `str.strip()` with no arguments.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_is_space)
}

/// Truthiness of `s.strip()` — "does this contain any non-whitespace?".
pub fn py_has_content(s: &str) -> bool {
    !py_strip(s).is_empty()
}

/// `str.split()` with no arguments: runs of whitespace, no empty parts.
pub fn py_split_ws(s: &str) -> Vec<&str> {
    s.split(py_is_space).filter(|t| !t.is_empty()).collect()
}

/// `s[:n]` — a char-count prefix, never a byte slice.
pub fn char_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// `len(s)` — Python counts chars.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// `json.dumps(v)` with default arguments: ", "/": " separators and
/// ensure_ascii=True (non-ASCII escaped as \uXXXX, astral chars as surrogate
/// pairs). Only the LENGTH of this string feeds features (blob_tokens), but we
/// reproduce the bytes so the length can never drift.
pub fn py_json_dumps(v: &serde_json::Value) -> String {
    py_json_dumps_opts(v, false, true)
}

/// `json.dumps(v, sort_keys=..., ensure_ascii=...)` — the fingerprint hash in
/// the freeze layer uses (sort_keys=True, ensure_ascii=False), and those bytes
/// feed sha256, so they must match Python exactly. Key sort: Python compares
/// str by code point; Rust's byte-wise UTF-8 Ord is the same order.
pub fn py_json_dumps_opts(v: &serde_json::Value, sort_keys: bool, ensure_ascii: bool) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Number(n) => py_number_repr(n),
        Value::String(s) => py_json_quote_opts(s, ensure_ascii),
        Value::Array(a) => {
            let items: Vec<String> = a
                .iter()
                .map(|x| py_json_dumps_opts(x, sort_keys, ensure_ascii))
                .collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(o) => {
            let mut entries: Vec<(&String, &Value)> = o.iter().collect();
            if sort_keys {
                entries.sort_by_key(|(k, _)| *k);
            }
            let items: Vec<String> = entries
                .iter()
                .map(|(k, val)| {
                    format!(
                        "{}: {}",
                        py_json_quote_opts(k, ensure_ascii),
                        py_json_dumps_opts(val, sort_keys, ensure_ascii)
                    )
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// `json.dumps` number formatting. Integers print as-is; floats follow
/// CPython `repr`: shortest round-trip digits, positional notation for
/// leading-digit exponents in [-4, 16), else scientific with a signed,
/// two-plus-digit exponent ("1e-05", "1e+16"), and a ".0" suffix on integral
/// positional floats. serde_json's ryu Display diverges on all of those
/// (verified: 1e-7 -> "1e-7", 1e-5 -> "0.00001", -0.0 -> "-0"), and the
/// fingerprint sha256 consumes these bytes. Requires the `float_roundtrip`
/// feature so parsed floats carry Python's exact value. Known deviation:
/// integers outside i64/u64 and floats Python would print from non-f64
/// sources are unrepresentable in serde_json and cannot match.
fn py_number_repr(n: &serde_json::Number) -> String {
    if n.is_i64() || n.is_u64() {
        return n.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    py_float_repr(f)
}

/// CPython `repr(float)` for finite values.
pub fn py_float_repr(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    // {:e} gives shortest round-trip digits as d[.ddd]e<exp> with the
    // exponent of the leading digit — the same exponent CPython's rule uses.
    let sci = format!("{:e}", f);
    let neg = sci.starts_with('-');
    let body = if neg { &sci[1..] } else { &sci[..] };
    let (mant, exp) = body.split_once('e').expect("{:e} always has an exponent");
    let exp: i32 = exp.parse().expect("exponent parses");
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let sign = if neg { "-" } else { "" };
    if !(-4..16).contains(&exp) {
        let m = if digits.len() == 1 {
            digits
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!(
            "{}{}e{}{:02}",
            sign,
            m,
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else if exp >= 0 {
        let e = exp as usize;
        if digits.len() <= e + 1 {
            format!("{}{}{}.0", sign, digits, "0".repeat(e + 1 - digits.len()))
        } else {
            format!("{}{}.{}", sign, &digits[..e + 1], &digits[e + 1..])
        }
    } else {
        format!("{}0.{}{}", sign, "0".repeat((-exp - 1) as usize), digits)
    }
}

fn py_json_quote_opts(s: &str, ensure_ascii: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if c.is_ascii() || !ensure_ascii => out.push(c),
            c => {
                let cp = c as u32;
                if cp > 0xFFFF {
                    let v = cp - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    ));
                } else {
                    out.push_str(&format!("\\u{:04x}", cp));
                }
            }
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------------------------------------------------------------------------
    // py_splitlines
    // -------------------------------------------------------------------------

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(py_splitlines("a\nb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\r\nb\rc"), vec!["a", "b", "c"]);
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(
            py_splitlines("a\x0bb\x1cc\u{2028}d"),
            vec!["a", "b", "c", "d"]
        );
    }

    #[test]
    fn test_py_splitlines_empty_string_returns_empty_vec() {
        // Python: "".splitlines() == []
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
    }

    #[test]
    fn test_py_splitlines_no_newline_returns_single_element() {
        // Python: "hello".splitlines() == ["hello"]
        assert_eq!(py_splitlines("hello"), vec!["hello"]);
    }

    #[test]
    fn test_py_splitlines_handles_crlf_as_single_break() {
        // \r\n must be consumed as ONE break, not two.
        // Python: "a\r\nb".splitlines() == ["a", "b"]
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
    }

    #[test]
    fn test_py_splitlines_crlf_does_not_produce_empty_intermediate() {
        // "a\r\nb\r\nc" must yield three elements, not four.
        assert_eq!(py_splitlines("a\r\nb\r\nc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_py_splitlines_bare_cr_is_independent_break() {
        // \r not followed by \n is its own line break.
        assert_eq!(py_splitlines("a\rb"), vec!["a", "b"]);
    }

    #[test]
    fn test_py_splitlines_all_eleven_line_break_chars() {
        // Python recognises exactly these 11 line-boundary characters:
        //  \n, \r, \x0b (VT), \x0c (FF), \x1c (FS), \x1d (GS), \x1e (RS),
        //  \x85 (NEL), \u2028 (LS), \u2029 (PS)  — plus \r\n as one unit.
        let s = "a\nb\rc\x0bd\x0ce\x1cf\x1dg\x1eh\u{85}i\u{2028}j\u{2029}k";
        assert_eq!(
            py_splitlines(s),
            vec!["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"]
        );
    }

    #[test]
    fn test_py_splitlines_trailing_newline_no_empty_element() {
        // Python: "a\n".splitlines() == ["a"]  (no trailing empty string)
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
        assert_eq!(py_splitlines("a\r\n"), vec!["a"]);
        assert_eq!(py_splitlines("a\r"), vec!["a"]);
    }

    #[test]
    fn test_py_splitlines_leading_newline_empty_first_element() {
        // Python: "\na".splitlines() == ["", "a"]
        assert_eq!(py_splitlines("\na"), vec!["", "a"]);
        assert_eq!(py_splitlines("\r\na"), vec!["", "a"]);
    }

    #[test]
    fn test_py_splitlines_consecutive_newlines_produce_empty_lines() {
        // Python: "a\n\nb".splitlines() == ["a", "", "b"]
        assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(py_splitlines("\n\n"), vec!["", ""]);
    }

    #[test]
    fn test_py_splitlines_only_newlines() {
        // Python: "\n\n\n".splitlines() == ["", "", ""]
        assert_eq!(py_splitlines("\n\n\n"), vec!["", "", ""]);
    }

    #[test]
    fn test_py_splitlines_mixed_breaks_and_unicode_content() {
        // Multi-byte content between line breaks
        assert_eq!(
            py_splitlines("héllo\nwörld\r\n日本語"),
            vec!["héllo", "wörld", "日本語"]
        );
    }

    #[test]
    fn test_py_splitlines_crlf_at_end_not_doubled() {
        // "a\r\n" must give ["a"], not ["a", ""]
        assert_eq!(py_splitlines("a\r\n"), vec!["a"]);
    }

    #[test]
    fn test_py_splitlines_nel_unicode_85() {
        // U+0085 NEXT LINE is a Python line break
        assert_eq!(py_splitlines("line1\u{85}line2"), vec!["line1", "line2"]);
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
    fn test_py_splitlines_reconstruction_invariant() {
        // Every character from the original string must appear in exactly one
        // line produced by py_splitlines (full-coverage invariant).
        let inputs = [
            "hello\nworld",
            "a\r\nb\rc\x0bd",
            "\n\n",
            "no newlines here",
            "\u{2029}end",
        ];
        for s in &inputs {
            let lines = py_splitlines(s);
            // Characters in lines == characters in s minus the line-break chars.
            let line_chars: usize = lines.iter().map(|l| l.chars().count()).sum();
            let break_chars: usize = s.chars().filter(|c| is_line_break(*c)).count();
            assert_eq!(
                line_chars,
                s.chars().count() - break_chars,
                "reconstruction failed for {:?}",
                s
            );
        }
    }

    // -------------------------------------------------------------------------
    // py_is_space / py_strip
    // -------------------------------------------------------------------------

    #[test]
    fn test_py_is_space_standard_ascii_whitespace() {
        // Space, tab, newline, carriage-return, vertical-tab, form-feed
        for c in [' ', '\t', '\n', '\r', '\x0b', '\x0c'] {
            assert!(py_is_space(c), "expected is_space for {:?}", c);
        }
    }

    #[test]
    fn test_py_is_space_extended_c0_separators() {
        // Python's str.strip() also treats \x1c..\x1f as whitespace.
        for c in ['\x1c', '\x1d', '\x1e', '\x1f'] {
            assert!(py_is_space(c), "expected is_space for \\x{:02x}", c as u32);
        }
    }

    #[test]
    fn test_py_is_space_unicode_whitespace() {
        // U+00A0 NO-BREAK SPACE, U+2003 EM SPACE are Rust-whitespace
        assert!(py_is_space('\u{00A0}'));
        assert!(py_is_space('\u{2003}'));
    }

    #[test]
    fn test_py_is_space_normal_chars_are_not_space() {
        for c in ['a', 'Z', '0', '!', '\x1b', '\x20'] {
            // \x20 = ' ' (space) IS whitespace — let's only test non-space ones.
            if c != '\x20' {
                assert!(!py_is_space(c), "expected NOT is_space for {:?}", c);
            }
        }
        assert!(!py_is_space('a'));
        assert!(!py_is_space('Z'));
        assert!(!py_is_space('0'));
        assert!(!py_is_space('!'));
        assert!(!py_is_space('\x1b')); // ESC is NOT in the set
    }

    #[test]
    fn test_py_strip_empty_string() {
        assert_eq!(py_strip(""), "");
    }

    #[test]
    fn test_py_strip_only_whitespace_returns_empty() {
        assert_eq!(py_strip("   "), "");
        assert_eq!(py_strip("\t\n\r"), "");
        assert_eq!(py_strip("\x1c\x1d\x1e\x1f"), "");
    }

    #[test]
    fn test_py_strip_no_whitespace_unchanged() {
        assert_eq!(py_strip("hello"), "hello");
        assert_eq!(py_strip("héllo"), "héllo");
    }

    #[test]
    fn test_py_strip_leading_and_trailing_whitespace() {
        assert_eq!(py_strip("  hello  "), "hello");
        assert_eq!(py_strip("\t\nhello\r\n"), "hello");
    }

    #[test]
    fn test_py_strip_c0_separators_stripped() {
        // \x1c..\x1f at the edges must be stripped
        assert_eq!(py_strip("\x1chello\x1f"), "hello");
        assert_eq!(py_strip("\x1d\x1e content \x1c\x1f"), "content");
    }

    #[test]
    fn test_py_strip_internal_whitespace_preserved() {
        // strip only removes leading/trailing, not internal
        assert_eq!(py_strip("  a  b  "), "a  b");
    }

    // -------------------------------------------------------------------------
    // char_prefix / char_len
    // -------------------------------------------------------------------------

    #[test]
    fn char_prefix_is_chars_not_bytes() {
        assert_eq!(char_prefix("héllo", 2), "hé");
        assert_eq!(char_prefix("ab", 10), "ab");
    }

    #[test]
    fn test_char_prefix_ascii_string() {
        assert_eq!(char_prefix("hello", 3), "hel");
        assert_eq!(char_prefix("hello", 0), "");
        assert_eq!(char_prefix("hello", 5), "hello");
    }

    #[test]
    fn test_char_prefix_multibyte_accented_chars() {
        // 'é' is 2 bytes in UTF-8; char_prefix counts chars
        assert_eq!(char_prefix("héllo", 1), "h");
        assert_eq!(char_prefix("héllo", 2), "hé");
        assert_eq!(char_prefix("héllo", 3), "hél");
    }

    #[test]
    fn test_char_prefix_japanese_characters() {
        // Each CJK character is 3 bytes
        let s = "日本語";
        assert_eq!(char_prefix(s, 1), "日");
        assert_eq!(char_prefix(s, 2), "日本");
        assert_eq!(char_prefix(s, 3), "日本語");
    }

    #[test]
    fn test_char_prefix_emoji_four_byte_sequences() {
        // Emoji like 🎯 are 4 bytes (U+1F3AF)
        let s = "🎯🚀💡";
        assert_eq!(char_prefix(s, 1), "🎯");
        assert_eq!(char_prefix(s, 2), "🎯🚀");
        assert_eq!(char_prefix(s, 3), "🎯🚀💡");
    }

    #[test]
    fn test_char_prefix_exceeds_string_length_returns_full_string() {
        // When n > char_len(s), must return the whole string
        assert_eq!(char_prefix("hi", 100), "hi");
        assert_eq!(char_prefix("日本", 999), "日本");
        assert_eq!(char_prefix("", 5), "");
    }

    #[test]
    fn test_char_prefix_zero_returns_empty() {
        assert_eq!(char_prefix("hello", 0), "");
        assert_eq!(char_prefix("日本語", 0), "");
    }

    #[test]
    fn test_char_len_ascii() {
        assert_eq!(char_len(""), 0);
        assert_eq!(char_len("hello"), 5);
    }

    #[test]
    fn test_char_len_multibyte() {
        // 'é' = 2 bytes, but 1 char
        assert_eq!(char_len("héllo"), 5);
        // Each CJK char = 3 bytes, but 1 char
        assert_eq!(char_len("日本語"), 3);
    }

    #[test]
    fn test_char_len_emoji() {
        // 🎯 = 4 bytes, but 1 char
        assert_eq!(char_len("🎯🚀"), 2);
    }

    #[test]
    fn test_char_len_invariant_le_byte_len() {
        // char_len(s) <= s.len() always (chars never exceed bytes)
        let cases = ["hello", "héllo", "日本語", "🎯", "", "a\nb"];
        for s in &cases {
            assert!(
                char_len(s) <= s.len(),
                "char_len({:?})={} > byte_len={}",
                s,
                char_len(s),
                s.len()
            );
        }
    }

    // -------------------------------------------------------------------------
    // py_json_dumps_opts
    // -------------------------------------------------------------------------

    #[test]
    fn float_repr_matches_cpython() {
        for (f, want) in [
            (1e-7, "1e-07"),
            (0.00001, "1e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (123.456, "123.456"),
            (1.0, "1.0"),
            (-0.0, "-0.0"),
            (0.5, "0.5"),
            (3.14e100, "3.14e+100"),
            (-2.5e-7, "-2.5e-07"),
            (1.1534175185142759, "1.1534175185142759"),
            (9999999999999998.0, "9999999999999998.0"),
        ] {
            assert_eq!(py_float_repr(f), want, "repr({f})");
        }
    }

    #[test]
    fn json_dumps_default_formatting() {
        let v: serde_json::Value = serde_json::from_str(r#"{"a": [1, "é"], "b": null}"#).unwrap();
        assert_eq!(py_json_dumps(&v), "{\"a\": [1, \"\\u00e9\"], \"b\": null}");
    }

    #[test]
    fn test_py_json_dumps_opts_null() {
        assert_eq!(py_json_dumps_opts(&json!(null), false, true), "null");
        assert_eq!(py_json_dumps_opts(&json!(null), true, false), "null");
    }

    #[test]
    fn test_py_json_dumps_opts_booleans() {
        assert_eq!(py_json_dumps_opts(&json!(true), false, true), "true");
        assert_eq!(py_json_dumps_opts(&json!(false), false, true), "false");
    }

    #[test]
    fn test_py_json_dumps_opts_integer() {
        assert_eq!(py_json_dumps_opts(&json!(42), false, true), "42");
        assert_eq!(py_json_dumps_opts(&json!(-7), false, true), "-7");
        assert_eq!(py_json_dumps_opts(&json!(0), false, true), "0");
    }

    #[test]
    fn test_py_json_dumps_opts_empty_string() {
        assert_eq!(py_json_dumps_opts(&json!(""), false, true), "\"\"");
    }

    #[test]
    fn test_py_json_dumps_opts_ascii_string_unchanged() {
        assert_eq!(
            py_json_dumps_opts(&json!("hello"), false, true),
            "\"hello\""
        );
    }

    #[test]
    fn test_py_json_dumps_opts_ensure_ascii_true_escapes_non_ascii() {
        // é (U+00E9) must be escaped as \u00e9 when ensure_ascii=True
        assert_eq!(
            py_json_dumps_opts(&json!("café"), false, true),
            "\"caf\\u00e9\""
        );
    }

    #[test]
    fn test_py_json_dumps_opts_ensure_ascii_false_keeps_raw_utf8() {
        // é must remain literal when ensure_ascii=False
        assert_eq!(py_json_dumps_opts(&json!("café"), false, false), "\"café\"");
    }

    #[test]
    fn test_py_json_dumps_opts_astral_codepoint_surrogate_pair_ensure_ascii() {
        // 🎯 = U+1F3AF → surrogate pair \uD83C\uDFAF when ensure_ascii=True
        let got = py_json_dumps_opts(&json!("🎯"), false, true);
        assert_eq!(got, "\"\\ud83c\\udfaf\"");
    }

    #[test]
    fn test_py_json_dumps_opts_astral_codepoint_raw_when_not_ensure_ascii() {
        // 🎯 stays literal when ensure_ascii=False
        assert_eq!(py_json_dumps_opts(&json!("🎯"), false, false), "\"🎯\"");
    }

    #[test]
    fn test_py_json_dumps_opts_sort_keys_true_sorts_alphabetically() {
        // sort_keys=True: keys must appear in code-point order
        let v: serde_json::Value = serde_json::from_str(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        let got = py_json_dumps_opts(&v, true, true);
        assert_eq!(got, "{\"a\": 2, \"m\": 3, \"z\": 1}");
    }

    #[test]
    fn test_py_json_dumps_opts_sort_keys_false_preserves_insertion_order() {
        // sort_keys=False: original insertion order kept
        // Note: serde_json with preserve_order retains insertion order.
        let v: serde_json::Value = serde_json::from_str(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        let got = py_json_dumps_opts(&v, false, true);
        assert_eq!(got, "{\"z\": 1, \"a\": 2, \"m\": 3}");
    }

    #[test]
    fn test_py_json_dumps_opts_empty_object() {
        assert_eq!(py_json_dumps_opts(&json!({}), false, true), "{}");
        assert_eq!(py_json_dumps_opts(&json!({}), true, true), "{}");
    }

    #[test]
    fn test_py_json_dumps_opts_empty_array() {
        assert_eq!(py_json_dumps_opts(&json!([]), false, true), "[]");
    }

    #[test]
    fn test_py_json_dumps_opts_nested_structure_sort_keys() {
        // sort_keys must propagate recursively into nested objects
        let v: serde_json::Value =
            serde_json::from_str(r#"{"b": {"z": 1, "a": 2}, "a": 3}"#).unwrap();
        let got = py_json_dumps_opts(&v, true, true);
        assert_eq!(got, "{\"a\": 3, \"b\": {\"a\": 2, \"z\": 1}}");
    }

    #[test]
    fn test_py_json_dumps_opts_control_chars_escaped() {
        // Control chars < 0x20 must be \uXXXX escaped (except \n, \r, \t, etc.)
        let v = json!("\u{0001}");
        let got = py_json_dumps_opts(&v, false, true);
        assert_eq!(got, "\"\\u0001\"");
    }

    #[test]
    fn test_py_json_dumps_opts_special_json_escapes() {
        // \" → \\\", \\ → \\\\, \n → \\n, \r → \\r, \t → \\t
        let v = json!("\"\\\n\r\t");
        let got = py_json_dumps_opts(&v, false, true);
        assert_eq!(got, "\"\\\"\\\\\\n\\r\\t\"");
    }

    #[test]
    fn test_py_json_dumps_opts_array_with_mixed_types() {
        let v = json!([1, "café", null, true, false]);
        // ensure_ascii=True: é escaped
        let got = py_json_dumps_opts(&v, false, true);
        assert_eq!(got, "[1, \"caf\\u00e9\", null, true, false]");
        // ensure_ascii=False: é raw
        let got2 = py_json_dumps_opts(&v, false, false);
        assert_eq!(got2, "[1, \"café\", null, true, false]");
    }

    #[test]
    fn test_py_json_dumps_opts_chinese_characters() {
        // CJK characters (3-byte UTF-8, BMP) — U+4E2D (中), U+6587 (文)
        let v = json!("中文");
        // ensure_ascii=True: both must be \uXXXX
        let got = py_json_dumps_opts(&v, false, true);
        assert_eq!(got, "\"\\u4e2d\\u6587\"");
        // ensure_ascii=False: raw
        let got2 = py_json_dumps_opts(&v, false, false);
        assert_eq!(got2, "\"中文\"");
    }
}
