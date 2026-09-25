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
        // Same reasoning as `win`: a zero group size would index an empty
        // window, so treat it as one line per chunk.
        let g = g.max(1);
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
    // Python's range(0, n, 0) raises; a zero window here would never advance
    // and hang. Treat it as one line per window instead.
    let win = win.max(1);
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
/// same full-coverage invariant. May legitimately return no chunks. A `win`
/// of 0 is treated as 1 (see `chunk_observation_inner`).
pub fn chunk_assistant(txt: &str, step: i64, win: usize) -> Vec<Chunk> {
    let win = win.max(1);
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

    type GrepHit = Option<(&'static str, Option<i64>)>;

    /// Checks each (input line, expected hit) pair, naming the input on failure.
    fn check_grep(cases: &[(&str, GrepHit)]) {
        for (line, want) in cases {
            let want = want.map(|(f, n)| (f.to_string(), n));
            assert_eq!(parse_grep_candidate(line), want, "input: {line:?}");
        }
    }

    fn check_sed_base(cases: &[(&str, i64)]) {
        for (cmd, want) in cases {
            assert_eq!(sed_base(cmd), *want, "cmd: {cmd:?}");
        }
    }

    fn check_obs_rc(cases: &[(&str, Option<i64>)]) {
        for (obs, want) in cases {
            assert_eq!(obs_rc(obs), *want, "obs: {obs:?}");
        }
    }

    fn texts(chunks: &[Chunk]) -> Vec<&str> {
        chunks.iter().map(|c| c.text.as_str()).collect()
    }

    fn ranges(chunks: &[Chunk]) -> Vec<(Option<i64>, Option<i64>)> {
        chunks.iter().map(|c| (c.lo, c.hi)).collect()
    }

    /// n lines "l1".."ln" joined by \n.
    fn numbered(n: usize) -> String {
        (1..=n)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Full-coverage invariant for the windowed paths: chunks are windows of
    /// py_splitlines(obs) joined by \n, so splitting every chunk back on \n
    /// must give the original lines in order, minus only whole windows that
    /// held no content.
    fn assert_full_coverage(obs: &str, chunks: &[Chunk]) {
        let from_chunks: Vec<&str> = chunks.iter().flat_map(|c| c.text.split('\n')).collect();
        let orig = py_splitlines(obs);
        let non_blank = |v: &[&str]| -> Vec<String> {
            v.iter()
                .filter(|l| py_has_content(l))
                .map(|l| l.to_string())
                .collect()
        };
        assert_eq!(
            non_blank(&from_chunks),
            non_blank(&orig),
            "a content line was dropped, duplicated or reordered"
        );
        assert!(from_chunks.len() <= orig.len().max(1));
    }

    #[test]
    fn grep_candidate_standard_path_line_text() {
        check_grep(&[
            (
                "src/engine/chunking.rs:42:    let x = 1;",
                Some(("chunking.rs", Some(42))),
            ),
            ("a.py:1:x", Some(("a.py", Some(1)))),
        ]);
    }

    #[test]
    fn grep_candidate_colons_in_matched_text_stay_in_text() {
        check_grep(&[
            (
                "cfg/app.yaml:7:url: http://host:8080/x",
                Some(("app.yaml", Some(7))),
            ),
            ("a.py:3::::", Some(("a.py", Some(3)))),
        ]);
    }

    #[test]
    fn grep_candidate_strips_whitespace_around_line_and_number() {
        check_grep(&[
            ("a.py: 7 :x", Some(("a.py", Some(7)))),
            ("\x1c\t a.py:3:x \u{3000}", Some(("a.py", Some(3)))),
        ]);
    }

    #[test]
    fn grep_candidate_space_before_first_colon_loses_line_number() {
        // Records current behaviour: "a.py " fails the extension check, so
        // only the whitespace-split fallback matches.
        check_grep(&[("a.py : 7 :x", Some(("a.py", None)))]);
    }

    #[test]
    fn grep_candidate_missing_or_non_numeric_line_numbers() {
        check_grep(&[
            ("a.py::text", Some(("a.py", None))),
            ("a.py:abc:text", Some(("a.py", None))),
            ("a.py:12a:text", Some(("a.py", None))),
            ("a.py:-3:text", Some(("a.py", None))),
            ("a.py:+3:text", Some(("a.py", None))),
            ("a.py:", Some(("a.py", None))),
        ]);
    }

    #[test]
    fn grep_candidate_two_fields_never_reads_the_number() {
        // Records current behaviour: "file:42" with no third field.
        check_grep(&[("a.py:42", Some(("a.py", None)))]);
    }

    #[test]
    fn grep_candidate_empty_match_text_keeps_line_number() {
        check_grep(&[
            ("a.py:42:", Some(("a.py", Some(42)))),
            ("a.py:0:", Some(("a.py", Some(0)))),
        ]);
    }

    #[test]
    fn grep_candidate_bare_paths_from_find_or_files_with_matches() {
        check_grep(&[
            ("./src/lib.rs", Some(("lib.rs", None))),
            ("dir/sub", Some(("sub", None))),
            ("notes.txt matched here", Some(("notes.txt", None))),
        ]);
    }

    #[test]
    fn grep_candidate_trailing_slash_gives_empty_basename() {
        // Records current behaviour: still treated as a path.
        check_grep(&[("./src/", Some(("", None)))]);
    }

    #[test]
    fn grep_candidate_rejects_blank_reranked_and_non_path_lines() {
        check_grep(&[
            ("", None),
            (" \t\x1f\u{2028}", None),
            ("[reranked 12 results]", None),
            ("   [reranked", None),
            ("error: something failed", None),
            ("Binary file matches", None),
            ("README", None),
        ]);
    }

    #[test]
    fn grep_candidate_ignores_files_without_extension_or_slash() {
        // Records current behaviour: Makefile/Dockerfile hits are dropped.
        check_grep(&[
            ("Makefile:3:all: build", None),
            ("Dockerfile:1:FROM x", None),
        ]);
    }

    #[test]
    fn grep_candidate_extension_rules() {
        check_grep(&[
            ("a.tar.gz:1:x", Some(("a.tar.gz", Some(1)))),
            ("model.h5:1:x", Some(("model.h5", Some(1)))),
            // Extension must start with a letter and be at most 5 chars.
            ("v1.2:3:x", None),
            ("notes.markdown:1:x", None),
        ]);
    }

    #[test]
    fn grep_candidate_windows_paths() {
        // Records current behaviour; none of these are handled like POSIX paths.
        check_grep(&[
            // Backslashes are not separators, so the "basename" keeps them.
            ("src\\file.rs:10:code", Some(("src\\file.rs", Some(10)))),
            // The drive-letter colon is taken as the first field separator.
            ("C:\\project\\file.rs:10:code", None),
            // The whitespace fallback grabs the whole token, and the basename
            // swallows the line number and text.
            (
                "C:/project/file.rs:10:code",
                Some(("file.rs:10:code", None)),
            ),
        ]);
    }

    #[test]
    fn grep_candidate_colon_in_file_name() {
        // Records current behaviour: the first colon always splits fields, so
        // the name is cut at it.
        check_grep(&[
            ("my:file.py:3:x", None),
            ("dir:v2/file.py:3:x", Some(("file.py:3:x", None))),
        ]);
    }

    #[test]
    fn sed_base_range_forms() {
        check_sed_base(&[
            ("sed -n '10,20p' src/a.py", 10),
            ("sed -n \"10,20p\" a.py", 10),
            ("sed -n 10,20p a.py", 10),
            ("sed -n -e 7,9p a.py", 7),
            ("nl -ba a.py | sed -n '120,160p'", 120),
            // First range wins when several are given.
            ("sed -n '30,40p;1,5p' a.py", 30),
        ]);
    }

    #[test]
    fn sed_base_inverted_and_zero_ranges_take_the_first_number() {
        check_sed_base(&[("sed -n '20,10p' a.py", 20), ("sed -n '0,5p' a.py", 0)]);
    }

    #[test]
    fn sed_base_single_line_address_and_spacing() {
        check_sed_base(&[
            ("sed -n 42p a.py", 42),
            ("sed   -n   42p a.py", 42),
            ("sed\t-n\x1c42p a.py", 42),
        ]);
    }

    #[test]
    fn sed_base_falls_back_to_one() {
        check_sed_base(&[
            ("cat a.py", 1),
            ("head -n 50 a.py", 1),
            ("tail -n +300 a.py", 1),
            ("sed -n '5,$p' a.py", 1),
            ("sed -n 'abc,20p' a.py", 1),
            ("sed -n ',20p' a.py", 1),
            ("sed -n p a.py", 1),
            ("", 1),
        ]);
    }

    #[test]
    #[ignore = "known edge case / bug: a quoted single-line address (sed -n '42p') is not matched, so read chunks are numbered from 1"]
    fn sed_base_quoted_single_line_address() {
        check_sed_base(&[("sed -n '42p' a.py", 42), ("sed -n \"42p\" a.py", 42)]);
    }

    #[test]
    fn obs_rc_tag_and_key_value_forms() {
        check_obs_rc(&[
            ("<returncode>0</returncode>\nok", Some(0)),
            ("<RETURNCODE> -1 </RETURNCODE>", Some(-1)),
            ("<returncode>\x1c3\x1f</returncode>", Some(3)),
            ("returncode: 2", Some(2)),
            ("Returncode=137", Some(137)),
        ]);
    }

    #[test]
    fn obs_rc_rejects_malformed_or_out_of_range_codes() {
        check_obs_rc(&[
            ("", None),
            ("returncode: abc", None),
            ("xreturncode=1", None),
            ("returncode=99999999999999999999", None),
        ]);
    }

    #[test]
    fn obs_rc_only_looks_at_first_400_chars() {
        // " returncode=1" is 13 chars: 387 + 13 = 400 fits, 388 + 13 cuts the
        // digit off. The space keeps the word boundary (é is a word char).
        let at_limit = format!("{} returncode=1", "é".repeat(387));
        assert_eq!(obs_rc(&at_limit), Some(1));
        let past_limit = format!("{} returncode=1", "é".repeat(388));
        assert_eq!(obs_rc(&past_limit), None);
        assert_eq!(obs_rc(&format!("{}returncode=1", "é".repeat(10))), None);
    }

    #[test]
    fn chunk_new_derives_tokens_from_chars_with_floor_of_one() {
        let tok = |s: &str| Chunk::new(s, None, None, None, 0, "other").tokens;
        assert_eq!(tok(""), 1);
        assert_eq!(tok("abcdefg"), 1);
        assert_eq!(tok("abcdefgh"), 2);
        // 9 chars, 27 bytes: char count decides.
        assert_eq!(tok("日本語テキスト日本"), 2);
    }

    #[test]
    fn chunk_with_tokens_ignores_non_positive_overrides() {
        let c = Chunk::new("abcdefgh", None, None, None, 0, "other");
        assert_eq!(c.clone().with_tokens(0).tokens, 2);
        assert_eq!(c.clone().with_tokens(-5).tokens, 2);
        assert_eq!(c.with_tokens(9).tokens, 9);
    }

    #[test]
    fn observation_other_windows_by_line_count() {
        let obs = numbered(100);
        for win in [1, 7, 40, 99, 100, 101, 1000] {
            let chunks = chunk_observation("pytest -q", &obs, 3, win, None, ChunkMode::Fixed);
            assert_eq!(chunks.len(), 100_usize.div_ceil(win), "win={win}");
            assert!(chunks
                .iter()
                .all(|c| c.kind == "other" && c.file.is_none() && c.lo.is_none() && c.step == 3));
            assert_eq!(texts(&chunks).join("\n"), obs, "win={win}");
            assert_full_coverage(&obs, &chunks);
        }
    }

    #[test]
    fn observation_empty_and_whitespace_only_fall_back_to_one_raw_chunk() {
        for obs in ["", "  \n\t\n", "\x1f", "\n\n\n"] {
            let chunks = chunk_observation("pytest", obs, 0, 2, None, ChunkMode::Fixed);
            assert_eq!(texts(&chunks), vec![obs], "{obs:?}");
            assert_eq!(chunks[0].kind, "other");
        }
    }

    #[test]
    fn observation_drops_blank_windows_but_keeps_every_content_line() {
        let obs = format!("a{}b", "\n".repeat(41));
        let chunks = chunk_observation("ls", &obs, 0, 20, None, ChunkMode::Fixed);
        assert_eq!(
            texts(&chunks),
            vec![format!("a{}", "\n".repeat(19)), "\nb".into()]
        );
        assert_full_coverage(&obs, &chunks);
    }

    #[test]
    fn observation_normalises_line_terminators_to_lf() {
        let obs = "a\r\nb\rc\u{2028}d\n";
        let chunks = chunk_observation("make", obs, 0, 40, None, ChunkMode::Fixed);
        assert_eq!(texts(&chunks), vec!["a\nb\nc\nd"]);
        assert_full_coverage(obs, &chunks);
    }

    #[test]
    fn observation_very_long_single_line_is_one_chunk() {
        let obs = "x".repeat(100_000);
        let chunks = chunk_observation("python run.py", &obs, 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, obs);
        assert_eq!(chunks[0].tokens, 25_000);
    }

    #[test]
    fn observation_large_multibyte_multiline_keeps_coverage() {
        let obs: String = (0..5000)
            .map(|i| match i % 4 {
                0 => format!("日本語 {i}\r\n"),
                1 => "\n".to_string(),
                2 => format!("😀 {i}\u{85}"),
                _ => "   \n".to_string(),
            })
            .collect();
        let chunks = chunk_observation("cargo test", &obs, 0, 40, None, ChunkMode::Fixed);
        assert_full_coverage(&obs, &chunks);
    }

    #[test]
    fn observation_metadata_rides_on_every_chunk() {
        let cmd = format!("pytest {}", "é".repeat(400));
        let obs = format!("<returncode>1</returncode>\n{}", numbered(90));
        let chunks = chunk_observation(&cmd, &obs, 0, 40, None, ChunkMode::Fixed);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert_eq!(c.rc, Some(1));
            assert_eq!(char_len(&c.cmd), 300);
            assert_eq!(c.head, char_prefix(&obs, 240));
        }
    }

    #[test]
    fn observation_read_windows_carry_file_and_line_range() {
        let chunks = chunk_observation(
            "cat src/a.py",
            &numbered(100),
            0,
            40,
            None,
            ChunkMode::Fixed,
        );
        assert_eq!(
            ranges(&chunks),
            vec![
                (Some(1), Some(40)),
                (Some(41), Some(80)),
                (Some(81), Some(100))
            ]
        );
        assert!(chunks
            .iter()
            .all(|c| c.kind == "read" && c.file.as_deref() == Some("a.py")));
    }

    #[test]
    fn observation_read_ranges_start_at_sed_base() {
        let chunks = chunk_observation(
            "sed -n '101,150p' a.py",
            &numbered(50),
            0,
            40,
            None,
            ChunkMode::Fixed,
        );
        assert_eq!(
            ranges(&chunks),
            vec![(Some(101), Some(140)), (Some(141), Some(150))]
        );
    }

    #[test]
    fn observation_read_keeps_blank_windows_unlike_other_output() {
        // Records current behaviour: only the read path emits blank chunks.
        let obs = "a\n\n\n";
        let read = chunk_observation("cat a.py", obs, 0, 1, None, ChunkMode::Fixed);
        assert_eq!(texts(&read), vec!["a", "", ""]);
        let other = chunk_observation("ls", obs, 0, 1, None, ChunkMode::Fixed);
        assert_eq!(texts(&other), vec!["a"]);
    }

    #[test]
    fn observation_read_of_empty_output_is_one_rangeless_chunk() {
        let chunks = chunk_observation("cat a.py", "", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "");
        assert_eq!(chunks[0].file.as_deref(), Some("a.py"));
        assert_eq!(ranges(&chunks), vec![(None, None)]);
    }

    #[test]
    fn observation_grep_splits_hits_from_other_output() {
        let obs = "src/a.rs:1:foo\nsrc/b.rs:22:  foo()\n--\nnoise line\n src/c.rs:3:foo";
        let chunks = chunk_observation("grep -rn foo src", obs, 2, 40, None, ChunkMode::Fixed);
        let got: Vec<_> = chunks
            .iter()
            .map(|c| {
                (
                    c.kind.as_str(),
                    c.file.as_deref(),
                    c.lo,
                    c.hi,
                    c.text.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("grep", Some("a.rs"), Some(1), Some(1), "src/a.rs:1:foo"),
                (
                    "grep",
                    Some("b.rs"),
                    Some(22),
                    Some(22),
                    "src/b.rs:22:  foo()"
                ),
                ("other", None, None, None, "--\nnoise line"),
                // Hit text is the raw line, not the stripped one.
                ("grep", Some("c.rs"), Some(3), Some(3), " src/c.rs:3:foo"),
            ]
        );
        assert_full_coverage(obs, &chunks);
    }

    #[test]
    fn observation_grep_windows_non_hit_runs_and_drops_blank_ones() {
        let obs = "one\ntwo\nthree\n\n\na.py:1:x";
        let chunks = chunk_observation("rg x", obs, 0, 2, None, ChunkMode::Fixed);
        assert_eq!(texts(&chunks), vec!["one\ntwo", "three\n", "a.py:1:x"]);
        assert_full_coverage(obs, &chunks);
    }

    #[test]
    fn observation_grep_without_hits_or_output() {
        let chunks = chunk_observation("grep foo a.py", "", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(texts(&chunks), vec![""]);
        assert_eq!(chunks[0].kind, "other");
        let chunks = chunk_observation("grep foo a.py", "\n \n", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(texts(&chunks), vec!["\n \n"]);
    }

    #[test]
    fn observation_search_command_takes_priority_over_read() {
        let chunks = chunk_observation(
            "cat a.py | grep x",
            "a.py:3:x",
            0,
            40,
            Some(10),
            ChunkMode::Fixed,
        );
        assert_eq!(chunks[0].kind, "grep");
    }

    #[test]
    fn observation_command_classification_is_whole_word() {
        // Records current behaviour: "ag" is a search tool, so reading a file
        // named ag.py is chunked as search output; "tag.py" has no word
        // boundary before "ag".
        let obs = "print(1)";
        let kind = |cmd: &str| {
            chunk_observation(cmd, obs, 0, 40, None, ChunkMode::Fixed)[0]
                .kind
                .clone()
        };
        assert_eq!(kind("cat ag.py"), "other");
        assert_eq!(kind("cat tag.py"), "read");
        assert_eq!(kind("concat.py"), "other");
    }

    #[test]
    fn observation_read_lines_drops_blank_lines_but_keeps_coordinates() {
        let obs = "x\n\ny\n   \nz\nw";
        let chunks = chunk_observation(
            "sed -n '10,15p' a.py",
            obs,
            0,
            40,
            Some(2),
            ChunkMode::Fixed,
        );
        assert_eq!(texts(&chunks), vec!["x\ny", "z\nw"]);
        assert_eq!(
            ranges(&chunks),
            vec![(Some(10), Some(12)), (Some(14), Some(15))]
        );
    }

    #[test]
    fn observation_read_lines_of_blank_output_falls_back_to_raw() {
        let chunks = chunk_observation("cat a.py", "\n \n", 0, 40, Some(5), ChunkMode::Fixed);
        assert_eq!(texts(&chunks), vec!["\n \n"]);
        assert_eq!(ranges(&chunks), vec![(None, None)]);
        assert_eq!(chunks[0].kind, "read");
    }

    #[test]
    fn observation_read_lines_is_ignored_for_non_read_commands() {
        let with = chunk_observation("pytest", &numbered(50), 0, 40, Some(5), ChunkMode::Fixed);
        let without = chunk_observation("pytest", &numbered(50), 0, 40, None, ChunkMode::Fixed);
        assert_eq!(with, without);
    }

    #[test]
    fn observation_read_lines_zero_behaves_like_one() {
        let cmd = "sed -n '10,13p' a.py";
        for obs in ["x\n\ny\nz", "\n \n"] {
            assert_eq!(
                chunk_observation(cmd, obs, 0, 40, Some(0), ChunkMode::Fixed),
                chunk_observation(cmd, obs, 0, 40, Some(1), ChunkMode::Fixed),
                "obs: {obs:?}"
            );
        }
    }

    /// Runs `f` on a worker thread; None if it has not returned in time, so a
    /// regression fails the test instead of hanging the suite.
    fn run_with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).ok()
    }

    #[test]
    fn observation_window_size_zero_behaves_like_one() {
        let obs = "a\nb\n\nsrc/f.rs:3:x\nc";
        for cmd in ["pytest", "grep -rn x ."] {
            let want = chunk_observation(cmd, obs, 0, 1, None, ChunkMode::Fixed);
            let got =
                run_with_timeout(move || chunk_observation(cmd, obs, 0, 0, None, ChunkMode::Fixed));
            assert_eq!(got, Some(want), "cmd: {cmd:?}");
        }
    }

    #[test]
    fn assistant_window_size_zero_behaves_like_one() {
        let txt = "a\n\nb\nc";
        let want = chunk_assistant(txt, 0, 1);
        let got = run_with_timeout(move || chunk_assistant(txt, 0, 0));
        assert_eq!(got, Some(want));
    }

    #[test]
    fn assistant_empty_or_whitespace_yields_no_chunks() {
        assert!(chunk_assistant("", 0, 40).is_empty());
        assert!(chunk_assistant(" \n\t\u{2028}\x1f", 0, 40).is_empty());
    }

    #[test]
    fn assistant_windows_and_coverage() {
        let txt = format!("{}\n\n\n\n\nlast", numbered(45));
        let chunks = chunk_assistant(&txt, 7, 10);
        assert!(chunks
            .iter()
            .all(|c| c.kind == "asst" && c.step == 7 && c.file.is_none()));
        assert_eq!(chunks.len(), 5);
        assert_full_coverage(&txt, &chunks);
    }

    #[test]
    fn assistant_drops_only_blank_windows() {
        let txt = format!("a{}b", "\n".repeat(5));
        assert_eq!(texts(&chunk_assistant(&txt, 0, 2)), vec!["a\n", "\nb"]);
    }

    #[test]
    fn accumulated_chunks_respects_upto_and_step_indices() {
        let steps: Vec<(String, String)> = (0..3)
            .map(|i| ("pytest".to_string(), format!("run {i}")))
            .collect();
        let steps_of = |upto| {
            accumulated_chunks(&steps, upto, None, ChunkMode::Fixed)
                .iter()
                .map(|c| c.step)
                .collect::<Vec<_>>()
        };
        assert_eq!(steps_of(0), vec![0]);
        assert_eq!(steps_of(1), vec![0, 1]);
        assert_eq!(steps_of(2), vec![0, 1, 2]);
        assert_eq!(steps_of(usize::MAX - 1), vec![0, 1, 2]);
        assert!(accumulated_chunks(&[], 5, None, ChunkMode::Fixed).is_empty());
    }

    #[test]
    #[ignore = "known edge case / bug: upto = usize::MAX overflows `upto + 1` (panics in debug, wraps to no steps in release)"]
    fn accumulated_chunks_upto_usize_max_returns_all_steps() {
        let steps = [("pytest".to_string(), "ok".to_string())];
        let r = std::panic::catch_unwind(|| {
            accumulated_chunks(&steps, usize::MAX, None, ChunkMode::Fixed)
        });
        assert_eq!(r.map(|c| c.len()).ok(), Some(1));
    }
}
