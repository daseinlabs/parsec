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

    // Expected values below were checked against CPython 3.12.

    /// Every char CPython 3.12 reports as `str.isspace()`.
    const PY_WHITESPACE: [u32; 29] = [
        0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x85, 0xa0, 0x1680, 0x2000,
        0x2001, 0x2002, 0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200a, 0x2028,
        0x2029, 0x202f, 0x205f, 0x3000,
    ];

    /// The chars `str.splitlines()` breaks on (\r\n is the eleventh boundary).
    const PY_LINE_BREAKS: [char; 10] = [
        '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
    ];

    fn all_chars() -> impl Iterator<Item = char> {
        (0..=0x10FFFF).filter_map(char::from_u32)
    }

    #[test]
    fn splitlines_breaks_on_exactly_the_python_boundary_set() {
        for c in all_chars() {
            let s = format!("a{c}b");
            let got = py_splitlines(&s);
            if PY_LINE_BREAKS.contains(&c) {
                assert_eq!(got, vec!["a", "b"], "U+{:04X} should split", c as u32);
            } else {
                assert_eq!(got, vec![s.as_str()], "U+{:04X} should not split", c as u32);
            }
        }
    }

    #[test]
    fn splitlines_does_not_break_on_unit_separator() {
        // \x1c..\x1e are line boundaries but \x1f is not, even though all
        // four count as whitespace for strip().
        assert_eq!(py_splitlines("a\x1fb\x1cc"), vec!["a\x1fb", "c"]);
    }

    #[test]
    fn splitlines_crlf_is_one_break_but_lf_cr_is_two() {
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\n\rb"), vec!["a", "", "b"]);
        assert_eq!(py_splitlines("\r\r\n"), vec!["", ""]);
        assert_eq!(py_splitlines("a\r\n"), vec!["a"]);
    }

    #[test]
    fn splitlines_keeps_leading_and_inner_empty_lines_but_no_trailing_one() {
        assert_eq!(
            py_splitlines("\n\na\r\n\r\nb\r\r\n\n"),
            vec!["", "", "a", "", "b", "", ""]
        );
        assert_eq!(py_splitlines("\n"), vec![""]);
        assert_eq!(py_splitlines("\u{2029}\u{2029}"), vec!["", ""]);
    }

    #[test]
    fn splitlines_handles_multibyte_breaks_between_multibyte_text() {
        assert_eq!(
            py_splitlines("é\u{85}日本\u{2028}😀\u{2029}x\x0cy"),
            vec!["é", "日本", "😀", "x", "y"]
        );
    }

    #[test]
    fn splitlines_lines_are_in_order_and_only_separators_are_dropped() {
        let s = "\u{2028}ab\r\n\rc\x0b\x0bdé\u{85}";
        let mut rebuilt = String::new();
        let mut rest = s;
        for line in py_splitlines(s) {
            let at = rest
                .find(line)
                .expect("line appears in the remaining input");
            assert!(
                rest[..at].chars().all(|c| PY_LINE_BREAKS.contains(&c)),
                "only line breaks may be skipped before {line:?}"
            );
            rebuilt.push_str(line);
            rest = &rest[at + line.len()..];
            let sep_len = if rest.starts_with("\r\n") {
                2
            } else {
                rest.chars().next().map_or(0, char::len_utf8)
            };
            rest = &rest[sep_len..];
        }
        assert_eq!(rest, "", "input fully consumed");
        assert_eq!(rebuilt, "abcdé");
    }

    #[test]
    fn is_space_matches_python_isspace_for_every_char() {
        for c in all_chars() {
            assert_eq!(
                py_is_space(c),
                PY_WHITESPACE.contains(&(c as u32)),
                "U+{:04X}",
                c as u32
            );
        }
    }

    #[test]
    fn strip_removes_python_whitespace_from_both_ends_only() {
        assert_eq!(
            py_strip("\x1c\x1d\x1e\x1f \t\u{a0}\u{3000}x\u{200b}\u{feff}"),
            "x\u{200b}\u{feff}"
        );
        assert_eq!(py_strip("\u{2028} a \x1f b \u{85}"), "a \x1f b");
        assert_eq!(py_strip("no-whitespace"), "no-whitespace");
    }

    #[test]
    fn strip_of_empty_or_all_whitespace_is_empty() {
        assert_eq!(py_strip(""), "");
        assert_eq!(
            py_strip(" \t\r\n\x0b\x0c\x1c\x1d\x1e\x1f\u{85}\u{3000}"),
            ""
        );
        assert!(!py_has_content(""));
        assert!(!py_has_content("\x1f\u{2029}"));
        assert!(py_has_content("\x1f.\u{2029}"));
        // Zero-width space and BOM are not whitespace in Python.
        assert!(py_has_content("\u{200b}"));
        assert!(py_has_content("\u{feff}"));
    }

    #[test]
    fn split_ws_drops_empty_parts_and_splits_on_c0_separators() {
        assert_eq!(py_split_ws("\x1fa\u{3000}\u{3000}b\x1c"), vec!["a", "b"]);
        assert_eq!(py_split_ws(" \t "), Vec::<&str>::new());
        assert_eq!(py_split_ws(""), Vec::<&str>::new());
        assert_eq!(py_split_ws("a\u{200b}b"), vec!["a\u{200b}b"]);
    }

    #[test]
    fn char_len_counts_code_points_not_bytes_or_graphemes() {
        assert_eq!(char_len(""), 0);
        assert_eq!(char_len("héllo"), 5);
        assert_eq!(char_len("日本語"), 3);
        // Python 3 str has no surrogate pairs: an astral emoji is one char.
        assert_eq!(char_len("😀"), 1);
        // Combining accent and ZWJ sequences count every code point.
        assert_eq!(char_len("e\u{301}"), 2);
        assert_eq!(char_len("👨\u{200d}👩\u{200d}👧"), 5);
    }

    #[test]
    fn char_len_never_exceeds_byte_len() {
        for s in ["", "a", "é", "日本語", "😀x", "e\u{301}\u{85}"] {
            assert!(char_len(s) <= s.len(), "{s:?}");
        }
    }

    #[test]
    fn char_prefix_slices_on_char_boundaries() {
        assert_eq!(char_prefix("日本語テキスト", 3), "日本語");
        assert_eq!(char_prefix("😀😃😄", 2), "😀😃");
        assert_eq!(char_prefix("e\u{301}x", 1), "e");
        assert_eq!(char_prefix("👨\u{200d}👩", 2), "👨\u{200d}");
    }

    #[test]
    fn char_prefix_zero_exact_and_oversized_lengths() {
        assert_eq!(char_prefix("héllo", 0), "");
        assert_eq!(char_prefix("héllo", 5), "héllo");
        assert_eq!(char_prefix("héllo", 6), "héllo");
        assert_eq!(char_prefix("héllo", usize::MAX), "héllo");
        assert_eq!(char_prefix("", 0), "");
        assert_eq!(char_prefix("", 3), "");
    }

    #[test]
    fn json_dumps_sort_keys_orders_by_code_point() {
        let v = serde_json::json!({"b": 1, "a": 2, "é": 3, "Z": 4, "😀": 5, "\u{ffff}": 6});
        assert_eq!(
            py_json_dumps_opts(&v, true, false),
            "{\"Z\": 4, \"a\": 2, \"b\": 1, \"é\": 3, \"\u{ffff}\": 6, \"😀\": 5}"
        );
    }

    #[test]
    fn json_dumps_without_sort_keys_keeps_insertion_order() {
        let v = serde_json::json!({"b": 1, "a": 2});
        assert_eq!(py_json_dumps_opts(&v, false, true), "{\"b\": 1, \"a\": 2}");
        assert_eq!(py_json_dumps_opts(&v, false, false), "{\"b\": 1, \"a\": 2}");
    }

    #[test]
    fn json_dumps_sort_keys_applies_to_nested_objects() {
        let v = serde_json::json!({"x": {"b": 1, "a": [{"d": 1, "c": 2}]}});
        assert_eq!(
            py_json_dumps_opts(&v, true, true),
            "{\"x\": {\"a\": [{\"c\": 2, \"d\": 1}], \"b\": 1}}"
        );
    }

    #[test]
    fn json_dumps_ensure_ascii_escapes_non_ascii_and_surrogate_pairs() {
        let v = serde_json::json!("😀é\u{2028}\u{0}\u{1f}\u{8}\u{c}/");
        assert_eq!(
            py_json_dumps_opts(&v, false, true),
            r#""\ud83d\ude00\u00e9\u2028\u0000\u001f\b\f/""#
        );
    }

    #[test]
    fn json_dumps_ensure_ascii_surrogate_pairs_start_above_the_bmp() {
        // U+FFFF is the last single escape; U+10000 is the first pair.
        let v = serde_json::json!("\u{ffff}\u{10000}\u{10ffff}");
        assert_eq!(
            py_json_dumps_opts(&v, false, true),
            r#""\uffff\ud800\udc00\udbff\udfff""#
        );
    }

    #[test]
    fn json_dumps_without_ensure_ascii_emits_raw_utf8_but_still_escapes_controls() {
        let v = serde_json::json!("😀é\u{2028}\u{0}\"\\\n\r\t");
        assert_eq!(
            py_json_dumps_opts(&v, false, false),
            "\"😀é\u{2028}\\u0000\\\"\\\\\\n\\r\\t\""
        );
    }

    #[test]
    fn json_dumps_ensure_ascii_applies_to_keys_too() {
        let v = serde_json::json!({"ключ": "é"});
        assert_eq!(
            py_json_dumps_opts(&v, false, true),
            r#"{"\u043a\u043b\u044e\u0447": "\u00e9"}"#
        );
        assert_eq!(py_json_dumps_opts(&v, false, false), "{\"ключ\": \"é\"}");
    }

    #[test]
    fn json_dumps_empty_containers_and_scalars() {
        let v = serde_json::json!([{}, [], null, true, false, ""]);
        assert_eq!(py_json_dumps(&v), r#"[{}, [], null, true, false, ""]"#);
    }

    #[test]
    fn json_dumps_numbers_match_python_after_parsing() {
        let v: serde_json::Value = serde_json::from_str(
            "[1e2, -0.0, 18446744073709551615, -9223372036854775808, 1.5e300]",
        )
        .unwrap();
        assert_eq!(
            py_json_dumps(&v),
            "[100.0, -0.0, 18446744073709551615, -9223372036854775808, 1.5e+300]"
        );
    }

    #[test]
    #[ignore = "known edge case / bug: ensure_ascii=true leaves DEL (\\x7f) raw; Python escapes it as \\u007f"]
    fn json_dumps_ensure_ascii_escapes_del() {
        let v = serde_json::json!("\u{7f}");
        assert_eq!(py_json_dumps_opts(&v, false, true), r#""\u007f""#);
        assert_eq!(py_json_dumps_opts(&v, false, false), "\"\u{7f}\"");
    }
}
