//! Deterministic quantized freezing — the session-free reformulation of the
//! reference serving stack (curator.py decision schedule v3, PURE-T0 birth
//! decisions + anthropic_shapes.py served_folds).
//!
//! THE RULE (docs/freeze-design.md): served bytes for message j are a pure
//! function of (messages[0..=j], checkpoint, config). The reference kept
//! birth decisions in session dicts (`dropped`/`decided_msgs`/`frozen`) that
//! die on restart/TTL/replica-switch; here the same decisions are a FOLD over
//! the message prefix — replayable from scratch at any time to byte-identical
//! output. The struct's registries are a memo of that fold, never a source of
//! truth: `Freezer::serve` on a cold instance MUST equal an incrementally
//! driven one (asserted by tests/parity_freeze.rs, CI invariant §8.1).
//!
//! Deliberate deviations from the reference (each is a determinism fix, all
//! documented in docs/freeze-design.md §"shifts"):
//! - Births are quantized to STEP boundaries, not proxy-call boundaries: a
//!   step skipped while a session was lost is decided on replay exactly as if
//!   it had been the current call. (The reference never re-enters the birth
//!   gate — restart made old turns permanently full.)
//! - Scores/taus cross the trait boundary as fixed-point integers on the
//!   1e-6 grid (`SCORE_SCALE`); a keep/cut can never flip from float jitter.
//! - No governor text is folded into resident turns and count_tokens traffic
//!   cannot advance state (there is no state to advance).
//! - The reference's `mi not in frozen` gate is subsumed by `decided_msgs`
//!   (frozen-without-decided only existed in restart pathologies).
//! - Render is CALL-HISTORY-FREE. The reference re-serves the untouched
//!   ORIGINAL bytes of an already-decided message on any repeat call at the
//!   same cur_step (curator.py:1154 gates on "carries current-step content
//!   and not changed THIS call"), so an identical-request retry flip-flops
//!   digest -> original -> digest. Here a decided message always renders its
//!   frozen form — the retry serves the same bytes as the first call.
//! - Malformed values where the reference raises a TypeError out of curate()
//!   (non-string `text` in a content part, non-string truthy action
//!   command/query, non-dict action) surface as `FreezeError::Invalid` from
//!   serve() BEFORE any state advances — the caller passes the original
//!   through, which is byte-what the reference's fail-open does.
//! - A client-edited resident turn (non-append-only history) resets the fold
//!   memo and replays from scratch, keeping warm == cold by construction.
//!   The reference silently kept stale decisions.
//! - Python's `\b`/`\w` regex word classes count No/Nl numerics (² ½) and
//!   exclude combining marks; the regex crate is the inverse on both. Only
//!   pathological unicode directly adjacent to command keywords diverges.
//! - Line numbers beyond i64 (Python ints are unbounded) saturate at
//!   i64::MAX in chunk coordinates.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use serde_json::Value;

use crate::chunking::{chunk_assistant, chunk_observation, Chunk, ChunkMode, DEFAULT_WIN};
use crate::messages::{actions, reasoning_chunk};
use crate::pystr::*;

/// Fixed-point grid for scores and taus: 1e-6. Python float(q/1e6) is
/// strictly monotone in q at this magnitude, so integer comparison here is
/// exactly the reference's float comparison.
pub const SCORE_SCALE: i64 = 1_000_000;

/// Provider/reasoning fields stripped when a provider chunk is dropped
/// (chunking._REASON_FIELDS).
const REASON_FIELDS: [&str; 3] = [
    "provider_specific_fields",
    "thinking_blocks",
    "reasoning_content",
];

#[derive(Debug, Clone)]
pub struct FreezeConfig {
    /// Read-atom granularity: Some(G) = trace path (product), None = legacy win=40.
    pub read_lines: Option<usize>,
    pub mode: ChunkMode,
    /// FIXED_TAUS[0] override on the 1e-6 grid; None = use the scorer's tau.
    pub tau_fixed_q: Option<i64>,
    /// Contiguous cut-run token floor (reference: 10).
    pub min_run_tokens: i64,
}

