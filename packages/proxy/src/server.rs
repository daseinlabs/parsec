//! `dasein proxy` — the local Anthropic-wire passthrough proxy (DIRECTION.md
//! §4): Claude Code points `ANTHROPIC_BASE_URL` here; we apply cache-safe
//! splicing + breakpoint placement and forward to api.anthropic.com with the
//! user's own auth headers. Port of the wire layer of
//! `service/curating_proxy.py` (367-520) + `service/anthropic_upstream.py`
//! (header forwarding, SSE relay) + `service/anthropic_sse.py`
//! (`usage_from_sse`).
//!
//! Invariants owned here:
//! - **Fail-open floor (§8.3)**: any internal error degrades to forwarding
//!   the ORIGINAL inbound body verbatim; fail-open events are counted
//!   ([`AppState::fail_open_count`]) and marked in the ledger row, never
//!   silent, and we never 500 from our own logic. Upstream HTTP errors are
//!   relayed to the client as-is.
//! - **Measurement honesty (§8.4)**: every request gets a free
//!   `count_tokens` probe on the ORIGINAL body; the ledger row carries that
//!   counterfactual next to the actually-billed usage. A failed probe
//!   records null — we never estimate.
//! - **Cache correctness over cache warmth**: the per-conversation memo
//!   ([`ConvState`]) is a CACHE. Losing it (restart) must only cost cache
//!   warmth, never correctness — folds replay byte-idempotently, and
//!   fingerprints are committed ONLY after upstream confirms success (the
//!   anthropic_shapes.py:484 ordering bug, fixed per
//!   docs/freeze-design.md "Open items").

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use dasein_engine::freeze::{FreezeConfig, Freezer};
use dasein_engine::pystr::py_json_dumps;

use crate::brain::{self, BrainConfig, BrainScorer};
use crate::internal::to_internal;
use crate::splice::{self, FoldMap};

/// Per-conversation memo. Strictly a cache (see module doc): `folds` are the
/// exact bytes already served per turn (splice::FoldMap), `last_fps` the
/// message fingerprints of the last SUCCESSFUL upstream call — the proof of
/// what Anthropic's cache actually holds. `freezer` is the deterministic
/// fold memo (replayable from scratch — losing it costs brain round trips,
/// never bytes); `tool_keep` is the once-per-conversation frozen keep-set
/// (re-pruning per turn would vary the prefix and bust the provider cache).
pub struct ConvState {
    pub folds: FoldMap,
    pub last_fps: Vec<String>,
    pub freezer: Option<Freezer<BrainScorer>>,
    pub tool_keep: Option<HashSet<String>>,
    /// Eviction clock (reference sessions.py TTL semantics). Memos are pure
    /// caches, so evicting a live conversation only costs replay round trips
    /// and one provider-cache re-seed — never bytes.
    pub touched: std::time::Instant,
}

impl Default for ConvState {
    fn default() -> Self {
        ConvState {
            folds: FoldMap::default(),
            last_fps: Vec::new(),
            freezer: None,
            tool_keep: None,
            touched: std::time::Instant::now(),
        }
    }
}

/// Evict conversation memos idle past `ttl`, then oldest-first down to `cap`
/// (reference: DASEIN_SESSION_TTL_S=3600, DASEIN_SESSION_MAX=512). A
/// long-lived auto-started proxy must not grow without bound.
fn evict_stale(
    convs: &mut HashMap<String, ConvState>,
    now: std::time::Instant,
    ttl: std::time::Duration,
    cap: usize,
) -> usize {
    let before = convs.len();
    convs.retain(|_, cs| now.duration_since(cs.touched) < ttl);
    if convs.len() > cap {
        let mut by_age: Vec<(String, std::time::Instant)> = convs
            .iter()
            .map(|(k, cs)| (k.clone(), cs.touched))
            .collect();
        by_age.sort_by_key(|(_, t)| *t);
        for (k, _) in by_age.iter().take(convs.len() - cap) {
            convs.remove(k);
        }
    }
    before - convs.len()
}

/// Shared proxy state. `convs` uses a std Mutex — every touch is short and
/// non-async (lock is never held across an await).
pub struct AppState {
    pub upstream_base: String,
    pub client: reqwest::Client,
    pub convs: Mutex<HashMap<String, ConvState>>,
    /// §8.3: fail-open is a first-class metric, not a silent branch.
    pub fail_open_count: AtomicU64,
    pub ledger_path: PathBuf,
    /// Real-scorer path (docs/brain-serving-v0.md). None = v0 passthrough
    /// curation exactly as before.
    pub brain: Option<BrainConfig>,
    /// Idle/self-shutdown bookkeeping: epoch-seconds of the last inbound
    /// request and the number currently in flight.
    pub last_request_epoch_s: AtomicU64,
    pub in_flight: AtomicU64,
}

