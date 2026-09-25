//! Property-based tests for the invariants pystr, chunking and messages are
//! built on. The RNG seed is fixed so every run explores the same inputs.

use parsec_engine::chunking::{
    chunk_assistant, chunk_observation, parse_grep_candidate, Chunk, ChunkMode,
};
use parsec_engine::messages::{assistant_chunks_of, blob_tokens, reasoning_chunks_of, steps_of};
use parsec_engine::pystr::*;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use serde_json::{json, Value};

const BREAKS: [char; 10] = [
    '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
];

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 512,
        rng_seed: RngSeed::Fixed(0x7061_7273_6563),
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// Strings weighted towards the characters these helpers treat specially:
/// every line break, \r\n pairs, Python-only whitespace and multi-byte text.
fn text() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        4 => any::<char>().prop_map(String::from),
        3 => prop::sample::select(vec!["a", "b", "x y", "é", "日本", "😀", "e\u{301}"])
            .prop_map(String::from),
        2 => prop::sample::select(BREAKS.to_vec()).prop_map(String::from),
        1 => Just("\r\n".to_string()),
        2 => prop::sample::select(vec![" ", "\t", "\x1f", "\u{a0}", "\u{3000}", "\u{200b}"])
            .prop_map(String::from),
    ];
    prop::collection::vec(piece, 0..60).prop_map(|v| v.concat())
}

fn json_value() -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<f64>().prop_filter_map("finite", |f| serde_json::Number::from_f64(f)
            .map(Value::Number)),
        text().prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::vec((text(), inner), 0..6)
                .prop_map(|kv| Value::Object(kv.into_iter().collect())),
        ]
    })
    .boxed()
}

/// Byte offset of a subslice within the string it was sliced from.
fn offset(outer: &str, inner: &str) -> usize {
    inner.as_ptr() as usize - outer.as_ptr() as usize
}

fn content_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    lines.into_iter().filter(|l| py_has_content(l)).collect()
}

/// Lines of every chunk in order. Windowed chunks are lines joined by \n
/// and lines never contain \n, so splitting on it recovers them.
fn chunk_lines(chunks: &[Chunk]) -> Vec<&str> {
    chunks.iter().flat_map(|c| c.text.split('\n')).collect()
}

