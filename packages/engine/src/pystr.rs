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

    // ── existing baseline tests ──────────────────────────────────────────────

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
        assert_eq!(char_prefix("h\u{e9}llo", 2), "h\u{e9}");
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
        let v: serde_json::Value =
            serde_json::from_str(r#"{"a": [1, "\u00e9"], "b": null}"#).unwrap();
        assert_eq!(py_json_dumps(&v), "{\"a\": [1, \"\\u00e9\"], \"b\": null}");
    }

    // ── py_splitlines edge cases ─────────────────────────────────────────────

    /// All eleven Python line-break characters individually produce a split.
    #[test]
    fn test_py_splitlines_all_eleven_line_break_chars() {
        let breaks: &[char] = &[
            '\n', '\r', '\x0b',     // vertical tab
            '\x0c',     // form feed
            '\x1c',     // file separator
            '\x1d',     // group separator
            '\x1e',     // record separator
            '\u{85}',   // NEL
            '\u{2028}', // line separator
            '\u{2029}', // paragraph separator
        ];
        for &lb in breaks {
            let mut s = String::from("a");
            s.push(lb);
            s.push('b');
            assert_eq!(
                py_splitlines(&s),
                vec!["a", "b"],
                "line break char U+{:04X} did not split",
                lb as u32
            );
        }
    }

    /// \r\n must be consumed as exactly one break, not two.
    #[test]
    fn test_py_splitlines_crlf_consumed_as_one() {
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\r\nb\r\nc"), vec!["a", "b", "c"]);
        // CR not followed by LF is its own single break
        assert_eq!(py_splitlines("a\rb"), vec!["a", "b"]);
    }

    /// Consecutive newlines preserve intermediate empty lines.
    #[test]
    fn test_py_splitlines_consecutive_newlines() {
        assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(py_splitlines("a\n\n\nb"), vec!["a", "", "", "b"]);
    }

    /// A leading newline produces an empty-string first element.
    #[test]
    fn test_py_splitlines_newline_at_start() {
        assert_eq!(py_splitlines("\na"), vec!["", "a"]);
        assert_eq!(py_splitlines("\n\na"), vec!["", "", "a"]);
    }

    /// A trailing newline does NOT produce a trailing empty element.
    /// This is the key Python `str.splitlines()` vs Rust `str::lines()` difference.
    #[test]
    fn test_py_splitlines_no_trailing_empty_on_trailing_newline() {
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
        assert_eq!(py_splitlines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\r\n"), vec!["a"]);
        assert_eq!(py_splitlines("a\r\nb\r\n"), vec!["a", "b"]);
    }

    /// A string of only newlines: n newlines -> n empty strings.
    #[test]
    fn test_py_splitlines_only_newlines() {
        assert_eq!(py_splitlines("\n\n\n"), vec!["", "", ""]);
        assert_eq!(py_splitlines("\n"), vec![""]);
    }

    // ── py_is_space / py_strip ───────────────────────────────────────────────

    /// Python's whitespace set includes \x1c..\x1f (C0 separators); Rust's does not.
    #[test]
    fn test_py_is_space_python_whitespace_set() {
        // Standard whitespace
        assert!(py_is_space(' '));
        assert!(py_is_space('\t'));
        assert!(py_is_space('\n'));
        assert!(py_is_space('\r'));
        // Python-specific C0 separators
        assert!(py_is_space('\x1c'));
        assert!(py_is_space('\x1d'));
        assert!(py_is_space('\x1e'));
        assert!(py_is_space('\x1f'));
        // Non-whitespace
        assert!(!py_is_space('a'));
        assert!(!py_is_space('0'));
        assert!(!py_is_space('\x1b')); // ESC - NOT in the Python set
    }

    /// py_strip removes \x1c..\x1f from edges (not stripped by Rust's trim()).
    #[test]
    fn test_py_strip_trims_ascii_file_separators() {
        assert_eq!(py_strip("\x1chello\x1e"), "hello");
        assert_eq!(py_strip("\x1f\x1d text \x1c\x1f"), "text");
    }

    /// Empty string and all-whitespace strings strip to empty.
    #[test]
    fn test_py_strip_empty_and_all_whitespace() {
        assert_eq!(py_strip(""), "");
        assert_eq!(py_strip("   "), "");
        assert_eq!(py_strip("\t\n\r"), "");
        assert_eq!(py_strip("\x1c\x1d\x1e\x1f"), "");
    }

    /// A string with no leading/trailing whitespace is unchanged.
    #[test]
    fn test_py_strip_no_whitespace() {
        assert_eq!(py_strip("hello"), "hello");
        assert_eq!(py_strip("hello world"), "hello world");
    }

    // ── char_prefix / char_len ───────────────────────────────────────────────

    /// Multi-byte UTF-8: each character counts as 1, regardless of byte width.
    #[test]
    fn test_char_prefix_multibyte_utf8() {
        // "\u{65e5}\u{672c}\u{8a9e}" (Japanese: Nihongo) is 3 chars, 9 bytes
        let s = "\u{65e5}\u{672c}\u{8a9e}";
        assert_eq!(char_prefix(s, 1), "\u{65e5}");
        assert_eq!(char_prefix(s, 2), "\u{65e5}\u{672c}");
        assert_eq!(char_prefix(s, 3), s);
        // Accented characters (2-byte UTF-8)
        assert_eq!(char_prefix("h\u{e9}llo", 3), "h\u{e9}l");
        // Emoji (4-byte UTF-8, astral plane)
        assert_eq!(
            char_prefix("\u{1f600}\u{1f601}\u{1f602}", 2),
            "\u{1f600}\u{1f601}"
        );
    }

    /// When n exceeds string character length, the whole string is returned.
    #[test]
    fn test_char_prefix_exceeds_length() {
        assert_eq!(char_prefix("abc", 100), "abc");
        assert_eq!(char_prefix("", 5), "");
        assert_eq!(
            char_prefix("\u{65e5}\u{672c}\u{8a9e}", 99),
            "\u{65e5}\u{672c}\u{8a9e}"
        );
    }

    /// char_prefix with n=0 returns an empty string.
    #[test]
    fn test_char_prefix_zero() {
        assert_eq!(char_prefix("hello", 0), "");
        assert_eq!(char_prefix("", 0), "");
    }

    /// char_len counts Unicode scalar values, not bytes.
    #[test]
    fn test_char_len_multibyte() {
        assert_eq!(char_len("\u{65e5}\u{672c}"), 2); // 6 bytes, 2 chars
        assert_eq!(char_len("h\u{e9}llo"), 5); // 6 bytes, 5 chars
        assert_eq!(char_len(""), 0);
        assert_eq!(char_len("abc"), 3);
    }

    /// Astral-plane emoji count as 1 char each (not 2 UTF-16 code units).
    #[test]
    fn test_char_len_emoji() {
        assert_eq!(char_len("\u{1f600}"), 1); // U+1F600, 4 bytes
        assert_eq!(char_len("\u{1f600}\u{1f601}\u{1f602}"), 3);
    }

    // ── py_json_dumps_opts ───────────────────────────────────────────────────

    /// sort_keys=true: keys appear in lexicographic (code-point) order.
    #[test]
    fn test_py_json_dumps_opts_sort_keys_true() {
        let v: serde_json::Value = serde_json::from_str(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        let result = py_json_dumps_opts(&v, true, false);
        assert_eq!(result, r#"{"a": 2, "m": 3, "z": 1}"#);
    }

    /// sort_keys=false: insertion order preserved (preserve_order feature).
    #[test]
    fn test_py_json_dumps_opts_sort_keys_false() {
        let v: serde_json::Value = serde_json::from_str(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        let result = py_json_dumps_opts(&v, false, false);
        assert_eq!(result, r#"{"z": 1, "a": 2, "m": 3}"#);
    }

    /// ensure_ascii=true: non-ASCII chars escaped as \uXXXX.
    #[test]
    fn test_py_json_dumps_opts_ensure_ascii_true() {
        // U+00E9 = e with acute
        let v = serde_json::Value::String("caf\u{e9}".to_string());
        let result = py_json_dumps_opts(&v, false, true);
        assert_eq!(result, r#""caf\u00e9""#);
    }

    /// ensure_ascii=false: non-ASCII chars appear as raw UTF-8.
    #[test]
    fn test_py_json_dumps_opts_ensure_ascii_false() {
        let v = serde_json::Value::String("caf\u{e9}".to_string());
        let result = py_json_dumps_opts(&v, false, false);
        // U+00E9 should appear as raw UTF-8 bytes
        assert_eq!(result, "\"caf\u{e9}\"");
    }

    /// Astral-plane chars are encoded as UTF-16 surrogate pairs when ensure_ascii=true.
    /// This matches Python's json.dumps behavior exactly.
    #[test]
    fn test_py_json_dumps_opts_astral_surrogate_pairs() {
        // U+1F600 (grinning face emoji) -> surrogate pair \uD83D\uDE00
        let v = serde_json::Value::String("\u{1f600}".to_string());
        let result = py_json_dumps_opts(&v, false, true);
        assert_eq!(result, r#""\ud83d\ude00""#);
    }

    /// Astral chars pass through unescaped when ensure_ascii=false.
    #[test]
    fn test_py_json_dumps_opts_astral_ensure_ascii_false() {
        let v = serde_json::Value::String("\u{1f600}".to_string());
        let result = py_json_dumps_opts(&v, false, false);
        assert_eq!(result, "\"\u{1f600}\"");
    }

    /// Control characters below 0x20 (excluding named escapes) -> \u00XX.
    #[test]
    fn test_py_json_dumps_control_chars() {
        // \x01 (SOH), \x1b (ESC)
        let v = serde_json::Value::String("\x01\x1b".to_string());
        let result = py_json_dumps_opts(&v, false, false);
        assert_eq!(result, r#""\u0001\u001b""#);
    }

    /// Named special escapes: `"` `\` `\n` `\r` `\t` `\b` `\f`.
    #[test]
    fn test_py_json_dumps_special_escapes() {
        let cases: &[(&str, &str)] = &[
            ("\"", r#""\"""#),
            ("\\", r#""\\""#),
            ("\n", r#""\n""#),
            ("\r", r#""\r""#),
            ("\t", r#""\t""#),
            ("\x08", r#""\b""#),
            ("\x0c", r#""\f""#),
        ];
        for (input, expected) in cases {
            let v = serde_json::Value::String(input.to_string());
            let result = py_json_dumps_opts(&v, false, false);
            assert_eq!(result, *expected, "input={input:?}");
        }
    }

    /// Nested arrays and objects use Python's ", " / ": " separators.
    #[test]
    fn test_py_json_dumps_opts_nested_structure() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"a": [1, 2, 3], "b": {"c": true, "d": null}}"#).unwrap();
        let result = py_json_dumps_opts(&v, true, true);
        assert_eq!(result, r#"{"a": [1, 2, 3], "b": {"c": true, "d": null}}"#);
    }

    /// Scalars: null, bool, integers serialise correctly.
    #[test]
    fn test_py_json_dumps_opts_scalars() {
        assert_eq!(
            py_json_dumps_opts(&serde_json::Value::Null, false, false),
            "null"
        );
        assert_eq!(
            py_json_dumps_opts(&serde_json::Value::Bool(true), false, false),
            "true"
        );
        assert_eq!(
            py_json_dumps_opts(&serde_json::Value::Bool(false), false, false),
            "false"
        );
        assert_eq!(
            py_json_dumps_opts(&serde_json::json!(42), false, false),
            "42"
        );
    }

    // ── py_float_repr additional boundary values ──────────────────────────────

    #[test]
    fn test_py_float_repr_boundary_values() {
        // exp == -4: boundary of the [-4, 16) positional range
        assert_eq!(py_float_repr(1e-4), "0.0001");
        // exp == 15: largest positional
        assert_eq!(py_float_repr(1e15), "1000000000000000.0");
        // Negative values
        assert_eq!(py_float_repr(-1.0), "-1.0");
        assert_eq!(py_float_repr(-123.456), "-123.456");
        // Very small negative
        assert_eq!(py_float_repr(-1e-7), "-1e-07");
        // Multi-digit mantissa in scientific notation
        assert_eq!(py_float_repr(1.23e20), "1.23e+20");
        // Zero
        assert_eq!(py_float_repr(0.0), "0.0");
    }
}
