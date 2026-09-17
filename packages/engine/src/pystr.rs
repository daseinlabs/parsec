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
    fn string_helpers() {
        assert!(py_is_space(' '));
        assert!(py_is_space('\t'));
        assert!(py_is_space('\n'));
        assert!(py_is_space('\x1c'));
        assert!(!py_is_space('a'));

        assert_eq!(py_strip("  hello \n"), "hello");
        assert_eq!(py_strip("abc"), "abc");

        assert!(py_has_content("  abc  "));
        assert!(!py_has_content("   \t\n "));

        assert_eq!(py_split_ws("  foo   bar\nbaz\t"), vec!["foo", "bar", "baz"]);
        assert_eq!(char_len("héllo"), 5);
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

    // -----------------------------------------------------------------------
    // py_splitlines — all 11 Python line-break characters (exhaustive)
    // -----------------------------------------------------------------------

    #[test]
    fn test_py_splitlines_all_11_line_break_chars() {
        assert_eq!(py_splitlines("a\nb"), vec!["a", "b"], "LF");
        assert_eq!(py_splitlines("a\rb"), vec!["a", "b"], "CR standalone");
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"], "CRLF");
        assert_eq!(py_splitlines("a\x0bb"), vec!["a", "b"], "VT");
        assert_eq!(py_splitlines("a\x0cb"), vec!["a", "b"], "FF");
        assert_eq!(py_splitlines("a\x1cb"), vec!["a", "b"], "FS");
        assert_eq!(py_splitlines("a\x1db"), vec!["a", "b"], "GS");
        assert_eq!(py_splitlines("a\x1eb"), vec!["a", "b"], "RS");
        assert_eq!(py_splitlines("a\u{85}b"), vec!["a", "b"], "NEL");
        assert_eq!(py_splitlines("a\u{2028}b"), vec!["a", "b"], "LS");
        assert_eq!(py_splitlines("a\u{2029}b"), vec!["a", "b"], "PS");
    }

    #[test]
    fn test_py_splitlines_crlf_no_phantom_line() {
        let result = py_splitlines("a\r\nb");
        assert_eq!(
            result,
            vec!["a", "b"],
            "\\r\\n must be a single line break, got {result:?}"
        );
        // Two consecutive CRLF sequences produce an empty middle line
        assert_eq!(
            py_splitlines("x\r\n\r\ny"),
            vec!["x", "", "y"],
            "two CRLF sequences produce an empty middle line"
        );
    }

    #[test]
    fn test_py_splitlines_consecutive_newlines() {
        assert_eq!(
            py_splitlines("a\n\nb"),
            vec!["a", "", "b"],
            "double LF produces empty middle line"
        );
        assert_eq!(
            py_splitlines("\n\n\n"),
            vec!["", "", ""],
            "three LFs produce three empty strings"
        );
    }

    #[test]
    fn test_py_splitlines_trailing_newline_no_empty_tail() {
        assert_eq!(
            py_splitlines("hello\n"),
            vec!["hello"],
            "trailing \\n must not create empty trailing element"
        );
        assert_eq!(
            py_splitlines("a\r\n"),
            vec!["a"],
            "trailing CRLF must not create empty trailing element"
        );
        assert_eq!(
            py_splitlines("a\u{2028}"),
            vec!["a"],
            "trailing LS must not create empty trailing element"
        );
    }

    #[test]
    fn test_py_splitlines_leading_newline() {
        assert_eq!(
            py_splitlines("\nhello"),
            vec!["", "hello"],
            "leading \\n produces empty first element"
        );
        assert_eq!(
            py_splitlines("\r\nhello"),
            vec!["", "hello"],
            "leading CRLF produces empty first element"
        );
    }

    #[test]
    fn test_py_splitlines_empty_string() {
        assert_eq!(
            py_splitlines(""),
            Vec::<&str>::new(),
            "empty string must return empty vec"
        );
    }

    #[test]
    fn test_py_splitlines_no_break_returns_single_element() {
        assert_eq!(py_splitlines("hello world"), vec!["hello world"]);
    }

    // -----------------------------------------------------------------------
    // py_is_space / py_strip
    // -----------------------------------------------------------------------

    #[test]
    fn test_py_is_space_includes_c0_separators() {
        for c in ['\x1c', '\x1d', '\x1e', '\x1f'] {
            assert!(
                py_is_space(c),
                "\\x{:02x} must be classified as Python whitespace",
                c as u32
            );
        }
    }

    #[test]
    fn test_py_is_space_standard_whitespace() {
        for c in [' ', '\t', '\n', '\r', '\x0b', '\x0c'] {
            assert!(
                py_is_space(c),
                "'{}' must be Python whitespace",
                c.escape_default()
            );
        }
    }

    #[test]
    fn test_py_is_space_non_whitespace() {
        for c in ['a', 'Z', '0', '!', 'é', '中'] {
            assert!(!py_is_space(c), "'{c}' must NOT be Python whitespace");
        }
    }

    #[test]
    fn test_py_strip_empty_string() {
        assert_eq!(py_strip(""), "");
    }

    #[test]
    fn test_py_strip_all_whitespace_returns_empty() {
        assert_eq!(py_strip("   "), "", "spaces");
        assert_eq!(py_strip("\t\n\r"), "", "common escapes");
        assert_eq!(py_strip("\x1c\x1d\x1e\x1f"), "", "C0 separators");
        assert_eq!(py_strip("  \x1c  \x1f  "), "", "mixed whitespace");
    }

    #[test]
    fn test_py_strip_no_whitespace_unchanged() {
        assert_eq!(py_strip("hello"), "hello");
        assert_eq!(py_strip("abc123"), "abc123");
    }

    #[test]
    fn test_py_strip_trims_both_ends() {
        assert_eq!(py_strip("  hello  "), "hello");
        assert_eq!(py_strip("\x1chello\x1f"), "hello");
        assert_eq!(py_strip("\n\r  hello world  \t\n"), "hello world");
    }

    #[test]
    fn test_py_strip_preserves_interior_whitespace() {
        assert_eq!(py_strip("  a b  "), "a b");
        assert_eq!(py_strip("\x1ca\x1cb\x1c"), "a\x1cb");
    }

    // -----------------------------------------------------------------------
    // char_prefix / char_len
    // -----------------------------------------------------------------------

    #[test]
    fn test_char_prefix_ascii() {
        assert_eq!(char_prefix("hello", 3), "hel");
        assert_eq!(char_prefix("hello", 0), "");
        assert_eq!(char_prefix("hello", 5), "hello");
    }

    #[test]
    fn test_char_prefix_accented_latin() {
        // 'é' is 2 bytes (U+00E9)
        assert_eq!(char_prefix("héllo", 1), "h");
        assert_eq!(char_prefix("héllo", 2), "hé");
        assert_eq!(char_prefix("héllo", 3), "hél");
    }

    #[test]
    fn test_char_prefix_cjk() {
        // Each CJK char is 3 bytes
        assert_eq!(char_prefix("日本語", 1), "日");
        assert_eq!(char_prefix("日本語", 2), "日本");
        assert_eq!(char_prefix("日本語", 3), "日本語");
    }

    #[test]
    fn test_char_prefix_emoji_four_byte() {
        // U+1F600 😀 is 4 bytes; it is ONE char
        let s = "😀😁😂";
        assert_eq!(char_prefix(s, 1), "😀");
        assert_eq!(char_prefix(s, 2), "😀😁");
    }

    #[test]
    fn test_char_prefix_n_exceeds_char_count_returns_full_string() {
        assert_eq!(char_prefix("hi", 100), "hi", "ASCII: n beyond length");
        assert_eq!(char_prefix("日本語", 10), "日本語", "CJK: n beyond length");
        assert_eq!(char_prefix("😀", 5), "😀", "emoji: n beyond length");
        assert_eq!(char_prefix("", 1), "", "empty: n beyond length");
    }

    #[test]
    fn test_char_len_counts_chars_not_bytes() {
        assert_eq!(char_len("hello"), 5);
        assert_eq!(char_len("é"), 1, "'é' is 2 bytes but 1 char");
        assert_eq!(char_len("héllo"), 5);
        assert_eq!(char_len("日本語"), 3, "each CJK is 3 bytes");
        assert_eq!(char_len("😀"), 1, "😀 is 4 bytes but 1 char");
    }

    #[test]
    fn test_char_len_never_exceeds_byte_len() {
        for s in ["", "hello", "héllo", "日本語", "😀😁", "a😀b"] {
            assert!(
                char_len(s) <= s.len(),
                "char_len({s:?})={} > byte_len={}",
                char_len(s),
                s.len()
            );
        }
    }

    #[test]
    fn test_char_len_empty_string_is_zero() {
        assert_eq!(char_len(""), 0);
    }

    // -----------------------------------------------------------------------
    // py_json_dumps_opts — all 4 sort_keys × ensure_ascii combinations
    // -----------------------------------------------------------------------

    #[test]
    fn test_py_json_dumps_opts_no_sort_ascii_true() {
        let v: serde_json::Value = serde_json::from_str(r#"{"b": 1, "a": 2}"#).unwrap();
        let s = py_json_dumps_opts(&v, false, true);
        assert!(
            s.starts_with("{\"b\""),
            "unsorted: b should appear first, got {s:?}"
        );
        let v2 = serde_json::json!({"k": "é"});
        let s2 = py_json_dumps_opts(&v2, false, true);
        assert!(
            s2.contains("\\u00e9"),
            "ensure_ascii=true must escape é as \\u00e9, got {s2:?}"
        );
    }

    #[test]
    fn test_py_json_dumps_opts_sort_ascii_true() {
        let v: serde_json::Value = serde_json::from_str(r#"{"b": 1, "a": 2}"#).unwrap();
        let s = py_json_dumps_opts(&v, true, true);
        assert!(
            s.starts_with("{\"a\""),
            "sorted: a should appear first, got {s:?}"
        );
        let v2 = serde_json::json!({"k": "é"});
        let s2 = py_json_dumps_opts(&v2, true, true);
        assert!(
            s2.contains("\\u00e9"),
            "ensure_ascii=true must escape é, got {s2:?}"
        );
    }

    #[test]
    fn test_py_json_dumps_opts_no_sort_ascii_false() {
        let v: serde_json::Value = serde_json::from_str(r#"{"b": 1, "a": 2}"#).unwrap();
        let s = py_json_dumps_opts(&v, false, false);
        assert!(
            s.starts_with("{\"b\""),
            "unsorted: b should appear first, got {s:?}"
        );
        let v2 = serde_json::json!({"k": "é"});
        let s2 = py_json_dumps_opts(&v2, false, false);
        assert!(
            s2.contains('é'),
            "ensure_ascii=false must emit é raw, got {s2:?}"
        );
        assert!(
            !s2.contains("\\u00e9"),
            "ensure_ascii=false must NOT escape é, got {s2:?}"
        );
    }

    #[test]
    fn test_py_json_dumps_opts_sort_ascii_false() {
        let v: serde_json::Value = serde_json::from_str(r#"{"b": 1, "a": 2}"#).unwrap();
        let s = py_json_dumps_opts(&v, true, false);
        assert!(
            s.starts_with("{\"a\""),
            "sorted: a should appear first, got {s:?}"
        );
        let v2 = serde_json::json!({"k": "日"});
        let s2 = py_json_dumps_opts(&v2, true, false);
        assert!(
            s2.contains('日'),
            "ensure_ascii=false must emit 日 raw, got {s2:?}"
        );
    }

    #[test]
    fn test_py_json_dumps_opts_astral_char_surrogate_pair_encoding() {
        // U+1F600 😀 → surrogate pair \uD83D\uDE00
        let v = serde_json::json!({"emoji": "😀"});
        let s = py_json_dumps_opts(&v, false, true);
        assert!(
            s.contains("\\ud83d\\ude00"),
            "astral char must be encoded as surrogate pair under ensure_ascii=true, got {s:?}"
        );
    }

    #[test]
    fn test_py_json_dumps_opts_astral_char_raw_when_ascii_false() {
        let v = serde_json::json!({"emoji": "😀"});
        let s = py_json_dumps_opts(&v, false, false);
        assert!(
            s.contains('😀'),
            "ensure_ascii=false must emit astral char raw, got {s:?}"
        );
    }
}
