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
//! - Scorer failure is a PER-STEP fail-open: the failed step commits nothing
//!   and stays out of the replay memo (the next serve retries it), decided
//!   steps still render, serve() succeeds. The reference raised out of
//!   curate() and the chunks aged out of the birth gate forever; here the
//!   decision is only deferred — brain downtime costs savings, never
//!   correctness.
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
//!
//! Two data-loss bugs were fixed in BOTH this port and the reference in
//! lockstep (so byte-parity holds and the freeze fixtures were regenerated):
//! - `digest` serves its tail line at `body.len() >= 3` (was `> 3`, which at
//!   exactly three body lines dropped the last line while printing "0 lines
//!   omitted" — silent truncation of the served bytes).
//! - `ckey` disambiguates the 60-char text prefix with a full-text sha256 (was
//!   the bare `text[:60]`, which let two chunks sharing their first 60 chars
//!   collide, so dropping one wrongly marked the other dropped).

use std::collections::{HashMap, HashSet};

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
    /// Whether the agent's own message text is cut-eligible. True on the
    /// Anthropic path (reference parity). False on the Responses/Codex path:
    /// reasoning there is `encrypted_content` and tool calls carry no text, so
    /// assistant prose is one of only two cuttable classes and the cutter
    /// leans on it — and that prose is where a Codex turn's plan lives.
    /// Tool output stays cut-eligible either way.
    pub cut_assistant: bool,
    /// Defer the birth decision of the CURRENT turn's observations — every
    /// user/tool message after the last assistant turn — until the next
    /// assistant turn has landed. The agent then always sees the full answer
    /// to the call it just made; the curator folds history, never the
    /// result the model is about to act on. Measured failure mode without
    /// this: a fresh read cut on first serving, the agent (which cannot know
    /// the override protocol from a bare marker) re-asks with a DIFFERENT
    /// narrower call, that is a new birth, cut again — a read spiral that
    /// burns turns and rate limit. False = reference parity (births decided
    /// at their own step; the parity fixtures were generated that way).
    pub protect_current: bool,
    /// Product marker wording: say who elided the chunk and how to get it
    /// back ("omitted by parsec · repeat the identical call to restore"),
    /// and name a line range only when the chunk's coordinates are real
    /// file lines (`chunking::coords_trusted`). False = the reference's
    /// terse "· re-read FILE:Llo-hi" pointer, which was rendered for chunks
    /// the model had NEVER seen and with output-relative line numbers for
    /// `awk`/`tail`-style reads — a marker the agent cannot trust. The
    /// parity fixtures encode the reference wording.
    pub product_markers: bool,
}

impl Default for FreezeConfig {
    fn default() -> Self {
        FreezeConfig {
            read_lines: Some(10),
            mode: ChunkMode::Fixed,
            tau_fixed_q: None,
            min_run_tokens: 10,
            cut_assistant: true,
            protect_current: true,
            product_markers: true,
        }
    }
}

/// One birth-step scoring request. `live` is the not-yet-dropped chunk set as
/// of this step (steps <= cur_step), in decision order; `mask` indexes the
/// undecided chunks within it. `live_gi` maps each live chunk to its GLOBAL
/// index in the parsed chunk array and `messages` is the internal view being
/// served — together they let a remote scorer serialize
/// (messages, live_gi, mask) itself; the engine stays transport-free.
pub struct BirthQuery<'a> {
    pub cur_step: i64,
    pub task_text: String,
    pub recent_cmds: String,
    pub live: &'a [Chunk],
    pub live_owner: &'a [usize],
    pub live_gi: Vec<usize>,
    pub messages: &'a [Value],
    /// sha256 over "\n".join("{step}:{kind}:{tokens}") of the parsed chunk
    /// array — the cross-language chunker-parity guard the brain enforces
    /// per request (contracts brain-api-dev/v0; mismatch = 409, fail open).
    pub chunk_checksum: String,
    pub mask: Vec<usize>,
}

pub struct ScoreResult {
    /// Keep score per live chunk, on the SCORE_SCALE grid.
    pub scores_q: Vec<i64>,
    /// The pool tau (reference `qhat`), same grid.
    pub tau_q: i64,
}

/// A scorer call failed (brain unreachable, timeout, drift 409, ...). The
/// Freezer answers with a per-step fail-open — never a guessed score.
#[derive(Debug, thiserror::Error)]
#[error("scorer: {0}")]
pub struct ScoreError(pub String);

/// The purity contract: same (query, checkpoint, config) -> same result,
/// every time, on every machine. The brain API guarantees this by pinning the
/// checkpoint bundle and emitting grid integers; failure surfaces as
/// `ScoreError` to the per-step fail-open layer, never as a guessed score.
pub trait ChunkScorer {
    fn score(&mut self, q: &BirthQuery) -> Result<ScoreResult, ScoreError>;
}

