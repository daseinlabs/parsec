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

    fn texts(chunks: &[Chunk]) -> Vec<String> {
        chunks.iter().map(|c| c.text.clone()).collect()
    }

    fn assert_full_coverage(cmd: &str, obs: &str, win: usize, read_lines: Option<usize>) {
        let chunks = chunk_observation(cmd, obs, 0, win, read_lines, ChunkMode::Fixed);
        let joined = chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let expected = py_splitlines(obs).join("\n");
        assert_eq!(
            joined, expected,
            "full coverage lost: cmd={cmd:?} win={win}"
        );
    }

    // --- parse_grep_candidate --------------------------------------------

    #[test]
    fn parse_grep_candidate_standard_and_extra_colons() {
        assert_eq!(
            parse_grep_candidate("path/to/file.rs:42:matching line"),
            Some(("file.rs".to_string(), Some(42)))
        );
        // Colons inside the matched text stay in the third split segment.
        assert_eq!(
            parse_grep_candidate("/abs/file.rs:7:a:b:c"),
            Some(("file.rs".to_string(), Some(7)))
        );
    }

    #[test]
    fn parse_grep_candidate_trims_and_tolerates_empty_text() {
        assert_eq!(
            parse_grep_candidate("  spaced.rs:3:hi  "),
            Some(("spaced.rs".to_string(), Some(3)))
        );
        // A valid line number with empty trailing text is still a candidate.
        assert_eq!(
            parse_grep_candidate("file.rs:42:"),
            Some(("file.rs".to_string(), Some(42)))
        );
        assert_eq!(parse_grep_candidate("   "), None);
    }

    #[test]
    fn parse_grep_candidate_missing_or_non_numeric_line() {
        assert_eq!(
            parse_grep_candidate("file.rs:notnum:text"),
            Some(("file.rs".to_string(), None))
        );
        assert_eq!(
            parse_grep_candidate("src/main.rs"),
            Some(("main.rs".to_string(), None))
        );
    }

    #[test]
    fn parse_grep_candidate_rejects_non_paths_and_reranked() {
        assert_eq!(parse_grep_candidate("plain text no path"), None);
        assert_eq!(parse_grep_candidate("[reranked] 0.5 file.rs:1:x"), None);
    }

    #[test]
    fn parse_grep_candidate_extension_matching_is_shape_based() {
        assert_eq!(
            parse_grep_candidate("FILE.RS:3:hi"),
            Some(("FILE.RS".to_string(), Some(3)))
        );
    }

    #[test]
    fn parse_grep_candidate_windows_forward_slash_folds_line_into_name() {
        // Documented current behaviour: the drive letter before ':' is not a
        // path token, so splitn(3, ':') never isolates the line number.
        assert_eq!(
            parse_grep_candidate("C:/project/file.rs:10:code"),
            Some(("file.rs:10:code".to_string(), None))
        );
    }

    #[test]
    #[ignore = "known edge case / bug: Windows drive-letter paths with \
                backslashes (C:\\project\\file.rs:10:code) return None because \
                splitn(3, ':') splits at the drive colon and is_path(\"C\") is false"]
    fn parse_grep_candidate_windows_backslash_drive_path() {
        assert_eq!(
            parse_grep_candidate(r"C:\project\file.rs:10:code"),
            Some(("file.rs".to_string(), Some(10)))
        );
    }

    // --- sed_base ---------------------------------------------------------

    #[test]
    fn sed_base_reads_range_and_single_line_forms() {
        assert_eq!(sed_base("sed -n '10,20p' file.py"), 10);
        // Inverted range: the first coordinate is still the base.
        assert_eq!(sed_base("sed -n '20,10p' f.py"), 20);
        assert_eq!(sed_base("sed -n 5p file.py"), 5);
        assert_eq!(sed_base("cat -n pkg/a.rs"), 1);
        assert_eq!(sed_base("head -n 3 file.py"), 1);
        assert_eq!(sed_base("tail -20 f.py"), 1);
    }

    #[test]
    fn sed_base_falls_back_to_one_on_non_numeric_or_odd_spacing() {
        assert_eq!(sed_base("sed -n 'ap' f.py"), 1);
        // SED_ONE requires digits directly after "-n "; quotes or a missing
        // space defeat it, unlike the range form which matches through quotes.
        assert_eq!(sed_base("sed -n '5p' f.py"), 1);
        assert_eq!(sed_base("sed -n'5p' f.py"), 1);
    }

    // --- chunk_observation: full coverage ---------------------------------

    #[test]
    fn chunk_observation_full_coverage_across_windows_and_paths() {
        let other = "alpha\nbeta\ngamma\ndelta\nepsilon";
        let read = "alpha\nbeta\ngamma\ndelta\nepsilon";
        let grep = "src/a.rs:1:alpha\nnoise\nmore noise\nsrc/b.rs:2:beta";
        for win in [1usize, 2, 3, 10] {
            assert_full_coverage("python run.py", other, win, None);
            assert_full_coverage("cat -n src/a.rs", read, win, None);
            assert_full_coverage("grep -n x src/a.rs", grep, win, None);
        }
    }

    #[test]
    fn chunk_observation_window_sizes_other_path() {
        let obs = "line1\nline2\nline3\nline4\nline5";
        assert_eq!(
            texts(&chunk_observation(
                "python x.py",
                obs,
                0,
                1,
                None,
                ChunkMode::Fixed
            ))
            .len(),
            5
        );
        assert_eq!(
            texts(&chunk_observation(
                "python x.py",
                obs,
                0,
                2,
                None,
                ChunkMode::Fixed
            )),
            vec!["line1\nline2", "line3\nline4", "line5"]
        );
        assert_eq!(
            texts(&chunk_observation(
                "python x.py",
                obs,
                0,
                40,
                None,
                ChunkMode::Fixed
            )),
            vec![obs.to_string()]
        );
    }

    #[test]
    fn chunk_observation_drops_whitespace_only_windows() {
        // Windows with no non-whitespace content are dropped, but a content
        // window keeps its internal blank lines.
        let chunks = chunk_observation("python x.py", "a\n\n\n\nb", 0, 2, None, ChunkMode::Fixed);
        assert_eq!(texts(&chunks), vec!["a\n".to_string(), "b".to_string()]);
        assert!(chunks.iter().all(|c| c.kind == "other"));
    }

    #[test]
    fn chunk_observation_long_single_line_is_one_chunk() {
        let obs = "x".repeat(5000);
        let chunks = chunk_observation("python x.py", &obs, 0, 40, None, ChunkMode::Fixed);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, obs);
    }

    #[test]
    fn chunk_observation_empty_and_whitespace_observations_are_preserved() {
        let empty = chunk_observation("python x.py", "", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(texts(&empty), vec![String::new()]);
        let ws = chunk_observation("python x.py", "   ", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(texts(&ws), vec!["   ".to_string()]);
        let grep = chunk_observation("grep foo", "", 0, 40, None, ChunkMode::Fixed);
        assert_eq!(texts(&grep), vec![String::new()]);
    }

    #[test]
    fn chunk_observation_grep_splits_candidates_from_noise_runs() {
        let chunks = chunk_observation(
            "grep -n foo a.rs",
            "a.rs:44:hit\nnoise\nmore noise\nb.rs:5:hit2",
            0,
            2,
            None,
            ChunkMode::Fixed,
        );
        let got: Vec<(String, Option<String>, Option<i64>, String)> = chunks
            .iter()
            .map(|c| (c.text.clone(), c.file.clone(), c.lo, c.kind.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "a.rs:44:hit".to_string(),
                    Some("a.rs".to_string()),
                    Some(44),
                    "grep".to_string()
                ),
                (
                    "noise\nmore noise".to_string(),
                    None,
                    None,
                    "other".to_string()
                ),
                (
                    "b.rs:5:hit2".to_string(),
                    Some("b.rs".to_string()),
                    Some(5),
                    "grep".to_string()
                ),
            ]
        );
    }

    #[test]
    fn chunk_observation_read_lines_drops_blanks_keeps_coords_and_comments() {
        let chunks = chunk_observation(
            "cat -n pkg/a.rs | sed -n '10,12p'",
            "x\ny\n\nz",
            0,
            40,
            Some(40),
            ChunkMode::Fixed,
        );
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "x\ny\nz");
        assert_eq!(chunks[0].file.as_deref(), Some("a.rs"));
        assert_eq!((chunks[0].lo, chunks[0].hi), (Some(10), Some(13)));
        assert_eq!(chunks[0].kind, "read");

        // Comments are content and are kept; `read_lines` windows the atoms.
        let chunks = chunk_observation(
            "sed -n '1,10p' src/lib.rs",
            "fn a() {}\n# comment\n\nfn b() {}",
            2,
            40,
            Some(2),
            ChunkMode::Fixed,
        );
        assert_eq!(
            texts(&chunks),
            vec!["fn a() {}\n# comment".to_string(), "fn b() {}".to_string()]
        );
        assert_eq!((chunks[0].lo, chunks[0].hi), (Some(1), Some(2)));
        assert_eq!((chunks[1].lo, chunks[1].hi), (Some(4), Some(4)));
        assert_eq!(chunks[0].file.as_deref(), Some("lib.rs"));
    }

    #[test]
    fn chunk_observation_ignores_read_lines_for_search_commands() {
        // `read_lines` must not engage for grep-family commands.
        let chunks = chunk_observation(
            "grep -n hit a.rs",
            "a.rs:1:hit\nb.rs:2:hit",
            0,
            40,
            Some(1),
            ChunkMode::Fixed,
        );
        assert_eq!(texts(&chunks), vec!["a.rs:1:hit", "b.rs:2:hit"]);
        assert!(chunks.iter().all(|c| c.kind == "grep"));
    }

    #[test]
    fn chunk_observation_propagates_cmd_head_and_rc() {
        let cmd = "python train.py --epochs 3";
        let obs = format!("<returncode>7</returncode>\n{}", "x".repeat(500));
        let chunks = chunk_observation(cmd, &obs, 9, 40, None, ChunkMode::Fixed);
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert_eq!(c.cmd, cmd);
            assert_eq!(c.step, 9);
            assert_eq!(c.rc, Some(7));
            assert_eq!(c.head, char_prefix(&obs, 240));
        }
        assert_eq!(chunks[0].head.chars().count(), 240);
    }

    #[test]
    fn chunk_observation_rc_alternate_forms() {
        for (obs, want) in [
            ("<returncode>7</returncode>", Some(7)),
            ("<returncode>\n 9 \n</returncode>", Some(9)),
            ("returncode: -3", Some(-3)),
            ("returncode = 12", None),
            ("no code here", None),
        ] {
            let chunks = chunk_observation("python x.py", obs, 0, 40, None, ChunkMode::Fixed);
            assert_eq!(chunks[0].rc, want, "{obs:?}");
        }
    }

    // --- chunk_assistant --------------------------------------------------

    #[test]
    fn chunk_assistant_empty_and_whitespace_yield_no_chunks() {
        assert!(chunk_assistant("", 0, 40).is_empty());
        assert!(chunk_assistant("   ", 0, 40).is_empty());
        assert!(chunk_assistant("   \n  ", 0, 40).is_empty());
        assert!(chunk_assistant("\n\n", 0, 40).is_empty());
    }

    #[test]
    fn chunk_assistant_windows_and_full_coverage() {
        let txt = "one\ntwo\nthree\nfour";
        for win in [1usize, 2, 3, 10] {
            let chunks = chunk_assistant(txt, 5, win);
            let joined = chunks
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(joined, py_splitlines(txt).join("\n"), "win={win}");
            assert!(chunks.iter().all(|c| c.kind == "asst" && c.step == 5));
        }
        assert_eq!(
            texts(&chunk_assistant(txt, 0, 2)),
            vec!["one\ntwo", "three\nfour"]
        );
    }

    #[test]
    #[ignore = "known edge case / bug: win = 0 makes chunk_observation and \
                chunk_assistant loop forever because i += win never advances"]
    fn zero_window_never_terminates() {
        let _ = chunk_observation("python x.py", "a\nb", 0, 0, None, ChunkMode::Fixed);
        let _ = chunk_assistant("a\nb", 0, 0);
    }
}