impl Default for FreezeConfig {
    fn default() -> Self {
        FreezeConfig {
            read_lines: Some(10),
            mode: ChunkMode::Fixed,
            tau_fixed_q: None,
            min_run_tokens: 10,
        }
    }
}

/// One birth-step scoring request. `live` is the not-yet-dropped chunk set as
/// of this step (steps <= cur_step), in decision order; `mask` indexes the
/// undecided chunks within it.
pub struct BirthQuery<'a> {
    pub cur_step: i64,
    pub task_text: String,
    pub recent_cmds: String,
    pub live: &'a [Chunk],
    pub live_owner: &'a [usize],
    pub mask: Vec<usize>,
}

pub struct ScoreResult {
    /// Keep score per live chunk, on the SCORE_SCALE grid.
    pub scores_q: Vec<i64>,
    /// The pool tau (reference `qhat`), same grid.
    pub tau_q: i64,
}

/// The purity contract: same (query, checkpoint, config) -> same result,
/// every time, on every machine. The brain API guarantees this by pinning the
/// checkpoint bundle and emitting grid integers; failure must surface as an
/// error to the caller's fail-open layer, never as a guessed score.
pub trait ChunkScorer {
    fn score(&mut self, q: &BirthQuery) -> ScoreResult;
}

/// Fail-open floor: keep everything (tau below every representable score).
pub struct PassthroughScorer;
impl ChunkScorer for PassthroughScorer {
    fn score(&mut self, q: &BirthQuery) -> ScoreResult {
        ScoreResult {
            scores_q: vec![SCORE_SCALE; q.live.len()],
            tau_q: 0,
        }
    }
}

/// Deterministic test scorer: sha256("{step}:{kind}:{text}") first 8 bytes
/// (big-endian) mod SCORE_SCALE per chunk. `tau_q: None` derives a
/// mask-dependent qhat (sha256 over the masked chunk texts) — exercising the
/// per-owner-pool tau path. Mirrored by parity/gen_freeze_fixtures.py.
pub struct StubScorer {
    pub tau_q: Option<i64>,
}
impl ChunkScorer for StubScorer {
    fn score(&mut self, q: &BirthQuery) -> ScoreResult {
        let scores_q = q.live.iter().map(stub_score_q).collect();
        let tau_q = self.tau_q.unwrap_or_else(|| {
            use sha2::{Digest, Sha256};
            let joined: Vec<&str> = q.mask.iter().map(|&j| q.live[j].text.as_str()).collect();
            let mut h = Sha256::new();
            h.update(format!("qhat:{}", joined.join("\u{0}")).as_bytes());
            let d = h.finalize();
            let mut b = [0u8; 8];
            b.copy_from_slice(&d[..8]);
            (u64::from_be_bytes(b) % SCORE_SCALE as u64) as i64
        });
        ScoreResult { scores_q, tau_q }
    }
}

pub fn stub_score_q(c: &Chunk) -> i64 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("{}:{}:{}", c.step, c.kind, c.text).as_bytes());
    let d = h.finalize();
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[..8]);
    (u64::from_be_bytes(b) % SCORE_SCALE as u64) as i64
}

/// curator._text: the internal flat view's text of a message.
fn m_text(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.as_object())
            .map(|p| p.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// curator._ckey: the (lossy, 60-char) chunk kill-registry key.
fn ckey(owner: usize, c: &Chunk) -> String {
    format!("{}:{}:{}", owner, c.kind, char_prefix(&c.text, 60))
}

/// Whitespace-normalized 120-char fingerprint (insist valve).
fn norm_fp(text: &str) -> String {
    char_prefix(&py_split_ws(text).join(" "), 120).to_string()
}

// [\s\x1c-\x1f]: Python re's \s includes the C0 separators the regex crate's
// \p{White_Space} excludes (same delta as pystr::py_is_space).
static RANGE_CMD: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\bsed[\s\x1c-\x1f]+-n\b|\bgrep\b|\brg\b|\begrep\b|\bfgrep\b|\back\b|\bag\b|\bawk\b.*NR|-A[\s\x1c-\x1f]*\d|-B[\s\x1c-\x1f]*\d|-C[\s\x1c-\x1f]*\d|\b\d+[\s\x1c-\x1f]*,[\s\x1c-\x1f]*\d+[\s\x1c-\x1f]*p\b",
    )
    .unwrap()
});
static SMALL_HEADTAIL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(head|tail)\b.*?-n?[\s\x1c-\x1f]*(\d{1,2})\b").unwrap()
});