fn epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl AppState {
    pub fn new(upstream_base: String, ledger_path: PathBuf) -> Self {
        Self::with_brain(upstream_base, ledger_path, None)
    }

    pub fn with_brain(
        upstream_base: String,
        ledger_path: PathBuf,
        brain: Option<BrainConfig>,
    ) -> Self {
        Self {
            upstream_base,
            client: reqwest::Client::new(),
            convs: Mutex::new(HashMap::new()),
            fail_open_count: AtomicU64::new(0),
            ledger_path,
            brain,
            last_request_epoch_s: AtomicU64::new(epoch_s()),
            in_flight: AtomicU64::new(0),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.upstream_base.trim_end_matches('/'), path)
    }

    fn touch(&self) {
        self.last_request_epoch_s
            .store(epoch_s(), Ordering::Relaxed);
    }
}

/// RAII in-flight marker so the idle-exit sweep never kills a request that
/// is mid-relay; owns an Arc so it can ride inside a streaming body and keep
/// the proxy alive until the last SSE byte.
struct InFlight(Arc<AppState>);
impl InFlight {
    fn enter(st: &Arc<AppState>) -> Self {
        st.touch();
        st.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlight(st.clone())
    }
}
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

/// `dasein proxy` entrypoint: serve on 127.0.0.1:$DASEIN_PROXY_PORT
/// (default 8082), upstream $DASEIN_UPSTREAM (default api.anthropic.com).
pub fn run() -> anyhow::Result<()> {
    let port: u16 = std::env::var("DASEIN_PROXY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8082);
    let upstream = std::env::var("DASEIN_UPSTREAM")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let ledger = PathBuf::from(home).join(".dasein").join("ledger.jsonl");
    let brain = BrainConfig::from_env();
    if let Some(b) = &brain {
        match b.contract {
            crate::brain::BrainContract::Dev => tracing::info!(
                url = %b.url, target_cov = %b.target_cov, tool_prune = b.tool_prune,
                "brain scorer active — DEV RAW-TEXT contract (brain-api-dev/v0), \
                 our-machines-only posture"
            ),
            crate::brain::BrainContract::V1 => tracing::info!(
                url = %b.url, target_cov = %b.target_cov, tool_prune = b.tool_prune,
                embed = %b.embed_backend,
                "brain scorer active — brain-api/v1 (client featurization, \
                 no raw text on the wire)"
            ),
        }
    }
    let state = Arc::new(AppState::with_brain(upstream, ledger, brain));

    // Lifecycle dials: DASEIN_PROXY_IDLE_EXIT_S (0/unset = run forever — the
    // manual default; the plugin's SessionStart auto-start sets 1800 so the
    // managed proxy also turns itself OFF), plus the reference's conversation
    // memo bounds (sessions.py: TTL 3600s, max 512).
    let idle_exit_s: u64 = std::env::var("DASEIN_PROXY_IDLE_EXIT_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let ttl_s: u64 = std::env::var("DASEIN_SESSION_TTL_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let cap: usize = std::env::var("DASEIN_SESSION_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    if idle_exit_s > 0 {
        tracing::info!("idle self-shutdown armed: exit after {idle_exit_s}s without traffic");
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let maint = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.tick().await; // the immediate first tick
            loop {
                tick.tick().await;
                let evicted = evict_stale(
                    &mut lock(&maint.convs),
                    std::time::Instant::now(),
                    std::time::Duration::from_secs(ttl_s),
                    cap,
                );
                if evicted > 0 {
                    tracing::info!(
                        evicted,
                        "conversation memos evicted (TTL/cap) — \
                                    pure caches, replayable"
                    );
                }
                if idle_exit_s > 0
                    && maint.in_flight.load(Ordering::SeqCst) == 0
                    && epoch_s().saturating_sub(maint.last_request_epoch_s.load(Ordering::Relaxed))
                        >= idle_exit_s
                {
                    tracing::info!(
                        "no traffic for {idle_exit_s}s and nothing in flight — exiting \
                         (the plugin SessionStart hook restarts the proxy on demand)"
                    );
                    std::process::exit(0);
                }
            }
        });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        tracing::info!("dasein proxy listening on 127.0.0.1:{port}");
        axum::serve(listener, router(state)).await?;
        Ok(())
    })
}

