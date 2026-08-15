//! Port of the det needed-label pipeline — the `/trim` keep-set.
//!
//! Reference: `dasein-trim/deps/adaptive_context_optimizer/chunking.py`
//! (is_needed, targets_after, future_text, refetch_targets, window_rare,
//! dup_stats, _covered, _planning_doc), `labelers.py @ adaptive-context-clean
//! 58db530` (parse_reads, parse_patch_hunks — not snapshotted in deps), and
//! `dasein-trim/deps/trace_graph.py` (the admission-readout loop the det
//! keep-set is read from). The dasein-trim snapshots are the exact code the
//! SWE-bench det arm measured; parity is against them, not any pushed tree.
//!
//! The det keep-set is NOT one needed_birth pass per chunk: `_det_via_established.py`
//! unions `y >= 0.5` over the admission readouts (ages 0 and 1, every step),
//! and each readout recomputes window_rare/dup_stats over the ALIVE set at t
//! (trace_graph.py:721) while needed_birth (which defines admission, and thus
//! the alive set) uses the global window. Both passes are replicated here.
//!
//! Deliberate deviations, all documented at the site:
//!   - the fight-target / fight-refetch / AC_DEDUP rules are not ported (inert
//!     in the measured det run: AC_FIGHT_TARGETS and AC_DEDUP unset,
//!     refetch_mode="dead" hardcoded at trace_graph.py:636);
//!   - targets_after's trailing parse_grep_candidate loop (dead code — result
//!     discarded) and future_text's unused `patch` parameter are dropped;
//!   - line numbers beyond i64 saturate (chunking.rs::parse_line_no).
//!
//! At runtime there are no gold patches: `patch = ""` and the label is the
//! `used` half only. A caller may union a diff in via `patch` (gold stand-in).

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use std::sync::LazyLock;

use crate::chunking::{accumulated_chunks, Chunk, ChunkMode};
use crate::features::spans;
use crate::messages::{assistant_chunks_of, reasoning_chunks_of, steps_of};
use crate::pystr::*;

/// The trace path's read-atom granularity G (trace_graph.CHUNK_LINES default).
pub const TRACE_CHUNK_LINES: usize = 10;

// labelers._PATHTOK: token that looks like a filename (END-anchored, unlike
// chunking._FILEARG).
static PATHTOK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[\w./+-]*\.[A-Za-z]{1,5}$").unwrap());
// [\s\x1c-\x1f]: Python re's \s includes the C0 separators (repo convention).
static SED_RANGE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(\d+),(\d+)p").unwrap());
static AWK_RANGE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"NR[\s\x1c-\x1f]*>=[\s\x1c-\x1f]*(\d+)[\s\x1c-\x1f]*&&[\s\x1c-\x1f]*NR[\s\x1c-\x1f]*<=[\s\x1c-\x1f]*(\d+)",
    )
    .unwrap()
});
static SED_ONE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\bsed[\s\x1c-\x1f]+-n[\s\x1c-\x1f]+(\d+)p").unwrap());
static HEAD_N: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\bhead[\s\x1c-\x1f]+(?:-n[\s\x1c-\x1f]*)?(\d+)").unwrap());
static PLUSFILE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^\+\+\+ b/(.+)$").unwrap());
static HUNK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+\d+(?:,\d+)? @@").unwrap());

fn basename(tok: &str) -> &str {
    tok.rsplit('/').next().unwrap_or(tok)
}

/// Python `int()` saturation for line numbers (mirrors chunking.rs).
fn parse_line_no(s: &str) -> i64 {
    s.parse::<i64>()
        .or_else(|_| s.parse::<u128>().map(|v| v.min(i64::MAX as u128) as i64))
        .unwrap_or(i64::MAX)
}

/// labelers.parse_reads: which FILES later commands read and (when the command
/// names a line range) at which lines. None = whole-file read.
/// basename -> ranges in command order, duplicates kept.
pub fn parse_reads(actions: &[String]) -> HashMap<String, Vec<Option<(i64, i64)>>> {
    let mut reads: HashMap<String, Vec<Option<(i64, i64)>>> = HashMap::new();
    for cmd in actions {
        let cleaned = cmd.replace(['\'', '"'], " ");
        let files: Vec<&str> = py_split_ws(&cleaned)
            .into_iter()
            .map(basename)
            .filter(|b| PATHTOK.is_match(b))
            .collect();
        if files.is_empty() {
            continue;
        }
        let rng: Option<(i64, i64)> =
            if let Some(m) = SED_RANGE.captures(cmd).or_else(|| AWK_RANGE.captures(cmd)) {
                Some((parse_line_no(&m[1]), parse_line_no(&m[2])))
            } else if let Some(m) = SED_ONE.captures(cmd) {
                let n = parse_line_no(&m[1]);
                Some((n, n))
            } else {
                HEAD_N.captures(cmd).map(|m| (1, parse_line_no(&m[1])))
            };
        for f in files {
            reads.entry(f.to_string()).or_default().push(rng);
        }
    }
    reads
}

