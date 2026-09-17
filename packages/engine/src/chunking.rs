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

    // ---- new tests below ----

    #[test]
    fn parse_grep_candidate_standard_format() {
        assert_eq!(
            parse_grep_candidate("path/to/file.rs:42:matching line"),
            Some(("file.rs".to_string(), Some(42)))
        );
    }

    #[test]
    fn parse_grep_candidate_windows_path_is_not_recognized() {
        // KNOWN LIMITATION: the drive-letter colon in a Windows path collides
        // with the "path:line:content" delimiter, so `parts[0]` is just "C"
        // and never matches `is_path`. Documenting rather than assuming this
        // "just works" for Windows-style grep output.
        assert_eq!(
            parse_grep_candidate(r"C:\project\file.rs:10:code"),
            None,
            "Windows-style paths are misparsed because the drive-letter colon \
             is consumed as the path/line separator"
        );
    }

    #[test]
    fn parse_grep_candidate_multiple_colons_in_content() {
        assert_eq!(
            parse_grep_candidate("src/main.rs:42:some: text: here"),
            Some(("main.rs".to_string(), Some(42)))
        );
    }

    #[test]
    fn parse_grep_candidate_missing_line_number() {
        assert_eq!(
            parse_grep_candidate("path/to/file.rs:matching line"),
            Some(("file.rs".to_string(), None))
        );
    }

    #[test]
    fn parse_grep_candidate_non_numeric_line_number() {
        assert_eq!(
            parse_grep_candidate("file.rs:abc:content"),
            Some(("file.rs".to_string(), None))
        );
    }

    #[test]
    fn parse_grep_candidate_empty_text_after_line_number() {
        assert_eq!(
            parse_grep_candidate("file.rs:42:"),
            Some(("file.rs".to_string(), Some(42)))
        );
    }

    #[test]
    fn parse_grep_candidate_reranked_and_empty() {
        assert_eq!(parse_grep_candidate(""), None);
        assert_eq!(parse_grep_candidate("   "), None);
        assert_eq!(parse_grep_candidate("[reranked results]"), None);
    }

    #[test]
    fn parse_grep_candidate_single_token_path() {
        // find-style output: a bare path, no colon at all.
        assert_eq!(
            parse_grep_candidate("src/lib.rs"),
            Some(("lib.rs".to_string(), None))
        );
    }

    #[test]
    fn sed_base_various_formats() {
        assert_eq!(sed_base("sed -n '10,20p' file.py"), 10);
        assert_eq!(sed_base("sed -n 5p file.py"), 5);
        assert_eq!(sed_base("cat file.py"), 1); // no sed spec -> default
        assert_eq!(sed_base("sed -n '20,10p' file.py"), 20); // inverted range: no validation, just takes lo
    }

    #[test]
    fn sed_base_unusual_spacing() {
        assert_eq!(sed_base("sed   -n   '15,30p'   file.py"), 15);
    }

    #[test]
    fn sed_base_non_numeric_falls_back_to_one() {
        assert_eq!(sed_base("sed -n 'x,yp' file.py"), 1);
    }

    #[test]
    fn chunk_observation_empty_and_whitespace_only() {
        let out = chunk_observation("ls", "", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "");

        let out = chunk_observation("ls", "   \n\n  ", 0, 40, None, ChunkMode::Fixed);
        // Whitespace-only content: no non-empty window gets pushed, so the
        // function falls back to a single chunk holding the raw observation.
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn chunk_observation_full_coverage_invariant_other_branch() {
        let obs = (0..100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = chunk_observation("python script.py", &obs, 0, 10, None, ChunkMode::Fixed);
        let rebuilt = out
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rebuilt, obs);
    }

    #[test]
    fn chunk_observation_other_branch_can_silently_drop_whitespace_only_windows() {
        // KNOWN LIMITATION vs the module's stated "full-coverage invariant":
        // a window that is *entirely* whitespace is dropped rather than kept.
        // With real content on both sides, a mid-stream blank-only window
        // vanishes from the reconstructed text. win=1 makes each line its
        // own window, so the blank line has nowhere to hide.
        let obs = "line1\n   \nline2";
        let out = chunk_observation("python x.py", obs, 0, 1, None, ChunkMode::Fixed);
        let rebuilt = out
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_ne!(
            rebuilt, obs,
            "the whitespace-only middle window was dropped, breaking full coverage"
        );
        assert_eq!(rebuilt, "line1\nline2");
    }

    #[test]
    fn chunk_observation_windows_by_win_size() {
        let obs = (0..25)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = chunk_observation("python script.py", &obs, 0, 10, None, ChunkMode::Fixed);
        assert_eq!(out.len(), 3); // 10, 10, 5
        assert_eq!(out[2].text.lines().count(), 5);
    }

    #[test]
    fn chunk_observation_very_long_single_line() {
        let obs = "x".repeat(10_000);
        let out = chunk_observation("cat huge.txt", &obs, 0, 40, None, ChunkMode::Fixed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, obs);
        assert_eq!(out[0].file, Some("huge.txt".to_string()));
    }

    #[test]
    fn chunk_observation_grep_mixes_grep_and_other_chunks() {
        let obs = "noise before\nsrc/a.rs:1:fn main() {}\nmore noise\nsrc/b.rs:2:let x = 1;";
        let out = chunk_observation("grep -rn fn src/", obs, 0, 40, None, ChunkMode::Fixed);
        let kinds: Vec<&str> = out.iter().map(|c| c.kind.as_str()).collect();
        assert!(kinds.contains(&"grep"));
        assert!(kinds.contains(&"other"));
    }

    #[test]
    fn chunk_observation_carries_cmd_rc_head_on_every_chunk() {
        let obs = "<returncode>0</returncode>\nsome output here";
        let out = chunk_observation("cat f.py", obs, 3, 40, None, ChunkMode::Fixed);
        for c in &out {
            assert_eq!(c.rc, Some(0));
            assert_eq!(c.cmd, "cat f.py");
            assert_eq!(c.step, 3);
        }
    }

    #[test]
    fn chunk_observation_read_lines_windows_by_group_size() {
        let obs = "line1\nline2\nline3\nline4\nline5";
        let out = chunk_observation("cat f.py", obs, 0, 40, Some(2), ChunkMode::Fixed);
        assert_eq!(out.len(), 3); // groups of 2, 2, 1
        assert_eq!(out[0].lo, Some(1));
        assert_eq!(out[0].hi, Some(2));
    }

    #[test]
    fn chunk_observation_read_lines_drops_blank_lines_but_keeps_coordinates() {
        let obs = "a\n\nb\n   \nc";
        let out = chunk_observation("cat f.py", obs, 0, 40, Some(10), ChunkMode::Fixed);
        // Blank/whitespace-only lines are dropped entirely, so coordinates
        // jump (1, 3, 5) instead of being contiguous.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lo, Some(1));
        assert_eq!(out[0].hi, Some(5));
        assert_eq!(out[0].text, "a\nb\nc");
    }

    #[test]
    fn chunk_assistant_empty_and_whitespace_returns_no_chunks() {
        assert_eq!(chunk_assistant("", 0, 40), vec![]);
        assert_eq!(chunk_assistant("   \n  \n ", 0, 40), vec![]);
    }

    #[test]
    fn chunk_assistant_full_coverage_invariant() {
        let txt = (0..50)
            .map(|i| format!("thought {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = chunk_assistant(&txt, 0, 12);
        let rebuilt = out
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rebuilt, txt);
    }

    #[test]
    fn chunk_assistant_large_multiline_windows_correctly() {
        let txt = (0..1000)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = chunk_assistant(&txt, 7, 40);
        assert_eq!(out.len(), 25);
        for c in &out {
            assert_eq!(c.step, 7);
            assert_eq!(c.kind, "asst");
        }
    }
}
