//! `parsec proxy` — the local Anthropic-wire passthrough proxy (DIRECTION.md
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

use parsec_engine::freeze::{FreezeConfig, Freezer};
use parsec_engine::pystr::py_json_dumps;

use crate::brain::{self, BrainConfig, BrainScorer};
use crate::governor::{self, GovMode};
use crate::internal::to_internal;
use crate::splice::{self, FoldMap};

/// Per-conversation memo. Strictly a cache (see module doc): `folds` are the
/// exact bytes already served per turn (splice::FoldMap), `last_fps` the
/// message fingerprints of the last SUCCESSFUL upstream call — the proof of
/// what Anthropic's cache actually holds. `freezer` is the deterministic
/// fold memo (replayable from scratch — losing it costs brain round trips,
/// never bytes); `tool_keep` is the once-per-conversation frozen keep-set
/// (re-pruning per turn would vary the prefix and bust the provider cache).
/// It grows monotonically via the reactive unfreeze — a cache too, since the
/// unfreezes replay from the tool_use blocks still in the prefix.
pub struct ConvState {
    pub folds: FoldMap,
    pub last_fps: Vec<String>,
    pub freezer: Option<Freezer<BrainScorer>>,
    pub tool_keep: Option<HashSet<String>>,
    /// Governor latches/accumulators (contract Track B item 3) — a CACHE:
    /// losing it only ever makes the governor LESS likely to fire.
    pub gov: governor::GovMemo,
    /// Eviction clock (reference sessions.py TTL semantics). Memos are pure
    /// caches, so evicting a live conversation only costs replay round trips
    /// and one provider-cache re-seed — never bytes.
    pub touched: std::time::Instant,
    /// Cache-loss guardrail latch (incident 2026-08-30 P1): once a warm lane
    /// would have rewritten more previously-covered tokens than
    /// `cache_guard_tokens`, every later request on the lane fails open to
    /// verbatim passthrough — the client's own bytes and cache anchors go
    /// upstream untouched, exactly as if parsec were not installed. Cleared
    /// with the rest of the memo by the fresh-run reset.
    pub bypass: bool,
}

impl Default for ConvState {
    fn default() -> Self {
        ConvState {
            folds: FoldMap::default(),
            last_fps: Vec::new(),
            freezer: None,
            tool_keep: None,
            gov: governor::GovMemo::default(),
            touched: std::time::Instant::now(),
            bypass: false,
        }
    }
}

/// Evict conversation memos idle past `ttl`, then oldest-first down to `cap`
/// (reference: PARSEC_SESSION_TTL_S=3600, PARSEC_SESSION_MAX=512). A
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
    /// OpenAI platform upstream for `/openai/*` (Codex BYOK);
    /// env-overridable, see [`crate::openai::upstream_from_env`].
    pub openai_upstream: String,
    /// ChatGPT backend upstream for `/chatgpt/*` (Codex subscription mode);
    /// env-overridable, see [`crate::openai::chatgpt_upstream_from_env`].
    pub chatgpt_upstream: String,
    pub client: reqwest::Client,
    pub convs: Mutex<HashMap<String, ConvState>>,
    /// Per-lane serialization (incident 2026-08-30 P0): one async mutex per
    /// conversation lane, held from curation through the post-2xx fingerprint
    /// commit. Concurrent requests on DIFFERENT lanes run fully parallel;
    /// two on the SAME lane run in turn instead of taking/rebuilding/
    /// overwriting each other's Freezer, fold map, and fingerprints — the
    /// clone-out/write-back pattern under `convs` made a same-lane race a
    /// silent full cache reseed.
    pub lane_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Cache-loss guardrail threshold in chars/4 tokens (incident 2026-08-30
    /// P1): a warm lane about to rewrite more previously-covered tokens than
    /// this fails open to passthrough instead. `PARSEC_CACHE_GUARD_TOKENS`;
    /// 0 disables.
    pub cache_guard_tokens: i64,
    /// Guardrail fires — counted separately from generic fail-opens so the
    /// incident signature (repeated large warm-lane rewrites) is alertable
    /// on its own.
    pub cache_guard_count: AtomicU64,
    /// §8.3: fail-open is a first-class metric, not a silent branch.
    pub fail_open_count: AtomicU64,
    pub ledger_path: PathBuf,
    /// Real-scorer path (docs/brain-serving-v0.md). None = v0 passthrough
    /// curation exactly as before.
    pub brain: Option<BrainConfig>,
    /// Governor dials (PARSEC_GOVERNOR et al.) — Off by default: zero
    /// behavior change, zero extra brain calls.
    pub governor: governor::GovernorConfig,
    /// Governor-seam fail-opens (rules/neighbors/signal errors): the request
    /// is NEVER failed or altered beyond passthrough — but every skip is
    /// counted, per the fail-open-but-measured rule.
    pub gov_fail_open_count: AtomicU64,
    /// Idle/self-shutdown bookkeeping: epoch-seconds of the last inbound
    /// request and the number currently in flight.
    pub last_request_epoch_s: AtomicU64,
    pub in_flight: AtomicU64,
    /// Construction instant, for the uptime figure in the shutdown summary.
    pub started: std::time::Instant,
    /// Entitlement (apikey gate), resolved ONCE at startup like `brain`: false
    /// ⇒ the serve path is a pure passthrough (no curation, probe, or ledger).
    /// Defaults true in the constructors so tests are hermetic; real `run` sets
    /// it from `apikey::enabled()`.
    pub entitled: bool,
}

fn epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `PARSEC_CACHE_GUARD_TOKENS` — cache-loss guardrail threshold; unset/junk
/// → 50_000 (the incident's warm-lane rewrite floor), 0/off → disabled.
fn cache_guard_tokens_from_env() -> i64 {
    match std::env::var("PARSEC_CACHE_GUARD_TOKENS") {
        Ok(v) if v.trim().eq_ignore_ascii_case("off") => 0,
        Ok(v) => v.trim().parse().unwrap_or(50_000),
        Err(_) => 50_000,
    }
}

/// Get-or-create the async mutex serializing one conversation lane. The map
/// lock is only held for the lookup; the caller awaits the lane lock outside
/// it, so different lanes never contend.
fn lane_lock(st: &AppState, conv_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    lock(&st.lane_locks)
        .entry(conv_id.to_string())
        .or_default()
        .clone()
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
        Self::with_brain_governor(
            upstream_base,
            ledger_path,
            brain,
            governor::GovernorConfig::from_env(),
        )
    }

    /// Explicit-governor constructor (tests inject a config; env untouched).
    pub fn with_brain_governor(
        upstream_base: String,
        ledger_path: PathBuf,
        brain: Option<BrainConfig>,
        gov: governor::GovernorConfig,
    ) -> Self {
        Self {
            upstream_base,
            openai_upstream: crate::openai::upstream_from_env(),
            chatgpt_upstream: crate::openai::chatgpt_upstream_from_env(),
            client: reqwest::Client::new(),
            convs: Mutex::new(HashMap::new()),
            lane_locks: Mutex::new(HashMap::new()),
            cache_guard_tokens: cache_guard_tokens_from_env(),
            cache_guard_count: AtomicU64::new(0),
            fail_open_count: AtomicU64::new(0),
            ledger_path,
            brain,
            governor: gov,
            gov_fail_open_count: AtomicU64::new(0),
            last_request_epoch_s: AtomicU64::new(epoch_s()),
            in_flight: AtomicU64::new(0),
            started: std::time::Instant::now(),
            entitled: true,
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
pub(crate) struct InFlight(Arc<AppState>);
impl InFlight {
    pub(crate) fn enter(st: &Arc<AppState>) -> Self {
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

/// One last line before the process ends, on EVERY exit path (idle timer,
/// /shutdown route, Ctrl-C/SIGTERM): the fail-open tallies must be visible
/// at least once per lifetime even if nobody scraped them mid-flight (§8.3).
fn log_shutdown(state: &AppState, reason: &str) {
    tracing::info!(
        reason,
        uptime_s = state.started.elapsed().as_secs(),
        in_flight = state.in_flight.load(Ordering::SeqCst),
        fail_open = state.fail_open_count.load(Ordering::SeqCst),
        gov_fail_open = state.gov_fail_open_count.load(Ordering::SeqCst),
        "parsec proxy shutting down"
    );
}

/// Resolves on Ctrl-C or (unix) SIGTERM, logging which one arrived; axum
/// then stops accepting and drains in-flight connections before `serve`
/// returns — a plain signal death would drop mid-stream SSE relays.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "SIGTERM handler unavailable — Ctrl-C only");
                    let _ = tokio::signal::ctrl_c().await;
                    tracing::info!("SIGINT received — draining in-flight requests");
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGINT received — draining in-flight requests");
            }
            _ = term.recv() => {
                tracing::info!("SIGTERM received — draining in-flight requests");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Ctrl-C received — draining in-flight requests");
    }
}

/// `parsec proxy` entrypoint: serve on 127.0.0.1:$PARSEC_PROXY_PORT
/// (default 8082), upstream $PARSEC_UPSTREAM (default api.anthropic.com).
pub fn run() -> anyhow::Result<()> {
    let port: u16 = std::env::var("PARSEC_PROXY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8082);
    let upstream = std::env::var("PARSEC_UPSTREAM")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let ledger = crate::setup::home_dir()
        .join(".parsec")
        .join("ledger.jsonl");
    let brain = BrainConfig::from_env();
    if let Some(b) = &brain {
        match b.contract {
            crate::brain::BrainContract::Dev => tracing::info!(
                url = %b.url, target_cov = %b.target_cov, tool_prune = b.tool_prune,
                "brain scorer active — DEV RAW-TEXT contract (brain-api-dev/v0), \
                 our-machines-only posture"
            ),
            crate::brain::BrainContract::V2 => tracing::info!(
                url = %b.url, target_cov = %b.target_cov, tool_prune = b.tool_prune,
                "brain scorer active — brain-api/v2 (structural featurization \
                 local, chunk text sent to the brain, which embeds)"
            ),
        }
    }
    if crate::ledger_ship::resolve().is_some() {
        tracing::info!(
            "per-account savings shipping active (API key configured) — rows post to \
             the platform ledger in addition to the local ~/.parsec ledger"
        );
    }
    let mut state = AppState::with_brain(upstream, ledger, brain);
    // Entitlement resolved once, like the brain config: no key ⇒ pure
    // passthrough serve path (apikey gate). The hook resolves live per
    // invocation; a mid-session `parsec key set` activates the proxy on its
    // next (re)start, same as the brain config.
    state.entitled = crate::apikey::enabled();
    if !state.entitled {
        tracing::warn!(
            "no API key — parsec is INERT: serving pure passthrough, saving nothing. \
             Get a key at {} and run `parsec key set <psc_…>`.",
            crate::apikey::SIGNUP_URL
        );
    }
    let state = Arc::new(state);
    if state.governor.mode != GovMode::Off {
        tracing::info!(
            mode = state.governor.mode.as_str(),
            rule_tau = state.governor.rule_tau,
            doom_thresh = state.governor.doom_thresh,
            doom_k = state.governor.doom_k,
            runaway_ratio = state.governor.runaway_ratio,
            kill_floor_tok = state.governor.kill_floor_tok,
            horizon_step = state.governor.horizon_step,
            "governor active (PARSEC_GOVERNOR) — directives {}",
            if state.governor.mode == GovMode::On {
                "INJECTED"
            } else {
                "recorded only (advise)"
            }
        );
    }

    // Memo bounds (reference sessions.py: TTL 3600s, max 512). There is NO
    // idle self-shutdown any more: the worker is owned by the supervisor,
    // which decides its lifetime — a worker that killed itself on a timer
    // would just be respawned, and (removed 2026-07-21) the old 1800s exit
    // was a live source of mid-session wedges when a still-active session
    // went briefly idle. The worker runs until the supervisor stops it.
    let ttl_s: u64 = std::env::var("PARSEC_SESSION_TTL_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let cap: usize = std::env::var("PARSEC_SESSION_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);

    // Orphan guard: if the supervisor that spawned us dies (crash, kill -9,
    // reboot-race), no OS mechanism reparents-then-kills us portably, so we
    // watch its heartbeat file and exit when it goes stale. Only armed when
    // spawned under a supervisor (PARSEC_SUPERVISOR_HEARTBEAT set); a
    // hand-run worker has no parent to outlive.
    crate::supervisor::arm_orphan_guard();

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
                // Lane locks follow the memos: keep a lock while its lane
                // memo lives OR a request holds it (strong_count > 1 —
                // dropping a HELD lock would let a racing request mint a
                // second mutex for the same lane and break serialization).
                {
                    let convs = lock(&maint.convs);
                    lock(&maint.lane_locks)
                        .retain(|k, l| convs.contains_key(k) || Arc::strong_count(l) > 1);
                }
            }
        });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        tracing::info!("parsec proxy worker listening on 127.0.0.1:{port}");
        // Fleet registration (docs/install-tracking.md): mint/refresh the
        // install id and ping the platform — fire-and-forget, daily-deduped,
        // never on the serving path.
        crate::install::spawn_reporter(&state.client);
        let on_exit = state.clone();
        axum::serve(listener, router(state))
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        log_shutdown(&on_exit, "signal");
        Ok(())
    })
}

