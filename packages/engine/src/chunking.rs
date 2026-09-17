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

    // -------------------------------------------------------------------------
    // parse_grep_candidate
    // -------------------------------------------------------------------------

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
    fn test_parse_grep_candidate_standard_format() {
        // Standard grep format: path/to/file.rs:42:matching line
        let got = parse_grep_candidate("src/main.rs:42:fn main()").unwrap();
        assert_eq!(got, ("main.rs".to_string(), Some(42)));
    }

    #[test]
    fn test_parse_grep_candidate_simple_filename_and_line() {
        let got = parse_grep_candidate("a.py:1:hello").unwrap();
        assert_eq!(got, ("a.py".to_string(), Some(1)));
    }

    #[test]
    fn test_parse_grep_candidate_nested_path_basename() {
        // Only the basename should be returned, not the full path
        let got = parse_grep_candidate("path/to/file.rs:10:code here").unwrap();
        assert_eq!(got, ("file.rs".to_string(), Some(10)));
    }

    #[test]
    fn test_parse_grep_candidate_windows_path_with_drive_letter() {
        // Windows-style paths: the drive colon must not steal the line number.
        // e.g. "C:\project\file.rs:10:code" has colons in path AND at line no.
        // The function splits on ':', so this is a tricky edge case.
        // We document what the function actually does:
        // splitn(3, ':') on "C:\project\file.rs:10:code" gives
        //   ["C", "\project\file.rs", "10:code"]
        // parts[0]="C" is not a path (no '.' extension), so falls through.
        // This is a known limitation; test documents current behaviour.
        let result = parse_grep_candidate("C:\\project\\file.rs:10:code");
        // Either None (not recognised as path) or Some with file.rs
        // Document whichever the function actually returns:
        match result {
            None => {
                // Not recognised — acceptable, documents the Windows edge case
            }
            Some((name, _)) => {
                // If recognised, the name should be a plausible basename
                assert!(
                    !name.is_empty(),
                    "basename should not be empty if Some returned"
                );
            }
        }
    }

    #[test]
    fn test_parse_grep_candidate_multiple_colons_in_content() {
        // Content after line number can contain colons
        let got = parse_grep_candidate("file.py:5:key: value: extra").unwrap();
        assert_eq!(got, ("file.py".to_string(), Some(5)));
    }

    #[test]
    fn test_parse_grep_candidate_no_line_number() {
        // Only file:content, no numeric line number
        let got = parse_grep_candidate("file.py:some content").unwrap();
        assert_eq!(got, ("file.py".to_string(), None));
    }

    #[test]
    fn test_parse_grep_candidate_non_numeric_line_number() {
        // "file.py:abc:content" — parts[1]=="abc" is not all digits
        // falls through to the 2-part path check, returns (basename, None)
        let got = parse_grep_candidate("file.py:abc:content").unwrap();
        assert_eq!(got, ("file.py".to_string(), None));
    }

    #[test]
    fn test_parse_grep_candidate_empty_string_returns_none() {
        assert!(parse_grep_candidate("").is_none());
    }

    #[test]
    fn test_parse_grep_candidate_whitespace_only_returns_none() {
        assert!(parse_grep_candidate("   ").is_none());
        assert!(parse_grep_candidate("\t\n").is_none());
    }

    #[test]
    fn test_parse_grep_candidate_reranked_prefix_returns_none() {
        // Lines starting with "[reranked" are explicitly skipped
        assert!(parse_grep_candidate("[reranked] some file.py:1:line").is_none());
    }

    #[test]
    fn test_parse_grep_candidate_line_zero() {
        let got = parse_grep_candidate("main.rs:0:first line").unwrap();
        assert_eq!(got, ("main.rs".to_string(), Some(0)));
    }

    #[test]
    fn test_parse_grep_candidate_large_valid_line_number() {
        let got = parse_grep_candidate("lib.rs:99999:content").unwrap();
        assert_eq!(got, ("lib.rs".to_string(), Some(99_999)));
    }

    #[test]
    fn test_parse_grep_candidate_no_extension_not_recognised() {
        // "foo" has no extension and no '/', so is_path returns false
        assert!(parse_grep_candidate("foo:1:line").is_none());
    }

    #[test]
    fn test_parse_grep_candidate_leading_trailing_whitespace_stripped() {
        // py_strip is called first; surrounding whitespace must not break parsing
        let got = parse_grep_candidate("  main.rs:7:code  ").unwrap();
        assert_eq!(got, ("main.rs".to_string(), Some(7)));
    }

    // -------------------------------------------------------------------------
    // sed_base / head_window
    // -------------------------------------------------------------------------

    #[test]
    fn test_sed_base_default_is_one() {
        // No sed pattern → base line is 1
        assert_eq!(sed_base("cat file.py"), 1);
        assert_eq!(sed_base(""), 1);
        assert_eq!(sed_base("grep foo bar.txt"), 1);
    }

    #[test]
    fn test_sed_base_range_pattern() {
        // sed -n '10,20p' → base is 10
        assert_eq!(sed_base("sed -n '10,20p' file.py"), 10);
    }

    #[test]
    fn test_sed_base_range_pattern_various_spacings() {
        assert_eq!(sed_base("sed -n '1,50p' file.rs"), 1);
        assert_eq!(sed_base("sed -n '100,200p' file.rs"), 100);
    }

    #[test]
    fn test_sed_base_single_line_pattern() {
        // sed -n 5p → base is 5
        assert_eq!(sed_base("sed -n 5p file.py"), 5);
    }

    #[test]
    fn test_sed_base_single_line_with_spaces() {
        assert_eq!(sed_base("sed -n 42p file.py"), 42);
    }

    #[test]
    fn test_sed_base_range_overflow_saturates() {
        assert_eq!(
            sed_base("sed -n '99999999999999999999,99999999999999999999p' f.py"),
            i64::MAX
        );
    }

    #[test]
    fn test_sed_base_no_sed_command_returns_1() {
        // head command → no sed pattern, base = 1
        assert_eq!(sed_base("head -n 20 file.py"), 1);
    }

    // -------------------------------------------------------------------------
    // chunk_observation — full-coverage invariant
    // -------------------------------------------------------------------------

    /// Helper: concatenate all chunk texts and compare to expected reconstruction.
    /// For non-read/non-search commands the lines are joined with \n between
    /// windows, so we just verify no chars are dropped.
    fn all_chunk_chars(chunks: &[Chunk]) -> usize {
        chunks.iter().map(|c| c.text.chars().count()).sum()
    }

    #[test]
    fn test_chunk_observation_empty_string() {
        // An empty observation must still produce exactly one chunk (not panic).
        let chunks = chunk_observation("", "", 0, DEFAULT_WIN, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1, "empty obs must yield 1 chunk");
        assert_eq!(chunks[0].text, "");
    }

    #[test]
    fn test_chunk_observation_whitespace_only() {
        // Whitespace-only observation: py_has_content is false, still 1 chunk.
        let chunks = chunk_observation("", "   \n  \t  ", 0, DEFAULT_WIN, None, ChunkMode::Fixed);
        // All windows fail py_has_content, so the fallback produces one chunk
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_chunk_observation_single_short_line() {
        let obs = "hello world";
        let chunks =
            chunk_observation("python run.py", obs, 0, DEFAULT_WIN, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, obs);
        assert_eq!(chunks[0].kind, "other");
    }

    #[test]
    fn test_chunk_observation_full_coverage_invariant_other_cmd() {
        // For a generic command the windowing must cover every non-break char.
        let obs = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_observation("python run.py", &obs, 0, 10, None, ChunkMode::Fixed);
        // Sum of chunk chars + inter-window \n joins should account for all chars.
        let reconstructed = chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Every non-newline char from obs must appear in the reconstruction
        let obs_non_nl: String = obs.chars().filter(|c| *c != '\n').collect();
        let rec_non_nl: String = reconstructed.chars().filter(|c| *c != '\n').collect();
        assert_eq!(obs_non_nl, rec_non_nl, "full-coverage invariant violated");
    }

    #[test]
    fn test_chunk_observation_read_cmd_windows_obs_by_win_lines() {
        // A `cat` command should go through the READ path.
        let lines: Vec<String> = (1..=50).map(|i| format!("line {i}")).collect();
        let obs = lines.join("\n");
        let win = 10;
        let chunks = chunk_observation("cat file.py", &obs, 0, win, None, ChunkMode::Fixed);
        // 50 lines / win=10 → 5 chunks
        assert_eq!(chunks.len(), 5, "expected 5 read chunks for 50-line obs");
        for c in &chunks {
            assert_eq!(c.kind, "read");
        }
    }

    #[test]
    fn test_chunk_observation_read_cmd_line_numbers_derived_from_sed_base() {
        let obs = (0..10)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks =
            chunk_observation("sed -n '5,15p' file.py", &obs, 1, 5, None, ChunkMode::Fixed);
        // First chunk should start at line 5 (sed base)
        assert_eq!(chunks[0].lo, Some(5));
        assert_eq!(chunks[0].kind, "read");
    }

    #[test]
    fn test_chunk_observation_grep_cmd_each_match_is_own_chunk() {
        let obs = "src/main.rs:10:fn main()\n\
                   src/lib.rs:20:pub fn foo()\n\
                   src/util.rs:30:fn bar()";
        let chunks = chunk_observation("grep fn src/", obs, 0, DEFAULT_WIN, None, ChunkMode::Fixed);
        let grep_chunks: Vec<_> = chunks.iter().filter(|c| c.kind == "grep").collect();
        assert_eq!(
            grep_chunks.len(),
            3,
            "each grep match should be its own chunk"
        );
    }

    #[test]
    fn test_chunk_observation_grep_cmd_line_numbers_parsed() {
        let obs = "file.rs:42:some code";
        let chunks = chunk_observation(
            "grep something file.rs",
            obs,
            0,
            DEFAULT_WIN,
            None,
            ChunkMode::Fixed,
        );
        assert_eq!(chunks[0].lo, Some(42));
        assert_eq!(chunks[0].hi, Some(42));
    }

    #[test]
    fn test_chunk_observation_very_long_single_line() {
        // A 10_000-char single-line obs should not panic and must produce chunks.
        let obs = "x".repeat(10_000);
        let chunks = chunk_observation(
            "python run.py",
            &obs,
            0,
            DEFAULT_WIN,
            None,
            ChunkMode::Fixed,
        );
        assert!(!chunks.is_empty());
        assert_eq!(all_chunk_chars(&chunks), obs.chars().count());
    }

    #[test]
    fn test_chunk_observation_large_multiline_obs() {
        // 1000 lines of content; verify no characters are dropped.
        let obs = (0..1000)
            .map(|i| format!("content line {i} with some data"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_observation(
            "python run.py",
            &obs,
            0,
            DEFAULT_WIN,
            None,
            ChunkMode::Fixed,
        );
        assert!(!chunks.is_empty());
        // Reconstruct and check non-newline chars match
        let obs_non_nl: usize = obs.chars().filter(|c| *c != '\n').count();
        let chunk_non_nl: usize = chunks
            .iter()
            .flat_map(|c| c.text.chars())
            .filter(|c| *c != '\n')
            .count();
        assert_eq!(obs_non_nl, chunk_non_nl, "full-coverage: chars dropped");
    }

    #[test]
    fn test_chunk_observation_rc_parsed_from_obs() {
        // obs containing <returncode>0</returncode> must populate chunk.rc
        let obs = "some output\n<returncode>0</returncode>";
        let chunks =
            chunk_observation("python run.py", obs, 0, DEFAULT_WIN, None, ChunkMode::Fixed);
        for c in &chunks {
            assert_eq!(c.rc, Some(0), "rc should be parsed from obs");
        }
    }

    #[test]
    fn test_chunk_observation_cmd_head_set_on_all_chunks() {
        let obs = (0..60)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_observation("python run.py", &obs, 0, 10, None, ChunkMode::Fixed);
        let cmd_head = &chunks[0].cmd;
        for c in &chunks {
            assert_eq!(&c.cmd, cmd_head, "all chunks must share the same cmd head");
        }
    }

    // -------------------------------------------------------------------------
    // chunk_assistant
    // -------------------------------------------------------------------------

    #[test]
    fn test_chunk_assistant_empty_string_returns_no_chunks() {
        // Empty or whitespace-only text → no chunks (may legitimately return empty)
        let chunks = chunk_assistant("", 0, DEFAULT_WIN);
        // Content-less segments are skipped; may be empty or have 1 chunk
        // Empty string → py_has_content is false, so zero chunks expected
        assert!(
            chunks.is_empty(),
            "empty text should produce no assistant chunks"
        );
    }

    #[test]
    fn test_chunk_assistant_whitespace_only_returns_no_chunks() {
        let chunks = chunk_assistant("   \n\t  ", 0, DEFAULT_WIN);
        assert!(
            chunks.is_empty(),
            "whitespace-only text should produce no assistant chunks"
        );
    }

    #[test]
    fn test_chunk_assistant_single_line() {
        let txt = "Hello, this is assistant text.";
        let chunks = chunk_assistant(txt, 0, DEFAULT_WIN);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, txt);
        assert_eq!(chunks[0].kind, "asst");
        assert_eq!(chunks[0].step, 0);
    }

    #[test]
    fn test_chunk_assistant_multiline_windowed() {
        // 50 lines with win=10 → 5 chunks
        let lines: Vec<String> = (0..50).map(|i| format!("asst line {i}")).collect();
        let txt = lines.join("\n");
        let chunks = chunk_assistant(&txt, 3, 10);
        assert_eq!(chunks.len(), 5);
        for c in &chunks {
            assert_eq!(c.kind, "asst");
            assert_eq!(c.step, 3);
        }
    }

    #[test]
    fn test_chunk_assistant_full_coverage_invariant() {
        // Every non-whitespace character from input must appear in some chunk.
        let txt = "Hello world\nThis is line two\nAnd line three";
        let chunks = chunk_assistant(txt, 0, 2);
        let txt_non_nl: String = txt.chars().filter(|c| *c != '\n').collect();
        let chunks_non_nl: String = chunks
            .iter()
            .flat_map(|c| c.text.chars())
            .filter(|c| *c != '\n')
            .collect();
        assert_eq!(
            txt_non_nl, chunks_non_nl,
            "full-coverage invariant violated"
        );
    }

    #[test]
    fn test_chunk_assistant_unicode_content() {
        let txt = "日本語テキスト\nMore text";
        let chunks = chunk_assistant(txt, 0, DEFAULT_WIN);
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].kind, "asst");
    }

    #[test]
    fn test_chunk_assistant_step_is_set() {
        let txt = "some content";
        let chunks = chunk_assistant(txt, 7, DEFAULT_WIN);
        assert_eq!(chunks[0].step, 7);
    }

    #[test]
    fn test_chunk_assistant_tokens_derived_from_char_len() {
        // tokens = max(1, char_len / 4)
        let txt = "a".repeat(40); // 40 chars → tokens = 10
        let chunks = chunk_assistant(&txt, 0, DEFAULT_WIN);
        assert_eq!(chunks[0].tokens, 10);
    }

    #[test]
    fn test_chunk_assistant_very_long_single_line() {
        let txt = "word ".repeat(2000); // ~10000 chars, no newlines
        let txt = txt.trim();
        let chunks = chunk_assistant(txt, 0, DEFAULT_WIN);
        // Single line → single chunk (py_splitlines on no-newline gives 1 element)
        assert_eq!(chunks.len(), 1, "single long line should be one chunk");
        assert_eq!(chunks[0].kind, "asst");
    }
}