/// Route table: the two Anthropic POST endpoints we speak; everything else
/// 404s (we are a messages proxy, not a general gateway).
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens_passthrough))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(state)
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock means a panic elsewhere; recovering the guard keeps the
    // fail-open floor — worst case is a stale memo, i.e. lost cache warmth.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// anthropic_upstream.AnthropicUpstream._headers, narrowed: forward ONLY the
/// auth surface — x-api-key, authorization, anthropic-* — verbatim, plus the
/// bench run-id tag `x-ccb-run-id` (the usage gateway keys its per-request
/// rows by it; the reference forwarded it, and a SHARED gateway below the
/// proxy cannot isolate runs without it). These values are never logged or
/// stored (§3: user's own auth headers pass through, subscription tokens
/// never touch our cloud).
fn forward_auth_headers(inbound: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        let n = name.as_str();
        if n == "x-api-key"
            || n == "authorization"
            || n == "x-ccb-run-id"
            || n.starts_with("anthropic-")
        {
            out.append(name.clone(), value.clone());
        }
    }
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    out
}

/// service/sessions.py conversation_id: sha256 of the JSON of the first two
/// messages' content (the TASK HEAD — constant across a run while the tail
/// grows), hex[..24]; prefixed by the bench run-id header when present so
/// concurrent tasks in one batch get separate memos. Like the reference
/// (which hashed the `to_internal` view, system entry first), the head is
/// taken from the INTERNAL view — hashing raw `body.messages[..2]` would
/// make turn 1 (`[u1]`) and turn 2 (`[u1, a1]`) different conversations.
fn conversation_id(headers: &HeaderMap, internal: &[Value]) -> String {
    let head: Vec<Value> = internal
        .iter()
        .take(2)
        .map(|m| m.get("content").cloned().unwrap_or(Value::Null))
        .collect();
    let mut h = Sha256::new();
    h.update(py_json_dumps(&Value::Array(head)).as_bytes());
    let hash = format!("{:x}", h.finalize());
    let hash = &hash[..24];
    match headers.get("x-ccb-run-id").and_then(|v| v.to_str().ok()) {
        Some(rid) if !rid.trim().is_empty() => format!("{}:{}", rid.trim(), hash),
        _ => hash.to_string(),
    }
}

fn sha8_of_fps<'a>(fps: impl Iterator<Item = &'a str>) -> String {
    let mut h = Sha256::new();
    for f in fps {
        h.update(f.as_bytes());
    }
    format!("{:x}", h.finalize())[..8].to_string()
}

fn note_fail_open(st: &AppState, why: &str) {
    let n = st.fail_open_count.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::warn!(
        fail_open_total = n,
        "proxy fail-open (§8.3): {why} — forwarding original body verbatim"
    );
}

/// §8.1 capture seam: with `DASEIN_RECORD_DIR` set, every real /v1/messages
/// body is dumped VERBATIM (pre-curation) to `<dir>/<conv_id>/turn_<n>.json`
/// — a maintainer runs real Claude Code through the proxy once, then feeds
/// the conversation directory to scripts/record_to_fixture.py. count_tokens
/// and client-metadata calls never record (they never curate either). The
/// per-conv counter is the count of files already on disk, so a proxy
/// restart mid-recording keeps appending instead of overwriting. Fail-open:
/// a dump error is logged and the request proceeds untouched.
fn record_inbound(headers: &HeaderMap, body: &Value, raw: &[u8]) {
    let Some(dir) = std::env::var("DASEIN_RECORD_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
    else {
        return;
    };
    let conv_id = conversation_id(headers, &to_internal(body));
    let res: io::Result<()> = (|| {
        let d = PathBuf::from(dir.trim()).join(&conv_id);
        std::fs::create_dir_all(&d)?;
        let n = std::fs::read_dir(&d)?
            .filter_map(Result::ok)
            .filter(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                s.starts_with("turn_") && s.ends_with(".json")
            })
            .count()
            + 1;
        std::fs::write(d.join(format!("turn_{n}.json")), raw)
    })();
    if let Err(e) = res {
        tracing::warn!("DASEIN_RECORD_DIR dump failed (recording is fail-open): {e}");
    }
}

// ── curation plan (spec steps b-d) ──────────────────────────────────────────

/// Capture-seam telemetry for the ledger row (docs/brain-serving-v0.md);
/// zero-valued fields are omitted from the row.
#[derive(Default, Clone)]
struct PlanStats {
    checkpoint_id: Option<String>,
    brain_ms: f64,
    scorer_fail_opens: u64,
    /// Internal-view chars/4 the freezer trimmed THIS call (uncut − rendered)
    /// — a diagnostic, never a savings claim (§8.4).
    freeze_cut_tokens: i64,
    tools_total: Option<usize>,
    tools_kept: Option<usize>,
    tools_pre_prune_sha8: Option<String>,
}

