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

    #[test]
    fn test_parse_grep_candidate() {
        // Basic case
        assert_eq!(
            parse_grep_candidate("path/to/file.rs:42:matching line"),
            Some(("file.rs".to_string(), Some(42)))
        );

        // Windows path (it will likely parse as parts[0]="C", parts[1]="\\project\\file.rs" which is not digits)
        assert_eq!(parse_grep_candidate("C:\\project\\file.rs:10:code"), None);

        // Multiple colons
        assert_eq!(
            parse_grep_candidate("file.rs:42:let x = \"a:b:c\";"),
            Some(("file.rs".to_string(), Some(42)))
        );

        // Missing line number (parts[1] is not digits -> fallback to parts[0] as path with None line)
        assert_eq!(
            parse_grep_candidate("file.rs:hello:code"),
            Some(("file.rs".to_string(), None))
        );

        // No matching text
        assert_eq!(
            parse_grep_candidate("file.rs:42:"),
            Some(("file.rs".to_string(), Some(42)))
        );

        // Completely invalid input
        assert_eq!(parse_grep_candidate("hello world"), None);
    }

    #[test]
    fn test_sed_base() {
        // Standard formats
        assert_eq!(sed_base("sed -n '10,20p' file.rs"), 10);
        assert_eq!(sed_base("sed -n \"10,20p\" file.rs"), 10);
        assert_eq!(sed_base("sed -n 10,20p file.rs"), 10);

        // Single line (SED_ONE regex)
        assert_eq!(sed_base("sed -n 42p file.rs"), 42);
        // Note: SED_ONE doesn't handle quotes around the single line number according to its regex,
        // so `sed -n '42p'` evaluates to 1. This tests the actual implemented contract!
        assert_eq!(sed_base("sed -n '42p' file.rs"), 1);

        // Unusual spacing
        assert_eq!(sed_base("sed    -n    '10,20p'"), 10);

        // Edge cases (boundaries/reversals)
        assert_eq!(sed_base("sed -n '10,10p'"), 10);
        assert_eq!(sed_base("sed -n '20,10p'"), 20); // Captures the first number

        // Invalid/Malformed numbers
        assert_eq!(sed_base("sed -n 'abc,20p'"), 1);
        assert_eq!(sed_base("sed -n '10,abcp'"), 1);
    }

    #[test]
    fn test_chunk_observation() {
        // Empty
        let empty = chunk_observation("echo", "", 0, 10, None, ChunkMode::Fixed);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].text, "");

        // One line
        let one = chunk_observation("echo", "hello", 0, 10, None, ChunkMode::Fixed);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].text, "hello");

        // Multiple lines (win = 10)
        let multi = chunk_observation("echo", "line1\nline2\nline3", 0, 10, None, ChunkMode::Fixed);
        assert_eq!(multi.len(), 1);
        assert_eq!(multi[0].text, "line1\nline2\nline3");

        // Multiple lines (win = 2)
        let multi_win2 =
            chunk_observation("echo", "line1\nline2\nline3", 0, 2, None, ChunkMode::Fixed);
        assert_eq!(multi_win2.len(), 2);
        assert_eq!(multi_win2[0].text, "line1\nline2");
        assert_eq!(multi_win2[1].text, "line3");

        // Multiple lines (win = 1)
        let multi_win1 =
            chunk_observation("echo", "line1\nline2\nline3", 0, 1, None, ChunkMode::Fixed);
        assert_eq!(multi_win1.len(), 3);
        assert_eq!(multi_win1[0].text, "line1");
        assert_eq!(multi_win1[1].text, "line2");
        assert_eq!(multi_win1[2].text, "line3");

        // Whitespace (tests fallback to raw obs when all chunks are empty)
        let ws = chunk_observation("echo", "   \n\t\n   ", 0, 1, None, ChunkMode::Fixed);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].text, "   \n\t\n   ");

        // Very long line
        let long_line = "a".repeat(10000);
        let long_obs = chunk_observation("echo", &long_line, 0, 10, None, ChunkMode::Fixed);
        assert_eq!(long_obs.len(), 1);
        assert_eq!(long_obs[0].text, long_line);
    }

    #[test]
    fn test_chunk_observation_preserves_full_content() {
        let original = "Hello\nWorld\nThis is Parsec";

        for win in [1, 2, 3, 10] {
            let chunks = chunk_observation("echo", original, 0, win, None, ChunkMode::Fixed);
            let reconstructed = chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");

            assert_eq!(
                reconstructed, original,
                "Failed full-coverage invariant for win={}",
                win
            );
        }
    }

    #[test]
    fn test_chunk_assistant_preserves_full_content() {
        let original = "Hello\nWorld\nThis is Parsec";

        for win in [1, 2, 3, 10] {
            let chunks = chunk_assistant(original, 0, win);
            let reconstructed = chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");

            assert_eq!(
                reconstructed, original,
                "Failed full-coverage invariant for win={}",
                win
            );
        }
    }

    #[test]
    fn test_chunk_assistant() {
        // "Hello"
        let single = chunk_assistant("Hello", 0, 10);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].text, "Hello");

        // "Hello\nWorld"
        let multi = chunk_assistant("Hello\nWorld", 0, 10);
        assert_eq!(multi.len(), 1);
        assert_eq!(multi[0].text, "Hello\nWorld");

        // Large output
        let large = "a".repeat(10000);
        let large_chunks = chunk_assistant(&large, 0, 10);
        assert_eq!(large_chunks.len(), 1);
        assert_eq!(large_chunks[0].text, large);

        // "" (empty string)
        let empty = chunk_assistant("", 0, 10);
        assert_eq!(empty.len(), 0); // "May legitimately return no chunks"

        // whitespace-only content
        let ws = chunk_assistant("   \n\t\n   ", 0, 10);
        assert_eq!(ws.len(), 0); // "May legitimately return no chunks"

        // Unicode + emoji + newlines + Reconstruction verification
        let unicode_text = "Hello 😀\nこんにちは\n你好";

        for win in [1, 2, 10] {
            let chunks = chunk_assistant(unicode_text, 0, win);
            let reconstructed = chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");

            assert_eq!(reconstructed, unicode_text);
        }
    }
}