fn sorted_keys(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(sorted_keys).collect()),
        Value::Object(o) => {
            let mut entries: Vec<_> = o.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), sorted_keys(v)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn splitlines_lines_and_separators_tile_the_input(s in text()) {
        let lines = py_splitlines(&s);
        let mut pos = 0;
        for (i, line) in lines.iter().enumerate() {
            prop_assert_eq!(offset(&s, line), pos, "line {} starts right after the previous break", i);
            prop_assert!(!line.contains(BREAKS), "line {} contains a break: {:?}", i, line);
            let end = pos + line.len();
            let rest = &s[end..];
            let sep = if rest.starts_with("\r\n") {
                2
            } else {
                rest.chars().next().map_or(0, char::len_utf8)
            };
            if i + 1 < lines.len() {
                prop_assert!(sep > 0, "consecutive lines need a break between them");
            }
            if let Some(c) = rest.chars().next() {
                prop_assert!(BREAKS.contains(&c), "line {} ended on a non-break {:?}", i, c);
            }
            pos = end + sep;
        }
        prop_assert_eq!(pos, s.len(), "all input consumed");
    }

    #[test]
    fn strip_removes_only_whitespace_and_is_idempotent(s in text()) {
        let t = py_strip(&s);
        let start = offset(&s, t);
        prop_assert!(s[..start].chars().all(py_is_space));
        prop_assert!(s[start + t.len()..].chars().all(py_is_space));
        prop_assert!(!t.starts_with(py_is_space) && !t.ends_with(py_is_space));
        prop_assert_eq!(py_strip(t), t);
        prop_assert_eq!(py_has_content(&s), !t.is_empty());
    }

    #[test]
    fn split_ws_keeps_every_non_space_char_in_order(s in text()) {
        let parts = py_split_ws(&s);
        prop_assert!(parts.iter().all(|p| !p.is_empty() && !p.contains(py_is_space)));
        let non_space: String = s.chars().filter(|c| !py_is_space(*c)).collect();
        prop_assert_eq!(parts.concat(), non_space);
    }

    #[test]
    fn char_prefix_and_char_len_agree(s in text(), n in 0usize..80) {
        let len = char_len(&s);
        prop_assert!(len <= s.len());
        let p = char_prefix(&s, n);
        prop_assert!(s.starts_with(p));
        prop_assert_eq!(char_len(p), n.min(len));
        prop_assert_eq!(char_prefix(&s, len), s.as_str());
    }

    #[test]
    fn code_point_order_matches_utf8_byte_order(a in text(), b in text()) {
        // pystr's sort_keys relies on this to match Python's str ordering.
        prop_assert_eq!(a.chars().cmp(b.chars()), a.cmp(&b));
    }

    #[test]
    fn float_repr_round_trips_exactly(f in any::<f64>().prop_filter("finite", |f| f.is_finite())) {
        let r = py_float_repr(f);
        prop_assert!(r.contains('.') || r.contains('e'), "{} looks like an int", r);
        let back: f64 = r.parse().map_err(|e| TestCaseError::fail(format!("{r}: {e}")))?;
        prop_assert_eq!(back.to_bits(), f.to_bits(), "{}", r);
    }

    #[test]
    fn json_dumps_parses_back_to_the_same_value(
        v in json_value(),
        sort_keys in any::<bool>(),
        ensure_ascii in any::<bool>(),
    ) {
        let out = py_json_dumps_opts(&v, sort_keys, ensure_ascii);
        let back: Value = serde_json::from_str(&out)
            .map_err(|e| TestCaseError::fail(format!("invalid JSON {out:?}: {e}")))?;
        prop_assert_eq!(back, v);
    }

    #[test]
    fn json_dumps_ensure_ascii_output_is_printable_ascii(v in json_value(), sort_keys in any::<bool>()) {
        let out = py_json_dumps_opts(&v, sort_keys, true);
        prop_assert!(out.chars().all(|c| (' '..='~').contains(&c)), "{:?}", out);
        let raw = py_json_dumps_opts(&v, sort_keys, false);
        prop_assert!(raw.chars().all(|c| c >= ' '), "raw control char in {:?}", raw);
    }

    #[test]
    fn json_dumps_sort_keys_equals_dumping_presorted_value(v in json_value(), ensure_ascii in any::<bool>()) {
        prop_assert_eq!(
            py_json_dumps_opts(&v, true, ensure_ascii),
            py_json_dumps_opts(&sorted_keys(&v), false, ensure_ascii)
        );
    }

    #[test]
    fn grep_candidate_never_panics_and_returns_a_basename(line in text()) {
        if let Some((file, n)) = parse_grep_candidate(&line) {
            prop_assert!(!file.contains('/'));
            prop_assert!(n.is_none_or(|n| n >= 0));
        }
    }

    #[test]
    fn assistant_chunks_cover_every_content_line(txt in text(), win in 1usize..12, step in 0i64..50) {
        let chunks = chunk_assistant(&txt, step, win);
        prop_assert_eq!(content_lines(chunk_lines(&chunks)), content_lines(py_splitlines(&txt)));
        for c in &chunks {
            prop_assert!(py_has_content(&c.text));
            prop_assert!(c.text.split('\n').count() <= win);
            prop_assert_eq!((c.kind.as_str(), c.step), ("asst", step));
        }
    }

    #[test]
    fn other_output_chunks_cover_every_content_line(obs in text(), win in 1usize..12) {
        let chunks = chunk_observation("pytest -q", &obs, 0, win, None, ChunkMode::Fixed);
        prop_assert!(!chunks.is_empty());
        prop_assert_eq!(content_lines(chunk_lines(&chunks)), content_lines(py_splitlines(&obs)));
        if py_has_content(&obs) {
            prop_assert!(chunks.iter().all(|c| py_has_content(&c.text)));
        }
    }

    #[test]
    fn grep_output_chunks_cover_every_content_line(
        lines in prop::collection::vec(
            prop_oneof![
                text(),
                (text(), 0u32..500).prop_map(|(t, n)| format!("src/f.rs:{n}:{t}")),
            ],
            0..20,
        ),
        win in 1usize..6,
    ) {
        let obs = lines.join("\n");
        let chunks = chunk_observation("grep -rn foo src", &obs, 0, win, None, ChunkMode::Fixed);
        prop_assert!(!chunks.is_empty());
        prop_assert_eq!(content_lines(chunk_lines(&chunks)), content_lines(py_splitlines(&obs)));
    }

    #[test]
    fn read_chunks_have_contiguous_line_ranges(obs in text(), win in 1usize..12) {
        let chunks = chunk_observation("sed -n '100,300p' a.py", &obs, 0, win, None, ChunkMode::Fixed);
        let lines = py_splitlines(&obs);
        if lines.is_empty() {
            prop_assert_eq!(chunks.len(), 1);
            prop_assert_eq!((chunks[0].lo, chunks[0].hi), (None, None));
            return Ok(());
        }
        prop_assert_eq!(chunk_lines(&chunks), lines.clone());
        let mut next = 100;
        for c in &chunks {
            let (lo, hi) = (c.lo.unwrap_or(-1), c.hi.unwrap_or(-1));
            prop_assert_eq!(lo, next);
            prop_assert_eq!(hi - lo + 1, c.text.split('\n').count() as i64);
            prop_assert!(hi - lo < win as i64);
            next = hi + 1;
        }
        prop_assert_eq!(next, 100 + lines.len() as i64);
    }

    #[test]
    fn read_lines_chunks_keep_content_lines_with_increasing_coordinates(obs in text(), g in 1usize..8) {
        let chunks = chunk_observation("cat a.py", &obs, 0, 40, Some(g), ChunkMode::Fixed);
        prop_assert!(!chunks.is_empty());
        let lines = py_splitlines(&obs);
        if !lines.iter().any(|l| py_has_content(l)) {
            prop_assert_eq!(chunks.len(), 1);
            prop_assert_eq!(chunks[0].text.as_str(), obs.as_str());
            return Ok(());
        }
        prop_assert_eq!(chunk_lines(&chunks), content_lines(lines));
        let mut prev_hi = 0;
        for c in &chunks {
            let (lo, hi) = (c.lo.unwrap_or(-1), c.hi.unwrap_or(-1));
            prop_assert!(prev_hi < lo && lo <= hi);
            prop_assert!(c.text.split('\n').count() <= g);
            prev_hi = hi;
        }
    }

    #[test]
    fn chunk_observation_never_panics(
        cmd in prop_oneof![
            prop::sample::select(vec![
                "cat a.py", "sed -n '5,9p' src/x.rs", "sed -n 3p b.go", "grep -rn foo .",
                "find . -name '*.py'", "pytest -q", "ls", "head -n 20 c.ts",
            ]).prop_map(String::from),
            text(),
        ],
        obs in text(),
        win in 0usize..50,
        read_lines in prop::option::of(1usize..50),
    ) {
        let chunks = chunk_observation(&cmd, &obs, 0, win, read_lines, ChunkMode::Fixed);
        prop_assert!(!chunks.is_empty());
        prop_assert!(chunks.iter().all(|c| c.tokens >= 1 && c.head == char_prefix(&obs, 240)));
    }

    #[test]
    fn message_parsing_is_total_and_step_aligned(
        msgs in prop::collection::vec(
            prop_oneof![
                json_value(),
                (
                    prop::sample::select(vec!["assistant", "user", "tool", "system"]),
                    json_value(),
                    prop::collection::vec((json_value(), json_value()), 0..3),
                    json_value(),
                ).prop_map(|(role, content, acts, reasoning)| {
                    let actions: Vec<Value> = acts
                        .into_iter()
                        .map(|(command, query)| json!({"command": command, "query": query}))
                        .collect();
                    json!({
                        "role": role,
                        "content": content,
                        "extra": {"actions": actions},
                        "reasoning_content": reasoning,
                    })
                }),
            ],
            0..12,
        ),
    ) {
        let steps = steps_of(&msgs).len() as i64;
        let role_count = |r: &str| msgs.iter().filter(|m| m.get("role").and_then(Value::as_str) == Some(r)).count() as i64;
        prop_assert!(steps <= role_count("assistant"));
        prop_assert!(steps <= role_count("user") + role_count("tool"));

        let reasoning = reasoning_chunks_of(&msgs);
        prop_assert!(reasoning.windows(2).all(|w| w[0].step < w[1].step));
        for c in &reasoning {
            prop_assert!(c.step < steps && c.tokens > 0 && c.evict == "provider");
        }
        let asst = assistant_chunks_of(&msgs);
        prop_assert!(asst.windows(2).all(|w| w[0].step <= w[1].step));
        prop_assert!(asst.iter().all(|c| c.step < steps && py_has_content(&c.text)));
        prop_assert!(msgs.iter().all(|m| blob_tokens(m) >= 0));
    }
}