struct Plan {
    conv_id: String,
    out_bytes: Vec<u8>,
    /// This call's message fingerprints — committed to ConvState only after
    /// upstream 2xx (the anthropic_shapes.py:484 fix).
    cur_fps: Vec<String>,
    /// cachePrefixSha8 bust attribution (§4.3): sha8 over the leading run of
    /// cur_fps proven byte-identical to the prior successful call.
    cache_prefix_sha8: String,
    stats: PlanStats,
}

/// Internal-view token mass (reference _PROMPT diag: Σ len(_text(m))//4).
fn internal_mass(msgs: &[Value]) -> i64 {
    msgs.iter()
        .map(|m| {
            let t = match m.get("content") {
                Some(Value::String(s)) => s.clone(),
                other => splice::content_text(other),
            };
            dasein_engine::pystr::char_len(&t) as i64 / 4
        })
        .sum()
}

/// Curation: the FULL to_internal view (roles + bash-twin actions) through
/// the deterministic Freezer when a brain is configured, else the v0
/// passthrough shape (internal text == original text, so apply_curation's
/// equality branch forwards every turn verbatim while recording folds — the
/// wire freeze is live even before a curator cuts anything).
async fn curate(st: &Arc<AppState>, headers: &HeaderMap, body: &Value) -> anyhow::Result<Plan> {
    if body.get("messages").and_then(Value::as_array).is_none() {
        anyhow::bail!("body has no messages array");
    }
    let internal = to_internal(body);
    let conv_id = conversation_id(headers, &internal);
    let mut stats = PlanStats::default();
    lock(&st.convs).entry(conv_id.clone()).or_default().touched = std::time::Instant::now();

    // sessions.py new-run reset: a request with no assistant turn is turn 1
    // of a fresh run — stale memos (folds, freezer, tool keep-set) from an
    // earlier run with the same task head must not leak into it.
    let has_assistant = internal
        .iter()
        .any(|m| m.get("role").and_then(Value::as_str) == Some("assistant"));
    if !has_assistant {
        lock(&st.convs).remove(&conv_id);
    }

    // The Freezer path: serve() replays un-replayed birth steps (brain round
    // trips) and renders. Runs in spawn_blocking — BrainScorer is a blocking
    // HTTP client. The freezer is TAKEN from the memo and put back after; a
    // racing request on the same conversation just rebuilds the memo from
    // scratch (pure fold — identical bytes, extra latency only).
    let curated_internal: Vec<Value> = if let Some(bcfg) = &st.brain {
        let taken = lock(&st.convs)
            .entry(conv_id.clone())
            .or_default()
            .freezer
            .take();
        let internal_in = internal.clone();
        let bcfg2 = bcfg.clone();
        let conv2 = conv_id.clone();
        // Freezer (and its blocking HTTP scorer) is built AND driven on a
        // blocking thread — reqwest::blocking panics on async runtime threads.
        let (fz, served, fails_before) = tokio::task::spawn_blocking(move || {
            let mut fz = taken.unwrap_or_else(|| {
                Freezer::new(FreezeConfig::default(), BrainScorer::new(bcfg2, conv2))
            });
            let fails_before = fz.scorer_fail_opens;
            let served = fz.serve(&internal_in);
            (fz, served, fails_before)
        })
        .await
        .map_err(|e| anyhow::anyhow!("freezer task panicked: {e}"))?;
        stats.scorer_fail_opens = fz.scorer_fail_opens - fails_before;
        stats.brain_ms = fz.scorer.stats.brain_ms;
        stats.checkpoint_id = fz.scorer.stats.checkpoint_id.clone();
        lock(&st.convs).entry(conv_id.clone()).or_default().freezer = Some(fz);
        match served {
            Ok(c) => {
                stats.freeze_cut_tokens = (internal_mass(&internal) - internal_mass(&c)).max(0);
                c
            }
            // Invalid internal shape: the reference raises out of curate()
            // before any commit and fails open — same observable behavior.
            Err(e) => anyhow::bail!("freeze rejected internal view: {e}"),
        }
    } else {
        internal.clone()
    };

    let (mut folds, prior_fps) = {
        let mut convs = lock(&st.convs);
        let cs = convs.entry(conv_id.clone()).or_default();
        (cs.folds.clone(), cs.last_fps.clone())
    };
    let curated = splice::apply_curation(body, &curated_internal, Some(&mut folds));
    // Folds MAY commit before the send: they memoize served bytes and replay
    // idempotently — unlike fingerprints, which assert "upstream has cached
    // these bytes" and must wait for the 2xx (see below).
    lock(&st.convs).entry(conv_id.clone()).or_default().folds = folds;

    let prior = if prior_fps.is_empty() {
        None
    } else {
        Some(prior_fps.as_slice())
    };
    let (mut out, cur_fps) = splice::place_cache_breakpoint(&curated, false, prior);

    // Tool-schema keep-set (reference _prepare_anthropic step 10): score the
    // roster ONCE per conversation via the brain tool head, freeze the
    // keep-set, filter every later request against it. Fail-open at every
    // seam: scoring failure = full roster + retry next request. Added guard
    // vs the reference: a client-forced tool_choice is served the full
    // roster (the reference could 400 upstream on a pruned forced tool).
    if let Some(bcfg) = &st.brain {
        if bcfg.tool_prune {
            if let Some(tools) = body
                .get("tools")
                .filter(|t| t.as_array().is_some_and(|a| !a.is_empty()))
            {
                stats.tools_total = Some(tools.as_array().map(|a| a.len()).unwrap_or(0));
                stats.tools_pre_prune_sha8 = Some(brain::roster_sha8(tools));
                let keep = lock(&st.convs)
                    .entry(conv_id.clone())
                    .or_default()
                    .tool_keep
                    .clone();
                let keep = match keep {
                    Some(k) => Some(k),
                    None => {
                        match brain::score_tools(&st.client, bcfg, &conv_id, &internal, tools).await
                        {
                            Some(ts) => {
                                let res = brain::prune(
                                    &ts.scores_q,
                                    &ts.tokens,
                                    &ts.names,
                                    bcfg.tool_cut,
                                );
                                let kset: HashSet<String> = res.keep.iter().cloned().collect();
                                tracing::info!(
                                    conv = %conv_id,
                                    kept = res.keep.len(),
                                    total = ts.names.len(),
                                    saved_tok = res.tokens_saved,
                                    "tool-prune keep-set frozen"
                                );
                                lock(&st.convs)
                                    .entry(conv_id.clone())
                                    .or_default()
                                    .tool_keep = Some(kset.clone());
                                Some(kset)
                            }
                            None => None,
                        }
                    }
                };
                let forced =
                    body.pointer("/tool_choice/type").and_then(Value::as_str) == Some("tool");
                if let (Some(keep), false) = (keep, forced) {
                    let src: Vec<Value> = out
                        .get("tools")
                        .or(Some(tools))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let kept: Vec<Value> = src
                        .into_iter()
                        .filter(|t| {
                            t.get("name")
                                .and_then(Value::as_str)
                                .is_some_and(|n| keep.contains(n))
                        })
                        .collect();
                    stats.tools_kept = Some(kept.len());
                    if let Some(o) = out.as_object_mut() {
                        o.insert("tools".into(), Value::Array(kept));
                    }
                }
            }
        }
    }

    let frozen = cur_fps
        .iter()
        .zip(prior_fps.iter())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.as_str());
    let cache_prefix_sha8 = sha8_of_fps(frozen);

    Ok(Plan {
        conv_id,
        out_bytes: serde_json::to_vec(&out)?,
        cur_fps,
        cache_prefix_sha8,
        stats,
    })
}