/// Route table: the two Anthropic POST endpoints we speak, plus the
/// `/openai/*` Responses passthrough (Codex); everything else 404s (we are a
/// model-traffic proxy, not a general gateway).
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens_passthrough))
        // Model listing — verbatim GET passthrough, no curation, no memo.
        // Nothing on the serving path needs it; it exists because Claude
        // Desktop calls it and the interceptor redirects whole hosts, not
        // individual routes (docs/claude-desktop-integration.md §2). A 404
        // here would surface in Desktop as a broken model picker.
        .route("/v1/models", axum::routing::get(models_passthrough))
        .route("/v1/models/{id}", axum::routing::get(models_passthrough))
        // OpenAI Responses wire, namespaced so the wires can never be
        // confused: `/openai/v1` is the BYOK provider base_url, `/chatgpt`
        // the subscription-mode `openai_base_url` (upstream chatgpt.com's
        // codex backend). Same handler, same curation.
        .route("/openai/{*rest}", axum::routing::any(crate::openai::relay))
        .route("/chatgpt/{*rest}", axum::routing::any(crate::openai::relay))
        // Liveness only: answered locally, never contacts upstream, so a
        // 200 here means "proxy is up", not "upstream is reachable".
        .route(
            "/health",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "ok": true,
                    "service": "parsec-proxy",
                    "version": env!("CARGO_PKG_VERSION"),
                    "wires": crate::setup::SERVED_WIRES,
                }))
            }),
        )
        // Localhost kill-switch for `parsec uninstall`: the bind is
        // 127.0.0.1-only, so only same-machine processes can reach it. Reply
        // first, exit off the response path so the 200 flushes.
        .route(
            "/shutdown",
            post(|State(st): State<Arc<AppState>>| async move {
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    tracing::info!("shutdown requested via /shutdown — exiting");
                    log_shutdown(&st, "/shutdown");
                    std::process::exit(0);
                });
                axum::Json(serde_json::json!({
                    "ok": true,
                    "service": "parsec-proxy",
                    "shutting_down": true,
                }))
            }),
        )
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
pub(crate) fn forward_auth_headers(inbound: &HeaderMap) -> HeaderMap {
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

/// Conversation lane identity. STABLE-FIRST since the 2026-08-30 Desktop
/// cache incident: the v0.2.6 scheme hashed the first two internal message
/// contents (system entry + first user) with the session id as a mere salt,
/// so a client that mutates its system/task head between turns — Claude
/// Desktop/Cowork does — got a fresh lane (and a fresh Freezer, fold map,
/// fingerprint set, and tool keep-set) on nearly every request, and the
/// breakpoint logic then re-seeded the entire history as cache WRITES
/// (≥10.76M excess write tokens in the audited three-hour window).
///
/// With a session id present, the lane key is now derived only from stable,
/// id-shaped material plus the FIRST USER message:
///   (session_id, model, source tool tag, first-user-content hash)
/// - session/model/source pin the lane to one client conversation on one
///   model; the system entry is deliberately EXCLUDED — it is the mutable
///   task head that broke continuity.
/// - the first-user hash is what separates parallel agents (parent and
///   fanned-out subagents) sharing one session and model: their opening task
///   prompts differ while each agent's own stays constant for its lifetime.
///   Two subagents launched with byte-identical prompts on one model still
///   collide — exactly as they did under the old scheme (identical head) —
///   and per-lane serialization now bounds that cost to latency.
///
/// Without a session id (non-Claude clients), the reference head-hash
/// behavior is unchanged. The bench run-id header still prefixes both forms
/// so concurrent tasks in one batch get separate memos.
fn conversation_id(
    headers: &HeaderMap,
    internal: &[Value],
    session: Option<&str>,
    model: Option<&str>,
) -> String {
    let mut h = Sha256::new();
    if let Some(sid) = session {
        let first_user = internal
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .and_then(|m| m.get("content").cloned())
            .unwrap_or(Value::Null);
        h.update(b"parsec-lane-v2");
        h.update(b"\x00session\x00");
        h.update(sid.as_bytes());
        h.update(b"\x00model\x00");
        h.update(model.unwrap_or("").as_bytes());
        h.update(b"\x00source\x00");
        h.update(
            tool_from_headers(headers)
                .as_deref()
                .unwrap_or("-")
                .as_bytes(),
        );
        h.update(b"\x00user-head\x00");
        h.update(py_json_dumps(&first_user).as_bytes());
    } else {
        // Reference fallback: the first two messages are NOT unique across
        // sessions (shared system prompt + canned opening turn), but with no
        // session id there is nothing more stable to key on.
        let head: Vec<Value> = internal
            .iter()
            .take(2)
            .map(|m| m.get("content").cloned().unwrap_or(Value::Null))
            .collect();
        h.update(py_json_dumps(&Value::Array(head)).as_bytes());
    }
    let hash = format!("{:x}", h.finalize());
    let hash = &hash[..24];
    match headers.get("x-ccb-run-id").and_then(|v| v.to_str().ok()) {
        Some(rid) if !rid.trim().is_empty() => format!("{}:{}", rid.trim(), hash),
        _ => hash.to_string(),
    }
}

/// Privacy-safe head-component hashes for the lane-identity diagnostic line
/// (incident Appendix B: log system and first-user hashes SEPARATELY, so a
/// head mutation is attributable to a component after the fact).
fn head_component_sha8(internal: &[Value], role: &str) -> String {
    let content = internal
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some(role))
        .and_then(|m| m.get("content").cloned())
        .unwrap_or(Value::Null);
    let mut h = Sha256::new();
    h.update(py_json_dumps(&content).as_bytes());
    format!("{:x}", h.finalize())[..8].to_string()
}

/// Client harness session identity from `metadata.user_id` — an id, never
/// content. Claude Code 2.1.x sends user_id as a JSON-encoded object
/// (`{"device_id":...,"session_id":"<uuid>",...}`, verified live against
/// 2.1.214); older builds used `user_<hash>_account_<uuid>_session_<uuid>`.
/// Both parse; anything else — or a value outside the hex/dash id charset —
/// yields None and the ledger row omits the field. The charset gate is what
/// keeps the ledger contract unable to carry raw text through this path.
fn session_id_from_metadata(body: &Value) -> Option<String> {
    let uid = body.pointer("/metadata/user_id").and_then(Value::as_str)?;
    let sid = match serde_json::from_str::<Value>(uid) {
        Ok(v) => v
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Err(_) => uid.rsplit_once("_session_").map(|(_, s)| s.to_owned()),
    }?;
    let id_shaped = !sid.is_empty()
        && sid.len() <= 64
        && sid
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' | b'-'));
    id_shaped.then_some(sid)
}

/// Calling-tool attribution from the `x-parsec-tool` header — set by the
/// shim a non-Claude-Code client installs (e.g. the opencode plugin), so the
/// ledger can report savings per tool. Charset-gated like session_id: the
/// contract must stay unable to carry raw text through this path. Absent or
/// malformed → None, and the row omits the field (Claude Code default).
pub(crate) fn tool_from_headers(headers: &HeaderMap) -> Option<String> {
    let t = headers.get("x-parsec-tool")?.to_str().ok()?.trim();
    let id_shaped = !t.is_empty()
        && t.len() <= 32
        && t.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));
    id_shaped.then(|| t.to_string())
}

pub(crate) fn sha8_of_fps<'a>(fps: impl Iterator<Item = &'a str>) -> String {
    let mut h = Sha256::new();
    for f in fps {
        h.update(f.as_bytes());
    }
    format!("{:x}", h.finalize())[..8].to_string()
}

pub(crate) fn note_fail_open(st: &AppState, why: &str) {
    let n = st.fail_open_count.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::warn!(
        fail_open_total = n,
        "proxy fail-open (§8.3): {why} — forwarding original body verbatim"
    );
}

/// §8.1 capture seam: with `PARSEC_RECORD_DIR` set, every real /v1/messages
/// body is dumped VERBATIM (pre-curation) to `<dir>/<conv_id>/turn_<n>.json`
/// — a maintainer runs real Claude Code through the proxy once, then feeds
/// the conversation directory to scripts/record_to_fixture.py. count_tokens
/// and client-metadata calls never record (they never curate either). The
/// per-conv counter is the count of files already on disk, so a proxy
/// restart mid-recording keeps appending instead of overwriting. Fail-open:
/// a dump error is logged and the request proceeds untouched.
fn record_inbound(headers: &HeaderMap, body: &Value, raw: &[u8]) {
    let Some(dir) = std::env::var("PARSEC_RECORD_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
    else {
        return;
    };
    let conv_id = conversation_id(
        headers,
        &to_internal(body),
        session_id_from_metadata(body).as_deref(),
        body.get("model").and_then(Value::as_str),
    );
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
        tracing::warn!("PARSEC_RECORD_DIR dump failed (recording is fail-open): {e}");
    }
}

// ── curation plan (spec steps b-d) ──────────────────────────────────────────