/// labelers.parse_patch_hunks: file basename -> ORIGINAL-side (start,end) line
/// ranges the diff touches. Files whose sections carry no hunks are dropped.
pub fn parse_patch_hunks(patch: &str) -> HashMap<String, Vec<(i64, i64)>> {
    let mut out: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
    let mut cur: Option<String> = None;
    for line in py_splitlines(patch) {
        if let Some(m) = PLUSFILE.captures(line) {
            let f = basename(py_strip(&m[1])).to_string();
            out.entry(f.clone()).or_default();
            cur = Some(f);
            continue;
        }
        if let (Some(m), Some(c)) = (HUNK.captures(line), cur.as_ref()) {
            let a = parse_line_no(&m[1]);
            let b = m.get(2).map(|g| parse_line_no(g.as_str())).unwrap_or(1);
            out.get_mut(c)
                .expect("cur is always inserted before use")
                .push((a, a + std::cmp::max(1, b) - 1));
        }
    }
    out.retain(|_, v| !v.is_empty());
    out
}

/// chunking.targets_after: files (and line ranges) the agent OPERATES ON in
/// the future — later reads plus the patch hunks. `precise_only` ignores
/// whole-file re-reads (a re-read replaces the old chunk, so it isn't needed).
/// The reference's trailing parse_grep_candidate loop discarded its result;
/// dropped here (documented deviation).
pub fn targets_after(
    steps: &[(String, String)],
    t: usize,
    patch: &str,
    horizon: Option<usize>,
    precise_only: bool,
) -> HashMap<String, Vec<(i64, i64)>> {
    let end = horizon.map_or(steps.len(), |h| std::cmp::min(steps.len(), t + 1 + h));
    let later: Vec<String> = steps
        .get(t + 1..end)
        .unwrap_or(&[])
        .iter()
        .map(|(cmd, _)| cmd.clone())
        .collect();
    let mut tgt: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
    for (f, ranges) in parse_reads(&later) {
        for r in ranges {
            match r {
                Some(r) => tgt.entry(f.clone()).or_default().push(r),
                // whole-file read -> mark all (loose only)
                None if !precise_only => tgt
                    .entry(f.clone())
                    .or_default()
                    .push((-1_000_000_000, 1_000_000_000)),
                None => {}
            }
        }
    }
    for (f, hunks) in parse_patch_hunks(patch) {
        tgt.entry(f).or_default().extend(hunks);
    }
    tgt
}