/// Python truthiness for `if c.file`: empty string is falsy. Grep lines like
/// "src/:5:match" produce a "" basename that must not count as a file.
fn live_file(c: &Chunk) -> Option<&str> {
    c.file.as_deref().filter(|f| !f.is_empty())
}

/// Parsed view of the internal message list (curate() lines 893-958).
struct Parsed {
    chunks: Vec<Chunk>,
    owner: Vec<usize>,
    /// (msg index, cmd, text, step) per observation.
    obs_items: Vec<(usize, String, String, i64)>,
    first_obs: Option<usize>,
    cur_step: i64,
}

fn parse(messages: &[Value], cfg: &FreezeConfig) -> Parsed {
    let mut last_cmd = String::new();
    let mut step: i64 = 0;
    let mut first_obs: Option<usize> = None;
    let mut obs_items: Vec<(usize, String, String, i64)> = Vec::new();
    let mut reason_items: Vec<(usize, Chunk)> = Vec::new();
    let mut asst_items: Vec<(usize, String, i64)> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        match m.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                last_cmd = actions(m).join(" ; ");
                if let Some(rc) = reasoning_chunk(m, step) {
                    reason_items.push((i, rc));
                }
                let c0 = m.get("content").unwrap_or(&Value::Null);
                let ok = c0.is_string()
                    || c0.as_array().is_some_and(|parts| {
                        parts.iter().all(|p| {
                            p.as_object().is_some_and(|o| match o.get("type") {
                                None => true,
                                Some(Value::String(s)) => s == "text",
                                Some(_) => false,
                            })
                        })
                    });
                if ok {
                    let at = m_text(m);
                    if py_has_content(&at) {
                        asst_items.push((i, at, step));
                    }
                }
            }
            Some("user") | Some("tool") => {
                if first_obs.is_none() {
                    first_obs = Some(i);
                } else {
                    obs_items.push((i, last_cmd.clone(), m_text(m), step));
                }
                step += 1;
            }
            _ => {}
        }
    }
    let cur_step = step - 1;
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut owner: Vec<usize> = Vec::new();
    for (mi, cmd, txt, st) in &obs_items {
        for c in chunk_observation(cmd, txt, *st, DEFAULT_WIN, cfg.read_lines, cfg.mode) {
            chunks.push(c);
            owner.push(*mi);
        }
    }
    for (mi, at, st) in &asst_items {
        if *st <= cur_step {
            for c in chunk_assistant(at, *st, DEFAULT_WIN) {
                chunks.push(c);
                owner.push(*mi);
            }
        }
    }
    for (mi, rc) in reason_items {
        chunks.push(rc);
        owner.push(mi);
    }
    // Node-order parity with the trainer: stable sort by (step, reasoning-last).
    let mut order: Vec<usize> = (0..chunks.len()).collect();
    order.sort_by_key(|&i| (chunks[i].step, i64::from(chunks[i].kind == "reasoning")));
    let chunks = order.iter().map(|&i| chunks[i].clone()).collect();
    let owner = order.iter().map(|&i| owner[i]).collect();
    Parsed {
        chunks,
        owner,
        obs_items,
        first_obs,
        cur_step,
    }
}

