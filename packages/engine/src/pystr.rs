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
    fn char_prefix_is_chars_not_bytes() {
        assert_eq!(char_prefix("héllo", 2), "hé");
        assert_eq!(char_prefix("ab", 10), "ab");
    }

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

    // ---- py_splitlines ----

    #[test]
    fn test_splitlines_all_eleven_breaks_individually() {
        // Every character is_line_break() recognizes, exercised on its own.
        let breaks: &[(char, &str)] = &[
            ('\n', "LF"),
            ('\r', "CR"),
            ('\x0b', "VT"),
            ('\x0c', "FF"),
            ('\x1c', "FS"),
            ('\x1d', "GS"),
            ('\x1e', "RS"),
            ('\u{85}', "NEL"),
            ('\u{2028}', "LS"),
            ('\u{2029}', "PS"),
        ];
        for (c, name) in breaks {
            let s = format!("a{}b", c);
            assert_eq!(
                py_splitlines(&s),
                vec!["a", "b"],
                "failed for break char {} ({:?})",
                name,
                c
            );
        }
    }

    #[test]
    fn test_splitlines_crlf_is_single_break_not_two() {
        // \r\n must be consumed together; a naive per-char split would produce
        // an extra empty element between \r and \n.
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
        // A lone \r\n, like a lone \n (see test_splitlines_only_a_single_break_char),
        // still yields one empty line — it is not the same as a *trailing*
        // break after content, which yields no trailing empty element.
        assert_eq!(py_splitlines("\r\n"), vec![""]);
        assert_eq!(py_splitlines("a\r\n"), vec!["a"]);
    }

    #[test]
    fn test_splitlines_bare_cr_not_followed_by_lf() {
        // A lone \r (not part of \r\n) is still its own break.
        assert_eq!(py_splitlines("a\rb"), vec!["a", "b"]);
        // \r followed by something other than \n: \r breaks, \n (if present later) breaks separately.
        assert_eq!(py_splitlines("a\r\rb"), vec!["a", "", "b"]);
    }

    #[test]
    fn test_splitlines_consecutive_breaks_produce_empty_lines() {
        assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(py_splitlines("\n\n\n"), vec!["", "", ""]);
        assert_eq!(py_splitlines("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn test_splitlines_leading_and_trailing_breaks() {
        // Leading break creates a leading empty line.
        assert_eq!(py_splitlines("\na"), vec!["", "a"]);
        // Trailing break does NOT create a trailing empty line (matches Python).
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
        assert_eq!(py_splitlines("\na\n"), vec!["", "a"]);
    }

    #[test]
    fn test_splitlines_no_breaks_returns_whole_string() {
        assert_eq!(py_splitlines("hello world"), vec!["hello world"]);
    }

    #[test]
    fn test_splitlines_empty_string_returns_empty_vec() {
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
    }

    #[test]
    fn test_splitlines_only_a_single_break_char() {
        assert_eq!(py_splitlines("\n"), vec![""]);
        assert_eq!(py_splitlines("\u{2028}"), vec![""]);
    }

    #[test]
    fn test_splitlines_mixed_break_types_in_sequence() {
        assert_eq!(
            py_splitlines("a\r\nb\nc\rd\u{85}e\u{2028}f\u{2029}g\x0bh\x0ci\x1cj\x1dk\x1el"),
            vec!["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"]
        );
    }

    // ---- py_strip / py_is_space / py_has_content / py_split_ws ----

    #[test]
    fn test_is_space_recognizes_extended_ascii_separators() {
        for c in ['\x1c', '\x1d', '\x1e', '\x1f'] {
            assert!(py_is_space(c), "expected {:#x} to be whitespace", c as u32);
        }
        // Neighboring non-whitespace control chars should NOT be treated as space.
        assert!(!py_is_space('\x1b'));
    }

    #[test]
    fn test_is_space_recognizes_unicode_whitespace() {
        assert!(py_is_space(' '));
        assert!(py_is_space('\t'));
        assert!(py_is_space('\u{00A0}')); // non-breaking space
        assert!(py_is_space('\u{2003}')); // em space
        assert!(!py_is_space('a'));
        assert!(!py_is_space('0'));
    }

    #[test]
    fn test_strip_empty_string() {
        assert_eq!(py_strip(""), "");
    }

    #[test]
    fn test_strip_only_whitespace_returns_empty() {
        assert_eq!(py_strip("   \t\n\x1c\x1f  "), "");
    }

    #[test]
    fn test_strip_no_whitespace_is_unchanged() {
        assert_eq!(py_strip("hello"), "hello");
    }

    #[test]
    fn test_strip_removes_only_leading_and_trailing() {
        assert_eq!(py_strip("  hello world  "), "hello world");
        assert_eq!(py_strip("\x1chello\x1d"), "hello");
        // Internal whitespace preserved.
        assert_eq!(py_strip("  a  b  "), "a  b");
    }

    #[test]
    fn test_has_content_edge_cases() {
        assert!(!py_has_content(""));
        assert!(!py_has_content("   "));
        assert!(!py_has_content("\x1c\x1d\x1e\x1f"));
        assert!(py_has_content(" a "));
        assert!(py_has_content("x"));
    }

    #[test]
    fn test_split_ws_collapses_runs_and_drops_empties() {
        assert_eq!(py_split_ws("  a   b\tc\n\nd  "), vec!["a", "b", "c", "d"]);
        assert_eq!(py_split_ws(""), Vec::<&str>::new());
        assert_eq!(py_split_ws("   "), Vec::<&str>::new());
        assert_eq!(py_split_ws("a\x1cb\x1fc"), vec!["a", "b", "c"]);
    }

    // ---- char_prefix / char_len ----

    #[test]
    fn test_char_len_counts_chars_not_bytes() {
        assert_eq!(char_len(""), 0);
        assert_eq!(char_len("abc"), 3);
        assert_eq!(char_len("héllo"), 5); // é is 2 bytes, 1 char
        assert_eq!(char_len("日本語"), 3); // each is 3 bytes
        assert_eq!(char_len("😀😀"), 2); // each emoji is 4 bytes, 1 char
    }

    #[test]
    fn test_char_prefix_multibyte_accents() {
        assert_eq!(char_prefix("héllo", 0), "");
        assert_eq!(char_prefix("héllo", 1), "h");
        assert_eq!(char_prefix("héllo", 2), "hé");
        assert_eq!(char_prefix("héllo", 5), "héllo");
    }

    #[test]
    fn test_char_prefix_cjk() {
        let s = "日本語のテスト";
        assert_eq!(char_prefix(s, 0), "");
        assert_eq!(char_prefix(s, 1), "日");
        assert_eq!(char_prefix(s, 3), "日本語");
        // Full length.
        assert_eq!(char_prefix(s, char_len(s)), s);
    }

    #[test]
    fn test_char_prefix_emoji_astral_chars() {
        // Emoji outside the BMP (astral plane) are a single Rust `char` each,
        // even though Python/JSON encode them as UTF-16 surrogate pairs.
        let s = "a😀b🎉c";
        assert_eq!(char_len(s), 5);
        assert_eq!(char_prefix(s, 1), "a");
        assert_eq!(char_prefix(s, 2), "a😀");
        assert_eq!(char_prefix(s, 3), "a😀b");
        assert_eq!(char_prefix(s, 5), s);
    }

    #[test]
    fn test_char_prefix_n_exceeds_char_len_returns_whole_string() {
        assert_eq!(char_prefix("ab", 10), "ab");
        assert_eq!(char_prefix("héllo", 1000), "héllo");
        assert_eq!(char_prefix("", 5), "");
        assert_eq!(char_prefix("日本語", 100), "日本語");
    }

    #[test]
    fn test_char_prefix_n_equals_zero() {
        assert_eq!(char_prefix("anything", 0), "");
        assert_eq!(char_prefix("", 0), "");
    }

    // ---- py_json_dumps_opts: sort_keys / ensure_ascii ----

    #[test]
    fn test_json_dumps_sort_keys_true_orders_by_codepoint() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        assert_eq!(
            py_json_dumps_opts(&v, true, true),
            r#"{"a": 2, "m": 3, "z": 1}"#
        );
    }

    #[test]
    fn test_json_dumps_sort_keys_false_preserves_insertion_order() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        assert_eq!(
            py_json_dumps_opts(&v, false, true),
            r#"{"z": 1, "a": 2, "m": 3}"#
        );
    }

    #[test]
    fn test_json_dumps_ensure_ascii_true_escapes_non_ascii() {
        let v = serde_json::Value::String("café".to_string());
        assert_eq!(py_json_dumps_opts(&v, false, true), "\"caf\\u00e9\"");
    }

    #[test]
    fn test_json_dumps_ensure_ascii_false_keeps_raw_utf8() {
        let v = serde_json::Value::String("café".to_string());
        assert_eq!(py_json_dumps_opts(&v, false, false), "\"café\"");
    }

    #[test]
    fn test_json_dumps_ensure_ascii_true_surrogate_pair_for_astral_chars() {
        // 😀 = U+1F600, outside the BMP: Python encodes it as a UTF-16 surrogate pair.
        let v = serde_json::Value::String("😀".to_string());
        assert_eq!(py_json_dumps_opts(&v, false, true), "\"\\ud83d\\ude00\"");
    }

    #[test]
    fn test_json_dumps_ensure_ascii_false_keeps_astral_char_raw() {
        let v = serde_json::Value::String("😀".to_string());
        assert_eq!(py_json_dumps_opts(&v, false, false), "\"😀\"");
    }

    #[test]
    fn test_json_dumps_control_chars_use_short_escapes() {
        let v = serde_json::Value::String("a\nb\rc\td\x08e\x0cf".to_string());
        assert_eq!(
            py_json_dumps_opts(&v, false, true),
            r#""a\nb\rc\td\be\ff""#
        );
    }

    #[test]
    fn test_json_dumps_control_char_without_short_escape_uses_uxxxx() {
        // \x01 has no dedicated short escape in JSON; must be \u0001.
        let v = serde_json::Value::String("\x01".to_string());
        assert_eq!(py_json_dumps_opts(&v, false, true), r#""\u0001""#);
    }

    #[test]
    fn test_json_dumps_nested_structure_sort_keys_and_ascii_combined() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"b": {"y": "é", "x": 1}, "a": [3, 2, 1]}"#).unwrap();
        assert_eq!(
            py_json_dumps_opts(&v, true, true),
            r#"{"a": [3, 2, 1], "b": {"x": 1, "y": "\u00e9"}}"#
        );
        assert_eq!(
            py_json_dumps_opts(&v, true, false),
            r#"{"a": [3, 2, 1], "b": {"x": 1, "y": "é"}}"#
        );
    }

    #[test]
    fn test_json_dumps_empty_object_and_array() {
        let v: serde_json::Value = serde_json::from_str(r#"{"a": [], "b": {}}"#).unwrap();
        assert_eq!(py_json_dumps_opts(&v, true, true), r#"{"a": [], "b": {}}"#);
    }

    #[test]
    fn test_json_dumps_default_wrapper_matches_ensure_ascii_true_unsorted() {
        // py_json_dumps() == py_json_dumps_opts(v, sort_keys=false, ensure_ascii=true)
        let v: serde_json::Value = serde_json::from_str(r#"{"z": "日", "a": 1}"#).unwrap();
        assert_eq!(py_json_dumps(&v), py_json_dumps_opts(&v, false, true));
        assert_eq!(py_json_dumps(&v), r#"{"z": "\u65e5", "a": 1}"#);
    }
}