/// chunking.future_text: the agent's commands in the future window. The
/// reference took (and ignored) `patch`; dropped here (documented deviation).
pub fn future_text(steps: &[(String, String)], t: usize, horizon: Option<usize>) -> String {
    let end = horizon.map_or(steps.len(), |h| std::cmp::min(steps.len(), t + 1 + h));
    steps
        .get(t + 1..end)
        .unwrap_or(&[])
        .iter()
        .map(|(cmd, _)| cmd.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// chunking.refetch_targets: file ranges the agent PRECISELY re-reads later.
pub fn refetch_targets(steps: &[(String, String)], t: usize) -> HashMap<String, Vec<(i64, i64)>> {
    let later: Vec<String> = steps
        .get(t + 1..)
        .unwrap_or(&[])
        .iter()
        .map(|(cmd, _)| cmd.clone())
        .collect();
    let mut out: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
    for (f, ranges) in parse_reads(&later) {
        for r in ranges.into_iter().flatten() {
            out.entry(f.clone()).or_default().push(r);
        }
    }
    out
}

/// chunking.window_rare over pre-filtered per-chunk span sets (each already
/// `len >= min_len`-filtered; sets are per-chunk-deduped by construction).
fn window_rare_sets(sets: &[&HashSet<String>], max_df_frac: f64) -> HashSet<String> {
    let mut df: HashMap<&str, usize> = HashMap::new();
    for s in sets {
        for x in s.iter() {
            *df.entry(x.as_str()).or_insert(0) += 1;
        }
    }
    let cut = f64::max(2.0, max_df_frac * sets.len() as f64);
    df.into_iter()
        .filter(|&(_, n)| n as f64 <= cut)
        .map(|(x, _)| x.to_string())
        .collect()
}

/// chunking.window_rare: tokens RARE within the window (df across chunks
/// <= max(2, max_df_frac*n)).
pub fn window_rare(chunks: &[&Chunk], max_df_frac: f64, min_len: usize) -> HashSet<String> {
    let sets: Vec<HashSet<String>> = chunks.iter().map(|c| span_set(&c.text, min_len)).collect();
    window_rare_sets(&sets.iter().collect::<Vec<_>>(), max_df_frac)
}

fn span_set(text: &str, min_len: usize) -> HashSet<String> {
    spans(text)
        .into_iter()
        .filter(|x| char_len(x) >= min_len)
        .collect()
}

fn jaccard_ov(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let inter = small.iter().filter(|x| big.contains(*x)).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// chunking.dup_stats core over pre-built `len >= 5` span sets.
/// Returns per chunk: (max overlap with any NEWER chunk in [0,1], count of
/// EARLIER near-duplicates). `rows` limits WHICH rows are computed (each still
/// scanned against ALL chunks, so values are identical to the full pass);
/// non-`rows` entries stay 0.
fn dup_stats_sets(
    chunks: &[&Chunk],
    sets: &[&HashSet<String>],
    rows: Option<&[usize]>,
) -> (Vec<f64>, Vec<i64>) {
    let n = chunks.len();
    let mut sup = vec![0.0f64; n];
    let mut dup_earlier = vec![0i64; n];
    let all_rows: Vec<usize>;
    let targets: &[usize] = match rows {
        Some(r) => r,
        None => {
            all_rows = (0..n).collect();
            &all_rows
        }
    };
    for &i in targets {
        let c = chunks[i];
        // `if c.file` is Python TRUTHINESS: an empty-string file skips the
        // line-overlap channel (unlike is_needed's `is not None`).
        let c_file = c.file.as_deref().filter(|f| !f.is_empty());
        for (j, d) in chunks.iter().enumerate() {
            if i == j {
                continue;
            }
            let mut ov = 0.0f64;
            if let (Some(cf), Some(clo), Some(chi), Some(dlo), Some(dhi)) =
                (c_file, c.lo, c.hi, d.lo, d.hi)
            {
                if d.file.as_deref() == Some(cf) {
                    let inter =
                        std::cmp::max(0, std::cmp::min(chi, dhi) - std::cmp::max(clo, dlo) + 1);
                    ov = inter as f64 / std::cmp::max(1, chi - clo + 1) as f64;
                }
            }
            if ov < 1.0 {
                ov = f64::max(ov, jaccard_ov(sets[i], sets[j]));
            }
            if d.step > c.step {
                sup[i] = f64::max(sup[i], ov);
            } else if d.step < c.step && ov >= 0.6 {
                dup_earlier[i] += 1;
            }
        }
    }
    (sup, dup_earlier)
}

/// chunking.dup_stats — the THOROUGH offline label version (fuzzy span-Jaccard
/// plus same-file line coverage). Deliberately NOT dup_feats_fast: the fast
/// hash variant has no cross-file Jaccard, so the sup>=0.8 kill would change
/// the keep-set.
pub fn dup_stats(chunks: &[&Chunk], rows: Option<&[usize]>) -> (Vec<f64>, Vec<i64>) {
    let sets: Vec<HashSet<String>> = chunks.iter().map(|c| span_set(&c.text, 5)).collect();
    dup_stats_sets(chunks, &sets.iter().collect::<Vec<_>>(), rows)
}

/// chunking._planning_doc: planning/handoff docs are ALWAYS needed.
/// Token-matched so planet*/plane/explanation do NOT hit.
pub fn planning_doc(path: &str) -> bool {
    let b = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_lowercase();
    if b.contains("handoff") {
        return true;
    }
    b.split(|ch: char| !ch.is_ascii_lowercase())
        .any(|t| matches!(t, "plan" | "plans" | "planning"))
}

/// chunking._covered: summed (unclipped — overlapping ranges may double-count)
/// intersection >= frac * span.
pub fn covered(lo: i64, hi: i64, ranges: &[(i64, i64)], frac: f64) -> bool {
    if hi < lo {
        return false;
    }
    let span = hi - lo + 1;
    let mut cov: i64 = 0;
    for &(a, b) in ranges {
        cov += std::cmp::max(0, std::cmp::min(hi, b) - std::cmp::max(lo, a) + 1);
    }
    cov as f64 >= frac * span as f64
}

/// The invariant is_needed knobs. det uses the call-site values from
/// trace_graph.needed_birth/_readout (NOT the Python signature defaults).
#[derive(Debug, Clone, PartialEq)]
pub struct NeedCfg {
    pub w: i64,
    pub other_min_len: usize,
    pub continuity: i64,
    pub sup_cut: f64,
    /// window_rare df cutoff fraction (reference call sites: 0.05).
    pub rare_df_frac: f64,
    /// HABIT-REFETCH coverage fraction (reference: 0.8).
    pub refetch_cover_frac: f64,
    /// targets_after precise_only — false counts whole-file re-reads as
    /// targets (reference det: true).
    pub precise_only: bool,
    /// Future window in steps for targets/spans (reference det: None = all).
    pub horizon: Option<usize>,
}

impl NeedCfg {
    pub fn det() -> Self {
        NeedCfg {
            w: 0,
            other_min_len: 8,
            continuity: 2,
            sup_cut: 0.8,
            rare_df_frac: 0.05,
            refetch_cover_frac: 0.8,
            precise_only: true,
            horizon: None,
        }
    }

    /// Trim-aggressiveness presets, 1 (keep most) .. 5 (trim hardest),
    /// clamped. Level 3 IS `det()` — the measured SWE-bench det arm, the only
    /// parity-locked configuration (parity/fixtures/needed.json pins it). The
    /// other levels are deliberate, UNMEASURED deviations: they shift what
    /// counts as "used later", so output size does not scale linearly with
    /// the level. `continuity` is left at det everywhere — det_trim passes
    /// cur_step=None, so it never influences the keep-set.
    pub fn level(level: u8) -> Self {
        let mut cfg = NeedCfg::det();
        match level.clamp(1, 5) {
            1 => {
                cfg.w = 20; // near-miss line ranges survive
                cfg.sup_cut = 0.95; // only near-exact duplicates cut
                cfg.rare_df_frac = 0.15;
                cfg.other_min_len = 6;
                cfg.refetch_cover_frac = 1.0; // only fully re-read ranges die
                cfg.precise_only = false; // whole-file re-reads count as use
            }
            2 => {
                cfg.w = 10;
                cfg.sup_cut = 0.9;
                cfg.rare_df_frac = 0.10;
                cfg.refetch_cover_frac = 0.9;
                cfg.precise_only = false;
            }
            3 => {}
            4 => {
                cfg.sup_cut = 0.7;
                cfg.rare_df_frac = 0.03;
                cfg.other_min_len = 10;
                cfg.refetch_cover_frac = 0.6;
                cfg.horizon = Some(40); // only the next 40 steps count as "later"
            }
            _ => {
                cfg.sup_cut = 0.6;
                cfg.rare_df_frac = 0.02;
                cfg.other_min_len = 12;
                cfg.refetch_cover_frac = 0.5;
                cfg.horizon = Some(15);
            }
        }
        cfg
    }
}

/// chunking.is_needed with the det-inert rules (fight_tgt, refetch_mode
/// ="fight", code_tokens/AC_DEDUP) omitted — see module docs.
///
/// `c_spans_min` is spans(c.text) pre-filtered to `len >= cfg.other_min_len`
/// (only read for non-file chunks); `fut_spans` is spans(future_text)
/// (spans() is pure, so precomputing is behavior-identical to the reference's
/// memoized calls).
#[allow(clippy::too_many_arguments)]
pub fn is_needed(
    c: &Chunk,
    tgt: &HashMap<String, Vec<(i64, i64)>>,
    fut_spans: &HashSet<String>,
    cur_step: Option<i64>,
    rare: Option<&HashSet<String>>,
    sup: f64,
    refetch: Option<&HashMap<String, Vec<(i64, i64)>>>,
    patch_tgt: Option<&HashMap<String, Vec<(i64, i64)>>>,
    c_spans_min: &HashSet<String>,
    cfg: &NeedCfg,
) -> bool {
    if let Some(cs) = cur_step {
        if c.step >= cs {
            return true; // the observation the agent is acting on NOW
        }
    }
    if c.kind == "reasoning" {
        return match cur_step {
            None => true,
            Some(cs) => (cs - c.step) < cfg.continuity,
        };
    }
    // `c.file is not None` in the reference — empty-string file IS a file here.
    if let Some(f) = c.file.as_deref() {
        if planning_doc(f) {
            return true; // planning/handoff doc: ALWAYS needed
        }
    }
    if sup >= cfg.sup_cut {
        return false; // duplicate with a fresh copy in window: cut
    }
    if let Some(f) = c.file.as_deref() {
        // HABIT-REFETCH (refetch_mode="dead"): precisely re-read later anyway
        // -> keeping buys nothing, UNLESS the range overlaps the final patch.
        if let (Some(rf), Some(lo), Some(hi)) = (refetch, c.lo, c.hi) {
            if let Some(rr) = rf.get(f).filter(|rr| !rr.is_empty()) {
                if covered(lo, hi, rr, cfg.refetch_cover_frac) {
                    let patch_overlap = patch_tgt
                        .and_then(|p| p.get(f))
                        .filter(|pr| !pr.is_empty())
                        .is_some_and(|pr| {
                            pr.iter().any(|&(a, b)| !(hi < a - cfg.w || lo > b + cfg.w))
                        });
                    if !patch_overlap {
                        return false;
                    }
                }
            }
        }
        let rngs = match tgt.get(f).filter(|r| !r.is_empty()) {
            Some(r) => r,
            None => return false,
        };
        return match (c.lo, c.hi) {
            (Some(lo), Some(hi)) => rngs
                .iter()
                .any(|&(a, b)| !(hi < a - cfg.w || lo > b + cfg.w)),
            // c.lo is None -> file appears at all => needed. (lo set with hi
            // unset cannot be produced by the chunkers; the reference would
            // raise there.)
            _ => true,
        };
    }
    // non-file chunk: RARE-distinctive content referenced downstream.
    let mut any = false;
    for s in c_spans_min {
        if let Some(r) = rare {
            if !r.contains(s) {
                continue;
            }
        }
        if fut_spans.contains(s) {
            any = true;
            break;
        }
    }
    any
}

/// Why a session was too short to trim (the reference's build guards).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrimSkip {
    TooFewSteps(usize),
    TooFewChunks(usize),
}

impl std::fmt::Display for TrimSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrimSkip::TooFewSteps(n) => write!(f, "session too short to trim ({n} steps < 6)"),
            TrimSkip::TooFewChunks(n) => write!(f, "session too short to trim ({n} chunks < 6)"),
        }
    }
}