/// Fail-open floor: keep everything (tau below every representable score).
pub struct PassthroughScorer;
impl ChunkScorer for PassthroughScorer {
    fn score(&mut self, q: &BirthQuery) -> Result<ScoreResult, ScoreError> {
        Ok(ScoreResult {
            scores_q: vec![SCORE_SCALE; q.live.len()],
            tau_q: 0,
        })
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
    fn score(&mut self, q: &BirthQuery) -> Result<ScoreResult, ScoreError> {
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
        Ok(ScoreResult { scores_q, tau_q })
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

/// curator._ckey: chunk kill-registry key. The human-readable 60-char text
/// prefix is disambiguated by a full-text sha256 so two chunks that share their
/// first 60 chars (repetitive logs, banner-prefixed read windows) get DISTINCT
/// keys — dropping one must never mark the other dropped. Matches the reference
/// byte-for-byte (this string feeds `registry_snapshot`, a parity assertion).
fn ckey(owner: usize, c: &Chunk) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(c.text.as_bytes());
    let hex = format!("{:x}", h.finalize());
    format!(
        "{}:{}:{}:{}",
        owner,
        c.kind,
        char_prefix(&c.text, 60),
        &hex[..16]
    )
}

/// Whitespace-normalized 120-char fingerprint (insist valve).
fn norm_fp(text: &str) -> String {
    char_prefix(&py_split_ws(text).join(" "), 120).to_string()
}

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
    /// Message indices of observations that DIRECTLY answer an action (the
    /// first user/tool message after an assistant turn). Later observations
    /// before the next assistant turn inherit `last_cmd` for featurization
    /// but are not that call's result, so the identical-call override never
    /// keys on them.
    direct_obs: HashSet<usize>,
    first_obs: Option<usize>,
    cur_step: i64,
    /// Step of the first observation after the LAST assistant turn — the
    /// start of the current turn's trailing observation run (== cur_step+1
    /// when the history ends on an assistant turn, i.e. nothing is current).
    /// `protect_current` leaves steps >= this undecided.
    tail_lo: i64,
    /// Chunker drift guard (contracts brain-api-dev/v0): sha256 over
    /// "\n".join("{step}:{kind}:{tokens}") of the parsed chunk array, in the
    /// decision order `live_gi` indexes into. The brain re-parses the
    /// internal view and refuses to score on mismatch (409).
    chunk_checksum: String,
}

fn parse(messages: &[Value], cfg: &FreezeConfig) -> Parsed {
    let mut last_cmd = String::new();
    let mut step: i64 = 0;
    let mut first_obs: Option<usize> = None;
    let mut obs_items: Vec<(usize, String, String, i64)> = Vec::new();
    let mut direct_obs: HashSet<usize> = HashSet::new();
    let mut fresh = false;
    let mut tail_lo: i64 = 0;
    let mut reason_items: Vec<(usize, Chunk)> = Vec::new();
    let mut asst_items: Vec<(usize, String, i64)> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        match m.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                last_cmd = actions(m).join(" ; ");
                fresh = true;
                tail_lo = step;
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
                    if fresh {
                        direct_obs.insert(i);
                    }
                }
                fresh = false;
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
        if cfg.cut_assistant && *st <= cur_step {
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
    let chunks: Vec<Chunk> = order.iter().map(|&i| chunks[i].clone()).collect();
    let owner = order.iter().map(|&i| owner[i]).collect();
    let chunk_checksum = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(
            chunks
                .iter()
                .map(|c| format!("{}:{}:{}", c.step, c.kind, c.tokens))
                .collect::<Vec<_>>()
                .join("\n")
                .as_bytes(),
        );
        format!("{:x}", h.finalize())
    };
    Parsed {
        chunks,
        owner,
        obs_items,
        direct_obs,
        first_obs,
        cur_step,
        tail_lo,
        chunk_checksum,
    }
}

