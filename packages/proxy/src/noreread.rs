//! No-reread core — port of `adaptive_context/optimizer/noreread.py` adapted
//! to the Claude Code hook seam (free tier, DIRECTION.md §3).
//!
//! The reference appended an advisory directive computed from the transcript;
//! the hook version gates the tool call itself. Same policy, three parts:
//! - READS are tracked as (file, line-ranges); a re-read of content already
//!   in context is denied with a pointer to where it already is.
//! - EDITS evict the file's records (its cached content is stale). Unlike
//!   the reference — whose edit inference from command vocabulary was its
//!   one UNSAFE gap — the hook also verifies (mtime, size) at deny time and
//!   allows + evicts when the file changed on disk, whatever changed it.
//! - The LOOP-BREAKER denies a Bash command already run >= LOOP_N times.
//!
//! Escape valve (mirrors the curator's insist valve): the FIRST identical
//! re-request after a denial is allowed through — the directive "forbids
//! habitual re-reads without blocking recovery of genuinely-omitted content".
//!
//! Searches (grep/rg/find, or reads piped into grep) are never reads:
//! re-searching is legitimate navigation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::LazyLock;

const BIG: i64 = 1_000_000_000;
/// Deny a Bash command after it has run this many times (AC_NRR_LOOP_N=3).
pub const LOOP_N: u32 = 3;

// ---- shell read/edit vocabulary (ported 1:1; see the reference's
// "MAINTAINED, HARNESS-COUPLED DEPENDENCY" note — extend reads and edits
// together) ----
static READ_VERB: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(cat|tac|nl|less|more|bat)\b").unwrap());
static SED_RANGE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"\bsed\s+-n\b[^|;&]*?['"]?(\d+)\s*,\s*(\d+)\s*p"#).unwrap()
});
static HEAD: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\bhead\b\s+(?:-n?\s*)?(\d+)").unwrap());
static TAIL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\btail\b\s+(?:-n?\s*)?(\d+)").unwrap());
static AWK_NR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\bawk\b[^|]*\bNR\b").unwrap());
static AWK_EQ: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"NR\s*==\s*(\d+)").unwrap());
static AWK_LO: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"NR\s*>=?\s*(\d+)").unwrap());
static AWK_HI: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"NR\s*<=?\s*(\d+)").unwrap());
static PATH_TOK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"/?(?:[\w.\-]+/)*[\w.\-]+\.[A-Za-z][A-Za-z0-9]*").unwrap());
static GREP_PIPE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\|\s*(?:grep|egrep|fgrep|rg|ack|ag|findstr)\b").unwrap());
static REDIR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r">>?\s*(/?[\w.\-/]+\.[A-Za-z][A-Za-z0-9]*)").unwrap());
static STRONG_EDIT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)\bsed\s+-i\b|\bperl\s+-[a-z]*i|\btee\b|\bpatch\b|git\s+apply|apply_patch|str_replace|replace_in_file|edit_file|write_file|create_file|multi_edit|\btruncate\b|\bdd\b[^|]*\bof=|\.write\(|\.write_text\b|\.writelines\b|open\([^)]*['"]w['"]|open\([^)]*,\s*['"][wa]"#,
    )
    .unwrap()
});
static GLOBAL_MUTATE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\bgit\s+(?:apply|checkout|stash|reset|restore|revert)\b|\bpatch\b[^|;&\n]*(?:-p\d|<)",
    )
    .unwrap()
});

const SRC_EXT: &[&str] = &[
    ".py", ".pyx", ".pyi", ".js", ".jsx", ".ts", ".tsx", ".java", ".go", ".rb", ".rs", ".c", ".cc",
    ".cpp", ".cxx", ".h", ".hpp", ".php", ".cs", ".scala", ".kt", ".swift", ".m", ".mm", ".sh",
    ".pl", ".lua", ".r", ".jl", ".ex", ".exs", ".clj", ".erl", ".hs", ".ml", ".vue", ".svelte",
    ".sql",
];
const READ_EXT_EXTRA: &[&str] = &[
    ".txt", ".rst", ".md", ".cfg", ".ini", ".toml", ".yaml", ".yml", ".json", ".xml", ".html",
    ".htm", ".css", ".rest", ".text", ".cnf", ".conf",
];