/// The det trim of a session: the full chunk set plus the keep mask.
#[derive(Debug)]
pub struct DetTrim {
    pub steps: Vec<(String, String)>,
    pub chunks: Vec<Chunk>,
    /// The det keep-set: union of y>=0.5 over the admission readouts.
    pub keep: Vec<bool>,
    /// chars/4 ESTIMATE over all chunks (label as estimated, never measured).
    pub tokens_total: i64,
}

impl DetTrim {
    pub fn kept(&self) -> usize {
        self.keep.iter().filter(|&&k| k).count()
    }

    /// The measured det arm's rendering, byte-faithful:
    /// `"\n".join(chunks[i].text for i in sorted(needed))`
    /// (_det_via_established.py:37).
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut first = true;
        for (c, &k) in self.chunks.iter().zip(&self.keep) {
            if !k {
                continue;
            }
            if !first {
                out.push('\n');
            }
            out.push_str(&c.text);
            first = false;
        }
        out
    }
}

/// Per-step future context, shared by needed_birth and the readouts (both
/// evaluate targets_after/future_text/refetch_targets at the same key with
/// the same parameters — det: full horizon, precise_only=true).
struct StepCtx {
    tgt: HashMap<String, Vec<(i64, i64)>>,
    fut_spans: HashSet<String>,
    rft: HashMap<String, Vec<(i64, i64)>>,
}