/// The deterministic freezer. All fields (except the `scorer_fail_opens`
/// counter) are a memo of the pure fold over the prefix — dropping the whole
/// struct and replaying yields identical output.
pub struct Freezer<S: ChunkScorer> {
    pub cfg: FreezeConfig,
    pub scorer: S,
    dropped: HashSet<String>,
    dropped_ranges: HashMap<String, Vec<(i64, i64)>>,
    dropped_fp: HashSet<String>,
    served_ranges: HashMap<String, Vec<(i64, i64)>>,
    served_fp: HashSet<String>,
    /// Override valve (the agent prompt's contract): the ACTION text of every
    /// observation that had at least one chunk elided. An identical call —
    /// same tool, same arguments, hence the same `Chunk::cmd` — is served in
    /// full, once; an observation served in full takes its call back out.
    dropped_cmds: HashSet<String>,
    decided_msgs: HashSet<usize>,
    replayed_steps: HashSet<i64>,
    /// Canonical hash per already-consumed message index. If a later serve
    /// presents different bytes for a consumed index (client edit/compaction,
    /// non-append-only history), the memo is stale: everything resets and the
    /// new prefix replays from scratch — keeping warm == cold always.
    seen_msg_hashes: Vec<String>,
    pub insists: u64,
    /// Scorer fail-open events (birth steps deferred because a score call
    /// failed). NOT memo state — reset() leaves it alone: the memo fold is
    /// recomputable but the operational record of brain failures is not, and
    /// it must survive client-edit resets to stay alertable (§8.3).
    pub scorer_fail_opens: u64,
    /// Purity-guard resets (a consumed message's bytes changed or the
    /// history shrank). Operational telemetry like `scorer_fail_opens` —
    /// reset() leaves both this and `last_reset_divergence` alone.
    pub resets: u64,
    /// (first divergent message index, history_shrunk) of the most recent
    /// reset — lets the caller log WHICH message churned; the incident
    /// analysis could not attribute resets after the fact.
    pub last_reset_divergence: Option<(usize, bool)>,
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
            dropped_cmds: HashSet::new(),
            decided_msgs: HashSet::new(),
            replayed_steps: HashSet::new(),
            seen_msg_hashes: Vec::new(),
            insists: 0,
            scorer_fail_opens: 0,
            resets: 0,
            last_reset_divergence: None,
        }
    }

    /// Clears the memo of the pure fold; `scorer_fail_opens` deliberately
    /// survives (operational telemetry, not fold state — see field doc).
    fn reset(&mut self) {
        self.dropped.clear();
        self.dropped_ranges.clear();
        self.dropped_fp.clear();
        self.served_ranges.clear();
        self.served_fp.clear();
        self.dropped_cmds.clear();
        self.decided_msgs.clear();
        self.replayed_steps.clear();
        self.seen_msg_hashes.clear();
        self.insists = 0;
    }

    /// Read-only cut registries for the serving layer's curator decision
    /// logs (which file ranges were cut, how many chunks total) —
    /// diagnostics only, never an input to the fold.
    pub fn dropped_ranges(&self) -> &HashMap<String, Vec<(i64, i64)>> {
        &self.dropped_ranges
    }

    /// Ranges served in FULL at least once (insist valve restores land
    /// here) — with `dropped_ranges`, the serving layer's visibility export
    /// for the no-reread hook: dropped-and-never-served content is NOT in
    /// the model's context, so a re-read of it must not be denied.
    pub fn served_ranges(&self) -> &HashMap<String, Vec<(i64, i64)>> {
        &self.served_ranges
    }

    pub fn dropped_count(&self) -> usize {
        self.dropped.len()
    }

    /// The override registry: action texts whose direct result currently
    /// has elided chunks. Exported to the no-reread hook so its loop-breaker
    /// never counts the one repeat the override protocol asks for.
    pub fn dropped_cmds(&self) -> &HashSet<String> {
        &self.dropped_cmds
    }

    /// Registries snapshot for parity assertions (sorted, deterministic).
    pub fn registry_snapshot(&self) -> Value {
        let mut dropped: Vec<&String> = self.dropped.iter().collect();
        dropped.sort();
        let mut dfp: Vec<&String> = self.dropped_fp.iter().collect();
        dfp.sort();
        let mut sfp: Vec<&String> = self.served_fp.iter().collect();
        sfp.sort();
        let mut dcmd: Vec<&String> = self.dropped_cmds.iter().collect();
        dcmd.sort();
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
            "dropped_cmds": dcmd,
            "dropped_ranges": ranges(&self.dropped_ranges),
            "served_ranges": ranges(&self.served_ranges),
            "insists": self.insists,
        })
    }

    /// curator._insisted — the OVERRIDE (the agent prompt's contract): the
    /// exact same call again, identical tool and identical arguments, overrides
    /// the elision and the full result is served. `Chunk::cmd` IS the action
    /// text that produced the observation, so equality on cmd is equality on
    /// the call — for the observation that directly answers it (`direct_obs`);
    /// a trailing user turn inherits cmd but is not its result. A different or
    /// narrower call is a new call and goes through birth admission like any
    /// other. Nothing is keyed on served text: a call whose earlier result was
    /// delivered in full is not in `dropped_cmds`, so a re-ask of it is habit
    /// and is trimmed per policy.
    fn insisted(&self, c: &Chunk, direct: bool) -> bool {
        direct && !c.cmd.is_empty() && self.dropped_cmds.contains(&c.cmd)
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
    /// steps < s. Pure: consumes only chunks with step <= s. STEP-ATOMIC:
    /// every scorer call (the shared score plus every per-owner-pool tau)
    /// happens before any registry commit, so an Err leaves NO trace of this
    /// step — the next serve replays it as if it were current.
    fn replay_birth(&mut self, s: i64, p: &Parsed, messages: &[Value]) -> Result<(), ScoreError> {
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
        // Undecided newborns, grouped per owner message in first-encounter
        // order. Insists count into a local so the valve commits with the
        // rest of the step, not before the fallible scorer calls.
        let mut step_insists: u64 = 0;
        let mut undecided: Vec<(usize, Vec<(usize, usize)>)> = Vec::new();
        for (j, c) in live.iter().enumerate() {
            let mi = live_owner[j];
            if c.evict != "provider" && c.step == s && !self.decided_msgs.contains(&mi) {
                if self.insisted(c, p.direct_obs.contains(&mi)) {
                    step_insists += 1; // identical call re-issued -> serve full, once
                    continue;
                }
                match undecided.iter_mut().find(|(m, _)| *m == mi) {
                    Some((_, v)) => v.push((j, live_gi[j])),
                    None => undecided.push((mi, vec![(j, live_gi[j])])),
                }
            }
        }
        let mut to_drop: Vec<usize> = Vec::new();
        let mut decided: Vec<usize> = Vec::new();
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
                live_gi: live_gi.clone(),
                messages,
                chunk_checksum: p.chunk_checksum.clone(),
                mask: all_mask,
            })?;
            // Multi-owner: ALL per-pool tau calls complete before any pool's
            // decision commits (a failure on pool k must not leave pools
            // 0..k decided).
            let multi = undecided.len() > 1;
            let mut taus: Vec<i64> = Vec::with_capacity(undecided.len());
            for (_, pairs) in &undecided {
                taus.push(if let Some(t) = self.cfg.tau_fixed_q {
                    t
                } else if multi {
                    self.scorer
                        .score(&BirthQuery {
                            cur_step: s,
                            task_text: task_text.clone(),
                            recent_cmds: recent_cmds.clone(),
                            live: &live,
                            live_owner: &live_owner,
                            live_gi: live_gi.clone(),
                            messages,
                            chunk_checksum: p.chunk_checksum.clone(),
                            mask: pairs.iter().map(|&(j, _)| j).collect(),
                        })?
                        .tau_q
                } else {
                    shared.tau_q
                });
            }
            for ((mi, pairs), &tau_q) in undecided.iter().zip(&taus) {
                let drops = self.budget_cut(pairs, &shared.scores_q, tau_q, &p.chunks, &p.owner);
                decided.push(*mi);
                to_drop.extend(drops.iter().map(|&(_, i)| i));
            }
        }
        // Commit point: every scorer call for this step succeeded.
        self.insists += step_insists;
        self.decided_msgs.extend(decided);
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
        // Override registry: a call is "elided" while any chunk of its
        // observation is cut; an observation served in full (an admission
        // survivor, or an override restore) takes its call back out, so the
        // override is one-shot.
        let cut_cmds: HashSet<&str> = to_drop
            .iter()
            .filter(|&&gi| p.direct_obs.contains(&p.owner[gi]))
            .map(|&gi| p.chunks[gi].cmd.as_str())
            .filter(|c| !c.is_empty())
            .collect();
        for &cmd in &cut_cmds {
            self.dropped_cmds.insert(cmd.to_string());
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
                if !c.cmd.is_empty()
                    && p.direct_obs.contains(&p.owner[gi])
                    && !cut_cmds.contains(c.cmd.as_str())
                {
                    self.dropped_cmds.remove(&c.cmd);
                }
            }
        }
        self.replayed_steps.insert(s);
        Ok(())
    }

    /// curator._ptr: recoverable-range pointer inside omission markers — the
    /// L-range (union over the elided chunks) the agent can name. Informational:
    /// what restores content is an IDENTICAL call (`insisted`), not the pointer.
    fn ptr(file: Option<&str>, lo: Option<i64>, hi: Option<i64>) -> String {
        match (file, lo, hi) {
            (Some(f), Some(lo), Some(hi)) => format!(" · re-read {}:L{}-{}", f, lo, hi),
            _ => String::new(),
        }
    }

    /// The omission marker for one elided run. `counts` is the measured
    /// mass ("23 lines (~574 tokens)" or "~1200 tokens"). Reference style:
    /// `[... {counts}{ptr} omitted ...]`. Product style names the elider
    /// and the recovery protocol, and a line range only when `trusted`
    /// (see `FreezeConfig::product_markers`):
    /// `[... {counts} omitted by parsec · was F:L1-9 · repeat the identical call to restore ...]`.
    fn marker(
        &self,
        counts: &str,
        file: Option<&str>,
        lo: Option<i64>,
        hi: Option<i64>,
        trusted: bool,
    ) -> String {
        if !self.cfg.product_markers {
            return format!("[... {}{} omitted ...]", counts, Self::ptr(file, lo, hi));
        }
        let range = match (trusted, file, lo, hi) {
            (true, Some(f), Some(lo), Some(hi)) => format!(" · was {}:L{}-{}", f, lo, hi),
            _ => String::new(),
        };
        format!(
            "[... {} omitted by parsec{} · repeat the identical call to restore ...]",
            counts, range
        )
    }

    /// Whether every read chunk in `gis` carries real file coordinates.
    fn coords_trusted(p: &Parsed, gis: &[usize]) -> bool {
        gis.iter()
            .filter(|&&g| live_file(&p.chunks[g]).is_some())
            .all(|&g| crate::chunking::coords_trusted(&p.chunks[g].cmd))
    }

    /// curator._digest: informative truncation; returncode line survives.
    ///
    /// Renders head(<=2 lines) + an omission marker + tail(1 line). The tail
    /// is served whenever the body has >= 3 non-blank lines: at exactly three,
    /// head(2)+tail(1) is the whole body, so no line is lost. Gating the tail
    /// on `> 3` drops the third line (silent truncation) — fixed here and in
    /// the reference.
    ///
    /// The marker's COUNTS ARE MEASURED against the bytes about to be served,
    /// not handed in by the caller. This used to take the summed tokens of
    /// every dropped chunk in the message and print that as "omitted" even
    /// though head + tail + the returncode line are still served — and print
    /// it unconditionally, so a body that fits entirely in head+tail rendered
    /// as "0 lines (~N tokens) omitted" while omitting nothing at all. A
    /// marker the agent cannot trust is worse than no marker: it makes
    /// present content look absent, which is exactly the failure mode the
    /// curator exists to avoid. Blank lines are excluded from the LINE count
    /// (they carry nothing) but are reflected in the token figure, as is any
    /// 300-char clipping of a kept line.
    fn digest(m: &Value, mark: &dyn Fn(&str) -> String) -> Value {
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
        let mut head: Vec<String> = Vec::new();
        if let Some(ri) = rc_idx {
            head.push(py_strip(lines[ri]).to_string());
        }
        for ln in body.iter().take(2) {
            head.push(char_prefix(ln, 300).to_string());
        }
        let tail: Vec<String> = if body.len() >= 3 {
            vec![char_prefix(body[body.len() - 1], 300).to_string()]
        } else {
            Vec::new()
        };
        let omitted_lines = body.len().saturating_sub(body.len().min(2) + tail.len());
        let kept_mass = {
            let kept = head
                .iter()
                .chain(tail.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            char_len(&kept) as i64 / 4
        };
        let omitted_tok = (char_len(&txt) as i64 / 4 - kept_mass).max(0);
        let marker = if omitted_lines > 0 {
            mark(&format!(
                "{} lines (~{} tokens)",
                omitted_lines, omitted_tok
            ))
        } else if omitted_tok > 0 {
            // Nothing whole was dropped, only clipped: same shape as the
            // partial-run marker, which is already tokens-only.
            mark(&format!("~{} tokens", omitted_tok))
        } else {
            // Head + tail already cover the whole observation. Nothing to
            // announce, so the message is served exactly as it arrived.
            return m.clone();
        };
        let parts: Vec<String> = head
            .into_iter()
            .chain(std::iter::once(marker))
            .chain(tail)
            .collect();
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
                let trusted = Self::coords_trusted(p, &cont);
                r = Self::digest(&r, &|counts| {
                    self.marker(counts, f.as_deref(), lo, hi, trusted)
                });
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
                            let trusted = Self::coords_trusted(p, &runset);
                            parts.push(self.marker(
                                &format!("~{} tokens", run),
                                f.as_deref(),
                                lo,
                                hi,
                                trusted,
                            ));
                            run = 0;
                            runset.clear();
                        }
                        parts.push(p.chunks[g].text.clone());
                    }
                }
                if run > 0 {
                    let (f, lo, hi) = Self::span(p, &runset);
                    let trusted = Self::coords_trusted(p, &runset);
                    parts.push(self.marker(
                        &format!("~{} tokens", run),
                        f.as_deref(),
                        lo,
                        hi,
                        trusted,
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
    /// a fail-open event; no state advanced. A scorer failure is NOT an Err:
    /// the failed step and later ones stay undecided (rendered full) for the
    /// next serve to retry, and `scorer_fail_opens` ticks once.
    pub fn serve(&mut self, messages: &[Value]) -> Result<Vec<Value>, FreezeError> {
        validate_internal(messages)?;
        // Purity guard: if any already-consumed message's bytes changed
        // (client edit, compaction, index shift), the memo describes a
        // different prefix — reset and replay so warm == cold.
        let hashes: Vec<String> = messages.iter().map(msg_hash).collect();
        let overlap = std::cmp::min(self.seen_msg_hashes.len(), hashes.len());
        let shrunk = hashes.len() < self.seen_msg_hashes.len();
        let diverged = (0..overlap).find(|&i| self.seen_msg_hashes[i] != hashes[i]);
        if diverged.is_some() || shrunk {
            self.resets += 1;
            self.last_reset_divergence = Some((diverged.unwrap_or(overlap), shrunk));
            self.reset();
        }
        self.seen_msg_hashes = hashes;

        let p = parse(messages, &self.cfg);
        if p.chunks.is_empty() {
            return Ok(messages.to_vec());
        }
        // From 0: an assistant message before the first user/tool message
        // births step-0 chunks (0..=-1 is empty when there are no steps).
        // `protect_current`: the current turn's observations (steps >=
        // tail_lo) stay undecided — rendered in full — until the next
        // assistant turn makes them history. Deferral keeps warm == cold:
        // a cold replay of the same prefix stops at the same bound, and a
        // deferred step is decided later by the same pure fold over the
        // same chunks (<= s) it would have seen at its own step.
        let replay_end = if self.cfg.protect_current {
            p.tail_lo.min(p.cur_step + 1)
        } else {
            p.cur_step + 1
        };
        for s in 0..replay_end {
            if !self.replayed_steps.contains(&s) && self.replay_birth(s, &p, messages).is_err() {
                // Per-step fail-open: later steps' decisions fold over this
                // step's registries, so stop replaying — every affected step
                // stays out of replayed_steps and the next serve retries.
                // Render still runs: the serve succeeds on decided state.
                self.scorer_fail_opens += 1;
                break;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Render `n` non-blank body lines (L0..L{n-1}) through `digest` with no
    /// re-read pointer and return the served content text.
    /// Reference-style marker builder for the digest unit tests.
    fn ref_mark(ptr: &'static str) -> impl Fn(&str) -> String {
        move |counts| format!("[... {counts}{ptr} omitted ...]")
    }

    fn digest_text(n: usize) -> String {
        let body = (0..n)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let m = json!({ "role": "user", "content": body });
        Freezer::<PassthroughScorer>::digest(&m, &ref_mark(""))
            .get("content")
            .and_then(Value::as_str)
            .unwrap()
            .to_string()
    }

    /// Regression for the served-bytes truncation: an observation digested down
    /// to exactly three body lines MUST keep its last line. The reference's
    /// `> 3` tail guard dropped `L2` here.
    ///
    /// And with all three lines served there is nothing to announce, so no
    /// marker is emitted at all — this used to read
    /// "L0\nL1\n[... 0 lines (~10 tokens) omitted ...]\nL2", a marker that
    /// named a token count while omitting nothing.
    #[test]
    fn digest_keeps_tail_line_at_three_lines_without_a_false_marker() {
        assert_eq!(digest_text(3), "L0\nL1\nL2");
    }

    /// The marker's counts are MEASURED against the served bytes: 10 body
    /// lines render as head(2) + marker + tail(1), so exactly 7 lines are
    /// omitted and the token figure is the mass of what actually went.
    #[test]
    fn digest_marker_counts_match_the_bytes_actually_dropped() {
        let out = digest_text(10);
        let (head, tail) = ("L0\nL1\n", "\nL9");
        assert!(out.starts_with(head) && out.ends_with(tail), "shape: {out}");
        let marker = &out[head.len()..out.len() - tail.len()];
        assert_eq!(marker, "[... 7 lines (~5 tokens) omitted ...]");
        // The claim is checkable: whole text mass minus served mass.
        let full = (0..10)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let kept = "L0\nL1\nL9";
        assert_eq!(
            char_len(&full) as i64 / 4 - char_len(kept) as i64 / 4,
            5,
            "the marker must state the real delta"
        );
    }

    /// A body that fits in head+tail but whose kept lines get CLIPPED at 300
    /// chars still lost content — announced tokens-only, since no whole line
    /// went missing.
    #[test]
    fn digest_announces_clipped_lines_without_claiming_whole_lines() {
        let m = json!({ "role": "user", "content": format!("{}\nshort", "x".repeat(1000)) });
        let out = Freezer::<PassthroughScorer>::digest(&m, &ref_mark(""));
        let txt = out.get("content").and_then(Value::as_str).unwrap();
        assert!(
            txt.contains("[... ~175 tokens omitted ...]"),
            "clipped mass must be announced, got: {txt}"
        );
        assert!(
            !txt.contains(" lines (~"),
            "no whole line was dropped: {txt}"
        );
    }

    /// Blank lines are dropped from the render but carry nothing, so they
    /// never inflate the LINE count — and when that is all that went, the
    /// message is served unchanged rather than carrying an empty claim.
    #[test]
    fn digest_does_not_count_blank_lines_as_omitted_content() {
        let m = json!({ "role": "user", "content": "L0\n\n\nL1" });
        let out = Freezer::<PassthroughScorer>::digest(&m, &ref_mark(""));
        assert_eq!(
            out.get("content").and_then(Value::as_str).unwrap(),
            "L0\n\n\nL1"
        );
    }

    /// No-loss guard: while `head(2)+tail(1)` still covers the whole body
    /// (1..=3 lines) every input line must survive — there is nothing to omit.
    /// (For 4+ lines digest DELIBERATELY omits the middle; that is its job.)
    #[test]
    fn digest_lossless_while_head_plus_tail_cover_the_body() {
        for n in 1..=3 {
            let out = digest_text(n);
            for i in 0..n {
                assert!(
                    out.contains(&format!("L{i}")),
                    "digest of {n} lines dropped L{i}: {out:?}"
                );
            }
        }
    }

    /// Boundaries. 1 and 2 body lines used to carry a spurious "0 lines
    /// (~N tokens) omitted" marker over content that was served in full; they
    /// now ride through untouched. From 4 lines up the middle really is
    /// omitted, and both numbers describe that omission — the token figure is
    /// the chars/4 delta of what went, so it rounds down to ~0 when a single
    /// short line is all that is missing.
    #[test]
    fn digest_boundaries() {
        assert_eq!(digest_text(1), "L0");
        assert_eq!(digest_text(2), "L0\nL1");
        assert_eq!(
            digest_text(4),
            "L0\nL1\n[... 1 lines (~0 tokens) omitted ...]\nL3"
        );
        assert_eq!(
            digest_text(5),
            "L0\nL1\n[... 2 lines (~1 tokens) omitted ...]\nL4"
        );
    }

    /// Regression for the wrongful-over-cut: two chunks in the same message
    /// that share their first 60 chars (repetitive logs, banner-prefixed read
    /// windows) MUST get distinct drop-registry keys — otherwise dropping one
    /// marks the other dropped in the `dropped` HashSet and its content is cut
    /// though it was never scored below tau.
    #[test]
    fn ckey_disambiguates_chunks_sharing_a_60_char_prefix() {
        let prefix = "x".repeat(60);
        let a = Chunk::new(format!("{prefix}AAAA"), None, None, None, 1, "other");
        let b = Chunk::new(format!("{prefix}BBBB"), None, None, None, 1, "other");
        // Identical first 60 chars: the old bare `text[:60]` key collided here.
        assert_eq!(char_prefix(&a.text, 60), char_prefix(&b.text, 60));
        assert_ne!(
            ckey(7, &a),
            ckey(7, &b),
            "distinct chunk texts must yield distinct ckeys"
        );
        // Stable per chunk (cold-replay memo depends on it) and owner-scoped.
        assert_eq!(ckey(7, &a), ckey(7, &a.clone()));
        assert_ne!(ckey(7, &a), ckey(8, &a));
    }

    /// The re-read pointer and the returncode line ride along with the
    /// (now-preserved) tail line — and no marker appears at the three-line
    /// boundary, where head+tail already serve everything.
    #[test]
    fn digest_three_lines_with_returncode_and_pointer() {
        // Three body lines: head+tail cover them, nothing is omitted, so the
        // message rides through whole — pointer and all, because there is
        // nothing to point AT.
        let m = json!({ "role": "user", "content": "returncode: 0\nL0\nL1\nL2" });
        let out = Freezer::<PassthroughScorer>::digest(&m, &ref_mark(" · re-read f.py:L5-9"));
        assert_eq!(
            out.get("content").and_then(Value::as_str).unwrap(),
            "returncode: 0\nL0\nL1\nL2"
        );
        // Once there IS something to omit, the returncode line and the
        // recoverable-range pointer both still ride along with the tail.
        let body = (0..8)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let m = json!({ "role": "user", "content": format!("returncode: 0\n{body}") });
        let out = Freezer::<PassthroughScorer>::digest(&m, &ref_mark(" · re-read f.py:L5-9"));
        assert_eq!(
            out.get("content").and_then(Value::as_str).unwrap(),
            "returncode: 0\nL0\nL1\n[... 5 lines (~4 tokens) · re-read f.py:L5-9 omitted ...]\nL7"
        );
    }

    /// A scorer that wants to cut everything: score 0 < tau, so every live
    /// chunk is below threshold and only `min_run_tokens` and the eligibility
    /// gates decide what survives.
    struct CutAllScorer;
    impl ChunkScorer for CutAllScorer {
        fn score(&mut self, q: &BirthQuery) -> Result<ScoreResult, ScoreError> {
            Ok(ScoreResult {
                scores_q: vec![0; q.live.len()],
                tau_q: SCORE_SCALE,
            })
        }
    }

    fn prose(tag: &str, n: usize) -> String {
        (0..n)
            .map(|i| format!("{tag} line {i} with enough words on it to carry mass"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Reference-parity policy: births decided at their own step, terse
    /// markers — what the parity fixtures encode.
    fn reference_cfg() -> FreezeConfig {
        FreezeConfig {
            protect_current: false,
            product_markers: false,
            ..FreezeConfig::default()
        }
    }

    /// The override valve is the agent prompt's contract: an elided result is
    /// served in full when the EXACT same call is made again — and only then.
    /// A trailing user turn that merely inherits the action's cmd is not that
    /// call's result; a different (narrower) call is a new call; a call whose
    /// result was already delivered in full is not restored on a repeat.
    #[test]
    fn identical_call_overrides_elision_once_for_its_direct_result_only() {
        let obs = format!("returncode: 0\n{}", prose("out", 60));
        let act = |cmd: &str| {
            json!({ "role": "assistant", "content": "ok",
                    "extra": { "actions": [{ "command": cmd }] } })
        };
        let msgs = vec![
            json!({ "role": "user", "content": "do the thing" }),
            act("cat a.py"),
            json!({ "role": "user", "content": obs.clone() }), // 2: cut at birth
            json!({ "role": "user", "content": obs.clone() }), // 3: inherits cmd, NOT the call's result
            act("cat a.py"),
            json!({ "role": "user", "content": obs.clone() }), // 5: identical call -> full
            act("sed -n '1,5p' a.py"),
            json!({ "role": "user", "content": obs.clone() }), // 7: different call -> curated
            act("cat a.py"),
            json!({ "role": "user", "content": obs.clone() }), // 9: served full at 5 -> curated
        ];
        let mut fz = Freezer::new(reference_cfg(), CutAllScorer);
        let out = fz.serve(&msgs).unwrap();
        let served = |i: usize| out[i].get("content").and_then(Value::as_str).unwrap();
        assert_ne!(served(2), obs, "first delivery is cut");
        assert_ne!(served(3), obs, "inherited cmd is not an identical call");
        assert_eq!(served(5), obs, "identical call overrides the elision");
        assert_ne!(served(7), obs, "a narrower call is a new call");
        assert_ne!(served(9), obs, "override is one-shot: served full at 5");
        assert!(fz.insists > 0);
        assert_eq!(
            fz.registry_snapshot()["dropped_cmds"],
            json!(["cat a.py", "sed -n '1,5p' a.py"]),
            "the repeat at 9 was cut again, so cat is back in the registry"
        );
    }

    /// `cut_assistant: false` (the Responses/Codex path) leaves the agent's own
    /// prose byte-identical while tool output stays cut-eligible. The default
    /// config (Anthropic path) still cuts both.
    #[test]
    fn cut_assistant_false_spares_prose_but_not_tool_output() {
        let asst = prose("plan", 40);
        let obs = format!("returncode: 0\n{}", prose("out", 40));
        let msgs = vec![
            json!({ "role": "user", "content": "do the thing" }),
            json!({ "role": "assistant", "content": asst.clone() }),
            json!({ "role": "user", "content": obs.clone() }),
            json!({ "role": "user", "content": "and now the next thing" }),
        ];

        let cfg = FreezeConfig {
            cut_assistant: false,
            ..reference_cfg()
        };
        let mut fz = Freezer::new(cfg, CutAllScorer);
        let out = fz.serve(&msgs).unwrap();
        let served_asst = out[1].get("content").and_then(Value::as_str).unwrap();
        let served_obs = out[2].get("content").and_then(Value::as_str).unwrap();
        assert_eq!(
            served_asst, asst,
            "assistant prose must ride through verbatim"
        );
        assert!(
            served_obs.contains(" omitted ...]"),
            "tool output must still be cut-eligible: {served_obs}"
        );

        let mut fz = Freezer::new(reference_cfg(), CutAllScorer);
        let out = fz.serve(&msgs).unwrap();
        assert!(
            out[1].get("content").and_then(Value::as_str).unwrap() != asst,
            "default config still cuts assistant prose (Anthropic parity)"
        );
    }

    /// `protect_current` (product default): the result the agent is about to
    /// act on is never score-cut. Once the next assistant turn lands it is
    /// history and goes through birth admission like any other chunk — and a
    /// cold freezer over the same prefix serves the same bytes.
    #[test]
    fn current_turn_result_is_served_full_then_curated_as_history() {
        let obs = format!("returncode: 0\n{}", prose("out", 60));
        let act = |cmd: &str| {
            json!({ "role": "assistant", "content": "ok",
                    "extra": { "actions": [{ "command": cmd }] } })
        };
        let turn1 = vec![
            json!({ "role": "user", "content": "do the thing" }),
            act("cat a.py"),
            json!({ "role": "user", "content": obs.clone() }), // 2: current -> full
        ];
        let mut warm = Freezer::new(FreezeConfig::default(), CutAllScorer);
        let out = warm.serve(&turn1).unwrap();
        assert_eq!(
            out[2].get("content").and_then(Value::as_str).unwrap(),
            obs,
            "the current turn's direct result is served verbatim"
        );
        assert_eq!(warm.dropped_count(), 0, "nothing decided while current");

        let mut turn2 = turn1.clone();
        turn2.push(act("cat b.py"));
        turn2.push(json!({ "role": "user", "content": obs.clone() })); // 4: current
        let out = warm.serve(&turn2).unwrap();
        let served = |o: &[Value], i: usize| {
            o[i].get("content")
                .and_then(Value::as_str)
                .unwrap()
                .to_string()
        };
        assert_ne!(
            served(&out, 2),
            obs,
            "yesterday's result is history: curated"
        );
        assert_eq!(served(&out, 4), obs, "today's result is current: full");

        let mut cold = Freezer::new(FreezeConfig::default(), CutAllScorer);
        let cold_out = cold.serve(&turn2).unwrap();
        assert_eq!(
            serde_json::to_value(&cold_out).unwrap(),
            serde_json::to_value(&out).unwrap(),
            "cold == warm under deferral"
        );
        // Idempotent re-serve of the same prefix.
        let again = warm.serve(&turn2).unwrap();
        assert_eq!(
            serde_json::to_value(&again).unwrap(),
            serde_json::to_value(&out).unwrap()
        );
    }

    /// Parallel results of one turn — several user/tool observations after a
    /// single assistant turn (the Responses shape, and the Anthropic shape
    /// once its tool_results are split) — are ALL current, not just the
    /// first one.
    #[test]
    fn every_observation_of_the_current_turn_is_protected() {
        let obs = |t: &str| format!("returncode: 0\n{}", prose(t, 40));
        let msgs = vec![
            json!({ "role": "user", "content": "do the thing" }),
            json!({ "role": "assistant", "content": "ok",
                    "extra": { "actions": [{ "command": "cat a.py" }, { "command": "cat b.py" }] } }),
            json!({ "role": "user", "content": obs("a") }),
            json!({ "role": "user", "content": obs("b") }),
            json!({ "role": "user", "content": obs("c") }),
        ];
        let mut fz = Freezer::new(FreezeConfig::default(), CutAllScorer);
        let out = fz.serve(&msgs).unwrap();
        for (i, tag) in [(2, "a"), (3, "b"), (4, "c")] {
            assert_eq!(
                out[i].get("content").and_then(Value::as_str).unwrap(),
                obs(tag),
                "sibling {i} of the current turn is served full"
            );
        }
    }

    /// Product markers name the elider and the recovery protocol, and only
    /// point at a line range when the coordinates are real file lines.
    #[test]
    fn product_markers_say_who_and_how_and_only_trusted_ranges() {
        let obs = format!("returncode: 0\n{}", prose("out", 60));
        let history = |cmd: &str| {
            vec![
                json!({ "role": "user", "content": "do the thing" }),
                json!({ "role": "assistant", "content": "ok",
                        "extra": { "actions": [{ "command": cmd }] } }),
                json!({ "role": "user", "content": obs.clone() }),
                json!({ "role": "assistant", "content": "next" }),
                json!({ "role": "user", "content": "go on" }),
            ]
        };
        let serve = |cmd: &str| {
            let mut fz = Freezer::new(FreezeConfig::default(), CutAllScorer);
            let out = fz.serve(&history(cmd)).unwrap();
            out[2]
                .get("content")
                .and_then(Value::as_str)
                .unwrap()
                .to_string()
        };
        // The Read tool renders as an explicit sed window: coordinates are real.
        let m = serve("sed -n '10,69p' a.py");
        assert!(m.contains(" omitted by parsec"), "names the elider: {m}");
        assert!(
            m.contains("repeat the identical call to restore"),
            "states the recovery protocol: {m}"
        );
        assert!(m.contains("· was a.py:L10-"), "trusted range is named: {m}");
        assert!(!m.contains("re-read"), "no false 're-read' claim: {m}");
        // `tail` renumbers its output: the range would be a lie, so none.
        let m = serve("tail -n 60 a.py");
        assert!(m.contains("by parsec"), "{m}");
        assert!(
            !m.contains(":L"),
            "untrusted coordinates are not named: {m}"
        );
        // Reference wording stays byte-identical under the parity policy.
        let mut fz = Freezer::new(reference_cfg(), CutAllScorer);
        let out = fz.serve(&history("sed -n '10,69p' a.py")).unwrap();
        let m = out[2].get("content").and_then(Value::as_str).unwrap();
        assert!(m.contains(" · re-read a.py:L10-"), "{m}");
        assert!(!m.contains("parsec"), "{m}");
    }
}