/// The deterministic freezer. All fields are a memo of the pure fold over the
/// prefix — dropping the whole struct and replaying yields identical output.
pub struct Freezer<S: ChunkScorer> {
    pub cfg: FreezeConfig,
    pub scorer: S,
    dropped: HashSet<String>,
    dropped_ranges: HashMap<String, Vec<(i64, i64)>>,
    dropped_fp: HashSet<String>,
    served_ranges: HashMap<String, Vec<(i64, i64)>>,
    served_fp: HashSet<String>,
    decided_msgs: HashSet<usize>,
    replayed_steps: HashSet<i64>,
    /// Canonical hash per already-consumed message index. If a later serve
    /// presents different bytes for a consumed index (client edit/compaction,
    /// non-append-only history), the memo is stale: everything resets and the
    /// new prefix replays from scratch — keeping warm == cold always.
    seen_msg_hashes: Vec<String>,
    pub insists: u64,
}

/// serve() failure: the caller must pass the ORIGINAL messages through and
/// advance nothing — the same observable behavior as the reference, where
/// these inputs raise out of curate() before any registry commit and the
/// proxy's fail-open serves the input unchanged.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FreezeError {
    #[error("invalid internal message shape: {0}")]
    Invalid(String),
}

impl<S: ChunkScorer> Freezer<S> {
    pub fn new(cfg: FreezeConfig, scorer: S) -> Self {
        Freezer {
            cfg,
            scorer,
            dropped: HashSet::new(),
            dropped_ranges: HashMap::new(),
            dropped_fp: HashSet::new(),
            served_ranges: HashMap::new(),
            served_fp: HashSet::new(),
            decided_msgs: HashSet::new(),
            replayed_steps: HashSet::new(),
            seen_msg_hashes: Vec::new(),
            insists: 0,
        }
    }

    fn reset(&mut self) {
        self.dropped.clear();
        self.dropped_ranges.clear();
        self.dropped_fp.clear();
        self.served_ranges.clear();
        self.served_fp.clear();
        self.decided_msgs.clear();
        self.replayed_steps.clear();
        self.seen_msg_hashes.clear();
        self.insists = 0;
    }

