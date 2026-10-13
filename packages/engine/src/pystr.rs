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

    // --- py_splitlines: the eleven line boundaries -----------------------

    #[test]
    fn test_py_splitlines_splits_on_every_single_char_line_break() {
        for brk in [
            '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
        ] {
            let s = format!("a{brk}b");
            assert_eq!(py_splitlines(&s), vec!["a", "b"], "break {brk:?}");
        }
    }

    #[test]
    fn test_py_splitlines_consumes_crlf_as_one_break() {
        assert_eq!(py_splitlines("a\r\nb\r\nc"), vec!["a", "b", "c"]);
        // A trailing CRLF must not fabricate an empty trailing line.
        assert_eq!(py_splitlines("a\r\n"), vec!["a"]);
        assert_eq!(py_splitlines("a\r\n\r\nb"), vec!["a", "", "b"]);
        // A lone \r is its own break; \r\r\n is \r followed by CRLF.
        assert_eq!(py_splitlines("a\rb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\r\r\nb"), vec!["a", "", "b"]);
    }

    #[test]
    fn test_py_splitlines_consecutive_and_edge_breaks() {
        assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(py_splitlines("\n"), vec![""]);
        assert_eq!(py_splitlines("\n\n"), vec!["", ""]);
        assert_eq!(py_splitlines("\r\n"), vec![""]);
        assert_eq!(py_splitlines("\na"), vec!["", "a"]);
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
        assert_eq!(py_splitlines("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn test_py_splitlines_empty_and_non_breaking_controls() {
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("abc"), vec!["abc"]);
        // \x1f is Python whitespace but NOT a line boundary.
        assert_eq!(py_splitlines("a\x1fb"), vec!["a\x1fb"]);
    }

    #[test]
    fn test_py_splitlines_breaks_around_multibyte_chars() {
        assert_eq!(py_splitlines("日本\u{2028}語"), vec!["日本", "語"]);
        assert_eq!(py_splitlines("😀\n🎉"), vec!["😀", "🎉"]);
    }

    // --- py_is_space / py_strip / py_split_ws ----------------------------

    #[test]
    fn test_py_is_space_matches_python_whitespace_set() {
        for c in [
            ' ', '\t', '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\x1f', '\u{85}',
            '\u{a0}', '\u{1680}', '\u{2000}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}',
            '\u{3000}',
        ] {
            assert!(py_is_space(c), "{c:?} should be Python whitespace");
        }
        for c in ['x', '0', '\u{200b}', '\u{1b}', '\u{feff}'] {
            assert!(!py_is_space(c), "{c:?} should not be Python whitespace");
        }
    }

    #[test]
    fn test_py_strip_handles_empty_whitespace_and_content() {
        assert_eq!(py_strip(""), "");
        assert_eq!(py_strip("   "), "");
        assert_eq!(py_strip("\n\t\r"), "");
        assert_eq!(py_strip("abc"), "abc");
        assert_eq!(py_strip("  abc  "), "abc");
        // \x1c-\x1f and NBSP/ideographic space are stripped too.
        assert_eq!(py_strip("  \u{a0}\x1c hi \x1f\u{3000} "), "hi");
    }

    #[test]
    fn test_py_has_content_is_strip_truthiness() {
        assert!(!py_has_content(""));
        assert!(!py_has_content(" \t\u{2028} "));
        assert!(py_has_content("x"));
        assert!(py_has_content("  x  "));
    }

    #[test]
    fn test_py_split_ws_collapses_runs_and_drops_empties() {
        assert_eq!(py_split_ws(""), Vec::<&str>::new());
        assert_eq!(py_split_ws("   "), Vec::<&str>::new());
        assert_eq!(py_split_ws("a b\tc"), vec!["a", "b", "c"]);
        assert_eq!(py_split_ws("  a   b  "), vec!["a", "b"]);
        assert_eq!(py_split_ws("a\x1cb\x1fc\u{a0}d"), vec!["a", "b", "c", "d"]);
    }

    // --- char_prefix / char_len ------------------------------------------

    #[test]
    fn test_char_prefix_zero_at_and_past_end() {
        assert_eq!(char_prefix("abc", 0), "");
        assert_eq!(char_prefix("abc", 3), "abc");
        assert_eq!(char_prefix("abc", 99), "abc");
        assert_eq!(char_prefix("", 5), "");
    }

    #[test]
    fn test_char_prefix_slices_by_code_point_not_byte() {
        assert_eq!(char_prefix("héllo", 2), "hé");
        assert_eq!(char_prefix("日本語", 1), "日");
        assert_eq!(char_prefix("😀x", 1), "😀");
        // A combining mark is its own code point.
        assert_eq!(char_prefix("e\u{301}", 1), "e");
        assert_eq!(char_prefix("e\u{301}", 2), "e\u{301}");
    }

    #[test]
    fn test_char_len_counts_code_points_not_bytes() {
        assert_eq!(char_len(""), 0);
        assert_eq!(char_len("abc"), 3);
        assert_eq!(char_len("héllo"), 5);
        assert_eq!(char_len("日本語"), 3);
        assert_eq!(char_len("😀"), 1);
        assert_eq!(char_len("e\u{301}"), 2);
    }

    #[test]
    fn test_char_len_never_exceeds_byte_len() {
        // Property-style invariant over representative multibyte inputs.
        for s in [
            "",
            "a",
            "é",
            "日本語",
            "😀",
            "aé日😀",
            "\u{301}",
            "\u{feff}",
        ] {
            assert!(char_len(s) <= s.len(), "char_len({s:?}) exceeds byte len");
        }
    }

    // --- py_json_dumps_opts ----------------------------------------------

    #[test]
    fn test_py_json_dumps_opts_sort_keys_by_code_point() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"b": 1, "a": 2, "Z": 3, "ä": 4}"#).unwrap();
        // preserve_order keeps authored order when sort_keys=false.
        assert_eq!(
            py_json_dumps_opts(&v, false, true),
            "{\"b\": 1, \"a\": 2, \"Z\": 3, \"\\u00e4\": 4}"
        );
        // Sorted by code point: 'Z'(0x5A) < 'a' < 'b' < 'ä'(0xE4).
        assert_eq!(
            py_json_dumps_opts(&v, true, true),
            "{\"Z\": 3, \"a\": 2, \"b\": 1, \"\\u00e4\": 4}"
        );
    }

    #[test]
    fn test_py_json_dumps_opts_ensure_ascii_bmp_and_astral() {
        let v = serde_json::Value::String("é😀".into());
        // U+00E9 stays BMP; U+1F600 becomes a surrogate pair.
        assert_eq!(
            py_json_dumps_opts(&v, false, true),
            "\"\\u00e9\\ud83d\\ude00\""
        );
        assert_eq!(py_json_dumps_opts(&v, false, false), "\"é😀\"");
    }

    #[test]
    fn test_py_json_dumps_escapes_special_and_control_chars() {
        let dump = |s: &str| py_json_dumps(&serde_json::Value::String(s.to_string()));
        assert_eq!(dump("\""), "\"\\\"\"");
        assert_eq!(dump("\\"), "\"\\\\\"");
        assert_eq!(dump("\u{1}"), "\"\\u0001\"");
        assert_eq!(dump("\u{8}"), "\"\\b\"");
        assert_eq!(dump("\u{c}"), "\"\\f\"");
        assert_eq!(dump("\u{b}"), "\"\\u000b\"");
        assert_eq!(dump("\n"), "\"\\n\"");
        assert_eq!(dump("\r"), "\"\\r\"");
        assert_eq!(dump("\t"), "\"\\t\"");
    }

    #[test]
    fn test_py_json_dumps_scalars_arrays_and_empty_containers() {
        assert_eq!(py_json_dumps(&serde_json::Value::Null), "null");
        assert_eq!(py_json_dumps(&serde_json::json!(true)), "true");
        assert_eq!(py_json_dumps(&serde_json::json!(false)), "false");
        assert_eq!(py_json_dumps(&serde_json::json!(42)), "42");
        assert_eq!(py_json_dumps(&serde_json::json!(-7)), "-7");
        assert_eq!(py_json_dumps(&serde_json::json!([])), "[]");
        assert_eq!(py_json_dumps(&serde_json::json!({})), "{}");
        assert_eq!(
            py_json_dumps(&serde_json::json!([1, "a", null])),
            "[1, \"a\", null]"
        );
        assert_eq!(
            py_json_dumps_opts(&serde_json::json!({"a": {"b": 1}}), true, true),
            "{\"a\": {\"b\": 1}}"
        );
    }

    #[test]
    fn test_py_json_dumps_routes_floats_through_py_float_repr() {
        assert_eq!(py_json_dumps(&serde_json::json!(1.0)), "1.0");
        assert_eq!(py_json_dumps(&serde_json::json!(1e-7)), "1e-07");
        assert_eq!(py_json_dumps(&serde_json::json!(123.456)), "123.456");
    }
}