/// curating_proxy._is_client_metadata_call: Claude Code housekeeping (title
/// generation / topic detection) is forwarded untouched and unbilled — it
/// must never advance curation state or pollute the ledger.
fn is_client_metadata_call(body: &Value) -> bool {
    let sys = splice::system_to_text(body.get("system")).to_lowercase();
    (sys.contains("generate a concise") && sys.contains("title"))
        || sys.contains("main topic or goal of this coding session")
}

// ── §8.4 count_tokens counterfactual ────────────────────────────────────────

/// Free probe: the ORIGINAL inbound body against upstream count_tokens. Any
/// failure returns None — the ledger records null, never an estimate (§8.4).
async fn count_tokens_probe(st: &AppState, headers: &HeaderMap, raw: Bytes) -> Option<i64> {
    let resp = match st
        .client
        .post(st.url("/v1/messages/count_tokens"))
        .headers(forward_auth_headers(headers))
        .body(raw)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            probe_failure_diagnosis(&format!("transport: {e}"));
            return None;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        probe_failure_diagnosis(&format!(
            "{status}: {}",
            body.chars().take(200).collect::<String>()
        ));
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    v.get("input_tokens").and_then(Value::as_i64)
}

/// A failing probe silently nulls every savings number (§8.4 forbids
/// estimating), so the FIRST failure per process explains itself loudly —
/// e.g. an OAuth token whose scope rejects count_tokens; later failures
/// stay at debug to keep the log readable.
fn probe_failure_diagnosis(why: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut first = false;
    ONCE.call_once(|| first = true);
    if first {
        tracing::warn!(
            "count_tokens counterfactual probe FAILED ({why}) — savings will read as \
             unmeasured (null) until this resolves; trimming itself is unaffected"
        );
    } else {
        tracing::debug!("count_tokens probe failed: {why}");
    }
}