    /// Registries snapshot for parity assertions (sorted, deterministic).
    pub fn registry_snapshot(&self) -> Value {
        let mut dropped: Vec<&String> = self.dropped.iter().collect();
        dropped.sort();
        let mut dfp: Vec<&String> = self.dropped_fp.iter().collect();
        dfp.sort();
        let mut sfp: Vec<&String> = self.served_fp.iter().collect();
        sfp.sort();
        let ranges = |m: &HashMap<String, Vec<(i64, i64)>>| -> Value {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), serde_json::to_value(&m[k]).unwrap()))
                    .collect(),
            )
        };
        serde_json::json!({
            "dropped": dropped, "dropped_fp": dfp, "served_fp": sfp,
            "dropped_ranges": ranges(&self.dropped_ranges),
            "served_ranges": ranges(&self.served_ranges),
            "insists": self.insists,
        })
    }

    fn olap(c: &Chunk, ranges: Option<&Vec<(i64, i64)>>) -> bool {
        let (Some(clo), Some(chi)) = (c.lo, c.hi) else {
            return false;
        };
        let span = std::cmp::max(1, chi - clo + 1);
        ranges.is_some_and(|rs| {
            rs.iter().any(|&(lo, hi)| {
                2 * std::cmp::max(0, std::cmp::min(hi, chi) - std::cmp::max(lo, clo) + 1) >= span
            })
        })
    }

    fn insisted(&self, c: &Chunk) -> bool {
        let fp = norm_fp(&c.text);
        if let (Some(f), Some(_)) = (live_file(c), c.lo) {
            if Self::olap(c, self.served_ranges.get(f)) {
                return false; // he GOT it before: re-read habit, trim away
            }
            return Self::olap(c, self.dropped_ranges.get(f));
        }
        self.dropped_fp.contains(&fp) && !self.served_fp.contains(&fp)
    }

    fn narrow(c: &Chunk) -> bool {
        if RANGE_CMD.is_match(&c.cmd) {
            return true;
        }
        if let Some(m) = SMALL_HEADTAIL.captures(&c.cmd) {
            if m[2].parse::<i64>().is_ok_and(|n| n <= 60) {
                return true;
            }
        }
        if let (Some(lo), Some(hi)) = (c.lo, c.hi) {
            if hi - lo < 60 {
                return true; // span (hi-lo+1) <= 60: one read-window -> targeted, honour it
            }
        }
        false
    }

    /// curator._budget_cut: cut everything scoring below tau, then keep only
    /// contiguous same-owner runs with >= min_run_tokens mass.
    fn budget_cut(
        &self,
        pairs: &[(usize, usize)],
        scores_q: &[i64],
        tau_q: i64,
        chunks: &[Chunk],
        owner: &[usize],
    ) -> Vec<(usize, usize)> {
        if pairs.is_empty() {
            return Vec::new();
        }
        let mut sorted_pairs: Vec<(usize, usize)> = pairs.to_vec();
        sorted_pairs.sort_by_key(|&(j, _)| scores_q[j]);
        let mut out: Vec<(usize, usize)> = Vec::new();
        for &(j, i) in &sorted_pairs {
            if scores_q[j] >= tau_q {
                break;
            }
            out.push((j, i));
        }
        out.sort_by_key(|&(_, i)| i);
        let mut kept: Vec<(usize, usize)> = Vec::new();
        let mut run: Vec<(usize, usize)> = Vec::new();
        let run_tokens =
            |run: &[(usize, usize)]| -> i64 { run.iter().map(|&(_, k)| chunks[k].tokens).sum() };
        for &(j, i) in &out {
            if let Some(&(_, last)) = run.last() {
                if i != last + 1 || owner[i] != owner[last] {
                    if run_tokens(&run) >= self.cfg.min_run_tokens {
                        kept.extend_from_slice(&run);
                    }
                    run.clear();
                }
            }
            run.push((j, i));
        }
        if !run.is_empty() && run_tokens(&run) >= self.cfg.min_run_tokens {
            kept.extend_from_slice(&run);
        }
        kept
    }

    /// Replay the birth decision for step `s` against the registries as of
    /// steps < s. Pure: consumes only chunks with step <= s.
    fn replay_birth(&mut self, s: i64, p: &Parsed, messages: &[Value]) {
        // Live set as of this step, in decision order.
        let mut live: Vec<Chunk> = Vec::new();
        let mut live_owner: Vec<usize> = Vec::new();
        let mut live_gi: Vec<usize> = Vec::new();
        for (gi, c) in p.chunks.iter().enumerate() {
            if c.step <= s && !self.dropped.contains(&ckey(p.owner[gi], c)) {
                live.push(c.clone());
                live_owner.push(p.owner[gi]);
                live_gi.push(gi);
            }
        }
        // Undecided newborns, grouped per owner message in first-encounter order.
        let mut undecided: Vec<(usize, Vec<(usize, usize)>)> = Vec::new();
        for (j, c) in live.iter().enumerate() {
            let mi = live_owner[j];
            if c.evict != "provider" && c.step == s && !self.decided_msgs.contains(&mi) {
                if self.insisted(c) && Self::narrow(c) {
                    self.insists += 1; // agent re-asked this narrow snippet -> serve full, once
                    continue;
                }
                match undecided.iter_mut().find(|(m, _)| *m == mi) {
                    Some((_, v)) => v.push((j, live_gi[j])),
                    None => undecided.push((mi, vec![(j, live_gi[j])])),
                }
            }
        }
        let mut to_drop: Vec<usize> = Vec::new();
        if !undecided.is_empty() {
            let task_text = p
                .first_obs
                .map(|i| char_prefix(&m_text(&messages[i]), 2000).to_string())
                .unwrap_or_default();
            let recent_cmds = char_prefix(
                &p.obs_items
                    .iter()
                    .filter(|(_, _, _, st)| *st <= s && *st >= s - 1)
                    .map(|(_, cmd, _, _)| cmd.clone())
                    .collect::<Vec<_>>()
                    .join(" "),
                4000,
            )
            .to_string();
            let all_mask: Vec<usize> = undecided
                .iter()
                .flat_map(|(_, v)| v.iter().map(|&(j, _)| j))
                .collect();
            let shared = self.scorer.score(&BirthQuery {
                cur_step: s,
                task_text: task_text.clone(),
                recent_cmds: recent_cmds.clone(),
                live: &live,
                live_owner: &live_owner,
                mask: all_mask,
            });
            let multi = undecided.len() > 1;
            for (mi, pairs) in &undecided {
                let tau_q = if let Some(t) = self.cfg.tau_fixed_q {
                    t
                } else if multi {
                    self.scorer
                        .score(&BirthQuery {
                            cur_step: s,
                            task_text: task_text.clone(),
                            recent_cmds: recent_cmds.clone(),
                            live: &live,
                            live_owner: &live_owner,
                            mask: pairs.iter().map(|&(j, _)| j).collect(),
                        })
                        .tau_q
                } else {
                    shared.tau_q
                };
                let drops = self.budget_cut(pairs, &shared.scores_q, tau_q, &p.chunks, &p.owner);
                self.decided_msgs.insert(*mi);
                to_drop.extend(drops.iter().map(|&(_, i)| i));
            }
        }
        for &gi in &to_drop {
            let c = &p.chunks[gi];
            self.dropped.insert(ckey(p.owner[gi], c));
            if let (Some(f), Some(lo), Some(hi)) = (live_file(c), c.lo, c.hi) {
                self.dropped_ranges
                    .entry(f.to_string())
                    .or_default()
                    .push((lo, hi));
            }
            self.dropped_fp.insert(norm_fp(&c.text));
        }
        // Newborns that SURVIVED were served: re-asks for these are habit.
        for (gi, c) in p.chunks.iter().enumerate() {
            if c.step == s && c.evict != "provider" && !self.dropped.contains(&ckey(p.owner[gi], c))
            {
                if let (Some(f), Some(lo), Some(hi)) = (live_file(c), c.lo, c.hi) {
                    self.served_ranges
                        .entry(f.to_string())
                        .or_default()
                        .push((lo, hi));
                }
                self.served_fp.insert(norm_fp(&c.text));
            }
        }
        self.replayed_steps.insert(s);
    }

    /// curator._ptr: recoverable-range pointer inside omission markers.
    fn ptr(file: Option<&str>, lo: Option<i64>, hi: Option<i64>) -> String {
        match (file, lo, hi) {
            (Some(f), Some(lo), Some(hi)) => format!(" · re-read {}:L{}-{}", f, lo, hi),
            _ => String::new(),
        }
    }

    /// curator._digest: informative truncation; returncode line survives.
    fn digest(m: &Value, ntok: i64, file: Option<&str>, lo: Option<i64>, hi: Option<i64>) -> Value {
        let txt = m_text(m);
        let lines = py_splitlines(&txt);
        let rc_idx = lines
            .iter()
            .take(4)
            .position(|ln| ln.to_lowercase().contains("returncode"));
        let body: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(i, ln)| py_has_content(ln) && Some(*i) != rc_idx)
            .map(|(_, ln)| *ln)
            .collect();
        let mut parts: Vec<String> = Vec::new();
        if let Some(ri) = rc_idx {
            parts.push(py_strip(lines[ri]).to_string());
        }
        for ln in body.iter().take(2) {
            parts.push(char_prefix(ln, 300).to_string());
        }
        let omitted = body.len().saturating_sub(3);
        parts.push(format!(
            "[... {} lines (~{} tokens){} omitted ...]",
            omitted,
            ntok,
            Self::ptr(file, lo, hi)
        ));
        if body.len() > 3 {
            parts.push(char_prefix(body[body.len() - 1], 300).to_string());
        }
        let mut out = m.as_object().cloned().unwrap_or_default();
        out.insert("content".into(), Value::String(parts.join("\n")));
        Value::Object(out)
    }

    /// Union L-range over a group of chunks (curator._span).
    fn span(p: &Parsed, gis: &[usize]) -> (Option<String>, Option<i64>, Option<i64>) {
        let f = gis
            .iter()
            .find_map(|&g| live_file(&p.chunks[g]).map(str::to_string));
        let Some(f) = f else {
            return (None, None, None);
        };
        let los: Vec<i64> = gis
            .iter()
            .filter(|&&g| p.chunks[g].file.as_deref() == Some(f.as_str()))
            .filter_map(|&g| p.chunks[g].lo)
            .collect();
        let his: Vec<i64> = gis
            .iter()
            .filter(|&&g| p.chunks[g].file.as_deref() == Some(f.as_str()))
            .filter_map(|&g| p.chunks[g].hi)
            .collect();
        if los.is_empty() || his.is_empty() {
            return (None, None, None);
        }
        (
            Some(f),
            los.iter().min().copied(),
            his.iter().max().copied(),
        )
    }

    /// Pure render of one message from the registries (curate render block).
    fn render_message(&self, m: &Value, idxs: &[usize], p: &Parsed) -> Value {
        let prov: Vec<usize> = idxs
            .iter()
            .copied()
            .filter(|&g| p.chunks[g].evict == "provider")
            .collect();
        let cont: Vec<usize> = idxs
            .iter()
            .copied()
            .filter(|&g| p.chunks[g].evict != "provider")
            .collect();
        let mut r = m.clone();
        if let Some(&g0) = prov.first() {
            if self.dropped.contains(&ckey(p.owner[g0], &p.chunks[g0])) {
                let mut o = r.as_object().cloned().unwrap_or_default();
                for k in REASON_FIELDS {
                    o.remove(k);
                }
                if let Some(Value::Array(tcs)) = o.get("tool_calls").cloned() {
                    let cleaned: Vec<Value> = tcs
                        .iter()
                        .map(|tc| match tc.as_object() {
                            Some(t) => Value::Object(
                                t.iter()
                                    .filter(|(k, _)| k.as_str() != "provider_specific_fields")
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect(),
                            ),
                            None => tc.clone(),
                        })
                        .collect();
                    o.insert("tool_calls".into(), Value::Array(cleaned));
                }
                r = Value::Object(o);
            }
        }
        if !cont.is_empty() {
            let flags: Vec<bool> = cont
                .iter()
                .map(|&g| self.dropped.contains(&ckey(p.owner[g], &p.chunks[g])))
                .collect();
            if flags.iter().all(|&f| f) {
                let (f, lo, hi) = Self::span(p, &cont);
                let ntok: i64 = cont.iter().map(|&g| p.chunks[g].tokens).sum();
                r = Self::digest(&r, ntok, f.as_deref(), lo, hi);
            } else if flags.iter().any(|&f| f) {
                let mut parts: Vec<String> = Vec::new();
                let mut run: i64 = 0;
                let mut runset: Vec<usize> = Vec::new();
                for (&g, &fl) in cont.iter().zip(flags.iter()) {
                    if fl {
                        run += p.chunks[g].tokens;
                        runset.push(g);
                    } else {
                        if run > 0 {
                            let (f, lo, hi) = Self::span(p, &runset);
                            parts.push(format!(
                                "[... ~{} tokens{} omitted ...]",
                                run,
                                Self::ptr(f.as_deref(), lo, hi)
                            ));
                            run = 0;
                            runset.clear();
                        }
                        parts.push(p.chunks[g].text.clone());
                    }
                }
                if run > 0 {
                    let (f, lo, hi) = Self::span(p, &runset);
                    parts.push(format!(
                        "[... ~{} tokens{} omitted ...]",
                        run,
                        Self::ptr(f.as_deref(), lo, hi)
                    ));
                }
                let mut body = parts.join("\n");
                let txt0 = m_text(m);
                if txt0.to_lowercase().contains("returncode")
                    && !body.to_lowercase().contains("returncode")
                {
                    if let Some(rcl) = py_splitlines(&txt0)
                        .iter()
                        .take(4)
                        .find(|ln| ln.to_lowercase().contains("returncode"))
                    {
                        body = format!("{}\n{}", py_strip(rcl), body);
                    }
                }
                let mut o = r.as_object().cloned().unwrap_or_default();
                o.insert("content".into(), Value::String(body));
                r = Value::Object(o);
            }
        }
        r
    }

    /// Serve one inbound internal message list: replay any un-replayed birth
    /// steps in order, then render. Output for any prefix is byte-identical
    /// whether this instance served every intermediate prefix or none. On
    /// Err the caller must serve the original messages and treat the call as
    /// a fail-open event; no state advanced.
    pub fn serve(&mut self, messages: &[Value]) -> Result<Vec<Value>, FreezeError> {
        validate_internal(messages)?;
        // Purity guard: if any already-consumed message's bytes changed
        // (client edit, compaction, index shift), the memo describes a
        // different prefix — reset and replay so warm == cold.
        let hashes: Vec<String> = messages.iter().map(msg_hash).collect();
        let overlap = std::cmp::min(self.seen_msg_hashes.len(), hashes.len());
        if self.seen_msg_hashes[..overlap] != hashes[..overlap]
            || hashes.len() < self.seen_msg_hashes.len()
        {
            self.reset();
        }
        self.seen_msg_hashes = hashes;

        let p = parse(messages, &self.cfg);
        if p.chunks.is_empty() {
            return Ok(messages.to_vec());
        }
        // From 0: an assistant message before the first user/tool message
        // births step-0 chunks (0..=-1 is empty when there are no steps).
        for s in 0..=p.cur_step {
            if !self.replayed_steps.contains(&s) {
                self.replay_birth(s, &p, messages);
            }
        }
        let mut by_msg: HashMap<usize, Vec<usize>> = HashMap::new();
        for (gi, &mi) in p.owner.iter().enumerate() {
            by_msg.entry(mi).or_default().push(gi);
        }
        Ok(messages
            .iter()
            .enumerate()
            .map(|(i, m)| match by_msg.get(&i) {
                Some(idxs) => self.render_message(m, idxs, &p),
                None => m.clone(),
            })
            .collect())
    }
}