fn step_ctx(steps: &[(String, String)], t: usize, patch: &str, cfg: &NeedCfg) -> StepCtx {
    StepCtx {
        tgt: targets_after(steps, t, patch, cfg.horizon, cfg.precise_only),
        fut_spans: spans(&future_text(steps, t, cfg.horizon)),
        rft: refetch_targets(steps, t),
    }
}

/// The det keep-set of a session, replicating trace_graph.build_trace_graph's
/// label path (STEPMODE off) + _det_via_established.py's readout union:
///
///   1. chunks = accumulated(G=10, fixed) + assistant + reasoning, sorted by
///      step (stable);
///   2. admitted = reasoning OR needed_birth (global rare/sup);
///   3. for age in (0,1), for t in 1+age..T-1: decided = non-reasoning chunks
///      at step t-age; alive = alive_at(t, raw_age1=age==1); skip if
///      |alive| < 6; rare/sup over ALIVE; keep |= is_needed(...) per decided.
///
/// Consequences faithful to the reference: step-0 and last-step chunks are
/// never decided (never kept), and reasoning chunks are admitted to the alive
/// pool but never enter the keep-set.
pub fn det_trim(messages: &[Value], patch: &str) -> Result<DetTrim, TrimSkip> {
    det_trim_with(messages, patch, &NeedCfg::det())
}