// ── savings ledger (contracts/schemas/savings-ledger.schema.json v0) ────────

fn request_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut h = Sha256::new();
    h.update(t.as_nanos().to_le_bytes());
    h.update(n.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    format!("req_{}", &format!("{:x}", h.finalize())[..32])
}

/// RFC 3339 UTC timestamp without a chrono dependency (Howard Hinnant's
/// civil-from-days). The schema's `ts` is `format: date-time`.
fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (days, sod) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// One savings-ledger row per completed request, appended to
/// ~/.dasein/ledger.jsonl. Field names follow
/// packages/contracts/schemas/savings-ledger.schema.json (flat `billed_*`,
/// `cachePrefixSha8`, RFC3339 `ts`). `counterfactual_input_tokens` is null
/// when the probe failed — measurement honesty (§8.4) forbids estimating.
/// Row identity, owned so the streaming finalizer can carry it to stream end.
#[derive(Clone)]
struct RowCtx {
    conv_id: String,
    model: Option<String>,
    cache_prefix_sha8: String,
    fail_open: bool,
}

fn write_ledger(
    st: &AppState,
    ctx: &RowCtx,
    counterfactual: Option<i64>,
    usage: Option<&Value>,
    stats: &PlanStats,
) {
    let conv_id = ctx.conv_id.as_str();
    let model = ctx.model.as_deref();
    let cache_prefix_sha8 = ctx.cache_prefix_sha8.as_str();
    let fail_open = ctx.fail_open;
    let g = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    let mut row = json!({
        "contract_version": "savings-ledger/v0",
        "request_id": request_id(),
        "ts": rfc3339_now(),
        "conv_id": conv_id,
        "counterfactual_input_tokens": counterfactual,
        "billed_input_tokens": g("input_tokens"),
        "billed_output_tokens": g("output_tokens"),
        "billed_cache_read_tokens": g("cache_read_input_tokens"),
        "billed_cache_write_tokens": g("cache_creation_input_tokens"),
        "cachePrefixSha8": cache_prefix_sha8,
        "fail_open": fail_open,
    });

    // The live savings line — what `tail -f ~/.dasein/proxy.log` (or the
    // proxy terminal) shows per request. Token-denominated per §8.4; the
    // input-side billed sum is uncached + cache read + cache write.
    let billed_side =
        g("input_tokens") + g("cache_read_input_tokens") + g("cache_creation_input_tokens");
    match counterfactual {
        Some(cf) => tracing::info!(
            conv = %&conv_id[..conv_id.len().min(12)],
            model = model.unwrap_or("?"),
            counterfactual_in = cf,
            billed_in = g("input_tokens"),
            cache_read = g("cache_read_input_tokens"),
            cache_write = g("cache_creation_input_tokens"),
            out = g("output_tokens"),
            saved = cf - billed_side,
            fail_open,
            "request served — ~{} input tok avoided", cf - billed_side
        ),
        None => tracing::info!(
            conv = %&conv_id[..conv_id.len().min(12)],
            model = model.unwrap_or("?"),
            billed_in = g("input_tokens"),
            fail_open,
            "request served — probe null, savings unmeasured (§8.4: never estimated)"
        ),
    }

    // Capture seams (savings-ledger optional fields): present only when the
    // real-scorer path ran — old rows stay schema-identical.
    if let Some(o) = row.as_object_mut() {
        if let Some(m) = model {
            o.insert("model".into(), json!(m));
        }
        if let Some(ck) = &stats.checkpoint_id {
            o.insert("checkpoint_id".into(), json!(ck));
        }
        if stats.brain_ms > 0.0 {
            o.insert(
                "brain_ms".into(),
                json!((stats.brain_ms * 10.0).round() / 10.0),
            );
        }
        if stats.scorer_fail_opens > 0 {
            o.insert("scorer_fail_opens".into(), json!(stats.scorer_fail_opens));
        }
        if stats.freeze_cut_tokens > 0 {
            o.insert("freeze_cut_tokens".into(), json!(stats.freeze_cut_tokens));
        }
        if let Some(t) = stats.tools_total {
            o.insert("tools_total".into(), json!(t));
        }
        if let Some(k) = stats.tools_kept {
            o.insert("tools_kept".into(), json!(k));
        }
        if let Some(s8) = &stats.tools_pre_prune_sha8 {
            o.insert("tools_pre_prune_sha8".into(), json!(s8));
        }
    }
    if let Some(dir) = st.ledger_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&st.ledger_path)
    {
        Ok(mut f) => {
            use std::io::Write;
            let _ = writeln!(f, "{row}");
        }
        Err(e) => tracing::warn!("ledger append failed: {e}"),
    }
}