/// Capture-seam telemetry for the ledger row (docs/brain-serving-v0.md);
/// zero-valued fields are omitted from the row.
#[derive(Default, Clone)]
pub(crate) struct PlanStats {
    pub(crate) checkpoint_id: Option<String>,
    pub(crate) brain_ms: f64,
    pub(crate) scorer_fail_opens: u64,
    /// Freezer purity-guard resets THIS request (a consumed message's bytes
    /// changed → full birth replay) — the replay-storm signature
    /// (docs/perf-research-2026-09-02.md §1).
    pub(crate) purity_resets: u64,
    /// Internal-view chars/4 the freezer trimmed THIS call (uncut − rendered)
    /// — a diagnostic, never a savings claim (§8.4).
    pub(crate) freeze_cut_tokens: i64,
    /// chars/4 the curator wanted to cut from HUMAN-authored content and was
    /// refused (protect::restore_protected). 0 on a correctly-typed wire;
    /// non-zero is the over-cut-onto-instructions alarm.
    pub(crate) freeze_cut_protected_tokens: i64,
    /// Per-internal-role chars/4 breakdown of what WAS cut — the role-aware
    /// accounting that made the user-message defect invisible by its absence.
    pub(crate) freeze_cut_roles: std::collections::BTreeMap<String, i64>,
    /// What the local BPE measured for this request (counterfact.rs). Some
    /// only on wires with no count_tokens endpoint; write_ledger turns it
    /// into a counterfactual anchored to billed usage and stamps the row
    /// `counterfactual_source: "local_bpe"`.
    pub(crate) counterfactual_local: Option<crate::counterfact::LocalCount>,
    /// Insist-valve fires THIS call: the agent re-asked for content the
    /// curator had cut, and the valve served it full. The per-request
    /// over-cut (regret) signal — 0 on a well-calibrated cut.
    pub(crate) curator_insists: u64,
    pub(crate) tools_total: Option<usize>,
    pub(crate) tools_kept: Option<usize>,
    /// Pruned tools re-added THIS request because the prefix called them
    /// (reactive unfreeze) — Some only on the request that unfroze them.
    pub(crate) tools_unfrozen: Option<usize>,
    /// Pruned tools served as name+note stubs THIS request
    /// (PARSEC_TOOL_STUB) — Some only when > 0.
    pub(crate) tools_stubbed: Option<usize>,
    pub(crate) tools_pre_prune_sha8: Option<String>,
    /// sha8 of the roster as SERVED (post keep-set/stubs) — with the
    /// pre-prune hash, attributes a provider-cache bust to roster churn.
    pub(crate) tools_served_sha8: Option<String>,
    // ── detailed-tracing seams (contract Track B item 6) ───────────────────
    /// Conversation turn = assistant messages in the internal view.
    pub(crate) turn: i64,
    /// Fold-map size after this call / folds newly recorded this call.
    pub(crate) folds_total: usize,
    pub(crate) folds_new: usize,
    /// Brain trace round trips this request (birth steps scored/replayed).
    pub(crate) births_scored: u64,
    /// `ChunkScorer::score` invocations this serve, cache hits included.
    /// `score_calls - births_scored` is what the same-live-set memo absorbed
    /// (chiefly the per-owner-pool tau loop, which is mask-only delta).
    pub(crate) score_calls: u64,
    /// Of those, how many the memo served without an HTTP round trip.
    pub(crate) cache_hits: u64,
    /// Message indices carrying a cache anchor in the served body.
    pub(crate) anchors: Vec<usize>,
    pub(crate) curate_ms: f64,
    /// Governor seams — Some only when PARSEC_GOVERNOR != off.
    pub(crate) gov: Option<GovStats>,
}

/// Ledger seams for the governor (contract Track B item 5) — recorded in
/// advise AND on; absent when off so existing rows stay byte-identical.
#[derive(Clone)]
pub(crate) struct GovStats {
    mode: &'static str,
    runaway_factor: f64,
    loop_frac: f64,
    doom_q: Option<i64>,
    n_src: usize,
    cum_tok: f64,
    rule_fires: usize,
    directive_injected: bool,
    /// Some(...) once fetched: the median, or null when hoods are inert.
    nbr_cost_median: Option<Option<f64>>,
}

/// One-shot governor consumables burned by THIS request's directives —
/// committed to the live memo only after upstream 2xx (the same
/// anthropic_shapes.py:484 bug class as fingerprints: a 429/529 means the
/// model never saw the directive, so the latch must survive for the retry).
/// The kill latch is deliberately NOT here — it re-appends every turn and
/// un-latching on failure would flap; it commits immediately.
#[derive(Clone, Default)]
struct GovCommit {
    /// Rule fires delivered this turn: (eid, fire_step) dedupe inserts.
    fired: Vec<(String, i64)>,
    coach_fired: bool,
    bank_fired: bool,
    horizon_fired: bool,
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
    /// One-shot governor latches awaiting the 2xx commit (None when off).
    gov_commit: Option<GovCommit>,
}

/// Internal-view token mass (reference _PROMPT diag: Σ len(_text(m))//4).
pub(crate) fn internal_mass(msgs: &[Value]) -> i64 {
    msgs.iter()
        .map(|m| {
            let t = match m.get("content") {
                Some(Value::String(s)) => s.clone(),
                other => splice::content_text(other),
            };
            parsec_engine::pystr::char_len(&t) as i64 / 4
        })
        .sum()
}

/// Curation: the FULL to_internal view (roles + bash-twin actions) through
/// the deterministic Freezer when a brain is configured, else the v0
/// passthrough shape (internal text == original text, so apply_curation's
/// equality branch forwards every turn verbatim while recording folds — the
/// wire freeze is live even before a curator cuts anything).
/// Tool names the assistant has already reached for anywhere in this
/// conversation prefix — the demand signal for the reactive keep-set
/// unfreeze. Scanning the full incoming prefix (not just the newest turn)
/// makes every past unfreeze re-derivable after a memo eviction: served
/// bytes stay a pure function of (prefix, checkpoint, config).
fn prefix_tool_use_names(messages: Option<&Value>) -> HashSet<String> {
    let mut out = HashSet::new();
    for m in messages.and_then(Value::as_array).into_iter().flatten() {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        for b in m
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let Some(n) = b.get("name").and_then(Value::as_str) {
                    out.insert(n.to_owned());
                }
            }
        }
    }
    out
}

/// What the curator decided during ONE serve() — deltas of the freezer's cut
/// registries across the call, computed on the blocking thread while the
/// freezer is still in hand. Diagnostics for the curator decision log only.
struct CutDelta {
    /// Insist-valve fires (re-asked cut content served full).
    insists: u64,
    /// Chunks newly cut (file-ranged or not, e.g. bash observations).
    chunks: u64,
    /// Newly cut (file, lo, hi) ranges, sorted for deterministic log order.
    ranges: Vec<(String, i64, i64)>,
}

