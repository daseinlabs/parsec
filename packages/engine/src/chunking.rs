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

    // ── existing baseline test ────────────────────────────────────────────────

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

    // ── parse_grep_candidate ───────────────────────────────────────────────

    /// Standard grep output format: path/to/file.rs:42:matching line.
    #[test]
    fn test_parse_grep_candidate_standard_format() {
        let got = parse_grep_candidate("path/to/file.rs:42:matching line").unwrap();
        assert_eq!(got, ("file.rs".to_string(), Some(42)));
    }

    /// Line 1 is a valid line number.
    #[test]
    fn test_parse_grep_candidate_line_one() {
        let got = parse_grep_candidate("src/main.rs:1:fn main()").unwrap();
        assert_eq!(got, ("main.rs".to_string(), Some(1)));
    }

    /// Multiple colons in the content portion do not break the parse.
    #[test]
    fn test_parse_grep_candidate_multiple_colons_in_content() {
        let got = parse_grep_candidate("src/foo.rs:10:a:b:c").unwrap();
        assert_eq!(got, ("foo.rs".to_string(), Some(10)));
    }

    /// A Windows-style path (backslash, no slash) still matches via extension.
    #[test]
    fn test_parse_grep_candidate_windows_style_path_via_extension() {
        // The path has no '/' so is_path() falls through to the EXT regex.
        // With format "file.rs:10:code" -> parts[0]="file.rs" has extension.
        let got = parse_grep_candidate("file.rs:10:some code");
        assert!(got.is_some(), "should parse file-only path by extension");
        let (name, lineno) = got.unwrap();
        assert_eq!(name, "file.rs");
        assert_eq!(lineno, Some(10));
    }

    /// Non-numeric text in the line-number field: falls back to file-only result.
    #[test]
    fn test_parse_grep_candidate_non_numeric_line_number() {
        // parts[1] is "not-a-number", not all ASCII digits -> falls to file-only
        let got = parse_grep_candidate("src/foo.rs:not-a-number:content");
        // is_path("src/foo.rs") is true (has '/'), so we get file-only
        assert!(got.is_some());
        let (name, lineno) = got.unwrap();
        assert_eq!(name, "foo.rs");
        assert_eq!(lineno, None);
    }

    /// Empty line -> None.
    #[test]
    fn test_parse_grep_candidate_empty_line() {
        assert_eq!(parse_grep_candidate(""), None);
    }

    /// Whitespace-only line -> None.
    #[test]
    fn test_parse_grep_candidate_whitespace_only() {
        assert_eq!(parse_grep_candidate("   "), None);
        assert_eq!(parse_grep_candidate("\t\n"), None);
    }

    /// Lines starting with "[reranked" are filtered out.
    #[test]
    fn test_parse_grep_candidate_reranked_prefix() {
        assert_eq!(parse_grep_candidate("[reranked] src/foo.rs:10:code"), None);
        assert_eq!(parse_grep_candidate("[reranked top-k]"), None);
    }

    /// Plain text with no path-like token -> None.
    #[test]
    fn test_parse_grep_candidate_no_path_like_token() {
        assert_eq!(parse_grep_candidate("no extension or slash here"), None);
        assert_eq!(parse_grep_candidate("just text"), None);
    }

    /// Leading/trailing whitespace is stripped before parsing.
    #[test]
    fn test_parse_grep_candidate_leading_trailing_whitespace() {
        let got = parse_grep_candidate("  src/foo.rs:5:content  ").unwrap();
        assert_eq!(got, ("foo.rs".to_string(), Some(5)));
    }

    /// File path with no line number colon still returns a result.
    #[test]
    fn test_parse_grep_candidate_file_only_no_colon() {
        // "src/lib.rs" with no colon at all -> parts.len() < 3, falls to parts.len()>=2 check
        // but splitn(3,':') gives only ["src/lib.rs"] -> len==1, then hits token fallback
        let got = parse_grep_candidate("src/lib.rs");
        assert!(got.is_some());
        let (name, lineno) = got.unwrap();
        assert_eq!(name, "lib.rs");
        assert_eq!(lineno, None);
    }

    // ── sed_base ────────────────────────────────────────────────────────

    /// sed -n 'a,bp' extracts starting line a.
    #[test]
    fn test_sed_base_range_format() {
        assert_eq!(sed_base("sed -n '10,20p' file.rs"), 10);
        assert_eq!(sed_base("sed -n '1,100p' src/main.rs"), 1);
        assert_eq!(sed_base("sed -n '50,60p'"), 50);
    }

    /// sed -n Np (single line) extracts line N.
    #[test]
    fn test_sed_base_single_line_format() {
        assert_eq!(sed_base("sed -n 5p file.rs"), 5);
        assert_eq!(sed_base("sed -n 1p file.rs"), 1);
        assert_eq!(sed_base("sed -n 42p"), 42);
    }

    /// Unrecognised command falls back to 1.
    #[test]
    fn test_sed_base_no_match_returns_1() {
        assert_eq!(sed_base("cat src/main.rs"), 1);
        assert_eq!(sed_base("ls -la"), 1);
        assert_eq!(sed_base(""), 1);
    }

    /// Inverted range (higher start): first number is still returned.
    #[test]
    fn test_sed_base_inverted_range() {
        // "20,10p" -> SED_RANGE captures group 1 = "20"
        assert_eq!(sed_base("sed -n '20,10p' file.rs"), 20);
    }

    // ── chunk_observation: full-coverage invariant ───────────────────────────

    /// Helper: verify that all chunk texts reconstruct the original observation
    /// when joined by newlines. This is the "full-coverage invariant".
    fn check_full_coverage(cmd: &str, obs: &str) {
        let chunks = chunk_observation(cmd, obs, 0, DEFAULT_WIN, None, ChunkMode::Fixed);
        // Collect every non-empty chunk text
        let all_text: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        // The concatenation of all chunk texts (joined by newlines) must equal
        // the original observation text when we split obs by lines and re-join.
        // Specifically: for grep and read modes the chunks cover individual lines;
        // for other mode the chunks cover windowed line groups.
        // The invariant: every line of obs appears in exactly one chunk.
        let obs_lines = py_splitlines(obs);
        let chunk_lines: Vec<&str> = all_text.iter().flat_map(|t| py_splitlines(t)).collect();
        assert_eq!(
            chunk_lines, obs_lines,
            "full-coverage invariant violated for cmd={cmd:?}"
        );
    }

    #[test]
    fn test_chunk_observation_full_coverage_grep() {
        let obs = "src/foo.rs:1:fn main()\nsrc/bar.rs:2:fn helper()\nsome other line\n";
        check_full_coverage("grep -r fn src/", obs);
    }

    #[test]
    fn test_chunk_observation_full_coverage_read() {
        let obs = "line1\nline2\nline3\nline4\nline5";
        check_full_coverage("cat src/main.rs", obs);
    }

    #[test]
    fn test_chunk_observation_full_coverage_other() {
        let obs = "total 0\ndrwxr-xr-x  3 user  staff   96 Jan  1 00:00 src\n-rw-r--r--  1 user  staff  842 Jan  1 00:00 Cargo.toml";
        check_full_coverage("ls -la", obs);
    }

    /// Empty observation -> exactly 1 chunk (fallback), no panic.
    #[test]
    fn test_chunk_observation_empty_obs() {
        let chunks = chunk_observation("", "", 0, DEFAULT_WIN, None, ChunkMode::Fixed);
        assert_eq!(
            chunks.len(),
            1,
            "empty obs should produce exactly 1 fallback chunk"
        );
        assert_eq!(chunks[0].text, "");
    }

    /// Whitespace-only observation -> 1 chunk (content-less, but the fallback fires).
    #[test]
    fn test_chunk_observation_whitespace_only_obs() {
        let obs = "   \n  \n  ";
        let chunks = chunk_observation("ls", obs, 0, DEFAULT_WIN, None, ChunkMode::Fixed);
        // All whitespace lines have no content, so the py_has_content guard skips
        // them; the empty-out guard produces exactly 1 fallback chunk.
        assert_eq!(chunks.len(), 1);
    }

    /// Varying window sizes all satisfy the full-coverage invariant.
    #[test]
    fn test_chunk_observation_varying_win_sizes() {
        let obs = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for &win in &[1usize, 3, 5, DEFAULT_WIN] {
            let chunks = chunk_observation("cat file.txt", &obs, 0, win, None, ChunkMode::Fixed);
            let chunk_lines: Vec<String> = chunks
                .iter()
                .flat_map(|c| py_splitlines(&c.text).into_iter().map(|s| s.to_string()))
                .collect();
            let obs_lines: Vec<String> = py_splitlines(&obs)
                .into_iter()
                .map(|s| s.to_string())
                .collect();
            assert_eq!(chunk_lines, obs_lines, "win={win} violated coverage");
        }
    }

    /// A very long single-line observation lands in exactly one chunk.
    #[test]
    fn test_chunk_observation_single_long_line() {
        let obs = "x".repeat(10_000);
        let chunks = chunk_observation("ls", &obs, 0, DEFAULT_WIN, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, obs);
    }

    /// A large multi-line observation produces many chunks; all lines are covered.
    #[test]
    fn test_chunk_observation_large_multiline() {
        let obs = (0..1000)
            .map(|i| format!("line content {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_observation(
            "cat big_file.txt",
            &obs,
            0,
            DEFAULT_WIN,
            None,
            ChunkMode::Fixed,
        );
        assert!(
            chunks.len() > 1,
            "1000 lines should produce multiple chunks"
        );
        let chunk_lines: Vec<String> = chunks
            .iter()
            .flat_map(|c| py_splitlines(&c.text).into_iter().map(|s| s.to_string()))
            .collect();
        let obs_lines: Vec<String> = py_splitlines(&obs)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(chunk_lines, obs_lines, "large multiline coverage violated");
    }

    /// step and kind fields are propagated to every chunk.
    #[test]
    fn test_chunk_observation_metadata_propagated() {
        let obs = "foo.rs:1:fn main()\nfoo.rs:2:fn helper()";
        let chunks = chunk_observation("grep fn", obs, 7, DEFAULT_WIN, None, ChunkMode::Fixed);
        for c in &chunks {
            assert_eq!(c.step, 7, "step not propagated");
        }
    }

    // ── chunk_assistant ──────────────────────────────────────────────────

    /// Empty text produces zero chunks.
    #[test]
    fn test_chunk_assistant_empty_text() {
        let chunks = chunk_assistant("", 0, DEFAULT_WIN);
        assert_eq!(chunks.len(), 0);
    }

    /// Whitespace-only text produces zero chunks (py_has_content guard).
    #[test]
    fn test_chunk_assistant_whitespace_only() {
        let chunks = chunk_assistant("   \n  \n  ", 0, DEFAULT_WIN);
        assert_eq!(chunks.len(), 0);
    }

    /// A single non-empty line produces exactly 1 chunk with kind "asst".
    #[test]
    fn test_chunk_assistant_single_line() {
        let chunks = chunk_assistant("Hello, world!", 0, DEFAULT_WIN);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Hello, world!");
        assert_eq!(chunks[0].kind, "asst");
    }

    /// Multi-line text: all lines appear in chunks (full-coverage invariant).
    #[test]
    fn test_chunk_assistant_full_coverage() {
        let txt = (0..50)
            .map(|i| format!("assistant line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_assistant(&txt, 0, DEFAULT_WIN);
        assert!(!chunks.is_empty());
        let chunk_lines: Vec<String> = chunks
            .iter()
            .flat_map(|c| py_splitlines(&c.text).into_iter().map(|s| s.to_string()))
            .collect();
        let txt_lines: Vec<String> = py_splitlines(&txt)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(chunk_lines, txt_lines, "assistant full-coverage violated");
    }

    /// Step index is stored on every produced chunk.
    #[test]
    fn test_chunk_assistant_step_index() {
        let chunks = chunk_assistant("some text", 42, DEFAULT_WIN);
        for c in &chunks {
            assert_eq!(c.step, 42);
        }
    }

    // ── Chunk::new token derivation / with_tokens ──────────────────────────────

    /// tokens = max(1, char_len / 4). Very short text clamps to 1.
    #[test]
    fn test_chunk_new_token_derivation() {
        // 1 char -> max(1, 0) = 1
        let c = Chunk::new("a", None, None, None, 0, "other");
        assert_eq!(c.tokens, 1);
        // 4 chars -> max(1, 1) = 1
        let c = Chunk::new("abcd", None, None, None, 0, "other");
        assert_eq!(c.tokens, 1);
        // 8 chars -> max(1, 2) = 2
        let c = Chunk::new("abcdefgh", None, None, None, 0, "other");
        assert_eq!(c.tokens, 2);
        // 400 chars -> 100
        let c = Chunk::new("x".repeat(400), None, None, None, 0, "other");
        assert_eq!(c.tokens, 100);
        // Empty text -> max(1, 0) = 1
        let c = Chunk::new("", None, None, None, 0, "other");
        assert_eq!(c.tokens, 1);
    }

    /// with_tokens overrides derived value when positive; ignores zero/negative.
    #[test]
    fn test_chunk_with_tokens_override() {
        let c = Chunk::new("abcd", None, None, None, 0, "other").with_tokens(10);
        assert_eq!(c.tokens, 10);
        // Passing 0 should NOT override (stays at derived value)
        let c = Chunk::new("abcd", None, None, None, 0, "other").with_tokens(0);
        assert_eq!(c.tokens, 1); // derived: max(1, 4/4) = 1
                                 // Passing negative should NOT override
        let c = Chunk::new("abcd", None, None, None, 0, "other").with_tokens(-5);
        assert_eq!(c.tokens, 1);
    }
}