// ── SSE usage extraction (anthropic_sse.usage_from_sse) ─────────────────────

/// Incremental port of `usage_from_sse`: `message_start.message.usage`
/// carries the input side (incl. the cache split); the LAST
/// `message_delta.usage` carries the authoritative `output_tokens`. Merge,
/// last write wins. Fed chunk-by-chunk so the relay stays unbuffered.
#[derive(Default)]
struct SseUsageScan {
    buf: String,
    merged: Map<String, Value>,
    saw: bool,
}

impl SseUsageScan {
    fn feed(&mut self, chunk: &[u8]) {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            self.line(line.trim());
        }
    }

    fn line(&mut self, line: &str) {
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(evt) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        match evt.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(u) = evt.pointer("/message/usage").and_then(Value::as_object) {
                    for (k, v) in u {
                        self.merged.insert(k.clone(), v.clone());
                    }
                    self.saw = true;
                }
            }
            Some("message_delta") => {
                if let Some(u) = evt.get("usage").and_then(Value::as_object) {
                    for k in [
                        "output_tokens",
                        "cache_creation_input_tokens",
                        "cache_read_input_tokens",
                        "input_tokens",
                    ] {
                        if let Some(v) = u.get(k) {
                            if !v.is_null() {
                                self.merged.insert(k.to_string(), v.clone());
                            }
                        }
                    }
                    self.saw = true;
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self) -> Option<Value> {
        let rest = std::mem::take(&mut self.buf);
        self.line(rest.trim());
        self.saw.then(|| Value::Object(self.merged.clone()))
    }
}

// ── handlers ────────────────────────────────────────────────────────────────

/// POST /v1/messages/count_tokens — pure verbatim passthrough, and
/// DELIBERATELY no session/memo touch. The reference routed count_tokens
/// through the same curating session as real turns (curating_proxy.py), so a
/// client-side token count advanced curator/fold state and corrupted the
/// next real turn's freeze — that bug must not be reproduced here.
async fn count_tokens_passthrough(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let _guard = InFlight::enter(&st);
    relay_buffered(&st, "/v1/messages/count_tokens", &headers, raw).await
}

/// Forward `raw` verbatim and relay the upstream response (status +
/// content-type + body) as-is.
async fn relay_buffered(st: &AppState, path: &str, headers: &HeaderMap, raw: Bytes) -> Response {
    match st
        .client
        .post(st.url(path))
        .headers(forward_auth_headers(headers))
        .body(raw)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let ct = resp.headers().get(header::CONTENT_TYPE).cloned();
            let bytes = resp.bytes().await.unwrap_or_default();
            respond(status, ct, Body::from(bytes))
        }
        Err(e) => bad_gateway(&e),
    }
}