fn msg_hash(m: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(py_json_dumps_opts(m, true, false).as_bytes());
    format!("{:x}", h.finalize())
}

/// Reject the message shapes on which the reference raises a TypeError out of
/// curate() (before any registry commit): a content part whose `text` key
/// holds a non-string, an action whose truthy `command`/`query` is a
/// non-string, and a non-object entry in `extra.actions`. Silently coercing
/// these (unwrap_or("")) would commit decisions the reference never made.
fn validate_internal(messages: &[Value]) -> Result<(), FreezeError> {
    for (i, m) in messages.iter().enumerate() {
        if let Some(Value::Array(parts)) = m.get("content") {
            for p in parts {
                if let Some(o) = p.as_object() {
                    if let Some(t) = o.get("text") {
                        if !t.is_string() {
                            return Err(FreezeError::Invalid(format!(
                                "message {i}: content part has non-string text"
                            )));
                        }
                    }
                }
            }
        }
        if let Some(acts) = m
            .get("extra")
            .and_then(|e| e.get("actions"))
            .and_then(Value::as_array)
        {
            for a in acts {
                let Some(o) = a.as_object() else {
                    return Err(FreezeError::Invalid(format!(
                        "message {i}: non-object action"
                    )));
                };
                // Python's `a.get("command") or a.get("query") or ""` only
                // crashes on a TRUTHY non-string; falsy ones fall through.
                for k in ["command", "query"] {
                    if let Some(v) = o.get(k) {
                        if !v.is_string() && crate::messages::truthy(v) {
                            return Err(FreezeError::Invalid(format!(
                                "message {i}: action {k} is truthy non-string"
                            )));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