/// `conv_id` is computed by the caller (the handler holds the per-lane
/// serialization lock keyed on it for the whole curate→forward→commit span).
async fn curate(
    st: &Arc<AppState>,
    _headers: &HeaderMap,
    body: &Value,
    conv_id: String,
) -> anyhow::Result<Plan> {
    let t_curate = std::time::Instant::now();
    if body.get("messages").and_then(Value::as_array).is_none() {
        anyhow::bail!("body has no messages array");
    }
    let internal = to_internal(body);
    // Human-authored entries the curator may score but must never rewrite
    // (protect.rs). Built from the inbound body, so it stays a pure
    // function of the prefix like everything else on this path.
    let protected = crate::internal::protected_mask(body);
    // Lane-identity diagnostics (incident Appendix B): separate head-
    // component hashes make "which head component mutated" answerable from
    // the log alone — the incident could not reconstruct it after the fact.
    tracing::debug!(
        conv = %conv_id,
        internal_msgs = internal.len(),
        internal_tokens = internal_mass(&internal),
        sys_head_sha8 = %head_component_sha8(&internal, "system"),
        user_head_sha8 = %head_component_sha8(&internal, "user"),
        "curate: internal view built"
    );
    let mut stats = PlanStats {
        turn: internal
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .count() as i64,
        ..PlanStats::default()
    };
    lock(&st.convs).entry(conv_id.clone()).or_default().touched = std::time::Instant::now();

    // sessions.py new-run reset: a request with no assistant turn is turn 1
    // of a fresh run — stale memos (folds, freezer, tool keep-set) from an
    // earlier run with the same task head must not leak into it.
    let has_assistant = internal
        .iter()
        .any(|m| m.get("role").and_then(Value::as_str) == Some("assistant"));
    if !has_assistant && lock(&st.convs).remove(&conv_id).is_some() {
        tracing::debug!(
            conv = %conv_id,
            "no assistant turn — fresh run, stale memo (folds/freezer/tool keep-set) reset"
        );
    }

    // Cache-loss guardrail latch (see ConvState::bypass): a lane that once
    // tripped the guardrail stays verbatim passthrough — cheaper AND safer
    // than alternating curated reseeds with bypasses. The bail routes
    // through the handler's fail-open arm, which forwards the ORIGINAL body.
    if lock(&st.convs).get(&conv_id).is_some_and(|c| c.bypass) {
        anyhow::bail!(
            "cache-loss guardrail latched for this lane — verbatim passthrough \
             until a fresh run resets it"
        );
    }

    // The Freezer path: serve() replays un-replayed birth steps (brain round
    // trips) and renders. Runs in spawn_blocking — BrainScorer is a blocking
    // HTTP client. The freezer is TAKEN from the memo and put back after; a
    // racing request on the same conversation just rebuilds the memo from
    // scratch (pure fold — identical bytes, extra latency only).
    let mut gov_doom_q: Option<i64> = None;
    let curated_internal: Vec<Value> = if let Some(bcfg) = &st.brain {
        let taken = lock(&st.convs)
            .entry(conv_id.clone())
            .or_default()
            .freezer
            .take();
        let internal_in = internal.clone();
        let bcfg2 = bcfg.clone();
        let conv2 = conv_id.clone();
        let conv_for_vis = conv_id.clone();
        // Visibility export target (defect 3): the Claude Code session this
        // request belongs to, so the no-reread hook can see what was elided.
        let vis_sid = session_id_from_metadata(body);
        let attach_gf = st.governor.mode != GovMode::Off;
        // Freezer (and its blocking HTTP scorer) is built AND driven on a
        // blocking thread — reqwest::blocking panics on async runtime threads.
        let (fz, served, fails_before, resets_before, calls_before, cut_delta) =
            tokio::task::spawn_blocking(move || {
                let mut fz = taken.unwrap_or_else(|| {
                    Freezer::new(FreezeConfig::default(), BrainScorer::new(bcfg2, conv2))
                });
                // Governor doom input rides the same trace calls; the latest
                // doom is harvested per request, so reset before the serve.
                fz.scorer.attach_gf = attach_gf;
                fz.scorer.stats.last_doom_q = None;
                let fails_before = fz.scorer_fail_opens;
                let resets_before = fz.resets;
                let calls_before = (
                    fz.scorer.stats.trace_calls,
                    fz.scorer.stats.score_calls,
                    fz.scorer.stats.cache_hits,
                    fz.scorer.stats.brain_ms,
                );
                // Cut-registry watermarks for the curator decision log. Deltas
                // saturate: a memo reset (client edit) mid-conversation clears
                // the registries, and a from-scratch replay then reads as all-new
                // cuts — which is what it is.
                let insists_before = fz.insists;
                let chunks_before = fz.dropped_count();
                let ranges_before: HashMap<String, usize> = fz
                    .dropped_ranges()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.len()))
                    .collect();
                let served = fz.serve(&internal_in);
                // Replay-storm attribution (perf research 2026-09-02 §1):
                // WHICH message's bytes churned decides the fix — index 0 is
                // the system entry (Desktop-style head mutation), a small
                // index is client compaction/trim, shrunk is a rewound run.
                if fz.resets > resets_before {
                    let (idx, shrunk) = fz.last_reset_divergence.unwrap_or((0, false));
                    tracing::warn!(
                        conv = %conv_for_vis,
                        divergence_idx = idx,
                        role = internal_in
                            .get(idx)
                            .and_then(|m| m.get("role"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("?"),
                        shrunk,
                        "freezer purity reset — consumed message bytes changed; \
                         full birth replay this request (replay-storm signature)"
                    );
                }
                let mut ranges: Vec<(String, i64, i64)> = Vec::new();
                for (file, after) in fz.dropped_ranges() {
                    let skip = ranges_before
                        .get(file)
                        .copied()
                        .unwrap_or(0)
                        .min(after.len());
                    for &(lo, hi) in &after[skip..] {
                        ranges.push((file.clone(), lo, hi));
                    }
                }
                ranges.sort();
                let cut_delta = CutDelta {
                    insists: fz.insists.saturating_sub(insists_before),
                    chunks: (fz.dropped_count().saturating_sub(chunks_before)) as u64,
                    ranges,
                };
                // Visibility export (docs/NOREREAD_HOOK_DEFECT.md defect 3):
                // the FULL current registries — dropped (invisible upstream)
                // and served (restored, visible again) — so the no-reread
                // hook never denies a re-read of content the model cannot
                // see. Written on the blocking thread while fz is in hand;
                // fail-open (record swallows every IO error).
                if let Some(sid) = &vis_sid {
                    let mut vis: std::collections::BTreeMap<String, crate::visibility::FileVis> =
                        std::collections::BTreeMap::new();
                    for (f, r) in fz.dropped_ranges() {
                        vis.entry(f.to_lowercase()).or_default().dropped = r.clone();
                    }
                    for (f, r) in fz.served_ranges() {
                        vis.entry(f.to_lowercase()).or_default().served = r.clone();
                    }
                    crate::visibility::record(sid, &conv_for_vis, vis);
                }
                (
                    fz,
                    served,
                    fails_before,
                    resets_before,
                    calls_before,
                    cut_delta,
                )
            })
            .await
            .map_err(|e| anyhow::anyhow!("freezer task panicked: {e}"))?;
        stats.scorer_fail_opens = fz.scorer_fail_opens - fails_before;
        stats.purity_resets = fz.resets - resets_before;
        // Per-request DELTA — brain_ms accumulates for the life of the
        // scorer; v0.2.6 copied it raw, so the logged/ledgered value was
        // cumulative-since-lane-birth (docs/perf-research-2026-09-02.md §0).
        stats.brain_ms = fz.scorer.stats.brain_ms - calls_before.3;
        stats.checkpoint_id = fz.scorer.stats.checkpoint_id.clone();
        stats.births_scored = fz.scorer.stats.trace_calls - calls_before.0;
        stats.score_calls = fz.scorer.stats.score_calls - calls_before.1;
        stats.cache_hits = fz.scorer.stats.cache_hits - calls_before.2;
        gov_doom_q = fz.scorer.stats.last_doom_q;
        lock(&st.convs).entry(conv_id.clone()).or_default().freezer = Some(fz);
        // Curator decision log — the per-cut twin of the tool-prune score
        // lines: one line per range cut this call, so an over-cut is
        // diagnosable from the log alone. File paths already ride verbatim
        // in the served re-read pointers (data plane local — tracing output
        // never leaves the machine). Logged on the error path too: steps
        // that committed before a mid-serve failure would otherwise vanish
        // from the log (the next request's watermark starts above them).
        for (file, lo, hi) in &cut_delta.ranges {
            tracing::debug!(
                conv = %conv_id,
                file = %file,
                lo,
                hi,
                "curator: range cut — digest + re-read pointer served"
            );
        }
        stats.curator_insists = cut_delta.insists;
        if cut_delta.insists > 0 {
            tracing::info!(
                conv = %conv_id,
                insists = cut_delta.insists,
                "curator: insist valve — agent re-asked cut content (over-cut signal)"
            );
        }
        match served {
            Ok(mut c) => {
                // Human turns get their original bytes back BEFORE anything
                // measures or folds: apply_curation's equality branch then
                // forwards them verbatim, and freeze_cut_tokens counts only
                // what was actually cut.
                stats.freeze_cut_protected_tokens =
                    crate::protect::restore_protected(&internal, &mut c, &protected);
                if stats.freeze_cut_protected_tokens > 0 {
                    tracing::warn!(
                        conv = %conv_id,
                        refused_tokens = stats.freeze_cut_protected_tokens,
                        "curator: cut refused on human-authored content — served verbatim"
                    );
                }
                stats.freeze_cut_roles = crate::protect::cut_by_role(&internal, &c);
                stats.freeze_cut_tokens = (internal_mass(&internal) - internal_mass(&c)).max(0);
                tracing::debug!(
                    conv = %conv_id,
                    cut_tokens = stats.freeze_cut_tokens,
                    cut_chunks = cut_delta.chunks,
                    cut_ranges = cut_delta.ranges.len(),
                    insists = cut_delta.insists,
                    brain_ms = stats.brain_ms,
                    scorer_fail_opens = stats.scorer_fail_opens,
                    checkpoint = stats.checkpoint_id.as_deref().unwrap_or("-"),
                    "freezer served"
                );
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
        // TAKE the fold map instead of deep-cloning it under the global
        // `convs` mutex (perf research 2026-09-02 §4): the clone of a big
        // conversation's folds stalled every other lane's memo access. The
        // per-lane lock guarantees exclusivity; an error path before the
        // write-back leaves the lane's folds empty, which only costs a
        // deterministic re-fold next request (pure function), never bytes.
        (std::mem::take(&mut cs.folds), cs.last_fps.clone())
    };
    let folds_before = folds.len();
    let mut curated = splice::apply_curation(body, &curated_internal, Some(&mut folds));
    stats.folds_total = folds.len();
    stats.folds_new = folds.len().saturating_sub(folds_before);
    // Folds MAY commit before the send: they memoize served bytes and replay
    // idempotently — unlike fingerprints, which assert "upstream has cached
    // these bytes" and must wait for the 2xx (see below).
    lock(&st.convs).entry(conv_id.clone()).or_default().folds = folds;
    tracing::debug!(
        conv = %conv_id,
        turn = stats.turn,
        folds_total = stats.folds_total,
        folds_new = stats.folds_new,
        births_scored = stats.births_scored,
        "curate: fold-back done"
    );

    // Tool-schema keep-set (reference _prepare_anthropic step 10): score the
    // roster ONCE per conversation via the brain tool head, freeze the
    // keep-set, filter every later request against it. Fail-open at every
    // seam: scoring failure = full roster + retry next request. Added guard
    // vs the reference: a client-forced tool_choice is served the full
    // roster (the reference could 400 upstream on a pruned forced tool).
    // Runs BEFORE breakpoints (it never touches messages/system, so the
    // served bytes are unchanged by the reorder).
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
                    Some(k) => {
                        tracing::debug!(
                            conv = %conv_id,
                            keep = k.len(),
                            "tool-prune: reusing frozen keep-set"
                        );
                        Some(k)
                    }
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
                                // The GNN tool-head predictions behind the
                                // decision — one line per tool so a mis-prune
                                // is diagnosable from the log alone (tool
                                // names are harness identifiers, never user
                                // code — same class as conv ids).
                                for (i, name) in ts.names.iter().enumerate() {
                                    tracing::debug!(
                                        conv = %conv_id,
                                        tool = %name,
                                        score_q = ts.scores_q[i],
                                        tokens = ts.tokens[i],
                                        kept = kset.contains(name),
                                        "tool-prune: GNN tool-head score"
                                    );
                                }
                                tracing::info!(
                                    conv = %conv_id,
                                    kept = res.keep.len(),
                                    total = ts.names.len(),
                                    saved_tok = res.tokens_saved,
                                    kept_tok = res.tokens_kept,
                                    cut_frac = format!("{:.3}", res.cut_frac).as_str(),
                                    "tool-prune keep-set frozen"
                                );
                                lock(&st.convs)
                                    .entry(conv_id.clone())
                                    .or_default()
                                    .tool_keep = Some(kset.clone());
                                Some(kset)
                            }
                            // Roster-stability invariant (incident
                            // 2026-08-30 §4.3): scoring failure used to
                            // serve the full roster and RETRY next request —
                            // a later successful prune then changed the
                            // serialized tools region and invalidated the
                            // provider cache for everything after it. The
                            // fail-open is now frozen like a successful one:
                            // the full roster becomes the lane's keep-set,
                            // byte-identical for the rest of the run. Costs
                            // the pruning savings for this lane; never a
                            // mid-run prefix flip.
                            None => {
                                let all: HashSet<String> = tools
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|t| t.get("name").and_then(Value::as_str))
                                    .map(str::to_owned)
                                    .collect();
                                tracing::warn!(
                                    conv = %conv_id,
                                    total = all.len(),
                                    "tool-prune: scorer unavailable at roster freeze — FULL \
                                     roster frozen for the lane (no mid-run prefix flip)"
                                );
                                lock(&st.convs)
                                    .entry(conv_id.clone())
                                    .or_default()
                                    .tool_keep = Some(all.clone());
                                Some(all)
                            }
                        }
                    }
                };
                // Reactive unfreeze (added vs the reference, like the forced
                // guard below): if the model reached for a pruned tool anyway
                // — a tool_use in the prefix naming a roster tool outside the
                // keep-set — serve its full schema from this request on. The
                // keep-set only ever GROWS, so the provider cache busts once
                // per unfrozen tool, never per turn; a mis-prune costs the
                // model one schema-blind call instead of losing the tool for
                // the whole conversation.
                let keep = keep.map(|mut k| {
                    let called = prefix_tool_use_names(body.get("messages"));
                    let unfroze: Vec<String> = tools
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|t| t.get("name").and_then(Value::as_str))
                        .filter(|n| called.contains(*n) && !k.contains(*n))
                        .map(str::to_owned)
                        .collect();
                    if !unfroze.is_empty() {
                        tracing::info!(
                            conv = %conv_id,
                            tools = ?unfroze,
                            "tool-prune: reactive unfreeze — prefix calls pruned tools"
                        );
                        stats.tools_unfrozen = Some(unfroze.len());
                        k.extend(unfroze);
                        lock(&st.convs)
                            .entry(conv_id.clone())
                            .or_default()
                            .tool_keep = Some(k.clone());
                    }
                    k
                });
                // Client-forced tool_choice: v0.2.6 served the FULL roster
                // for that one call — a transient full↔pruned prefix flip
                // that busted the provider cache twice per forced call
                // (incident roster-stability invariant). The forced tool is
                // now UNFROZEN into the keep-set instead — same monotonic
                // growth as the reactive unfreeze: the roster changes once
                // and stays changed. Full roster only when the forced name
                // is unresolvable (never prune into an upstream 400).
                let forced =
                    body.pointer("/tool_choice/type").and_then(Value::as_str) == Some("tool");
                let forced_name = forced
                    .then(|| body.pointer("/tool_choice/name").and_then(Value::as_str))
                    .flatten();
                let keep = match (keep, forced, forced_name) {
                    (Some(mut k), true, Some(name)) => {
                        if !k.contains(name) {
                            tracing::info!(
                                conv = %conv_id,
                                tool = %name,
                                "tool-prune: forced tool_choice unfroze the forced tool"
                            );
                            stats.tools_unfrozen = Some(stats.tools_unfrozen.unwrap_or(0) + 1);
                            k.insert(name.to_owned());
                            lock(&st.convs)
                                .entry(conv_id.clone())
                                .or_default()
                                .tool_keep = Some(k.clone());
                        }
                        Some(k)
                    }
                    (_, true, None) => {
                        tracing::debug!(
                            conv = %conv_id,
                            "tool-prune: forced tool_choice with no resolvable name — \
                             full roster served"
                        );
                        None
                    }
                    (k, _, _) => k,
                };
                if let Some(keep) = keep {
                    let src: Vec<Value> = curated
                        .get("tools")
                        .or(Some(tools))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    // Pruned tools are served as name+note stubs (default;
                    // PARSEC_TOOL_STUB=off restores the hard-drop) so the
                    // model knows they exist and can call one to get its
                    // full schema back via the reactive unfreeze above.
                    // Stub bytes are deterministic, so the frozen keep-set
                    // still yields a byte-stable roster across turns.
                    let mut kept_full = 0usize;
                    let mut stubbed = 0usize;
                    let served: Vec<Value> = src
                        .into_iter()
                        .filter_map(|t| {
                            let in_keep = t
                                .get("name")
                                .and_then(Value::as_str)
                                .is_some_and(|n| keep.contains(n));
                            if in_keep {
                                kept_full += 1;
                                Some(t)
                            } else if bcfg.tool_stub {
                                let s = brain::stub_tool(&t);
                                if s.is_some() {
                                    stubbed += 1;
                                }
                                s
                            } else {
                                None
                            }
                        })
                        .collect();
                    stats.tools_kept = Some(kept_full);
                    if stubbed > 0 {
                        stats.tools_stubbed = Some(stubbed);
                    }
                    // Served-roster hash beside the pre-prune hash: a
                    // roster-driven cache bust is attributable from the
                    // ledger alone (incident P1 diagnostics).
                    let served = Value::Array(served);
                    stats.tools_served_sha8 = Some(brain::roster_sha8(&served));
                    tracing::debug!(
                        conv = %conv_id,
                        pre_prune_sha8 = stats.tools_pre_prune_sha8.as_deref().unwrap_or("-"),
                        served_sha8 = stats.tools_served_sha8.as_deref().unwrap_or("-"),
                        "tool-prune: roster served"
                    );
                    if let Some(o) = curated.as_object_mut() {
                        o.insert("tools".into(), served);
                    }
                }
            }
        }
    }

    // ── governor stage (contract Track B item 4) ────────────────────────────
    // off  => byte-identical current behavior INCLUDING directive_appended=
    //         false (zero extra brain calls);
    // advise => compute + record only, wire untouched;
    // on   => advise + append the directive turn via append_user_text.
    // Fail-open at every seam: a governor error never fails or alters the
    // request beyond passthrough — counted in gov_fail_open_count.
    let mut directive_appended = false;
    let mut gov_commit: Option<GovCommit> = None;
    if st.governor.mode != GovMode::Off {
        let (mut gov_stats, directive, commit) =
            govern_stage(st, &conv_id, body, &internal, &curated_internal, gov_doom_q).await;
        if let (GovMode::On, Some(text)) = (st.governor.mode, directive) {
            curated = splice::append_user_text(&curated, &text);
            directive_appended = true;
            gov_stats.directive_injected = true;
        }
        stats.gov = Some(gov_stats);
        gov_commit = Some(commit);
    }

    let prior = if prior_fps.is_empty() {
        None
    } else {
        Some(prior_fps.as_slice())
    };
    let (out, cur_fps) = splice::place_cache_breakpoint(&curated, directive_appended, prior);
    stats.anchors = anchor_indices(&out);
    let frozen_len = cur_fps
        .iter()
        .zip(prior_fps.iter())
        .take_while(|(a, b)| a == b)
        .count();
    tracing::debug!(
        conv = %conv_id,
        fps = cur_fps.len(),
        prior_fps = prior_fps.len(),
        frozen_prefix = frozen_len,
        anchors = ?stats.anchors,
        directive_appended,
        ttl = splice::client_cache_ttl(body).as_deref().unwrap_or("-"),
        "cache breakpoint placed"
    );

    // Cache-loss guardrail (incident 2026-08-30 P1): a WARM lane (prior
    // fingerprints exist) about to rewrite a large previously-covered extent
    // is the incident's exact signature — continuity that should have held
    // did not (client history edit, an identity bug this fix missed, or a
    // race). Reseeding that extent bills it all as cache creation; failing
    // open serves the client's own bytes and anchors instead, which is
    // strictly cheaper. Cold lanes (empty prior) are exempt: a first sight
    // may legitimately seed in full, per the incident's own methodology.
    if st.cache_guard_tokens > 0 && !prior_fps.is_empty() {
        let covered = prior_fps.len().min(cur_fps.len());
        if frozen_len < covered {
            let rewritten_tokens: i64 = out
                .get("messages")
                .and_then(Value::as_array)
                .map(|msgs| {
                    msgs[frozen_len..covered]
                        .iter()
                        .map(|m| serde_json::to_string(m).map_or(0, |s| s.len() as i64 / 4))
                        .sum()
                })
                .unwrap_or(0);
            if rewritten_tokens > st.cache_guard_tokens {
                lock(&st.convs).entry(conv_id.clone()).or_default().bypass = true;
                let fires = st.cache_guard_count.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    conv = %conv_id,
                    rewritten_tokens,
                    covered,
                    frozen_prefix = frozen_len,
                    threshold = st.cache_guard_tokens,
                    cache_guard_total = fires,
                    "cache-loss guardrail TRIPPED — warm lane would rewrite \
                     previously covered prefix; lane latched to verbatim passthrough"
                );
                anyhow::bail!(
                    "cache-loss guardrail: warm lane would rewrite ~{rewritten_tokens} \
                     previously covered tokens (> {}) — passing original body through",
                    st.cache_guard_tokens
                );
            }
        }
    }

    let frozen = cur_fps
        .iter()
        .zip(prior_fps.iter())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.as_str());
    let cache_prefix_sha8 = sha8_of_fps(frozen);
    stats.curate_ms = t_curate.elapsed().as_secs_f64() * 1000.0;

    Ok(Plan {
        conv_id,
        out_bytes: serde_json::to_vec(&out)?,
        cur_fps,
        cache_prefix_sha8,
        stats,
        gov_commit,
    })
}

