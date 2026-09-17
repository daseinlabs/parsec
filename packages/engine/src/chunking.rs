//! Port of `adaptive_context/optimizer/chunking.py` (+ the grep-line parser
//! from `labelers.py`). Full-coverage invariant: every character of a tool
//! observation lands in exactly one chunk, in original order.
//!
//! One deliberate deviation from the reference: `AC_CHUNK_MODE` was a
//! read-once module global in Python; here the mode is an explicit parameter
//! (`ChunkMode`), with [`ChunkMode::from_env`] for the launcher seam. Serving
//! must re-chunk with the same mode the checkpoint was trained with.

use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::pystr::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChunkMode {
    /// Fixed line windows (default; byte-identical to the legacy path).
    #[default]
    Fixed,
    /// Tree-sitter semantic atoms (Arm-2).
    Cst,
}

impl ChunkMode {
    pub fn from_env() -> Self {
        match std::env::var("AC_CHUNK_MODE")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "cst" => ChunkMode::Cst,
            _ => ChunkMode::Fixed,
        }
    }
}

pub const DEFAULT_WIN: usize = 40;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub text: String,
    /// Basename, or None for non-file content (test output, reasoning).
    pub file: Option<String>,
    /// Line range covered (for file chunks).
    pub lo: Option<i64>,
    pub hi: Option<i64>,
    /// Which step produced it (age).
    pub step: i64,
    /// 'read' | 'grep' | 'other' | 'reasoning' | 'asst'.
    pub kind: String,
    /// Token weight; derived from char length when the producer passes 0.
    pub tokens: i64,
    /// Eviction primitive: 'content' | 'provider'.
    pub evict: String,
    /// The raw ACTION text that produced this observation.
    pub cmd: String,
    /// Parsed exit status when the harness exposes one.
    pub rc: Option<i64>,
    /// Observation head (~240 chars): status line territory.
    pub head: String,
    /// CST node class for code-read atoms (func|class|import|body); "" when
    /// fixed-window chunked or non-code.
    pub struct_class: String,
}

impl Chunk {
    /// Mirrors `Chunk(...)` + `__post_init__`: tokens<=0 derives from char length.
    pub fn new(
        text: impl Into<String>,
        file: Option<String>,
        lo: Option<i64>,
        hi: Option<i64>,
        step: i64,
        kind: &str,
    ) -> Self {
        let text = text.into();
        let tokens = std::cmp::max(1, char_len(&text) as i64 / 4);
        Chunk {
            text,
            file,
            lo,
            hi,
            step,
            kind: kind.into(),
            tokens,
            evict: "content".into(),
            cmd: String::new(),
            rc: None,
            head: String::new(),
            struct_class: String::new(),
        }
    }

    pub fn with_tokens(mut self, tokens: i64) -> Self {
        if tokens > 0 {
            self.tokens = tokens;
        }
        self
    }
}

static SEARCH: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(grep|rg|egrep|fgrep|ag|ack|find|git grep)\b").unwrap());
static READ: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\b(cat|sed|head|tail|less|more|nl|awk|view|open)\b").unwrap()
});
static FILEARG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[\w./+-]*\.[A-Za-z]{1,5}").unwrap());
// [\s\x1c-\x1f]: Python re's \s includes the C0 separators that the regex
// crate's \p{White_Space} excludes (same set as pystr::py_is_space).
static RC_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)<returncode>[\s\x1c-\x1f]*(-?\d+)[\s\x1c-\x1f]*</returncode>|\breturncode[:=][\s\x1c-\x1f]*(-?\d+)",
    )
    .unwrap()
});
static SED_RANGE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(\d+),(\d+)p").unwrap());
static SED_ONE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\bsed[\s\x1c-\x1f]+-n[\s\x1c-\x1f]+(\d+)p").unwrap());