fn has_read_ext(low: &str) -> bool {
    SRC_EXT
        .iter()
        .chain(READ_EXT_EXTRA)
        .any(|e| low.ends_with(e))
}

/// A recorded read range. Tail(n) = `tail -n` (position unknowable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineRange {
    Lines(i64, i64),
    Tail(i64),
}

impl LineRange {
    fn bounds(self) -> (i64, i64) {
        match self {
            LineRange::Lines(lo, hi) => (lo, hi),
            LineRange::Tail(_) => (1, BIG),
        }
    }
    pub fn overlaps(self, other: LineRange) -> bool {
        let (al, ah) = self.bounds();
        let (bl, bh) = other.bounds();
        !(ah < bl || al > bh)
    }
}

/// Per-file record: merged-enough ranges + the on-disk identity at read time.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileReads {
    pub ranges: Vec<LineRange>,
    pub mtime_ns: i64,
    pub size: u64,
}

/// Session store, one JSON file per Claude Code session under ~/.dasein.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    /// abs path -> reads (most recent touch is a read; edits evict).
    pub reads: BTreeMap<String, FileReads>,
    /// insist valve: denial key -> count (an identical re-request after a
    /// denial is allowed once).
    pub denials: BTreeMap<String, u32>,
    /// normalized bash command -> times run (loop-breaker).
    pub cmd_counts: BTreeMap<String, u32>,
    /// measured savings (the statusline reads these).
    pub blocked_rereads: u64,
    pub tokens_saved: u64,
    pub loops_broken: u64,
    /// Stop-hook adjudicator blocks issued this session (Track C block
    /// mode's per-session budget, `DASEIN_ADJ_MAX_BLOCKS`).
    #[serde(default)]
    pub adj_blocks: u32,
}

/// What the gate decides for one tool call.
#[derive(Debug, PartialEq)]
pub enum Gate {
    Allow,
    Deny { reason: String, tokens_avoided: u64 },
}

/// Resolve a possibly-relative path against cwd (public seam for the hook).
pub fn abs_path(path: &str, cwd: &str) -> String {
    norm_path(path, cwd)
}