/// Message indices carrying a cache anchor in the served body (tracing seam).
fn anchor_indices(out: &Value) -> Vec<usize> {
    out.get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter()
                .enumerate()
                .filter(|(_, m)| {
                    m.get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|blocks| {
                            blocks.iter().any(|b| b.get("cache_control").is_some())
                        })
                })
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap_or_default()
}

/// The governor's per-request work (mode != off): note the doom score, fetch
/// the neighbour baseline once per conversation, compute the pure signal
/// layer, evaluate triggers/latches, score + fire rules, and return (ledger
/// seams, the joined directive text if any, the one-shot latch commit). The
/// CALLER decides whether the directive touches the wire (mode == on only) —
/// advise runs the identical evaluation as a dry-run.
///
/// Memo discipline: all persistent-cache updates (dooms, nbr median) are
/// TARGETED mutations under short locks — never clone→write-back, which
/// raced write_ledger's in-place `billed_in_cum +=` (lost update). One-shot
/// consumables (rule-fire dedupe inserts, bank/coach/horizon latches) burn
/// on a SCRATCH copy and only reach the live memo via the caller's 2xx
/// commit. Every internal failure fail-opens in place (skip + count) — this
/// function cannot fail the request.
async fn govern_stage(
    st: &Arc<AppState>,
    conv_id: &str,
    body: &Value,
    internal: &[Value],
    curated_internal: &[Value],
    doom_q: Option<i64>,
) -> (GovStats, Option<String>, GovCommit) {
    let cfg = &st.governor;
    // note_doom before govern (reference cadence): this request's trace
    // scoring already ran, so its doom is part of this turn's decision.
    // Accumulator, not a consumable — targeted push, immediate.
    if let Some(dq) = doom_q {
        lock(&st.convs)
            .entry(conv_id.to_string())
            .or_default()
            .gov
            .note_doom(dq as f64 / 1_000_000.0);
    }
    // Neighbour-cost baseline: fetched ONCE per conversation (per-task
    // constant); a failed fetch stays None and retries next turn. Cache,
    // not a consumable — check-then-set under the lock (a racing request
    // fetching the same constant is harmless; first writer wins).
    let need_nbr = lock(&st.convs)
        .entry(conv_id.to_string())
        .or_default()
        .gov
        .nbr_cost_median
        .is_none();
    if need_nbr {
        if let Some(bcfg) = &st.brain {
            match brain::fetch_neighbors(&st.client, bcfg, conv_id, internal).await {
                Some(nb) => {
                    let mut convs = lock(&st.convs);
                    let gov = &mut convs.entry(conv_id.to_string()).or_default().gov;
                    if gov.nbr_cost_median.is_none() {
                        gov.nbr_cost_median = Some(nb.nbr_cost_median);
                    }
                    tracing::info!(
                        conv = %&conv_id[..conv_id.len().min(12)],
                        nbr_count = nb.nbr_count,
                        neighbors_active = nb.neighbors_active,
                        nbr_cost_median = ?nb.nbr_cost_median,
                        "governor: neighbour baseline fetched"
                    );
                }
                None => {
                    st.gov_fail_open_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        } else {
            // No brain: hoods can never answer — record "fetched, inert" so
            // the runaway arm is knowingly off rather than retried forever.
            let mut convs = lock(&st.convs);
            let gov = &mut convs.entry(conv_id.to_string()).or_default().gov;
            if gov.nbr_cost_median.is_none() {
                gov.nbr_cost_median = Some(None);
            }
        }
    }
    // Pure evaluation over a snapshot; one-shot latches burn on a SCRATCH
    // copy only (committed by the caller after upstream 2xx — a 429/529
    // must not consume DELIVER/HORIZON or a rule fire the model never saw).
    let snapshot = lock(&st.convs)
        .entry(conv_id.to_string())
        .or_default()
        .gov
        .clone();
    let sig = governor::compute_signals(cfg, curated_internal, body, &snapshot);
    let mut scratch = snapshot.clone();
    let plan = governor::evaluate_triggers(cfg, &sig, &mut scratch);
    // The kill latch commits IMMEDIATELY (check-then-set): it re-appends
    // every turn anyway, and un-latching on upstream failure would flap.
    if scratch.kill_latched && !snapshot.kill_latched {
        lock(&st.convs)
            .entry(conv_id.to_string())
            .or_default()
            .gov
            .kill_latched = true;
    }
    let mut directives = plan.directives.clone();
    let mut rule_fires = 0usize;
    let mut fired_pairs: Vec<(String, i64)> = Vec::new();
    // Rule delivery (skipped on a killed run, like the reference's early
    // return; and with no brain there is no roster to score).
    if !plan.kill {
        if let Some(bcfg) = &st.brain {
            let tools = body.get("tools").cloned().unwrap_or_else(|| json!([]));
            match brain::score_rules(
                &st.client,
                bcfg,
                conv_id,
                curated_internal,
                &tools,
                sig.cur_step,
            )
            .await
            {
                Some(rr) => {
                    // Dedupes against the snapshot's fired set + within this
                    // call; the inserts land on scratch and commit on 2xx.
                    let fired = governor::rule_fires(&rr.rules, cfg.tau_q(), &mut scratch);
                    rule_fires = fired.len();
                    fired_pairs = fired.iter().map(|r| (r.eid.clone(), r.fire_step)).collect();
                    if !fired.is_empty() {
                        tracing::info!(
                            conv = %&conv_id[..conv_id.len().min(12)],
                            step = sig.cur_step,
                            eids = ?fired.iter().map(|r| r.eid.as_str()).collect::<Vec<_>>(),
                            tau_q = cfg.tau_q(),
                            tau_hint_q = ?rr.tau_hint_q,
                            "governor: rule fire"
                        );
                        directives.push(governor::rules_directive(&fired));
                    }
                }
                None => {
                    // 404/500/501/transport: skip rules this turn, counted.
                    st.gov_fail_open_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
    // Governor flag trace: DEBUG full dict every turn; INFO on any action.
    tracing::debug!(
        conv = %&conv_id[..conv_id.len().min(12)],
        step = sig.cur_step,
        cum_tok = sig.cum_tok.round(),
        nsteps = sig.nsteps,
        n_src = sig.n_src,
        lf = ?sig.lf_last,
        gf = ?sig.gf,
        runaway = (sig.runaway_factor * 1000.0).round() / 1000.0,
        mech = sig.mech_flagged,
        head = sig.head_flagged,
        budget = sig.budget_flagged,
        bank = sig.bank_flagged,
        doomed = sig.doomed_flagged,
        floor_ok = sig.floor_ok,
        kill_latched = scratch.kill_latched,
        coach_fired = scratch.coach_fired,
        bank_fired = scratch.bank_fired,
        horizon_fired = scratch.horizon_fired,
        dooms = snapshot.dooms.len(),
        billed_in_cum = snapshot.billed_in_cum,
        "governor: flag trace"
    );
    if plan.kill || plan.deliver || plan.horizon || rule_fires > 0 {
        tracing::info!(
            conv = %&conv_id[..conv_id.len().min(12)],
            step = sig.cur_step,
            mode = cfg.mode.as_str(),
            kill = plan.kill,
            kill_new = plan.kill_new,
            deliver = plan.deliver,
            horizon = plan.horizon,
            rule_fires,
            "governor: directive(s) armed"
        );
    }
    let gov_stats = GovStats {
        mode: cfg.mode.as_str(),
        runaway_factor: sig.runaway_factor,
        loop_frac: sig.lf_last.unwrap_or(0.0),
        doom_q,
        n_src: sig.n_src,
        cum_tok: sig.cum_tok,
        rule_fires,
        directive_injected: false, // the caller flips it on actual injection
        nbr_cost_median: scratch.nbr_cost_median,
    };
    let commit = GovCommit {
        fired: fired_pairs,
        coach_fired: scratch.coach_fired && !snapshot.coach_fired,
        bank_fired: scratch.bank_fired && !snapshot.bank_fired,
        horizon_fired: scratch.horizon_fired && !snapshot.horizon_fired,
    };
    let directive = if directives.is_empty() {
        None
    } else {
        Some(directives.join("\n\n"))
    };
    (gov_stats, directive, commit)
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

/// count_tokens accepts a SUBSET of the /v1/messages body; anything else is
/// a 400 "Extra inputs are not permitted". Claude Code always sends
/// `metadata` (plus max_tokens/stream/temperature), so probing with the raw
/// body verbatim failed on EVERY real CC request — savings read as
/// unmeasured across whole sessions. Keep only the token-bearing fields the
/// endpoint accepts; none of the stripped fields affect the count.
const COUNT_TOKENS_FIELDS: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "thinking",
];

/// The probe body: the ORIGINAL (pre-curation) content, narrowed to the
/// count_tokens field set. None when the inbound body isn't a JSON object —
/// the caller falls back to the raw bytes (which then fail loudly upstream).
fn probe_body(body: &Value) -> Option<Vec<u8>> {
    let obj = body.as_object()?;
    let narrowed: Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| COUNT_TOKENS_FIELDS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    serde_json::to_vec(&Value::Object(narrowed)).ok()
}

/// Free probe: the ORIGINAL inbound content (narrowed per [`probe_body`])
/// against upstream count_tokens. Any failure returns None — the ledger
/// records null, never an estimate (§8.4).
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
pub(crate) fn rfc3339_now() -> String {
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
/// ~/.parsec/ledger.jsonl. Field names follow
/// packages/contracts/schemas/savings-ledger.schema.json (flat `billed_*`,
/// `cachePrefixSha8`, RFC3339 `ts`). `counterfactual_input_tokens` is null
/// when the probe failed — measurement honesty (§8.4) forbids estimating.
/// Row identity, owned so the streaming finalizer can carry it to stream end.
#[derive(Clone)]
pub(crate) struct RowCtx {
    pub(crate) conv_id: String,
    /// Claude Code session the request belongs to (metadata.user_id) — the
    /// cross-conversation grouping conv_id can't provide: compaction and
    /// subagents mint new conv_ids inside one session.
    pub(crate) session_id: Option<String>,
    /// Calling-tool attribution (`x-parsec-tool` header, set by non-Claude-
    /// Code shims like the opencode plugin). None = Claude Code.
    pub(crate) tool: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) cache_prefix_sha8: String,
    pub(crate) fail_open: bool,
    /// Stage timings for the ledger row (0.0 = not measured on this path):
    /// lane-lock queue wait, count_tokens probe wall time (overlapped with
    /// the upstream call), upstream time-to-first-byte.
    pub(crate) queued_ms: f64,
    pub(crate) probe_ms: f64,
    pub(crate) upstream_ttfb_ms: f64,
}

pub(crate) fn write_ledger(
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
    // The input side the provider actually billed — also the anchor a
    // local-BPE counterfactual is expressed against.
    let billed_side =
        g("input_tokens") + g("cache_read_input_tokens") + g("cache_creation_input_tokens");
    // §8.4 keeps its shape: a PROBED counterfactual always wins, a missing one
    // is null, and the locally-counted delta only fills a hole the provider
    // gave us no way to fill. The two never pool silently — the row says which.
    let (counterfactual, cf_source) = match (counterfactual, stats.counterfactual_local) {
        (Some(cf), _) => (Some(cf), Some("count_tokens")),
        (None, Some(m)) => {
            crate::counterfact::sanity_check(m.served, billed_side);
            (Some(billed_side + m.delta), Some("local_bpe"))
        }
        (None, None) => (None, None),
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
        // Stage timings (perf research 2026-09-02 §5): previously log-only,
        // which made the latency split unqueryable after the fact.
        "curate_ms": (stats.curate_ms * 10.0).round() / 10.0,
        "queued_ms": (ctx.queued_ms * 10.0).round() / 10.0,
        "probe_ms": (ctx.probe_ms * 10.0).round() / 10.0,
        "upstream_ttfb_ms": (ctx.upstream_ttfb_ms * 10.0).round() / 10.0,
    });

    // The live savings line — what `tail -f ~/.parsec/proxy.log` (or the
    // proxy terminal) shows per request. Token-denominated per §8.4; the
    // input-side billed sum is uncached + cache read + cache write.

    // Governor accumulator (mode != off): billed input-side tokens per
    // conversation feed the kill floor + runaway numerator. Post-response by
    // construction — SSE rows land here at stream end (or abort) too, and
    // fail-open rows accrue as well (finding 5: the kill floor must not
    // undercount on exactly the blow-out conversations it exists for).
    if st.governor.mode != GovMode::Off && !conv_id.is_empty() && billed_side > 0 {
        lock(&st.convs)
            .entry(conv_id.to_string())
            .or_default()
            .gov
            .billed_in_cum += billed_side;
    }
    match counterfactual {
        Some(cf) => tracing::info!(
            source = cf_source.unwrap_or("?"),
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
        if let Some(sid) = &ctx.session_id {
            o.insert("session_id".into(), json!(sid));
        }
        if let Some(t) = &ctx.tool {
            o.insert("tool".into(), json!(t));
        }
        // Fleet join key (docs/install-tracking.md). READ-only here: minting
        // happens off the serving path (setup/key CLI, worker startup), so a
        // row simply lacks the field until this machine has an id.
        if let Some(iid) = crate::install::current_id() {
            o.insert("install_id".into(), json!(iid));
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
        if stats.purity_resets > 0 {
            o.insert("purity_resets".into(), json!(stats.purity_resets));
        }
        if stats.freeze_cut_tokens > 0 {
            o.insert("freeze_cut_tokens".into(), json!(stats.freeze_cut_tokens));
        }
        // Only the LOCAL instrument is nameable here. The contract's enum is
        // ["local_bpe"] and its description makes the probed case the absent
        // one ("Absent = provider-probed"), so stamping "count_tokens" put the
        // row outside the schema: the platform answers 422 extra/literal and
        // drops it whole. A probed saving is the honest, measured kind — and
        // it was the one that never reached the dashboard. `cf_source` still
        // carries "count_tokens" for the log line above, which is free text.
        if let Some(src) = cf_source.filter(|s| *s != "count_tokens") {
            o.insert("counterfactual_source".into(), json!(src));
        }
        if !stats.freeze_cut_roles.is_empty() {
            o.insert(
                "freeze_cut_roles".into(),
                json!(stats.freeze_cut_roles.clone()),
            );
        }
        if stats.freeze_cut_protected_tokens > 0 {
            o.insert(
                "freeze_cut_protected_tokens".into(),
                json!(stats.freeze_cut_protected_tokens),
            );
        }
        if stats.curator_insists > 0 {
            o.insert("curator_insists".into(), json!(stats.curator_insists));
        }
        if let Some(t) = stats.tools_total {
            o.insert("tools_total".into(), json!(t));
        }
        if let Some(k) = stats.tools_kept {
            o.insert("tools_kept".into(), json!(k));
        }
        if let Some(u) = stats.tools_unfrozen {
            o.insert("tools_unfrozen".into(), json!(u));
        }
        if let Some(s) = stats.tools_stubbed {
            o.insert("tools_stubbed".into(), json!(s));
        }
        if let Some(s8) = &stats.tools_pre_prune_sha8 {
            o.insert("tools_pre_prune_sha8".into(), json!(s8));
        }
        if let Some(s8) = &stats.tools_served_sha8 {
            o.insert("tools_served_sha8".into(), json!(s8));
        }
        // Governor seams (contract Track B item 5) — only when mode != off,
        // so pre-governor rows stay schema-identical.
        if let Some(gv) = &stats.gov {
            let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
            o.insert("governor_mode".into(), json!(gv.mode));
            o.insert("gov_runaway_factor".into(), json!(r3(gv.runaway_factor)));
            o.insert("gov_loop_frac".into(), json!(r3(gv.loop_frac)));
            if let Some(dq) = gv.doom_q {
                o.insert("gov_doom_q".into(), json!(dq));
            }
            o.insert("gov_n_src".into(), json!(gv.n_src));
            o.insert("gov_cum_tok".into(), json!(gv.cum_tok.round()));
            o.insert("gov_rule_fires".into(), json!(gv.rule_fires));
            o.insert(
                "gov_directive_injected".into(),
                json!(gv.directive_injected),
            );
            if let Some(median) = gv.nbr_cost_median {
                o.insert("nbr_cost_median".into(), json!(median));
            }
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

    // Also ship the row to the platform for per-account attribution (the
    // dashboard's savings view). Fire-and-forget and fail-open: the row is
    // already on disk, and shipping never blocks or fails the request. `ship`
    // resolves the API key live (env → ~/.parsec/credentials.json) and no-ops
    // when shipping is unconfigured.
    crate::ledger_ship::ship(&st.client, &row);
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
    /// Relay telemetry (tracing seam): raw bytes relayed + data events seen.
    bytes: u64,
    events: u64,
}

impl SseUsageScan {
    fn feed(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
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
        self.events += 1;
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
    tracing::debug!(
        bytes = raw.len(),
        "inbound /v1/messages/count_tokens — verbatim passthrough, no memo touch"
    );
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

/// GET /v1/models[/{id}] — verbatim passthrough. Deliberately no session or
/// memo touch (same discipline as count_tokens): it carries no conversation,
/// so letting it mutate curation state would be a pure source of drift.
async fn models_passthrough(
    State(st): State<Arc<AppState>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    let _guard = InFlight::enter(&st);
    let path = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/v1/models");
    tracing::debug!(path, "inbound /v1/models — verbatim passthrough");
    match st
        .client
        .get(st.url(path))
        .headers(forward_auth_headers(&headers))
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

pub(crate) fn respond(status: StatusCode, ct: Option<HeaderValue>, body: Body) -> Response {
    let mut b = Response::builder().status(status);
    if let Some(ct) = ct {
        b = b.header(header::CONTENT_TYPE, ct);
    }
    b.body(body)
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

pub(crate) fn bad_gateway(e: &reqwest::Error) -> Response {
    // Upstream unreachable is not "our own logic" failing — surface it as a
    // gateway error rather than fabricating an Anthropic-shaped response.
    tracing::warn!("upstream unreachable: {e}");
    (
        StatusCode::BAD_GATEWAY,
        format!("parsec proxy: upstream unreachable: {e}"),
    )
        .into_response()
}

/// POST /v1/messages — the curating pipeline (spec §4 flow: curate → probe →
/// forward → relay → ledger), with the §8.3 fail-open floor around every
/// internal step.
async fn messages(State(st): State<Arc<AppState>>, headers: HeaderMap, raw: Bytes) -> Response {
    let _guard = InFlight::enter(&st);
    let body: Option<Value> = serde_json::from_slice(&raw).ok();
    if let Some(b) = body.as_ref() {
        let model = b.get("model").and_then(Value::as_str).unwrap_or("?");
        let n_msgs = b
            .get("messages")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let n_tools = b.get("tools").and_then(Value::as_array).map_or(0, Vec::len);
        let stream = b.get("stream").and_then(Value::as_bool).unwrap_or(false);
        tracing::debug!(
            bytes = raw.len(),
            model,
            messages = n_msgs,
            tools = n_tools,
            stream,
            "inbound /v1/messages"
        );
    }

    // Claude Code housekeeping (title/topic generation): forwarded untouched
    // and unbilled — no curation state advance, no probe, no ledger row.
    if let Some(b) = body.as_ref() {
        if is_client_metadata_call(b) {
            tracing::debug!("client-metadata call (title/topic) — relayed untouched, unbilled");
            return relay_buffered(&st, "/v1/messages", &headers, raw).await;
        }
        record_inbound(&headers, b, &raw);
    }

    // Entitlement gate (apikey, resolved at startup into `st.entitled`): with
    // no API key parsec saves nothing, so the serve path is a PURE PASSTHROUGH
    // — no curation, no counterfactual probe, no ledger row. Claude Code still
    // streams normally (the forward/relay below sends the ORIGINAL body). This
    // is NOT a fail-open (no error).
    let entitled = st.entitled;

    // Lane identity + per-lane serialization (incident 2026-08-30 P0):
    // conv_id is computed ONCE here, and the lane's async mutex is held from
    // before curation until this handler returns — past the post-2xx
    // fingerprint commit — so a concurrent same-lane request can never take/
    // rebuild/overwrite the Freezer, folds, or fingerprints mid-flight.
    // Different lanes (parallel subagents, other sessions) don't contend.
    let conv_id: Option<String> = body.as_ref().map(|b| {
        conversation_id(
            &headers,
            &to_internal(b),
            session_id_from_metadata(b).as_deref(),
            b.get("model").and_then(Value::as_str),
        )
    });
    let t_queued = std::time::Instant::now();
    let _lane_guard = match (&conv_id, entitled) {
        (Some(id), true) => Some(lane_lock(&st, id).lock_owned().await),
        _ => None,
    };
    let queued_ms = t_queued.elapsed().as_secs_f64() * 1000.0;

    // (b-d) curation (freeze when a brain is configured), breakpoint
    // placement, tool keep-set — any error here means forwarding the
    // ORIGINAL body verbatim (fail-open, counted).
    let plan = if !entitled {
        None
    } else {
        match body.as_ref() {
            Some(b) => match curate(&st, &headers, b, conv_id.clone().unwrap_or_default()).await {
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
        }
    };
    // Unentitled passthrough is intentional, not a degraded fail-open.
    let fail_open = entitled && plan.is_none();
    let stats = plan.as_ref().map(|p| p.stats.clone()).unwrap_or_default();

    // (e) §8.4 counterfactual — always on the ORIGINAL inbound content,
    // narrowed to the count_tokens field set (CC's `metadata` 400s there).
    // Skipped when unentitled: no savings are claimed, so the extra
    // count_tokens call would be pure waste.
    // Spawned CONCURRENT with the upstream forward and collected after the
    // upstream's first byte (perf research 2026-09-02 §4): the probe is a
    // full extra Anthropic RTT used only by the ledger, and it used to run
    // serially on the time-to-first-byte path. Overlapped, it is usually
    // fully hidden behind upstream TTFB.
    let t_probe = std::time::Instant::now();
    let probe_task = if entitled {
        let probe_bytes = body
            .as_ref()
            .and_then(probe_body)
            .map(Bytes::from)
            .unwrap_or_else(|| raw.clone());
        let st_p = st.clone();
        let headers_p = headers.clone();
        Some(tokio::spawn(async move {
            count_tokens_probe(&st_p, &headers_p, probe_bytes).await
        }))
    } else {
        None
    };

    // (f) forward: curated body, or the original verbatim on fail-open.
    let send = plan
        .as_ref()
        .map(|p| Bytes::from(p.out_bytes.clone()))
        .unwrap_or_else(|| raw.clone());
    tracing::debug!(
        in_bytes = raw.len(),
        out_bytes = send.len(),
        fail_open,
        "forwarding to upstream /v1/messages"
    );
    let t_upstream = std::time::Instant::now();
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
    let upstream_ttfb_ms = t_upstream.elapsed().as_secs_f64() * 1000.0;
    let counterfactual = match probe_task {
        Some(h) => h.await.unwrap_or(None),
        None => None,
    };
    // Wall time since spawn — the overlapped cost is max(0, probe - ttfb).
    let probe_ms = t_probe.elapsed().as_secs_f64() * 1000.0;
    tracing::debug!(
        status = %status,
        counterfactual = ?counterfactual,
        first_byte_ms = upstream_ttfb_ms as u64,
        "upstream responded"
    );

    // Per-request pipeline trace (contract Track B item 6): stage timings +
    // fold/anchor/tool telemetry. Structured fields only — no message text.
    tracing::info!(
        conv = %plan
            .as_ref()
            .map(|p| &p.conv_id[..p.conv_id.len().min(12)])
            .unwrap_or("?"),
        turn = stats.turn,
        queued_ms = (queued_ms * 10.0).round() / 10.0,
        curate_ms = (stats.curate_ms * 10.0).round() / 10.0,
        brain_ms = (stats.brain_ms * 10.0).round() / 10.0,
        probe_ms = (probe_ms * 10.0).round() / 10.0,
        upstream_ms = (upstream_ttfb_ms * 10.0).round() / 10.0,
        purity_resets = stats.purity_resets,
        births_scored = stats.births_scored,
        score_calls = stats.score_calls,
        cache_hits = stats.cache_hits,
        folds_total = stats.folds_total,
        folds_new = stats.folds_new,
        anchors = ?stats.anchors,
        tools_kept = ?stats.tools_kept,
        tools_total = ?stats.tools_total,
        governor = stats
            .gov
            .as_ref()
            .map(|g| g.mode)
            .unwrap_or(st.governor.mode.as_str()),
        gov_directive = stats
            .gov
            .as_ref()
            .map(|g| g.directive_injected)
            .unwrap_or(false),
        fail_open,
        status = status.as_u16(),
        ledger = %st.ledger_path.display(),
        "request pipeline"
    );

    // (d) CRITICAL ORDERING: commit this call's fingerprints — and the
    // governor's one-shot consumables (rule-fire dedupe inserts, bank/coach/
    // horizon latches) — only now that the upstream 2xx proves the model saw
    // these bytes. The reference wrote fps before the send
    // (anthropic_shapes.py:484) — a failed call anchored the retry on bytes
    // Anthropic never cached; the same class of bug would burn a DELIVER/
    // HORIZON/rule fire on a 429/529 the model never received. Targeted
    // merge into the LIVE memo (extend/OR), never a whole-struct write.
    if status.is_success() {
        if let Some(p) = &plan {
            let mut convs = lock(&st.convs);
            let cs = convs.entry(p.conv_id.clone()).or_default();
            cs.last_fps = p.cur_fps.clone();
            if let Some(gc) = &p.gov_commit {
                if !gc.fired.is_empty() || gc.coach_fired || gc.bank_fired || gc.horizon_fired {
                    for f in &gc.fired {
                        cs.gov.fired.insert(f.clone());
                    }
                    cs.gov.coach_fired |= gc.coach_fired;
                    cs.gov.bank_fired |= gc.bank_fired;
                    cs.gov.horizon_fired |= gc.horizon_fired;
                    tracing::debug!(
                        conv = %p.conv_id,
                        fired = gc.fired.len(),
                        coach = gc.coach_fired,
                        bank = gc.bank_fired,
                        horizon = gc.horizon_fired,
                        "governor one-shot latches committed (upstream 2xx)"
                    );
                }
            }
            drop(convs);
            tracing::debug!(
                conv = %p.conv_id,
                fps = p.cur_fps.len(),
                "fingerprints committed (upstream 2xx confirms cache)"
            );
        }
    } else if plan.is_some() {
        tracing::debug!(
            status = %status,
            "upstream non-2xx — fingerprints and governor one-shot latches NOT \
             committed, the retry re-fires"
        );
    }

    let row_ctx = RowCtx {
        // Finding 5: on curation fail-open the billed usage must still
        // accrue to the conversation's governor memo (the kill floor
        // undercounts on exactly the blow-out conversations otherwise) —
        // derive the id from the body when it parses; "" when it doesn't.
        conv_id: plan
            .as_ref()
            .map(|p| p.conv_id.clone())
            .or_else(|| conv_id.clone())
            .unwrap_or_default(),
        session_id: body.as_ref().and_then(session_id_from_metadata),
        // Untagged Anthropic-wire traffic IS Claude Code (Desktop/codex/
        // opencode shims all tag or route elsewhere) — stamp it explicitly so
        // the harness spread is complete instead of encoding CC as absence.
        tool: tool_from_headers(&headers).or_else(|| Some("claude-code".to_string())),
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
        queued_ms,
        probe_ms,
        upstream_ttfb_ms,
    };

    let wants_stream = body
        .as_ref()
        .and_then(|b| b.get("stream"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ct = resp.headers().get(header::CONTENT_TYPE).cloned();

    if wants_stream && status.is_success() {
        // Relay the SSE bytes UNMODIFIED while scanning for usage; the
        // ledger row is written when the upstream stream ends — or on DROP
        // when the client aborts mid-stream (finding 4a: the chained
        // finalizer is never polled on abort, but input tokens were already
        // billed at message_start; StreamFinalize's Drop keeps the row +
        // billed_in_cum accrual from vanishing).
        tracing::debug!(conv = %row_ctx.conv_id, "relaying SSE stream");
        let scan = Arc::new(Mutex::new(SseUsageScan::default()));
        let scan_tee = scan.clone();
        let relayed = Arc::new(AtomicU64::new(0));
        let relayed_tee = relayed.clone();
        let tee = resp.bytes_stream().map(move |r| {
            if let Ok(b) = &r {
                relayed_tee.fetch_add(b.len() as u64, Ordering::Relaxed);
                lock(&scan_tee).feed(b);
            }
            r.map_err(io::Error::other)
        });
        // The stream outlives this handler; its own guard keeps the idle
        // sweep from shooting the proxy mid-SSE.
        let stream_guard = InFlight::enter(&st);
        let mut fin = StreamFinalize {
            st: st.clone(),
            row_ctx,
            counterfactual,
            stats,
            scan,
            relayed,
            t_stream: t_upstream,
            done: false,
        };
        let finalize = futures_util::stream::once(async move {
            let _guard = stream_guard;
            fin.finalize(true);
            Ok::<Bytes, io::Error>(Bytes::new())
        });
        return respond(status, ct, Body::from_stream(tee.chain(finalize)));
    }

    // The lane's cross-request state is fully committed above — release the
    // lane before buffering the response body and writing the ledger, so a
    // queued same-lane request stops paying for THIS response's generation
    // (perf research 2026-09-02 §4, lane-lock span).
    drop(_lane_guard);

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

/// Stream-end bookkeeping that must survive a client abort (finding 4a): a
/// dropped response body never polls the chained once-finalizer, so the
/// ledger row and the governor's billed_in_cum accrual run from Drop too.
/// Measurement honesty on the abort path: only what the scan actually
/// OBSERVED is written — an aborted stream that never saw usage
/// (no message_start) writes NO row, exactly as before.
struct StreamFinalize {
    st: Arc<AppState>,
    row_ctx: RowCtx,
    counterfactual: Option<i64>,
    stats: PlanStats,
    scan: Arc<Mutex<SseUsageScan>>,
    relayed: Arc<AtomicU64>,
    t_stream: std::time::Instant,
    done: bool,
}

impl StreamFinalize {
    fn finalize(&mut self, completed: bool) {
        if self.done {
            return;
        }
        self.done = true;
        let (usage, sse_bytes, sse_events) = {
            let mut s = lock(&self.scan);
            (s.finish(), s.bytes, s.events)
        };
        tracing::debug!(
            conv = %self.row_ctx.conv_id,
            relayed_bytes = self.relayed.load(Ordering::Relaxed),
            usage_seen = usage.is_some(),
            completed,
            stream_ms = self.t_stream.elapsed().as_millis() as u64,
            "SSE stream ended — writing ledger row"
        );
        tracing::info!(
            conv = %&self.row_ctx.conv_id[..self.row_ctx.conv_id.len().min(12)],
            stream_ms = (self.t_stream.elapsed().as_secs_f64() * 10_000.0).round() / 10.0,
            sse_bytes,
            sse_events,
            aborted = !completed,
            "stream relayed"
        );
        if !completed && usage.is_none() {
            // Aborted before message_start: nothing was observed — never
            // fabricate a row (§8.4).
            return;
        }
        write_ledger(
            &self.st,
            &self.row_ctx,
            self.counterfactual,
            usage.as_ref(),
            &self.stats,
        );
    }
}

impl Drop for StreamFinalize {
    fn drop(&mut self) {
        self.finalize(false);
    }
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

    #[test]
    fn conv_id_separates_sessions_sharing_an_opening_prefix() {
        // Two sessions, byte-identical first two messages — the shape every
        // short auxiliary call has. Before the session id was mixed in these
        // collided onto one Freezer and reset each other's memo every turn.
        let internal = vec![
            serde_json::json!({"role": "system", "content": "you are a helpful assistant"}),
            serde_json::json!({"role": "user", "content": "summarize this"}),
        ];
        let h = HeaderMap::new();
        let model = Some("claude-sonnet-5");
        let sid_a = Some("8068d98c-4176-4b0e-8e2b-a543aa24f204");
        let sid_b = Some("deadbeef-0000-4000-8000-000000000000");
        let a = conversation_id(&h, &internal, sid_a, model);
        let b = conversation_id(&h, &internal, sid_b, model);
        assert_ne!(
            a, b,
            "same opening prefix, different session must not share"
        );
        // Stable within a session: the id is what keys the memo across turns,
        // so it must not drift as the conversation grows past the head.
        let mut grown = internal.clone();
        grown.push(serde_json::json!({"role": "assistant", "content": "sure"}));
        assert_eq!(a, conversation_id(&h, &grown, sid_a, model));
        // No session id (non-Claude-Code clients) → previous behaviour, and
        // still distinct from any session-scoped id.
        let anon = conversation_id(&h, &internal, None, model);
        assert_eq!(anon, conversation_id(&h, &grown, None, model));
        assert_ne!(anon, a);
    }

    #[test]
    fn conv_id_survives_a_mutating_system_head() {
        // THE incident regression (2026-08-30): Desktop/Cowork mutates its
        // system/task head between turns of one session. v0.2.6 hashed that
        // head into the id, so every request minted a fresh lane and the
        // whole history was re-seeded as cache writes. With a session id the
        // system entry must not participate in the lane key.
        let h = HeaderMap::new();
        let sid = Some("8068d98c-4176-4b0e-8e2b-a543aa24f204");
        let model = Some("claude-fable-5");
        let turn1 = vec![
            serde_json::json!({"role": "system", "content": "task head v1 [ctx 14:50]"}),
            serde_json::json!({"role": "user", "content": "run the sales bench"}),
        ];
        let mut turn2 = vec![
            serde_json::json!({"role": "system", "content": "task head v2 [ctx 14:52]"}),
            serde_json::json!({"role": "user", "content": "run the sales bench"}),
        ];
        turn2.push(serde_json::json!({"role": "assistant", "content": "on it"}));
        turn2.push(serde_json::json!({"role": "user", "content": "continue"}));
        assert_eq!(
            conversation_id(&h, &turn1, sid, model),
            conversation_id(&h, &turn2, sid, model),
            "a mutating system head must not break lane continuity"
        );
        // Without a session id the head hash is all we have — mutation still
        // separates (unchanged reference behavior).
        assert_ne!(
            conversation_id(&h, &turn1, None, model),
            conversation_id(&h, &turn2, None, model)
        );
    }

    #[test]
    fn conv_id_separates_agents_and_models_within_a_session() {
        // Parallel subagents share the session id; their opening prompts (and
        // often models) differ. The first-user hash and the model keep their
        // lanes apart so they don't reset each other's Freezer.
        let h = HeaderMap::new();
        let sid = Some("8068d98c-4176-4b0e-8e2b-a543aa24f204");
        let parent = vec![
            serde_json::json!({"role": "system", "content": "s"}),
            serde_json::json!({"role": "user", "content": "fix the failing test"}),
        ];
        let sub = vec![
            serde_json::json!({"role": "system", "content": "s"}),
            serde_json::json!({"role": "user", "content": "search for the parser entry point"}),
        ];
        let a = conversation_id(&h, &parent, sid, Some("claude-fable-5"));
        assert_ne!(a, conversation_id(&h, &sub, sid, Some("claude-fable-5")));
        assert_ne!(a, conversation_id(&h, &parent, sid, Some("claude-opus-5")));
    }

    #[test]
    fn session_id_extraction_accepts_ids_never_text() {
        // CC 2.1.x shape: user_id is a JSON-encoded object (verified live
        // against 2.1.214).
        let cc = serde_json::json!({"metadata": {"user_id":
            r#"{"device_id":"d1fe","account_uuid":"","session_id":"8068d98c-4176-4b0e-8e2b-a543aa24f204"}"#}});
        assert_eq!(
            session_id_from_metadata(&cc).as_deref(),
            Some("8068d98c-4176-4b0e-8e2b-a543aa24f204")
        );
        // Legacy underscore shape.
        let legacy = serde_json::json!({"metadata": {"user_id":
            "user_ab12_account_cd34_session_deadbeef-0000-4000-8000-000000000000"}});
        assert_eq!(
            session_id_from_metadata(&legacy).as_deref(),
            Some("deadbeef-0000-4000-8000-000000000000")
        );
        // No session id, no metadata, or non-id content → absent, never junk.
        for body in [
            serde_json::json!({"metadata": {"user_id": "u_123"}}),
            serde_json::json!({"metadata": {"user_id": r#"{"device_id":"d"}"#}}),
            serde_json::json!({"metadata": {"user_id": r#"{"session_id":"rm -rf / #text"}"#}}),
            serde_json::json!({"messages": []}),
        ] {
            assert_eq!(session_id_from_metadata(&body), None);
        }
    }

    #[test]
    fn tool_header_accepts_slugs_never_text() {
        let hdr = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-parsec-tool", HeaderValue::from_str(v).unwrap());
            h
        };
        assert_eq!(
            tool_from_headers(&hdr("opencode")).as_deref(),
            Some("opencode")
        );
        assert_eq!(tool_from_headers(&hdr("cline")).as_deref(), Some("cline"));
        // Whitespace is trimmed; anything outside the slug charset — or too
        // long to be an identifier — is dropped, never sanitized into a row.
        assert_eq!(
            tool_from_headers(&hdr(" opencode ")).as_deref(),
            Some("opencode")
        );
        for bad in [
            "",
            "Open Code",
            "rm -rf /",
            "UPPER",
            "x".repeat(33).as_str(),
        ] {
            assert_eq!(tool_from_headers(&hdr(bad)), None, "{bad:?}");
        }
        assert_eq!(tool_from_headers(&HeaderMap::new()), None);
    }

    #[test]
    fn probe_body_narrows_to_count_tokens_fields() {
        // The real CC body shape that 400'd the probe: metadata + max_tokens
        // + stream must be stripped; token-bearing fields survive verbatim.
        let body = serde_json::json!({
            "model": "claude-fable-5",
            "max_tokens": 32000,
            "stream": true,
            "temperature": 1.0,
            "metadata": {"user_id": "u_123"},
            "system": "be terse",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "enabled", "budget_tokens": 1024}
        });
        let probed: Value =
            serde_json::from_slice(&probe_body(&body).expect("object body")).unwrap();
        // The narrow preserves the BODY's key order (preserve_order); compare
        // as sets — field order is irrelevant to count_tokens.
        let mut keys: Vec<&str> = probed
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        let mut expect = vec![
            "messages",
            "model",
            "system",
            "thinking",
            "tool_choice",
            "tools",
        ];
        expect.sort_unstable();
        assert_eq!(keys, expect);
        for k in ["metadata", "max_tokens", "stream", "temperature"] {
            assert!(probed.get(k).is_none(), "{k} must be stripped");
        }
        assert_eq!(probed["messages"], body["messages"]);
        // Non-object bodies fall back to raw at the call site.
        assert!(probe_body(&serde_json::json!("nope")).is_none());
    }
}