/// Python `int()` of an ASCII-digit run is unbounded; i64 is not. Saturate
/// instead of failing the whole parse (documented deviation for line numbers
/// beyond i64::MAX — the coordinates, not the chunk set, differ there).
fn parse_line_no(s: &str) -> i64 {
    s.parse::<i64>()
        .or_else(|_| s.parse::<u128>().map(|v| v.min(i64::MAX as u128) as i64))
        .unwrap_or(i64::MAX)
}
static EXT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\.[A-Za-z][A-Za-z0-9]{0,4}$").unwrap());

fn obs_rc(obs: &str) -> Option<i64> {
    let head = char_prefix(obs, 400);
    RC_RE.captures(head).and_then(|m| {
        m.get(1)
            .or_else(|| m.get(2))
            .and_then(|g| g.as_str().parse().ok())
    })
}

/// labelers._is_path
fn is_path(tok: &str) -> bool {
    !tok.is_empty() && (tok.contains('/') || EXT.is_match(tok))
}

fn basename(tok: &str) -> &str {
    tok.rsplit('/').next().unwrap_or(tok)
}

/// labelers.parse_grep_candidate: one grep/find result line -> (file_basename, line|None).
pub fn parse_grep_candidate(line: &str) -> Option<(String, Option<i64>)> {
    let line = py_strip(line);
    if line.is_empty() || line.starts_with("[reranked") {
        return None;
    }
    let parts: Vec<&str> = line.splitn(3, ':').collect();
    if parts.len() >= 3
        && !py_strip(parts[1]).is_empty()
        && py_strip(parts[1]).chars().all(|c| c.is_ascii_digit())
        && is_path(parts[0])
    {
        let n = parse_line_no(py_strip(parts[1]));
        return Some((basename(parts[0]).to_string(), Some(n)));
    }
    if parts.len() >= 2 && is_path(parts[0]) {
        return Some((basename(parts[0]).to_string(), None));
    }
    let tok = py_split_ws(line)[0].trim_end_matches(':');
    if is_path(tok) {
        Some((basename(tok).to_string(), None))
    } else {
        None
    }
}

fn first_file(cmd: &str) -> Option<String> {
    let cleaned = cmd.replace(['\'', '"'], " ");
    for tok in py_split_ws(&cleaned) {
        if FILEARG.is_match(basename(tok)) {
            return Some(basename(tok).to_string());
        }
    }
    None
}

/// Detect the starting line from `sed -n 'a,bp'` / `sed -n Np`; else 1.
fn sed_base(cmd: &str) -> i64 {
    if let Some(m) = SED_RANGE.captures(cmd) {
        return parse_line_no(&m[1]);
    }
    if let Some(m) = SED_ONE.captures(cmd) {
        return parse_line_no(&m[1]);
    }
    1
}

