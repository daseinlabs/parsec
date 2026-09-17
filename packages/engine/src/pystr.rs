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

    #[test]
    fn splitlines_handles_all_line_boundaries() {
        assert_eq!(py_splitlines("a\nb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\rb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\x0bb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\x0cb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\x1cb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\x1db"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\x1eb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\u{85}b"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\u{2028}b"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\u{2029}b"), vec!["a", "b"]);
    }

    #[test]
    fn splitlines_handles_crlf() {
        assert_eq!(py_splitlines("hello\r\nworld"), vec!["hello", "world"]);
        assert_eq!(py_splitlines("hello\r\n"), vec!["hello"]);
    }

    #[test]
    fn splitlines_handles_consecutive_and_empty_lines() {
        assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(py_splitlines("\n\n"), vec!["", ""]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("hello\n"), vec!["hello"]);
    }

    #[test]
    fn splitlines_handles_unicode() {
        assert_eq!(
            py_splitlines("こんにちは\n世界"),
            vec!["こんにちは", "世界"]
        );
        assert_eq!(py_splitlines("😀\u{2028}🚀"), vec!["😀", "🚀"]);
    }

    #[test]
    fn is_space_handles_python_whitespace() {
        for c in [
            ' ', '\t', '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\x1f', '\u{85}',
            '\u{00a0}', '\u{2000}',
        ] {
            assert!(py_is_space(c));
        }
    }

    #[test]
    fn is_space_rejects_non_whitespace() {
        for c in ['a', 'Z', '0', '_', '-', '😀', '中'] {
            assert!(!py_is_space(c));
        }
    }

    #[test]
    fn strip_handles_whitespace() {
        assert_eq!(py_strip(""), "");
        assert_eq!(py_strip("hello"), "hello");
        assert_eq!(py_strip("  hello  "), "hello");
        assert_eq!(py_strip("\t hello \n"), "hello");
        assert_eq!(py_strip("\x1chello\x1f"), "hello");
        assert_eq!(py_strip("\u{00a0}hello\u{00a0}"), "hello");
        assert_eq!(py_strip("   \t\n"), "");
    }

    #[test]
    fn has_content_handles_empty_and_whitespace() {
        assert!(!py_has_content(""));
        assert!(!py_has_content("   "));
        assert!(!py_has_content("\t\n"));
        assert!(py_has_content("hello"));
        assert!(py_has_content(" hello "));
        assert!(py_has_content("😀"));
    }

    #[test]
    fn split_ws_handles_runs_of_whitespace() {
        assert_eq!(
            py_split_ws("  hello   world\tfrom\nrust  "),
            vec!["hello", "world", "from", "rust"]
        );
        assert_eq!(py_split_ws(""), Vec::<&str>::new());
        assert_eq!(py_split_ws("   "), Vec::<&str>::new());
    }

    #[test]
    fn char_prefix_counts_characters_not_bytes() {
        assert_eq!(char_prefix("héllo", 2), "hé");
        assert_eq!(char_prefix("こんにちは", 3), "こんに");
        assert_eq!(char_prefix("你好世界", 2), "你好");
        assert_eq!(char_prefix("😀🚀🎉", 2), "😀🚀");
    }

    #[test]
    fn char_prefix_handles_boundaries() {
        assert_eq!(char_prefix("hello", 0), "");
        assert_eq!(char_prefix("hello", 5), "hello");
        assert_eq!(char_prefix("hello", 10), "hello");
        assert_eq!(char_prefix("", 10), "");
    }

    #[test]
    fn char_len_counts_unicode_characters() {
        assert_eq!(char_len(""), 0);
        assert_eq!(char_len("hello"), 5);
        assert_eq!(char_len("héllo"), 5);
        assert_eq!(char_len("こんにちは"), 5);
        assert_eq!(char_len("你好世界"), 4);
        assert_eq!(char_len("😀🚀🎉"), 3);
    }

    #[test]
    fn json_dumps_handles_unicode() {
        let value = json!("café");

        assert_eq!(py_json_dumps_opts(&value, false, true), "\"caf\\u00e9\"");

        assert_eq!(py_json_dumps_opts(&value, false, false), "\"café\"");
    }

    #[test]
    fn json_dumps_handles_emoji() {
        let value = json!("😀");

        assert_eq!(
            py_json_dumps_opts(&value, false, true),
            "\"\\ud83d\\ude00\""
        );

        assert_eq!(py_json_dumps_opts(&value, false, false), "\"😀\"");
    }

    #[test]
    fn json_dumps_sorts_keys() {
        let value: serde_json::Value = serde_json::from_str(r#"{"z":1,"a":2,"m":3}"#).unwrap();

        assert_eq!(
            py_json_dumps_opts(&value, true, false),
            "{\"a\": 2, \"m\": 3, \"z\": 1}"
        );
    }

    #[test]
    fn json_dumps_handles_basic_values() {
        assert_eq!(py_json_dumps(&json!(null)), "null");
        assert_eq!(py_json_dumps(&json!(true)), "true");
        assert_eq!(py_json_dumps(&json!(42)), "42");
        assert_eq!(py_json_dumps(&json!([1, 2, 3])), "[1, 2, 3]");
    }

    #[test]
    fn float_repr_handles_python_boundaries() {
        assert_eq!(py_float_repr(1e-7), "1e-07");
        assert_eq!(py_float_repr(1e-5), "1e-05");
        assert_eq!(py_float_repr(1e-4), "0.0001");
        assert_eq!(py_float_repr(1e16), "1e+16");
        assert_eq!(py_float_repr(1e15), "1000000000000000.0");
        assert_eq!(py_float_repr(1.0), "1.0");
        assert_eq!(py_float_repr(0.0), "0.0");
        assert_eq!(py_float_repr(-0.0), "-0.0");
    }
}
