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


    /// Helper: join chunk texts back together with "\n", the same separator
    /// the windowing/grep code uses between adjacent lines/chunks. Only
    /// meaningful for checking exact reconstruction when no lines were
    /// dropped (see the blank-line tests below for cases where it does NOT
    /// hold, by design).
    fn reconstruct(chunks: &[Chunk]) -> String {
        chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ------------------------------------------------------------
    // parse_grep_candidate
    // ------------------------------------------------------------

    #[test]
    fn test_grep_standard_path_line_and_text() {
        let got = parse_grep_candidate("path/to/file.rs:42:matching line").unwrap();
        assert_eq!(got, ("file.rs".to_string(), Some(42)));
    }

    #[test]
    fn test_grep_extension_only_no_slash_is_still_a_path() {
        // No '/', but a recognized extension is enough to count as a path.
        let got = parse_grep_candidate("main.rs:7:content").unwrap();
        assert_eq!(got, ("main.rs".to_string(), Some(7)));
    }

    #[test]
    fn test_grep_windows_drive_letter_path_not_recognized() {
        // Known limitation: the leading "C:" is itself parsed as a colon
        // field, "C" isn't a recognized path, and the whole-line fallback
        // doesn't match a trailing extension either (it ends in ":code",
        // not ".ext"). This documents current behavior, not a should-be.
        assert_eq!(
            parse_grep_candidate(r"C:\project\file.rs:10:code"),
            None
        );
    }

    #[test]
    fn test_grep_multiple_colons_in_matched_content() {
        // Extra colons inside the matched text (third splitn field) don't
        // break parsing — splitn(3, ':') leaves them untouched.
        let got = parse_grep_candidate("src/a.py:5:note: foo: bar: baz").unwrap();
        assert_eq!(got, ("a.py".to_string(), Some(5)));
    }

    #[test]
    fn test_grep_single_colon_no_line_number() {
        // Exactly one colon: falls to the two-part branch, no line number.
        let got = parse_grep_candidate("src/main.rs:build failed").unwrap();
        assert_eq!(got, ("main.rs".to_string(), None));
    }

    #[test]
    fn test_grep_bare_path_no_colon_at_all() {
        let got = parse_grep_candidate("src/main.rs").unwrap();
        assert_eq!(got, ("main.rs".to_string(), None));
    }

    #[test]
    fn test_grep_non_numeric_line_field_falls_back_to_no_line_number() {
        let got = parse_grep_candidate("src/main.rs:abc:message").unwrap();
        assert_eq!(got, ("main.rs".to_string(), None));
    }

    #[test]
    fn test_grep_empty_line_returns_none() {
        assert_eq!(parse_grep_candidate(""), None);
    }

    #[test]
    fn test_grep_whitespace_only_line_returns_none() {
        assert_eq!(parse_grep_candidate("   \t  "), None);
        // Extended whitespace (py_strip's \x1c..\x1f set) counts too.
        assert_eq!(parse_grep_candidate("\x1c\x1d\x1e\x1f"), None);
    }

    #[test]
    fn test_grep_reranked_marker_returns_none() {
        assert_eq!(parse_grep_candidate("[reranked results below]"), None);
    }

    #[test]
    fn test_grep_free_text_with_no_path_returns_none() {
        assert_eq!(parse_grep_candidate("just some free text here"), None);
    }

    #[test]
    fn test_grep_spaced_colons_lose_the_numeric_field() {
        // Spaces around the colons break the `parts[1]` adjacency needed for
        // the file:line:text form, AND break is_path's extension match on
        // "file.py " (trailing space). Falls all the way to the
        // whitespace-token fallback, which recovers the path but not the
        // line number. Documents current (not obviously "correct") behavior.
        let got = parse_grep_candidate("file.py : 10 : text").unwrap();
        assert_eq!(got, ("file.py".to_string(), None));
    }

    #[test]
    fn test_grep_trailing_colon_token_is_trimmed_in_fallback() {
        // Fallback path: single whitespace-separated token ending in ':'
        // with no other colons to split on.
        let got = parse_grep_candidate("src/util.py").unwrap();
        assert_eq!(got, ("util.py".to_string(), None));
    }

    // ------------------------------------------------------------
    // sed_base
    // ------------------------------------------------------------

    #[test]
    fn test_sed_base_dash_n_range() {
        assert_eq!(sed_base("sed -n '15,30p' file.py"), 15);
    }

    #[test]
    fn test_sed_base_dash_n_single_line() {
        assert_eq!(sed_base("sed -n 42p file.py"), 42);
    }

    #[test]
    fn test_sed_base_inverted_range_takes_first_number_unvalidated() {
        // No ordering check: whichever number comes first in "a,bp" wins,
        // even if a > b.
        assert_eq!(sed_base("sed -n '50,10p' file.py"), 50);
    }

    #[test]
    fn test_sed_base_no_sed_pattern_defaults_to_one() {
        assert_eq!(sed_base("cat file.py"), 1);
        assert_eq!(sed_base(""), 1);
        assert_eq!(sed_base("head -n 20 file.py"), 1);
    }

    #[test]
    fn test_sed_base_range_pattern_matches_without_sed_keyword() {
        // SED_RANGE has no "sed" anchor: any bare "N,Mp" substring matches,
        // even in an unrelated command. Documented quirk.
        assert_eq!(sed_base("head -c 100 3,7p garbage"), 3);
    }

    #[test]
    fn test_sed_base_range_takes_priority_over_single_form() {
        // SED_RANGE is checked first regardless of where each pattern sits
        // in the string.
        assert_eq!(sed_base("sed -n 5p; sed -n '10,20p' other"), 10);
    }

    #[test]
    fn test_sed_base_extra_whitespace_and_control_char_separators() {
        // The [\s\x1c-\x1f] class covers the Python-style extended
        // whitespace separators, not just ' '.
        assert_eq!(sed_base("sed\x1c-n\x1c\x1f7p file"), 7);
        assert_eq!(sed_base("sed   -n    99p   file"), 99);
    }

    #[test]
    fn test_sed_base_leading_zeros_parsed_as_decimal() {
        assert_eq!(sed_base("sed -n 007p file"), 7);
    }

    // ------------------------------------------------------------
    // Windowing of READ commands (win / "head-window" behavior) —
    // there is no standalone `head_window` function in this version of
    // chunking.rs; the equivalent behavior lives in the READ branch of
    // `chunk_observation_inner` (win-sized line windows) and in the
    // read_lines-driven grouping inside `chunk_observation`. Both are
    // exercised directly below since they're private but reachable from
    // this child test module.
    // ------------------------------------------------------------

    #[test]
    fn test_read_inner_windows_by_line_count_and_preserves_blank_lines() {
        // Unlike the generic/grep branches, the plain READ branch never
        // drops blank-only windows — every line lands in some chunk.
        let chunks = chunk_observation_inner("cat f.py", "a\n\nb", 0, 1);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "a");
        assert_eq!((chunks[0].lo, chunks[0].hi), (Some(1), Some(1)));
        assert_eq!(chunks[1].text, "");
        assert_eq!((chunks[1].lo, chunks[1].hi), (Some(2), Some(2)));
        assert_eq!(chunks[2].text, "b");
        assert_eq!((chunks[2].lo, chunks[2].hi), (Some(3), Some(3)));
        for c in &chunks {
            assert_eq!(c.kind, "read");
            assert_eq!(c.file.as_deref(), Some("f.py"));
        }
    }

    #[test]
    fn test_read_inner_win_larger_than_line_count_yields_one_chunk() {
        let chunks = chunk_observation_inner("cat f.py", "a\nb\nc", 0, 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "a\nb\nc");
        assert_eq!((chunks[0].lo, chunks[0].hi), (Some(1), Some(3)));
    }

    #[test]
    fn test_read_inner_win_one_gives_each_line_its_own_chunk() {
        let chunks = chunk_observation_inner("cat f.py", "a\nb\nc", 0, 1);
        assert_eq!(chunks.len(), 3);
        let coords: Vec<(Option<i64>, Option<i64>)> =
            chunks.iter().map(|c| (c.lo, c.hi)).collect();
        assert_eq!(
            coords,
            vec![(Some(1), Some(1)), (Some(2), Some(2)), (Some(3), Some(3))]
        );
    }

    #[test]
    fn test_read_inner_win_two_groups_pairs_of_lines() {
        let chunks = chunk_observation_inner("cat file.py", "a\nb\nc\nd\ne", 0, 2);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "a\nb");
        assert_eq!((chunks[0].lo, chunks[0].hi), (Some(1), Some(2)));
        assert_eq!(chunks[1].text, "c\nd");
        assert_eq!((chunks[1].lo, chunks[1].hi), (Some(3), Some(4)));
        assert_eq!(chunks[2].text, "e");
        assert_eq!((chunks[2].lo, chunks[2].hi), (Some(5), Some(5)));
    }

    // ------------------------------------------------------------
    // chunk_observation_inner: generic ("other") branch
    // ------------------------------------------------------------

    #[test]
    fn test_other_branch_drops_blank_only_windows_but_keeps_content_windows() {
        // With win=1 every line is its own window; blank windows are
        // dropped entirely (not preserved anywhere) — a real gap in the
        // literal full-coverage invariant for mixed blank/non-blank input,
        // which is why it's pinned down explicitly here.
        let chunks = chunk_observation_inner("python script.py", "line1\n\nline3", 0, 1);
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["line1", "line3"]);
        assert!(chunks.iter().all(|c| c.kind == "other"));
    }

    #[test]
    fn test_other_branch_all_blank_input_falls_back_to_one_whole_chunk() {
        // When every window is blank, `out` stays empty and the fallback
        // pushes the *original* obs verbatim as a single chunk — so in this
        // all-blank case, full coverage is exact after all.
        let obs = "\n\n\n";
        let chunks = chunk_observation_inner("python script.py", obs, 0, 1);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, obs);
        assert_eq!(chunks[0].kind, "other");
    }

    #[test]
    fn test_other_branch_whitespace_only_single_line_falls_back_regardless_of_win() {
        let obs = "   ";
        for win in [1usize, 5, 40] {
            let chunks = chunk_observation_inner("python script.py", obs, 0, win);
            assert_eq!(chunks.len(), 1, "win={win}");
            assert_eq!(chunks[0].text, obs, "win={win}");
        }
    }

    #[test]
    fn test_other_branch_full_coverage_reconstruction_when_no_blank_lines() {
        // No blank lines anywhere: every window has content, so nothing is
        // dropped and the chunk texts reconstruct the input exactly for
        // any window size.
        let obs = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for win in [1usize, 3, 7, 40] {
            let chunks = chunk_observation_inner("python script.py", &obs, 0, win);
            assert_eq!(reconstruct(&chunks), obs, "win={win}");
        }
    }

    #[test]
    fn test_other_branch_empty_observation_produces_one_empty_chunk() {
        let chunks = chunk_observation_inner("python script.py", "", 0, 40);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "");
        assert_eq!(chunks[0].kind, "other");
    }

    #[test]
    fn test_other_branch_very_long_single_line_stays_one_chunk_regardless_of_win() {
        let obs = "x".repeat(10_000);
        for win in [1usize, 5, 40] {
            let chunks = chunk_observation_inner("python script.py", &obs, 0, win);
            assert_eq!(chunks.len(), 1, "win={win}");
            assert_eq!(chunks[0].text.len(), 10_000, "win={win}");
        }
    }

    #[test]
    fn test_other_branch_large_multiline_output_windows_and_reconstructs() {
        let obs = (0..1000)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_observation_inner("python script.py", &obs, 0, 100);
        assert_eq!(chunks.len(), 10);
        assert!(chunks[0].text.starts_with("line0\n"));
        assert!(chunks[0].text.ends_with("line99"));
        assert!(chunks[9].text.starts_with("line900\n"));
        assert!(chunks[9].text.ends_with("line999"));
        assert_eq!(reconstruct(&chunks), obs);
    }

    // ------------------------------------------------------------
    // chunk_observation_inner: grep/search branch
    // ------------------------------------------------------------

    #[test]
    fn test_search_branch_interleaves_grep_chunks_and_windowed_other_runs() {
        let obs = "src/a.py:3:TODO one\nno match here\nalso no match\nsrc/b.py:9:TODO two";
        let chunks = chunk_observation_inner("grep -rn TODO src", obs, 0, 10);
        assert_eq!(chunks.len(), 3);

        assert_eq!(chunks[0].kind, "grep");
        assert_eq!(chunks[0].text, "src/a.py:3:TODO one");
        assert_eq!(chunks[0].file.as_deref(), Some("a.py"));
        assert_eq!((chunks[0].lo, chunks[0].hi), (Some(3), Some(3)));

        assert_eq!(chunks[1].kind, "other");
        assert_eq!(chunks[1].text, "no match here\nalso no match");
        assert_eq!(chunks[1].file, None);

        assert_eq!(chunks[2].kind, "grep");
        assert_eq!(chunks[2].text, "src/b.py:9:TODO two");
        assert_eq!(chunks[2].file.as_deref(), Some("b.py"));
        assert_eq!((chunks[2].lo, chunks[2].hi), (Some(9), Some(9)));

        assert_eq!(reconstruct(&chunks), obs);
    }

    #[test]
    fn test_search_branch_no_candidates_falls_back_to_windowed_other_chunks() {
        let obs = "nothing matches\nstill nothing";
        let chunks = chunk_observation_inner("grep -rn TODO src", obs, 0, 40);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "other");
        assert_eq!(chunks[0].text, obs);
    }

    #[test]
    fn test_search_branch_all_candidates_produces_all_grep_chunks() {
        let obs = "a.py:1:x\nb.py:2:y\nc.py:3:z";
        let chunks = chunk_observation_inner("grep -n x", obs, 0, 40);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.kind == "grep"));
        assert_eq!(reconstruct(&chunks), obs);
    }

    // ------------------------------------------------------------
    // chunk_observation (public API): branch selection, rc/head/cmd,
    // and the read_lines-driven code-read path.
    // ------------------------------------------------------------

    #[test]
    fn test_outer_search_takes_priority_over_read_even_with_read_lines_set() {
        // cmd matches both SEARCH ("grep") and READ ("cat"); SEARCH must win,
        // and the read_lines path (which would tag chunks "read") must not
        // be taken at all.
        let cmd = "grep foo $(cat files.txt)";
        let obs = "files.txt:2:foo bar";
        let chunks = chunk_observation(cmd, obs, 0, 10, Some(2), ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "grep");
        assert_eq!(chunks[0].file.as_deref(), Some("files.txt"));
    }

    #[test]
    fn test_outer_sets_cmd_rc_head_uniformly_on_every_chunk() {
        let cmd = "python -m pytest -k thing";
        let obs = "<returncode>0</returncode>\nOK: 3 passed\nOK: 4 passed";
        let chunks = chunk_observation(cmd, obs, 0, 1, None, ChunkMode::Fixed);
        assert!(chunks.len() > 1, "expect multiple windows at win=1");
        for c in &chunks {
            assert_eq!(c.cmd, cmd);
            assert_eq!(c.rc, Some(0));
            assert_eq!(c.head, char_prefix(obs, 240));
        }
    }

    #[test]
    fn test_outer_returncode_key_value_form_case_insensitive_and_negative() {
        let chunks = chunk_observation("run", "RETURNCODE=-1 done", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks[0].rc, Some(-1));

        let chunks2 = chunk_observation("run", "returncode: 137\nDone", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks2[0].rc, Some(137));
    }

    #[test]
    fn test_outer_returncode_beyond_400_char_window_is_not_found() {
        let padding = "x".repeat(401);
        let obs = format!("{padding}<returncode>5</returncode>");
        let chunks = chunk_observation("run", &obs, 0, 40, None, ChunkMode::Fixed);
        assert!(chunks.iter().all(|c| c.rc.is_none()));
    }

    #[test]
    fn test_outer_returncode_within_400_char_window_is_found() {
        let padding = "x".repeat(350);
        let obs = format!("{padding}<returncode>5</returncode>");
        let chunks = chunk_observation("run", &obs, 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks[0].rc, Some(5));
    }

    #[test]
    fn test_outer_head_and_cmd_are_truncated_to_240_and_300_chars() {
        let obs = "y".repeat(500);
        let cmd = "z".repeat(400);
        let chunks = chunk_observation(&cmd, &obs, 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks[0].head, "y".repeat(240));
        assert_eq!(chunks[0].cmd, "z".repeat(300));
    }

    #[test]
    fn test_outer_read_lines_path_drops_blank_lines_but_keeps_original_line_numbers() {
        let chunks = chunk_observation("cat file.py", "a\n\nb", 0, 40, Some(1), ChunkMode::Fixed);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "a");
        assert_eq!((chunks[0].lo, chunks[0].hi), (Some(1), Some(1)));
        assert_eq!(chunks[1].text, "b");
        // Original line 3 (0-indexed line 2), NOT renumbered to 2 despite
        // the blank line being dropped from the output.
        assert_eq!((chunks[1].lo, chunks[1].hi), (Some(3), Some(3)));
        for c in &chunks {
            assert_eq!(c.kind, "read");
            assert_eq!(c.file.as_deref(), Some("file.py"));
        }
    }

    #[test]
    fn test_outer_read_lines_grouping_spans_the_gap_left_by_a_dropped_blank_line() {
        let chunks = chunk_observation("cat file.py", "a\n\nb", 0, 40, Some(2), ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        // The blank line's newline is not preserved in the text...
        assert_eq!(chunks[0].text, "a\nb");
        // ...but the coordinate span still reveals the gap.
        assert_eq!((chunks[0].lo, chunks[0].hi), (Some(1), Some(3)));
    }

    #[test]
    fn test_outer_read_lines_path_falls_back_to_whole_obs_when_all_lines_blank() {
        let chunks = chunk_observation("cat file.py", "\n\n", 0, 40, Some(1), ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "\n\n");
        assert_eq!(chunks[0].kind, "read");
    }

    #[test]
    fn test_outer_empty_observation_yields_single_empty_chunk_with_rc_none() {
        let chunks = chunk_observation("ls", "", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "");
        assert_eq!(chunks[0].rc, None);
        assert_eq!(chunks[0].head, "");
    }

    // ------------------------------------------------------------
    // chunk_assistant
    // ------------------------------------------------------------

    #[test]
    fn test_assistant_empty_text_returns_no_chunks() {
        assert_eq!(chunk_assistant("", 0, 40), Vec::new());
    }

    #[test]
    fn test_assistant_whitespace_only_text_returns_no_chunks() {
        assert_eq!(chunk_assistant("   ", 0, 40), Vec::new());
        assert_eq!(chunk_assistant("\n\n\n", 0, 1), Vec::new());
    }

    #[test]
    fn test_assistant_windows_and_drops_blank_only_windows() {
        let txt = "line1\n\nline3\nline4";
        let chunks = chunk_assistant(txt, 0, 2);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "line1\n");
        assert_eq!(chunks[1].text, "line3\nline4");
        assert!(chunks.iter().all(|c| c.kind == "asst"));
    }

    #[test]
    fn test_assistant_large_text_reconstruction_with_no_blank_lines() {
        let txt = (0..200)
            .map(|i| format!("thought {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for win in [1usize, 9, 40] {
            let chunks = chunk_assistant(&txt, 0, win);
            assert_eq!(reconstruct(&chunks), txt, "win={win}");
        }
    }

    #[test]
    fn test_assistant_single_very_long_line_is_one_chunk() {
        let txt = "x".repeat(5000);
        let chunks = chunk_assistant(&txt, 0, 10);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text.len(), 5000);
    }
}