/// (file, [(orig_line_no, text)]) for a CODE-READ observation: fully-blank
/// lines dropped, comments kept, true original-file coordinates preserved.
fn read_atom_lines<'a>(cmd: &str, obs: &'a str) -> (Option<String>, Vec<(i64, &'a str)>) {
    let f = first_file(cmd);
    let base = sed_base(cmd);
    let mut out = Vec::new();
    for (i, ln) in py_splitlines(obs).into_iter().enumerate() {
        if py_strip(ln).is_empty() {
            continue;
        }
        out.push((base + i as i64, ln));
    }
    (f, out)
}

/// chunking.chunk_observation: slice one observation into chunks with
/// file/line metadata where possible, then ride cmd/rc/head on every chunk.
pub fn chunk_observation(
    cmd: &str,
    obs: &str,
    step: i64,
    win: usize,
    read_lines: Option<usize>,
    mode: ChunkMode,
) -> Vec<Chunk> {
    let mut out: Vec<Chunk>;
    if let Some(g) = read_lines.filter(|_| !SEARCH.is_match(cmd) && READ.is_match(cmd)) {
        let (f, coord_lines) = read_atom_lines(cmd, obs);
        out = Vec::new();
        let atoms = if mode == ChunkMode::Cst && !coord_lines.is_empty() {
            crate::cst::cst_read_atoms(f.as_deref(), &coord_lines, crate::cst::DEFAULT_MAX_LINES)
        } else {
            None
        };
        if let Some(atoms) = atoms {
            for (lo, hi, text, kls) in atoms {
                let mut c = Chunk::new(text, f.clone(), Some(lo), Some(hi), step, "read");
                c.struct_class = kls;
                out.push(c);
            }
        } else {
            let mut k = 0;
            while k < coord_lines.len() {
                let seg = &coord_lines[k..std::cmp::min(k + g, coord_lines.len())];
                let text = seg.iter().map(|(_, t)| *t).collect::<Vec<_>>().join("\n");
                out.push(Chunk::new(
                    text,
                    f.clone(),
                    Some(seg[0].0),
                    Some(seg[seg.len() - 1].0),
                    step,
                    "read",
                ));
                k += g;
            }
        }
        if out.is_empty() {
            out = vec![Chunk::new(obs, f, None, None, step, "read")];
        }
    } else {
        out = chunk_observation_inner(cmd, obs, step, win);
    }
    let rc = obs_rc(obs);
    let cmd_head = char_prefix(cmd, 300).to_string();
    let obs_head = char_prefix(obs, 240).to_string();
    for c in &mut out {
        c.cmd = cmd_head.clone();
        c.rc = rc;
        c.head = obs_head.clone();
    }
    out
}

/// chunking._chunk_observation (the legacy win-window path).
fn chunk_observation_inner(cmd: &str, obs: &str, step: i64, win: usize) -> Vec<Chunk> {
    let lines = py_splitlines(obs);
    if SEARCH.is_match(cmd) {
        // grep/find: each match line is a tiny chunk; non-candidate runs window.
        let mut out: Vec<Chunk> = Vec::new();
        let mut run: Vec<&str> = Vec::new();
        let flush = |run: &mut Vec<&str>, out: &mut Vec<Chunk>| {
            let mut k = 0;
            while k < run.len() {
                let seg = run[k..std::cmp::min(k + win, run.len())].join("\n");
                if py_has_content(&seg) {
                    out.push(Chunk::new(seg, None, None, None, step, "other"));
                }
                k += win;
            }
            run.clear();
        };
        for ln in &lines {
            if let Some((f, n)) = parse_grep_candidate(ln) {
                flush(&mut run, &mut out);
                out.push(Chunk::new(*ln, Some(f), n, n, step, "grep"));
            } else {
                run.push(ln);
            }
        }
        flush(&mut run, &mut out);
        if out.is_empty() {
            out.push(Chunk::new(obs, None, None, None, step, "other"));
        }
        return out;
    }
    if READ.is_match(cmd) {
        // file read: window the file by `win` lines from the sed/head base.
        let f = first_file(cmd);
        let base = sed_base(cmd);
        let mut out = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let seg = &lines[i..std::cmp::min(i + win, lines.len())];
            out.push(Chunk::new(
                seg.join("\n"),
                f.clone(),
                Some(base + i as i64),
                Some(base + i as i64 + seg.len() as i64 - 1),
                step,
                "read",
            ));
            i += win;
        }
        if out.is_empty() {
            out.push(Chunk::new(obs, f, None, None, step, "read"));
        }
        return out;
    }
    // everything else (test output, python, ls, git): windowed, no file.
    let mut out = Vec::new();
    let mut i = 0;
    let end = std::cmp::max(1, lines.len());
    while i < end {
        let seg = lines[i..std::cmp::min(i + win, lines.len())].join("\n");
        if py_has_content(&seg) {
            out.push(Chunk::new(seg, None, None, None, step, "other"));
        }
        i += win;
    }
    if out.is_empty() {
        out.push(Chunk::new(obs, None, None, None, step, "other"));
    }
    out
}