fn respond(status: StatusCode, ct: Option<HeaderValue>, body: Body) -> Response {
    let mut b = Response::builder().status(status);
    if let Some(ct) = ct {
        b = b.header(header::CONTENT_TYPE, ct);
    }
    b.body(body)
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn bad_gateway(e: &reqwest::Error) -> Response {
    // Upstream unreachable is not "our own logic" failing — surface it as a
    // gateway error rather than fabricating an Anthropic-shaped response.
    tracing::warn!("upstream unreachable: {e}");
    (
        StatusCode::BAD_GATEWAY,
        format!("dasein proxy: upstream unreachable: {e}"),
    )
        .into_response()
}

/// POST /v1/messages — the curating pipeline (spec §4 flow: curate → probe →
/// forward → relay → ledger), with the §8.3 fail-open floor around every
/// internal step.
async fn messages(State(st): State<Arc<AppState>>, headers: HeaderMap, raw: Bytes) -> Response {
    let _guard = InFlight::enter(&st);
    let body: Option<Value> = serde_json::from_slice(&raw).ok();

    // Claude Code housekeeping (title/topic generation): forwarded untouched
    // and unbilled — no curation state advance, no probe, no ledger row.
    if let Some(b) = body.as_ref() {
        if is_client_metadata_call(b) {
            return relay_buffered(&st, "/v1/messages", &headers, raw).await;
        }
        record_inbound(&headers, b, &raw);
    }

    // (b-d) conversation id, curation (freeze when a brain is configured),
    // breakpoint placement, tool keep-set — any error here means forwarding
    // the ORIGINAL body verbatim (fail-open, counted).
    let plan = match body.as_ref() {
        Some(b) => match curate(&st, &headers, b).await {
            Ok(p) => Some(p),
            Err(e) => {
                note_fail_open(&st, &format!("curation failed: {e}"));
                None
            }
        },
        None => {
            note_fail_open(&st, "inbound body is not JSON");
            None
        }
    };
    let fail_open = plan.is_none();
    let stats = plan.as_ref().map(|p| p.stats.clone()).unwrap_or_default();

    // (e) §8.4 counterfactual — always on the ORIGINAL inbound bytes.
    let counterfactual = count_tokens_probe(&st, &headers, raw.clone()).await;

    // (f) forward: curated body, or the original verbatim on fail-open.
    let send = plan
        .as_ref()
        .map(|p| Bytes::from(p.out_bytes.clone()))
        .unwrap_or_else(|| raw.clone());
    let resp = match st
        .client
        .post(st.url("/v1/messages"))
        .headers(forward_auth_headers(&headers))
        .body(send)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return bad_gateway(&e),
    };
    let status = resp.status();

    // (d) CRITICAL ORDERING: commit this call's fingerprints only now that
    // the upstream 2xx proves Anthropic cached these bytes. The reference
    // wrote them before the send (anthropic_shapes.py:484) — a failed call
    // anchored the retry on bytes Anthropic never cached.
    if status.is_success() {
        if let Some(p) = &plan {
            lock(&st.convs)
                .entry(p.conv_id.clone())
                .or_default()
                .last_fps = p.cur_fps.clone();
        }
    }

    let row_ctx = RowCtx {
        conv_id: plan.as_ref().map(|p| p.conv_id.clone()).unwrap_or_default(),
        model: body
            .as_ref()
            .and_then(|b| b.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string),
        cache_prefix_sha8: plan
            .as_ref()
            .map(|p| p.cache_prefix_sha8.clone())
            .unwrap_or_else(|| sha8_of_fps(std::iter::empty())),
        fail_open,
    };

    let wants_stream = body
        .as_ref()
        .and_then(|b| b.get("stream"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ct = resp.headers().get(header::CONTENT_TYPE).cloned();

    if wants_stream && status.is_success() {
        // Relay the SSE bytes UNMODIFIED while scanning for usage; the
        // ledger row is written when the upstream stream ends.
        let scan = Arc::new(Mutex::new(SseUsageScan::default()));
        let scan_tee = scan.clone();
        let tee = resp.bytes_stream().map(move |r| {
            if let Ok(b) = &r {
                lock(&scan_tee).feed(b);
            }
            r.map_err(io::Error::other)
        });
        let st2 = st.clone();
        // The stream outlives this handler; its own guard keeps the idle
        // sweep from shooting the proxy mid-SSE.
        let stream_guard = InFlight::enter(&st);
        let finalize = futures_util::stream::once(async move {
            let _guard = stream_guard;
            let usage = lock(&scan).finish();
            write_ledger(&st2, &row_ctx, counterfactual, usage.as_ref(), &stats);
            Ok::<Bytes, io::Error>(Bytes::new())
        });
        return respond(status, ct, Body::from_stream(tee.chain(finalize)));
    }

    // Non-streaming (or upstream error): buffer, relay as-is, read .usage.
    let bytes = resp.bytes().await.unwrap_or_default();
    let usage = if status.is_success() {
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v.get("usage").cloned())
    } else {
        None
    };
    write_ledger(&st, &row_ctx, counterfactual, usage.as_ref(), &stats);
    respond(status, ct, Body::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evict_stale_applies_ttl_then_cap_oldest_first() {
        let now = std::time::Instant::now();
        let old = now - std::time::Duration::from_secs(4000);
        let mid = now - std::time::Duration::from_secs(120);
        let mut convs: HashMap<String, ConvState> = HashMap::new();
        for (k, t) in [("expired", old), ("warm", mid), ("hot", now)] {
            let cs = ConvState {
                touched: t,
                ..Default::default()
            };
            convs.insert(k.to_string(), cs);
        }
        // TTL evicts "expired"; cap 1 then drops the older survivor ("warm").
        let evicted = evict_stale(&mut convs, now, std::time::Duration::from_secs(3600), 1);
        assert_eq!(evicted, 2);
        assert!(convs.contains_key("hot"));
        assert_eq!(convs.len(), 1);
    }
}