/// `det_trim` with the is_needed knobs exposed (`NeedCfg::level`). Only
/// `NeedCfg::det()` (= level 3) is the parity-locked measured configuration;
/// everything else is a documented deviation. Still fully deterministic: the
/// keep-set is a pure function of (messages, patch, cfg).
pub fn det_trim_with(messages: &[Value], patch: &str, cfg: &NeedCfg) -> Result<DetTrim, TrimSkip> {
    let steps = steps_of(messages);
    let t_n = steps.len();
    if t_n < 6 {
        return Err(TrimSkip::TooFewSteps(t_n));
    }
    let last = (t_n - 1) as i64;
    let mut chunks = accumulated_chunks(&steps, t_n - 1, Some(TRACE_CHUNK_LINES), ChunkMode::Fixed);
    chunks.extend(
        assistant_chunks_of(messages)
            .into_iter()
            .filter(|c| c.step <= last),
    );
    chunks.extend(
        reasoning_chunks_of(messages)
            .into_iter()
            .filter(|c| c.step <= last),
    );
    chunks.sort_by_key(|c| c.step); // stable, like Python's sorted
    let n = chunks.len();
    if n < 6 {
        return Err(TrimSkip::TooFewChunks(n));
    }
    if n > 25_000 {
        eprintln!("parsec trim: {n} chunks — the exact-label pass may take a while");
    }

    // Per-chunk span sets once (the reference memoizes spans()). The rare
    // pass and c_spans_min share the same min_len, as at the reference call
    // sites (both 8 under det).
    let sets5: Vec<HashSet<String>> = chunks.iter().map(|c| span_set(&c.text, 5)).collect();
    let sets_min: Vec<HashSet<String>> = chunks
        .iter()
        .map(|c| span_set(&c.text, cfg.other_min_len))
        .collect();
    let chunk_refs: Vec<&Chunk> = chunks.iter().collect();

    let ptgt = parse_patch_hunks(patch);
    let mut ever: HashMap<usize, StepCtx> = HashMap::new();

    // --- global admission (needed_birth): rare/sup over ALL chunks ---------
    let o_rare = window_rare_sets(&sets_min.iter().collect::<Vec<_>>(), cfg.rare_df_frac);
    let non_reasoning: Vec<usize> = (0..n).filter(|&i| chunks[i].kind != "reasoning").collect();
    let (o_sup, _) = dup_stats_sets(
        &chunk_refs,
        &sets5.iter().collect::<Vec<_>>(),
        Some(&non_reasoning),
    );
    let mut admitted = vec![false; n];
    for i in 0..n {
        if chunks[i].kind == "reasoning" {
            admitted[i] = true; // short-circuit before needed_birth
            continue;
        }
        let step = chunks[i].step as usize;
        let ctx = ever
            .entry(step)
            .or_insert_with(|| step_ctx(&steps, step, patch, cfg));
        admitted[i] = is_needed(
            &chunks[i],
            &ctx.tgt,
            &ctx.fut_spans,
            None,
            Some(&o_rare),
            o_sup[i],
            Some(&ctx.rft),
            Some(&ptgt),
            &sets_min[i],
            cfg,
        );
    }

    // --- admission readouts: keep = union of y>=0.5 ------------------------
    let mut by_step: Vec<Vec<usize>> = vec![Vec::new(); t_n];
    for (i, c) in chunks.iter().enumerate() {
        by_step[c.step as usize].push(i);
    }
    let tokens_total: i64 = chunks.iter().map(|c| c.tokens).sum();
    let mut keep = vec![false; n];
    for age in 0..=1usize {
        for t in (1 + age)..t_n.saturating_sub(1) {
            let decided: Vec<usize> = by_step[t - age]
                .iter()
                .copied()
                .filter(|&i| chunks[i].kind != "reasoning")
                .collect();
            if decided.is_empty() {
                continue;
            }
            // alive_at(t, raw_age1=age==1)
            let alive: Vec<usize> = (0..n)
                .filter(|&i| {
                    let s = chunks[i].step as usize;
                    s <= t && (s == t || (age == 1 && s + 1 == t) || admitted[i])
                })
                .collect();
            if alive.len() < 6 {
                continue; // _readout returns None
            }
            let alive_sets_min: Vec<&HashSet<String>> =
                alive.iter().map(|&i| &sets_min[i]).collect();
            let rare = window_rare_sets(&alive_sets_min, cfg.rare_df_frac);
            let alive_chunks: Vec<&Chunk> = alive.iter().map(|&i| &chunks[i]).collect();
            let alive_sets5: Vec<&HashSet<String>> = alive.iter().map(|&i| &sets5[i]).collect();
            let pos_in_alive: HashMap<usize, usize> =
                alive.iter().enumerate().map(|(k, &i)| (i, k)).collect();
            let rows: Vec<usize> = decided.iter().map(|&i| pos_in_alive[&i]).collect();
            let (sup, _) = dup_stats_sets(&alive_chunks, &alive_sets5, Some(&rows));
            let ctx = ever
                .entry(t)
                .or_insert_with(|| step_ctx(&steps, t, patch, cfg));
            for (&i, &row) in decided.iter().zip(&rows) {
                if keep[i] {
                    continue; // union — an earlier readout already kept it
                }
                if is_needed(
                    &chunks[i],
                    &ctx.tgt,
                    &ctx.fut_spans,
                    None,
                    Some(&rare),
                    sup[row],
                    Some(&ctx.rft),
                    Some(&ptgt),
                    &sets_min[i],
                    cfg,
                ) {
                    keep[i] = true;
                }
            }
        }
    }

    Ok(DetTrim {
        steps,
        chunks,
        keep,
        tokens_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Value {
        serde_json::json!({"role": role, "content": content, "extra": {}})
    }

    fn asst(cmd: &str) -> Value {
        serde_json::json!({
            "role": "assistant", "content": "",
            "extra": {"actions": [{"command": cmd, "query": ""}]}
        })
    }

    #[test]
    fn parse_reads_regex_table() {
        let r = parse_reads(&["sed -n '12,34p' src/query.py".into()]);
        assert_eq!(r["query.py"], vec![Some((12, 34))]);
        let r = parse_reads(&["awk 'NR>=5 && NR<=9' a.py".into()]);
        assert_eq!(r["a.py"], vec![Some((5, 9))]);
        let r = parse_reads(&["sed -n 7p b.txt".into()]);
        assert_eq!(r["b.txt"], vec![Some((7, 7))]);
        let r = parse_reads(&["head -n 50 c.rs".into()]);
        assert_eq!(r["c.rs"], vec![Some((1, 50))]);
        let r = parse_reads(&["head 25 c.rs".into()]);
        assert_eq!(r["c.rs"], vec![Some((1, 25))]);
        let r = parse_reads(&["cat d.py".into()]);
        assert_eq!(r["d.py"], vec![None]);
        assert!(parse_reads(&["ls -la".into()]).is_empty());
        // duplicates kept, both files get the range
        let r = parse_reads(&["sed -n '1,2p' a.py b.py".into()]);
        assert_eq!(r["a.py"], vec![Some((1, 2))]);
        assert_eq!(r["b.py"], vec![Some((1, 2))]);
    }

    #[test]
    fn parse_patch_hunks_original_side() {
        let patch =
            "--- a/x.py\n+++ b/pkg/x.py\n@@ -10,5 +12,6 @@ def f():\n ctx\n+++ b/empty.py\n";
        let h = parse_patch_hunks(patch);
        assert_eq!(h["x.py"], vec![(10, 14)]);
        assert!(!h.contains_key("empty.py")); // no hunks -> dropped
                                              // single-line hunk omits the count
        let h = parse_patch_hunks("+++ b/y.py\n@@ -3 +3 @@\n");
        assert_eq!(h["y.py"], vec![(3, 3)]);
    }

    #[test]
    fn covered_boundary_fractions() {
        assert!(covered(10, 19, &[(10, 17)], 0.8)); // exactly 0.8
        assert!(!covered(10, 19, &[(10, 16)], 0.8)); // 0.7
        assert!(!covered(10, 9, &[(1, 100)], 0.8)); // hi < lo
                                                    // unclipped overlapping ranges double-count (reference behavior)
        assert!(covered(10, 19, &[(10, 14), (12, 15)], 0.8));
    }

    #[test]
    fn planning_doc_token_matched() {
        assert!(planning_doc("DASEIN_TRIM_HANDOFF.md"));
        assert!(planning_doc("plan.md"));
        assert!(planning_doc("sprint-plans.txt"));
        assert!(planning_doc("plan9.py")); // digits split tokens
        assert!(!planning_doc("planet.py"));
        assert!(!planning_doc("airplane.md"));
        assert!(!planning_doc("explanation.md"));
    }

    #[test]
    fn window_rare_cut() {
        // 3 chunks: token in all 3 -> df 3 > max(2, .15) -> not rare;
        // token in 2 -> rare (df 2 <= 2).
        let mk = |t: &str| Chunk::new(t, None, None, None, 0, "other");
        let c1 = mk("everywhere_token special_one");
        let c2 = mk("everywhere_token special_one");
        let c3 = mk("everywhere_token unrelated_here");
        let rare = window_rare(&[&c1, &c2, &c3], 0.05, 8);
        assert!(rare.contains("special_one"));
        assert!(!rare.contains("everywhere_token"));
    }

    #[test]
    fn dup_stats_cross_file_jaccard() {
        // Same distinctive span sets, DIFFERENT files: the fast hash variant
        // scores ov=0 here; the label's fuzzy Jaccard must see 1.0.
        let a = Chunk::new(
            "singular_error_string another_rare_span",
            Some("a.py".into()),
            Some(1),
            Some(10),
            0,
            "read",
        );
        let b = Chunk::new(
            "singular_error_string another_rare_span",
            Some("b.py".into()),
            Some(1),
            Some(10),
            2,
            "read",
        );
        let (sup, dup) = dup_stats(&[&a, &b], None);
        assert_eq!(sup[0], 1.0); // newer b covers a
        assert_eq!(sup[1], 0.0);
        assert_eq!(dup[1], 1); // a is an earlier near-duplicate of b
    }

    #[test]
    fn refetch_kill_and_patch_exemption() {
        let c = Chunk::new("code", Some("q.py".into()), Some(10), Some(19), 1, "read");
        let cfg = NeedCfg::det();
        let tgt: HashMap<_, _> = [("q.py".to_string(), vec![(12, 12)])].into();
        let rft: HashMap<_, _> = [("q.py".to_string(), vec![(10, 19)])].into();
        let empty = HashSet::new();
        // fully re-read later, no patch overlap -> dead
        assert!(!is_needed(
            &c,
            &tgt,
            &empty,
            None,
            None,
            0.0,
            Some(&rft),
            None,
            &empty,
            &cfg
        ));
        // same, but the range overlaps a patch hunk -> kept
        let ptgt: HashMap<_, _> = [("q.py".to_string(), vec![(15, 16)])].into();
        assert!(is_needed(
            &c,
            &tgt,
            &empty,
            None,
            None,
            0.0,
            Some(&rft),
            Some(&ptgt),
            &empty,
            &cfg
        ));
        // no refetch, target overlap -> kept; no target -> dead
        assert!(is_needed(
            &c, &tgt, &empty, None, None, 0.0, None, None, &empty, &cfg
        ));
        assert!(!is_needed(
            &c,
            &HashMap::new(),
            &empty,
            None,
            None,
            0.0,
            None,
            None,
            &empty,
            &cfg
        ));
        // superseded -> dead even with a target
        assert!(!is_needed(
            &c, &tgt, &empty, None, None, 0.85, None, None, &empty, &cfg
        ));
    }

    #[test]
    fn nonfile_rare_token_trigger() {
        let c = Chunk::new(
            "FAILED with singular_error_string",
            None,
            None,
            None,
            1,
            "other",
        );
        let cfg = NeedCfg::det();
        let cs = span_set(&c.text, cfg.other_min_len);
        let fut: HashSet<String> = ["singular_error_string".to_string()].into();
        let rare: HashSet<String> = ["singular_error_string".to_string()].into();
        let tgt = HashMap::new();
        assert!(is_needed(
            &c,
            &tgt,
            &fut,
            None,
            Some(&rare),
            0.0,
            None,
            None,
            &cs,
            &cfg
        ));
        // not rare -> dead
        let not_rare = HashSet::new();
        assert!(!is_needed(
            &c,
            &tgt,
            &fut,
            None,
            Some(&not_rare),
            0.0,
            None,
            None,
            &cs,
            &cfg
        ));
        // rare but never referenced -> dead
        let no_fut = HashSet::new();
        assert!(!is_needed(
            &c,
            &tgt,
            &no_fut,
            None,
            Some(&rare),
            0.0,
            None,
            None,
            &cs,
            &cfg
        ));
    }

    #[test]
    fn level_three_is_det_and_levels_clamp() {
        assert_eq!(NeedCfg::level(3), NeedCfg::det());
        assert_eq!(NeedCfg::level(0), NeedCfg::level(1));
        assert_eq!(NeedCfg::level(9), NeedCfg::level(5));
        // the dial is monotone where it's meant to be: sup_cut and the
        // refetch coverage bar only ever tighten as the level rises
        for l in 1..5u8 {
            assert!(NeedCfg::level(l).sup_cut >= NeedCfg::level(l + 1).sup_cut);
            assert!(
                NeedCfg::level(l).refetch_cover_frac >= NeedCfg::level(l + 1).refetch_cover_frac
            );
        }
    }

    #[test]
    fn level_one_keeps_whole_file_rereads_that_det_drops() {
        // q.py is read at step 1 and re-read WHOLE-FILE (`cat`) at step 6.
        // det (precise_only=true) sees no precise target -> drops it; level 1
        // (precise_only=false) counts the whole-file re-read as use -> keeps.
        let plan_body: String = (1..=30)
            .map(|i| format!("plan_item_number_{i}\n"))
            .collect();
        let body: String = (1..=30).map(|i| format!("content_line_{i}\n")).collect();
        let mut msgs = vec![
            asst("cat plan.md"),
            msg("tool", &plan_body),
            asst("sed -n '1,30p' q.py"),
            msg("tool", &body),
        ];
        for i in 0..4 {
            msgs.push(asst(&format!("echo filler_{i}")));
            msgs.push(msg("tool", "ok"));
        }
        msgs.push(asst("cat q.py"));
        msgs.push(msg("tool", "fresh_copy_marker"));
        msgs.push(asst("echo done"));
        msgs.push(msg("tool", "done"));

        let d3 = det_trim(&msgs, "").expect("long enough");
        assert!(
            !d3.chunks
                .iter()
                .zip(&d3.keep)
                .any(|(c, &k)| k && c.file.as_deref() == Some("q.py") && c.step == 1),
            "det must drop the whole-file-reread q.py chunks"
        );
        let d1 = det_trim_with(&msgs, "", &NeedCfg::level(1)).expect("long enough");
        assert!(
            d1.chunks
                .iter()
                .zip(&d1.keep)
                .any(|(c, &k)| k && c.file.as_deref() == Some("q.py") && c.step == 1),
            "level 1 must keep them; kept = {:?}",
            d1.chunks
                .iter()
                .zip(&d1.keep)
                .filter(|(_, &k)| k)
                .map(|(c, _)| (&c.file, c.lo, c.hi, c.step))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn det_trim_bails_below_six_steps() {
        let mut msgs = Vec::new();
        for _ in 0..5 {
            msgs.push(asst("cat a.py"));
            msgs.push(msg("tool", "line1\nline2"));
        }
        match det_trim(&msgs, "") {
            Err(TrimSkip::TooFewSteps(5)) => {}
            other => panic!("expected TooFewSteps(5), got {other:?}"),
        }
    }

    #[test]
    fn det_trim_keeps_reread_range_and_render_joins() {
        // Step 0 reads a planning doc (always-admitted -> keeps the alive pool
        // >= 6 so readouts fire at all). Step 1 reads q.py:1..30; step 6
        // precisely re-reads lines 25-28 — so the 21-30 atom is targeted
        // (interval overlap) but not refetch-covered (4/10 lines < 0.8).
        let plan_body: String = (1..=30)
            .map(|i| format!("plan_item_number_{i}\n"))
            .collect();
        let body: String = (1..=30).map(|i| format!("content_line_{i}\n")).collect();
        let mut msgs = vec![
            asst("cat plan.md"),
            msg("tool", &plan_body),
            asst("sed -n '1,30p' q.py"),
            msg("tool", &body),
        ];
        for i in 0..4 {
            msgs.push(asst(&format!("echo filler_{i}")));
            msgs.push(msg("tool", "ok"));
        }
        msgs.push(asst("sed -n '25,28p' q.py"));
        msgs.push(msg("tool", "content_line_25\ncontent_line_26"));
        msgs.push(asst("echo done"));
        msgs.push(msg("tool", "done"));
        let d = det_trim(&msgs, "").expect("long enough");
        // the q.py atom covering lines 21-30 (step 1) must be kept: targeted
        // by the later precise read, not refetch-covered (4/10 lines).
        let kept: Vec<&Chunk> = d
            .chunks
            .iter()
            .zip(&d.keep)
            .filter(|(_, &k)| k)
            .map(|(c, _)| c)
            .collect();
        assert!(
            kept.iter()
                .any(|c| c.file.as_deref() == Some("q.py") && c.lo == Some(21)),
            "expected the q.py 21-30 atom kept; kept = {:?}",
            kept.iter()
                .map(|c| (&c.file, c.lo, c.hi, c.step))
                .collect::<Vec<_>>()
        );
        // render is exactly the plain join of kept texts
        let r = d.render();
        let joined = kept
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(r, joined);
        assert!(r.contains("content_line_25"));
    }
}
