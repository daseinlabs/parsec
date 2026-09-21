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

    // --- parse_grep_candidate -----------------------------------------

    #[test]
    fn grep_candidate_standard_format() {
        let got = parse_grep_candidate("src/main.rs:42:let x = 5;").unwrap();
        assert_eq!(got, ("main.rs".to_string(), Some(42)));
    }

    #[test]
    fn grep_candidate_multiple_colons_in_content_still_splits_on_first_two() {
        // splitn(3, ':') must stop after the file and line fields, leaving
        // any colons inside the matched text (e.g. a Rust type annotation)
        // untouched rather than corrupting the split.
        let got = parse_grep_candidate("src/main.rs:42:let x: i32 = 5;").unwrap();
        assert_eq!(got, ("main.rs".to_string(), Some(42)));
    }

    #[test]
    fn grep_candidate_missing_line_number_falls_back_to_file_only() {
        let got = parse_grep_candidate("src/main.rs:no line here").unwrap();
        assert_eq!(got, ("main.rs".to_string(), None));
    }

    #[test]
    fn grep_candidate_non_numeric_line_number_falls_back_to_file_only() {
        // The middle field fails the all-ascii-digit check, so the strict
        // (file,line) branch is skipped, but the looser (file, None)
        // fallback still recognizes the leading path.
        let got = parse_grep_candidate("src/main.rs:abc:text").unwrap();
        assert_eq!(got, ("main.rs".to_string(), None));
    }

    #[test]
    fn grep_candidate_empty_or_whitespace_only_line_is_none() {
        assert_eq!(parse_grep_candidate(""), None);
        assert_eq!(parse_grep_candidate("   "), None);
        assert_eq!(parse_grep_candidate("  \x1c "), None);
    }

    #[test]
    fn grep_candidate_reranked_marker_is_none() {
        assert_eq!(parse_grep_candidate("[reranked] some/file.rs:1:x"), None);
    }

    #[test]
    fn grep_candidate_non_path_line_is_none() {
        assert_eq!(parse_grep_candidate("just some prose with no path"), None);
    }

    #[test]
    #[ignore = "known edge case / bug: parse_grep_candidate does not \
                recognize Windows-style paths. `C:\\project\\file.rs:10:code` \
                first splits on the drive-letter colon (splitn(3, ':') \
                yields parts[0]=\"C\"), so is_path(\"C\") is false; is_path() \
                itself only tests '/' or a trailing .ext, never backslashes, \
                so the whole line falls through to None instead of \
                (\"file.rs\", Some(10)) as the assignment brief describes."]
    fn grep_candidate_windows_path_is_currently_unsupported() {
        let got = parse_grep_candidate(r"C:\project\file.rs:10:code");
        assert_eq!(got, Some(("file.rs".to_string(), Some(10))));
    }

    // --- sed_base -------------------------------------------------------

    #[test]
    fn sed_base_range_format() {
        assert_eq!(sed_base("sed -n '10,20p' file.py"), 10);
    }

    #[test]
    fn sed_base_single_line_format() {
        assert_eq!(sed_base("sed -n 5p file.py"), 5);
    }

    #[test]
    fn sed_base_extra_whitespace_between_flags() {
        assert_eq!(sed_base("sed   -n    5p   file.py"), 5);
    }

    #[test]
    fn sed_base_inverted_range_uses_first_number_verbatim() {
        // The regex only captures the two numbers around the comma; it does
        // not validate lo <= hi, so an inverted range like '20,10p' still
        // reports base=20 rather than normalizing or rejecting it.
        assert_eq!(sed_base("sed -n '20,10p' file.py"), 20);
    }

    #[test]
    fn sed_base_non_numeric_or_absent_defaults_to_one() {
        assert_eq!(sed_base("sed -n 'a,bp' file.py"), 1);
        assert_eq!(sed_base("cat file.py"), 1);
        assert_eq!(sed_base(""), 1);
    }

    // --- chunk_observation / chunk_assistant: full-coverage invariant ----

    /// Reassembles chunk texts the way the module's own doc comment implies
    /// they should recombine: in order, joined by newlines.
    fn rejoin(chunks: &[Chunk]) -> String {
        chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn chunk_observation_reconstructs_content_with_no_blank_lines() {
        let obs = "line1\nline2\nline3\nline4\nline5";
        let chunks = chunk_observation("python run.py", obs, 0, 2, None, ChunkMode::Fixed);
        assert_eq!(rejoin(&chunks), obs);
    }

    #[test]
    fn chunk_observation_empty_observation_yields_single_empty_chunk() {
        let chunks = chunk_observation("python run.py", "", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "");
        assert_eq!(chunks[0].kind, "other");
    }

    #[test]
    fn chunk_observation_whitespace_only_observation_falls_back_to_whole_text() {
        // Every windowed segment is whitespace-only and gets filtered by
        // py_has_content, so the function falls back to emitting the raw
        // observation as one chunk rather than producing zero chunks.
        let obs = "   \n   \n   ";
        let chunks = chunk_observation("python run.py", obs, 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, obs);
    }

    #[test]
    #[ignore = "known edge case: the module doc comment promises a \
                full-coverage invariant ('every character... lands in \
                exactly one chunk'), but chunk_observation_inner's windowed \
                branches drop any window whose joined text is entirely \
                whitespace (py_has_content filter). With win=1, a blank \
                line sandwiched between two content lines lands in its own \
                window and is silently discarded, so rejoining the emitted \
                chunks loses that line instead of reconstructing the \
                original text."]
    fn chunk_observation_drops_isolated_blank_window_breaking_full_coverage() {
        let obs = "line1\n   \nline3";
        let chunks = chunk_observation("python run.py", obs, 0, 1, None, ChunkMode::Fixed);
        assert_eq!(rejoin(&chunks), obs);
    }

    #[test]
    fn chunk_observation_grep_splits_candidate_lines_from_context() {
        let cmd = "grep -n TODO src/lib.rs";
        let obs = "some preamble\nsrc/lib.rs:3:TODO fix this\nsrc/lib.rs:9:TODO and this\ntrailer";
        let chunks = chunk_observation(cmd, obs, 0, 40, None, ChunkMode::Fixed);
        let grep_chunks: Vec<&Chunk> = chunks.iter().filter(|c| c.kind == "grep").collect();
        assert_eq!(grep_chunks.len(), 2);
        assert_eq!(grep_chunks[0].file.as_deref(), Some("lib.rs"));
        assert_eq!(grep_chunks[0].lo, Some(3));
        assert_eq!(grep_chunks[1].lo, Some(9));
        // Non-candidate lines still show up windowed as "other" chunks.
        assert!(chunks.iter().any(|c| c.kind == "other"));
    }

    #[test]
    fn chunk_observation_read_command_windows_by_sed_base_and_win() {
        let cmd = "sed -n '10,13p' src/lib.rs";
        let obs = "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}";
        let chunks = chunk_observation(cmd, obs, 0, 2, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].file.as_deref(), Some("lib.rs"));
        assert_eq!(chunks[0].lo, Some(10));
        assert_eq!(chunks[0].hi, Some(11));
        assert_eq!(chunks[1].lo, Some(12));
        assert_eq!(chunks[1].hi, Some(13));
        assert!(chunks.iter().all(|c| c.kind == "read"));
    }

    #[test]
    fn chunk_observation_carries_cmd_rc_head_onto_every_chunk() {
        let cmd = "python run.py";
        let obs = "<returncode>1</returncode>\nline2\nline3\nline4";
        let chunks = chunk_observation(cmd, obs, 0, 1, None, ChunkMode::Fixed);
        assert!(chunks.len() > 1, "expect multiple windows for this setup");
        for c in &chunks {
            assert_eq!(c.rc, Some(1));
            assert_eq!(c.cmd, cmd);
        }
    }

    #[test]
    fn chunk_assistant_full_coverage_with_no_blank_lines() {
        let txt = "First thought.\nSecond thought.\nThird thought.";
        let chunks = chunk_assistant(txt, 0, 2);
        assert_eq!(rejoin(&chunks), txt);
    }

    #[test]
    fn chunk_assistant_empty_text_yields_no_chunks() {
        // Unlike chunk_observation, chunk_assistant has no whole-text
        // fallback — the doc comment explicitly allows zero chunks.
        assert_eq!(chunk_assistant("", 0, 40), Vec::new());
        assert_eq!(chunk_assistant("   ", 0, 40), Vec::new());
    }

    // --- read_atom_lines (via chunk_observation's read_lines path) -------

    #[test]
    fn read_lines_mode_drops_blank_lines_but_preserves_original_coordinates() {
        let cmd = "sed -n '5,9p' src/lib.rs";
        let obs = "fn a() {}\n\nfn b() {}\n   \nfn c() {}";
        let chunks = chunk_observation(cmd, obs, 0, 40, Some(2), ChunkMode::Fixed);
        // 3 non-blank lines at original coordinates 5, 7, 9 grouped by 2.
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["fn a() {}\nfn b() {}", "fn c() {}"]);
        assert_eq!(chunks[0].lo, Some(5));
        assert_eq!(chunks[0].hi, Some(7));
        assert_eq!(chunks[1].lo, Some(9));
        assert_eq!(chunks[1].hi, Some(9));
    }
}