fn norm_path(path: &str, cwd: &str) -> String {
    // Windows inputs (backslashes, drive-letter absolutes) fold into the same
    // forward-slash key space, so Read-tool and shell paths unify there too.
    let p = path
        .trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .replace('\\', "/");
    let drive_abs = {
        let b = p.as_bytes();
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/'
    };
    let joined = if p.starts_with('/') || drive_abs {
        p
    } else {
        format!("{}/{}", cwd.replace('\\', "/").trim_end_matches('/'), p)
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

fn skip_path(low: &str) -> bool {
    low == "/dev/null"
        || low.ends_with(".pyc")
        || low.contains("/__pycache__/")
        || low.starts_with("/tmp/")
        || low.starts_with("/private/tmp/")
        || low.contains("/appdata/local/temp/")
        || low.contains("/windows/temp/")
        || low.ends_with(".diff")
        || low.ends_with(".patch")
}

/// Split a shell command into statements on && / ; / newline, but never
/// inside quotes; pipelines stay intact (port of _split_statements).
fn split_statements(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = quote {
            buf.push(ch);
            if ch == q {
                quote = None;
            }
            i += 1;
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            buf.push(ch);
            i += 1;
        } else if ch == ';' || ch == '\n' {
            out.push(std::mem::take(&mut buf));
            i += 1;
        } else if ch == '&' && chars.get(i + 1) == Some(&'&') {
            out.push(std::mem::take(&mut buf));
            i += 2;
        } else {
            buf.push(ch);
            i += 1;
        }
    }
    out.push(buf);
    out
}

fn seg_range(seg: &str) -> Option<LineRange> {
    if let Some(m) = SED_RANGE.captures(seg) {
        return Some(LineRange::Lines(m[1].parse().ok()?, m[2].parse().ok()?));
    }
    if let Some(m) = HEAD.captures(seg) {
        return Some(LineRange::Lines(1, m[1].parse().ok()?));
    }
    if let Some(m) = TAIL.captures(seg) {
        return Some(LineRange::Tail(m[1].parse().ok()?));
    }
    if AWK_NR.is_match(seg) {
        if let Some(m) = AWK_EQ.captures(seg) {
            let n: i64 = m[1].parse().ok()?;
            return Some(LineRange::Lines(n, n));
        }
        let lo = AWK_LO.captures(seg).and_then(|m| m[1].parse::<i64>().ok());
        let hi = AWK_HI.captures(seg).and_then(|m| m[1].parse::<i64>().ok());
        return match (lo, hi) {
            (Some(l), Some(h)) => Some(LineRange::Lines(l, h)),
            _ => None, // awk read with no clear range: don't over-claim
        };
    }
    if READ_VERB.is_match(seg) {
        return Some(LineRange::Lines(1, BIG));
    }
    None
}

/// {abs_path -> range} the shell reads in `cmd` pull into context
/// (port of _read_targets, per-statement; grep-pipes and heredocs excluded).
pub fn shell_read_targets(cmd: &str, cwd: &str) -> BTreeMap<String, LineRange> {
    let mut out = BTreeMap::new();
    for stmt in split_statements(cmd) {
        if GREP_PIPE.is_match(&stmt) {
            continue; // `cat f | grep X` = symbol search, not a read
        }
        let seg = stmt.split("<<").next().unwrap_or("");
        let Some(rng) = seg_range(seg) else { continue };
        let redirects: Vec<String> = REDIR
            .captures_iter(seg)
            .map(|m| m[1].trim_matches(|c| c == '\'' || c == '"').to_string())
            .collect();
        for tok in PATH_TOK.find_iter(seg) {
            let tok = tok.as_str();
            if redirects
                .iter()
                .any(|r| r == tok.trim_matches(|c| c == '\'' || c == '"'))
            {
                continue; // `> file` targets are writes
            }
            let low = tok.trim_matches(|c| c == '\'' || c == '"').to_lowercase();
            if !has_read_ext(&low) {
                continue; // rejects dotted identifiers in python -c bodies
            }
            let ap = norm_path(tok, cwd);
            if skip_path(&ap.to_lowercase()) {
                continue;
            }
            out.insert(ap, rng);
        }
    }
    out
}

/// Basenames of source files this command edits (port of
/// _edited_src_files_local; test/scratch paths excluded).
pub fn shell_edited_basenames(cmd: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    if STRONG_EDIT.is_match(cmd) {
        paths.extend(PATH_TOK.find_iter(cmd).map(|m| m.as_str().to_string()));
    }
    paths.extend(REDIR.captures_iter(cmd).map(|m| m[1].to_string()));
    let mut out = Vec::new();
    for p in paths {
        let low = p
            .trim_matches(|c| c == '\'' || c == '"')
            .to_lowercase()
            .replace('\\', "/");
        if !SRC_EXT.iter().any(|e| low.ends_with(e)) {
            continue;
        }
        if is_test_path(&low)
            || low.contains("/tmp/")
            || low.starts_with("tmp/")
            || low.contains("/appdata/local/temp/")
            || low.contains("/windows/temp/")
            || low == "/dev/null"
        {
            continue;
        }
        let base = low.rsplit('/').next().unwrap_or(&low).to_string();
        if !out.contains(&base) {
            out.push(base);
        }
    }
    out
}

pub fn is_global_mutate(cmd: &str) -> bool {
    GLOBAL_MUTATE.is_match(cmd)
}

fn is_test_path(p: &str) -> bool {
    let p = p.replace('\\', "/");
    let parts: Vec<&str> = p
        .trim_matches(|c| c == '\'' || c == '"')
        .split('/')
        .collect();
    let base = parts.last().copied().unwrap_or(&p);
    if parts[..parts.len().saturating_sub(1)]
        .iter()
        .any(|s| matches!(*s, "test" | "tests" | "testing"))
    {
        return true;
    }
    if base.starts_with("test_")
        || base.starts_with("tests_")
        || base.starts_with("conftest")
        || base.contains("_test.")
        || base.contains("_tests.")
    {
        return true;
    }
    base.contains("reproduce")
        || matches!(
            base.split('.').next().unwrap_or(""),
            "repro" | "repro_bug" | "bug"
        )
}

/// Whitespace-normalized command for repeat detection (port of _norm_cmd).
pub fn norm_cmd(cmd: &str) -> String {
    cmd.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn file_identity(path: &str) -> Option<(i64, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?;
    let ns = mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as i64;
    Some((ns, md.len()))
}

/// Merge recorded ranges for display: (whole?, merged line ranges, tails).
fn merge(ranges: &[LineRange]) -> (bool, Vec<(i64, i64)>, Vec<i64>) {
    let whole = ranges
        .iter()
        .any(|r| matches!(r, LineRange::Lines(1, hi) if *hi >= BIG));
    let mut ints: Vec<(i64, i64)> = ranges
        .iter()
        .filter_map(|r| match r {
            LineRange::Lines(lo, hi) if *hi < BIG => Some((*lo, *hi)),
            _ => None,
        })
        .collect();
    ints.sort();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (lo, hi) in ints {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    let mut tails: Vec<i64> = ranges
        .iter()
        .filter_map(|r| match r {
            LineRange::Tail(n) => Some(*n),
            _ => None,
        })
        .collect();
    tails.sort();
    tails.dedup();
    (whole, merged, tails)
}

fn range_desc(reads: &FileReads) -> String {
    let (whole, merged, tails) = merge(&reads.ranges);
    if whole {
        return "the entire file".into();
    }
    let mut parts: Vec<String> = merged
        .iter()
        .map(|(lo, hi)| format!("L{lo}-{hi}"))
        .collect();
    parts.extend(tails.iter().map(|n| format!("last {n} lines")));
    if parts.is_empty() {
        "(read)".into()
    } else {
        parts.join(", ")
    }
}

/// Tokens the denied re-read would have re-paid: actual bytes of the
/// requested range on disk / 4 (measured, never modeled — §8.4).
fn tokens_for_range(path: &str, rng: LineRange) -> u64 {
    let Ok(content) = std::fs::read_to_string(path) else {
        return 0;
    };
    let bytes: usize = match rng {
        LineRange::Lines(lo, hi) => content
            .lines()
            .skip((lo.max(1) - 1) as usize)
            .take((hi - lo.max(1) + 1).max(0) as usize)
            .map(|l| l.len() + 1)
            .sum(),
        LineRange::Tail(n) => {
            let lines: Vec<&str> = content.lines().collect();
            lines
                .iter()
                .rev()
                .take(n.max(0) as usize)
                .map(|l| l.len() + 1)
                .sum()
        }
    };
    (bytes / 4) as u64
}

impl SessionState {
    /// Record a successful read of `path` over `rng`.
    pub fn record_read(&mut self, path: &str, rng: LineRange) {
        let (mtime_ns, size) = file_identity(path).unwrap_or((0, 0));
        let entry = self.reads.entry(path.to_string()).or_default();
        // A changed file means every earlier range is stale — start over.
        if entry.mtime_ns != mtime_ns || entry.size != size {
            entry.ranges.clear();
        }
        entry.mtime_ns = mtime_ns;
        entry.size = size;
        if !entry.ranges.contains(&rng) {
            entry.ranges.push(rng);
        }
    }

    /// Evict files edited by basename (the reference's safe bias: better to
    /// permit a re-read than forbid re-reading something that just changed).
    pub fn evict_basenames(&mut self, basenames: &[String]) {
        self.reads.retain(|path, _| {
            let base = path.rsplit('/').next().unwrap_or(path).to_lowercase();
            !basenames.contains(&base)
        });
    }

    pub fn evict_path(&mut self, path: &str) {
        self.reads.remove(path);
    }

    pub fn evict_all(&mut self) {
        self.reads.clear();
    }

    /// Gate a structured Read of `path` over `rng`.
    pub fn gate_read(&mut self, path: &str, rng: LineRange) -> Gate {
        let Some(rec) = self.reads.get(path) else {
            return Gate::Allow;
        };
        // File changed on disk since we recorded it -> stale record: allow + evict.
        match file_identity(path) {
            Some((m, s)) if m == rec.mtime_ns && s == rec.size => {}
            _ => {
                self.evict_path(path);
                return Gate::Allow;
            }
        }
        if !rec.ranges.iter().any(|r| r.overlaps(rng)) {
            return Gate::Allow; // a region not seen yet is always fine
        }
        // Insist valve: the first identical re-request after a denial passes.
        let key = format!("{}:{:?}", path, rng);
        let denials = self.denials.entry(key).or_insert(0);
        if *denials % 2 == 1 {
            *denials += 1;
            return Gate::Allow;
        }
        *denials += 1;
        let tokens = tokens_for_range(path, rng);
        self.blocked_rereads += 1;
        self.tokens_saved += tokens;
        let disp = path.rsplit('/').next().unwrap_or(path);
        Gate::Deny {
            reason: format!(
                "[CONTEXT-REUSE — do not re-read what you already have] You already read {disp} \
                 ({}) earlier in THIS conversation and the file has NOT changed since. Reuse the \
                 content from the messages above instead of re-reading (~{tokens} tokens saved). \
                 You MAY still read a region you have NOT seen yet, and searching with grep to \
                 locate a symbol is always fine. If the earlier content was genuinely summarized \
                 away and you need it back, re-issue this exact read and it will be allowed once.",
                range_desc(rec)
            ),
            tokens_avoided: tokens,
        }
    }

    /// Gate a Bash command: loop-breaker plus shell re-read detection.
    pub fn gate_bash(&mut self, cmd: &str, cwd: &str) -> Gate {
        let key = norm_cmd(cmd);
        if !key.is_empty() {
            let n = self.cmd_counts.get(&key).copied().unwrap_or(0);
            if n >= LOOP_N {
                // Insist valve applies here too: deny, then let one through.
                let dkey = format!("loop:{key}");
                let denials = self.denials.entry(dkey).or_insert(0);
                if denials.is_multiple_of(2) {
                    *denials += 1;
                    self.loops_broken += 1;
                    let shown = if key.chars().count() > 200 {
                        format!("{}...", key.chars().take(197).collect::<String>())
                    } else {
                        key.clone()
                    };
                    return Gate::Deny {
                        reason: format!(
                            "[STOP — repeated command loop detected] You have already run \
                             `{shown}` {n} times in THIS conversation and it has produced no new \
                             result across those repeats. Running it again will not help. Take a \
                             DIFFERENT action instead: reuse the output already in your context, \
                             inspect a different file or region, or make the edit and move on."
                        ),
                        tokens_avoided: 0,
                    };
                }
                *denials += 1;
            }
        }
        // Shell re-reads of recorded content: same gate as structured reads.
        for (path, rng) in shell_read_targets(cmd, cwd) {
            if let Gate::Deny {
                reason,
                tokens_avoided,
            } = self.gate_read(&path, rng)
            {
                return Gate::Deny {
                    reason,
                    tokens_avoided,
                };
            }
        }
        Gate::Allow
    }

    /// Record the effects of a completed Bash command.
    pub fn record_bash(&mut self, cmd: &str, cwd: &str) {
        let key = norm_cmd(cmd);
        if !key.is_empty() {
            *self.cmd_counts.entry(key).or_insert(0) += 1;
        }
        // Reads first, then edits — a command that both reads and writes a
        // file ends EVICTED (the reference's safe bias).
        for (path, rng) in shell_read_targets(cmd, cwd) {
            self.record_read(&path, rng);
        }
        if is_global_mutate(cmd) {
            self.evict_all();
            return;
        }
        let edited = shell_edited_basenames(cmd);
        if !edited.is_empty() {
            self.evict_basenames(&edited);
        }
    }

    /// Bump the Stop-hook block budget (Track C, block mode only).
    pub fn note_adj_block(&mut self) {
        self.adj_blocks += 1;
    }
}

// ---- persistence ----

pub fn sessions_dir() -> PathBuf {
    crate::setup::home_dir().join(".dasein").join("sessions")
}

pub fn session_path(session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sessions_dir().join(format!("{safe}.json"))
}

pub fn load_session(session_id: &str) -> SessionState {
    std::fs::read_to_string(session_path(session_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic write (tmp + rename); hooks for parallel tool calls race on this
/// file, and losing one update is acceptable — losing the whole file is not.
pub fn save_session(session_id: &str, state: &SessionState) -> std::io::Result<()> {
    let path = session_path(session_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(state).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)
}

/// Remove session files idle for more than `max_age_days`.
pub fn prune_sessions(max_age_days: u64) {
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return;
    };
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(max_age_days * 24 * 3600);
    for e in entries.flatten() {
        if let Ok(md) = e.metadata() {
            if md.modified().map(|m| m < cutoff).unwrap_or(false) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Extract (path, range) from a Claude Code Read tool_input.
pub fn read_tool_range(tool_input: &Value, cwd: &str) -> Option<(String, LineRange)> {
    let path = tool_input.get("file_path").and_then(Value::as_str)?;
    let ap = norm_path(path, cwd);
    if skip_path(&ap.to_lowercase()) {
        return None;
    }
    let offset = tool_input.get("offset").and_then(Value::as_i64);
    let limit = tool_input.get("limit").and_then(Value::as_i64);
    let rng = match (offset, limit) {
        (Some(o), Some(l)) => LineRange::Lines(o, o + l - 1),
        (Some(o), None) => LineRange::Lines(o, BIG),
        (None, Some(l)) => LineRange::Lines(1, l),
        (None, None) => LineRange::Lines(1, BIG),
    };
    Some((ap, rng))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_reads_and_searches() {
        let r = shell_read_targets("cat src/app.py", "/repo");
        assert_eq!(r.get("/repo/src/app.py"), Some(&LineRange::Lines(1, BIG)));
        // piped into grep = navigation, not a read
        assert!(shell_read_targets("cat src/app.py | grep foo", "/repo").is_empty());
        let r = shell_read_targets("sed -n '10,30p' src/app.py", "/repo");
        assert_eq!(r.get("/repo/src/app.py"), Some(&LineRange::Lines(10, 30)));
        // redirect target is a write, not a read
        assert!(!shell_read_targets("cat notes.md > out.md", "/repo").contains_key("/repo/out.md"));
    }

    #[test]
    fn edits_evict_and_loops_count() {
        let mut st = SessionState::default();
        st.record_read("/repo/a.py", LineRange::Lines(1, BIG));
        st.evict_basenames(&shell_edited_basenames("sed -i 's/x/y/' /repo/a.py"));
        assert!(st.reads.is_empty());
        assert!(is_global_mutate("git checkout -- ."));
        assert_eq!(norm_cmd("ls   -la\n"), "ls -la");
    }

    #[test]
    fn windows_paths_normalize_and_skip() {
        // Drive-letter absolutes are absolute — never joined onto cwd — and
        // backslash forms unify with forward-slash forms of the same file.
        assert_eq!(
            norm_path(r"C:\repo\src\app.py", r"C:\other"),
            "/C:/repo/src/app.py"
        );
        assert_eq!(
            norm_path(r"C:\repo\src\app.py", r"C:\repo"),
            norm_path("C:/repo/src/app.py", "C:/repo")
        );
        // Relative + windows cwd, with `..` collapsed across the join.
        assert_eq!(
            norm_path(r"..\lib\util.py", r"C:\repo\src"),
            "/C:/repo/lib/util.py"
        );
        // Windows scratch dirs are exempt like /tmp.
        assert!(skip_path(
            &norm_path(r"C:\Users\s\AppData\Local\Temp\x.py", r"C:\repo").to_lowercase()
        ));
        assert!(skip_path("/c:/windows/temp/x.py"));
        // Backslash test paths are still recognized as tests.
        assert!(is_test_path(r"c:\repo\tests\test_app.py"));
    }

    #[test]
    fn insist_valve_alternates() {
        // No real file on disk -> record with (0,0) identity, gate must
        // still work through the identity mismatch path (allow + evict).
        let mut st = SessionState::default();
        st.record_read("/nonexistent/zz.py", LineRange::Lines(1, 100));
        assert_eq!(
            st.gate_read("/nonexistent/zz.py", LineRange::Lines(1, 100)),
            Gate::Allow
        );
    }
}