/// chunking.chunk_assistant: the agent's own message text, windowed with the
/// same full-coverage invariant. May legitimately return no chunks.
pub fn chunk_assistant(txt: &str, step: i64, win: usize) -> Vec<Chunk> {
    let lines = py_splitlines(txt);
    let mut out = Vec::new();
    let mut i = 0;
    let end = std::cmp::max(1, lines.len());
    while i < end {
        let seg = lines[i..std::cmp::min(i + win, lines.len())].join("\n");
        if py_has_content(&seg) {
            out.push(Chunk::new(seg, None, None, None, step, "asst"));
        }
        i += win;
    }
    out
}

/// chunking.accumulated_chunks: all chunks the agent has SEEN through `upto`.
pub fn accumulated_chunks(
    steps: &[(String, String)],
    upto: usize,
    read_lines: Option<usize>,
    mode: ChunkMode,
) -> Vec<Chunk> {
    let mut out = Vec::new();
    for (s, (cmd, obs)) in steps
        .iter()
        .enumerate()
        .take(std::cmp::min(upto + 1, steps.len()))
    {
        out.extend(chunk_observation(
            cmd,
            obs,
            s as i64,
            DEFAULT_WIN,
            read_lines,
            mode,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_numbers_beyond_i64_saturate_not_abort() {
        // Python's unbounded int() keeps the line a grep chunk; we saturate
        // the coordinate (documented deviation) but MUST keep the chunk.
        let got = parse_grep_candidate("a.py:12345678901234567890:huge").unwrap();
        assert_eq!(got, ("a.py".to_string(), Some(i64::MAX)));
        assert_eq!(
            sed_base("sed -n '99999999999999999999,99999999999999999999p' f.py"),
            i64::MAX
        );
    }

    // -----------------------------------------------------------------------
    // parse_grep_candidate
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_grep_candidate_standard_format() {
        // path/to/file.rs:42:matching line
        let r = parse_grep_candidate("src/main.rs:42:let x = 1;").unwrap();
        assert_eq!(r, ("main.rs".to_string(), Some(42)));
    }

    #[test]
    fn test_parse_grep_candidate_deep_path() {
        let r = parse_grep_candidate("packages/engine/src/chunking.rs:100:fn foo()").unwrap();
        assert_eq!(r.0, "chunking.rs");
        assert_eq!(r.1, Some(100));
    }

    #[test]
    fn test_parse_grep_candidate_multiple_colons_in_content() {
        // The match text itself contains colons — only first two splits matter
        let r = parse_grep_candidate("foo.rs:7:url: http://example.com:8080/").unwrap();
        assert_eq!(r.0, "foo.rs");
        assert_eq!(r.1, Some(7));
    }

    #[test]
    fn test_parse_grep_candidate_missing_line_number_returns_none_line() {
        // path:content (no numeric middle field) → file without line number
        let r = parse_grep_candidate("src/lib.rs:some content without a line").unwrap();
        assert_eq!(r.0, "lib.rs");
        assert_eq!(r.1, None);
    }

    #[test]
    fn test_parse_grep_candidate_non_numeric_line_number() {
        // path:abc:content — "abc" is not numeric, so no line
        let r = parse_grep_candidate("src/lib.rs:abc:content").unwrap();
        assert_eq!(r.0, "lib.rs");
        assert_eq!(r.1, None);
    }

    #[test]
    fn test_parse_grep_candidate_empty_match_text() {
        // path:10:  (empty match text after the line number)
        let r = parse_grep_candidate("foo.rs:10:").unwrap();
        assert_eq!(r.0, "foo.rs");
        assert_eq!(r.1, Some(10));
    }

    #[test]
    fn test_parse_grep_candidate_empty_line_returns_none() {
        assert!(
            parse_grep_candidate("").is_none(),
            "empty line must return None"
        );
    }

    #[test]
    fn test_parse_grep_candidate_whitespace_only_returns_none() {
        assert!(
            parse_grep_candidate("   ").is_none(),
            "whitespace-only line must return None"
        );
    }

    #[test]
    fn test_parse_grep_candidate_no_path_like_token_returns_none() {
        // A plain word with no dot or slash is not a path
        assert!(
            parse_grep_candidate("justwords notafile 42").is_none(),
            "no path-like token must return None"
        );
    }

    #[test]
    fn test_parse_grep_candidate_reranked_prefix_returns_none() {
        assert!(
            parse_grep_candidate("[reranked] src/main.rs:1:x").is_none(),
            "[reranked] lines must be filtered out"
        );
    }

    #[test]
    fn test_parse_grep_candidate_large_but_valid_line_number() {
        // u128 max saturates to i64::MAX
        let r = parse_grep_candidate("a.py:12345678901234567890:huge").unwrap();
        assert_eq!(r.1, Some(i64::MAX));
    }

    #[test]
    fn test_parse_grep_candidate_line_1() {
        let r = parse_grep_candidate("foo.rs:1:first line").unwrap();
        assert_eq!(r.1, Some(1));
    }

    #[test]
    fn test_parse_grep_candidate_file_no_path_separator_with_ext() {
        // A filename with extension but no slash still counts as a path via EXT regex
        let r = parse_grep_candidate("readme.md:1:hello").unwrap();
        assert_eq!(r.0, "readme.md");
        assert_eq!(r.1, Some(1));
    }

    // -----------------------------------------------------------------------
    // sed_base / head_window
    // -----------------------------------------------------------------------

    #[test]
    fn test_sed_base_range_format() {
        // sed -n '10,20p'  →  base 10
        assert_eq!(sed_base("sed -n '10,20p' file.rs"), 10);
        assert_eq!(sed_base("sed -n '1,5p' a.txt"), 1);
    }

    #[test]
    fn test_sed_base_single_line_format() {
        // sed -n 42p  →  base 42
        assert_eq!(sed_base("sed -n 42p file.rs"), 42);
    }

    #[test]
    fn test_sed_base_no_match_defaults_to_1() {
        // cat, head, tail without a recognised sed pattern → 1
        assert_eq!(sed_base("cat file.rs"), 1);
        assert_eq!(sed_base("head -n 20 file.rs"), 1);
        assert_eq!(sed_base("some random command"), 1);
    }

    #[test]
    fn test_sed_base_inverted_range_uses_first_number() {
        // SED_RANGE regex extracts the FIRST capture group (lower bound);
        // for an inverted range like '20,10p' it still returns 20.
        assert_eq!(sed_base("sed -n '20,10p' file.rs"), 20);
    }

    #[test]
    fn test_sed_base_overflow_saturates() {
        assert_eq!(
            sed_base("sed -n '99999999999999999999,99999999999999999999p' f.py"),
            i64::MAX
        );
    }

    // -----------------------------------------------------------------------
    // chunk_observation — "other" path (non-search, non-read)
    // -----------------------------------------------------------------------

    fn other_chunks(obs: &str, win: usize) -> Vec<Chunk> {
        // "echo" matches neither SEARCH nor READ regexes → "other" path
        chunk_observation("echo", obs, 0, win, None, ChunkMode::Fixed)
    }

    #[test]
    fn test_chunk_observation_empty_string_produces_one_chunk() {
        let chunks = other_chunks("", 40);
        // Even for empty obs the function guarantees at least one chunk
        assert_eq!(chunks.len(), 1, "empty obs must produce exactly 1 chunk");
        assert_eq!(chunks[0].kind, "other");
    }

    #[test]
    fn test_chunk_observation_whitespace_only_produces_one_chunk() {
        let chunks = other_chunks("   \n\t\n   ", 40);
        // Pure whitespace obs: py_has_content is false for every window so the
        // empty-fallback fires → exactly one "other" chunk
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_chunk_observation_single_long_line_single_chunk() {
        let long_line = "x".repeat(5000);
        let chunks = other_chunks(&long_line, 40);
        // py_splitlines on a no-break string returns a single element;
        // win=40 lines → one chunk (a single line < 40 lines)
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, long_line);
    }

    #[test]
    fn test_chunk_observation_windows_over_multiline_input() {
        // 100 lines, win=40 → ceil(100/40) = 3 chunks
        let obs: String = (1..=100).map(|i| format!("line{}\n", i)).collect();
        let chunks = other_chunks(obs.trim_end_matches('\n'), 40);
        // We can't assert exact count without knowing exact splitlines count
        // but we can assert all lines are covered (full-coverage invariant)
        let reconstructed = chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Original split by lines, windows joined by \n, then joined by \n
        let original_lines = py_splitlines(obs.trim_end_matches('\n'));
        let expected = original_lines.join("\n");
        assert_eq!(
            reconstructed, expected,
            "chunk_observation full-coverage invariant violated"
        );
    }

    // -----------------------------------------------------------------------
    // chunk_assistant — full-coverage invariant
    // -----------------------------------------------------------------------

    fn assert_full_coverage(original: &str, win: usize) {
        let chunks = chunk_assistant(original, 0, win);
        if chunks.is_empty() {
            // chunk_assistant may legitimately return no chunks for whitespace-only input
            assert!(
                !py_has_content(original),
                "non-empty content produced zero chunks for {original:?}"
            );
            return;
        }
        let reconstructed = chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let lines = py_splitlines(original);
        let expected = lines.join("\n");
        assert_eq!(
            reconstructed, expected,
            "full-coverage violated for win={win}: reconstructed != original lines joined"
        );
    }

    #[test]
    fn test_chunk_assistant_empty_string() {
        let chunks = chunk_assistant("", 0, 40);
        assert!(chunks.is_empty(), "empty string → no assistant chunks");
    }

    #[test]
    fn test_chunk_assistant_whitespace_only_no_chunks() {
        let chunks = chunk_assistant("   \n\n  ", 0, 40);
        assert!(
            chunks.is_empty(),
            "whitespace-only text → no assistant chunks"
        );
    }

    #[test]
    fn test_chunk_assistant_single_line_full_coverage() {
        assert_full_coverage("hello world", 40);
    }

    #[test]
    fn test_chunk_assistant_multiline_full_coverage_win1() {
        let txt = "alpha\nbeta\ngamma\ndelta";
        assert_full_coverage(txt, 1);
    }

    #[test]
    fn test_chunk_assistant_multiline_full_coverage_win40() {
        let txt: String = (1..=80)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_full_coverage(&txt, 40);
    }

    #[test]
    fn test_chunk_assistant_very_long_single_line_full_coverage() {
        let long_line = "x".repeat(10_000);
        assert_full_coverage(&long_line, 40);
    }

    #[test]
    fn test_chunk_assistant_large_multiline_full_coverage() {
        let txt: String = (1..=500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_full_coverage(&txt, 40);
    }

    #[test]
    fn test_chunk_assistant_kind_is_asst() {
        let chunks = chunk_assistant("hello\nworld", 0, 40);
        for c in &chunks {
            assert_eq!(
                c.kind, "asst",
                "chunk_assistant must produce 'asst' kind chunks"
            );
        }
    }

    #[test]
    fn test_chunk_observation_full_coverage_invariant_other_path() {
        // Representative inputs across different sizes and break chars
        let inputs: &[&str] = &[
            "single line",
            "line one\nline two\nline three",
            &"x".repeat(5000),
            &(1..=150)
                .map(|i| format!("L{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            // consecutive newlines
            "a\n\nb\n\nc",
            // ends with newline (trailing empty not produced by splitlines)
            "a\nb\n",
        ];
        for input in inputs {
            let chunks = other_chunks(input, 40);
            if chunks.len() == 1 && chunks[0].text == *input {
                // Single-chunk passthrough (empty/whitespace fallback) — OK
                continue;
            }
            let reconstructed = chunks
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let expected = py_splitlines(input).join("\n");
            assert_eq!(
                reconstructed,
                expected,
                "full-coverage invariant failed for input of len={}",
                input.len()
            );
        }
    }
}
