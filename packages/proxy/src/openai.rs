//! `/openai/*` and `/chatgpt/*` — the OpenAI Responses wire (Codex CLI;
//! docs/codex-integration.md). Two routing namespaces, one wire:
//!
//! - `/openai/*` → `api.openai.com` — BYOK custom provider
//!   (`codex --profile parsec` in `--byok` mode, `env_key = OPENAI_API_KEY`).
//! - `/chatgpt/*` → `chatgpt.com/backend-api/codex` — ChatGPT-subscription
//!   mode (the default install): the profile's `openai_base_url` points the
//!   BUILT-IN provider here, and the user's OAuth token + chatgpt-account-id
//!   header make one localhost hop before the ChatGPT backend, exactly the
//!   Claude Code subscription posture (§3).
//!
//! Model calls are curated through the Freezer (see `curate_responses`);
//! everything else relays verbatim. What the route layer owns:
//!
//! - **WebSocket deflection**: an Upgrade request is answered 426 locally so
//!   the client falls back to HTTP SSE (custom providers default to HTTP, but
//!   the `openai_base_url` override path would try Responses-over-WebSocket).
//! - **Ledger attribution**: POST `/openai/v1/responses` writes a savings-
//!   ledger row from the ACTUALLY-BILLED usage (JSON body or the SSE
//!   `response.completed` event), tagged by the charset-gated `x-parsec-tool`
//!   header the installer bakes into the provider table. OpenAI has no
//!   count_tokens endpoint, so `counterfactual_input_tokens` is null — §8.4
//!   forbids estimating, and a passthrough claims no savings anyway. Sibling
//!   endpoints (`responses/compact`, `models`, `search`, `session`, …) relay
//!   without rows: they are not model turns.
//! - **No memo touch**: nothing here reads or writes `AppState::convs` — the
//!   Anthropic curation state cannot be polluted by Codex traffic.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use parsec_engine::freeze::{FreezeConfig, Freezer};
use parsec_engine::pystr::py_json_dumps;

use crate::brain::BrainScorer;
use crate::server::{AppState, InFlight, PlanStats, RowCtx};

/// Same bound as the supervisor's MAX_BODY: never reject a real conversation,
/// still bound a hostile local caller (the bind is 127.0.0.1-only regardless).
const MAX_BODY: usize = 256 * 1024 * 1024;

/// The OpenAI platform upstream (`/openai/*`, BYOK API-key traffic).
/// Env-overridable for tests.
pub fn upstream_from_env() -> String {
    std::env::var("PARSEC_OPENAI_UPSTREAM")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://api.openai.com".to_string())
}

/// The ChatGPT-subscription upstream (`/chatgpt/*`): where Codex's built-in
/// provider sends model traffic under ChatGPT auth. The user's OAuth token
/// rides Bearer + `chatgpt-account-id` through this LOCAL hop only — §3's
/// "subscription tokens never through our cloud" is satisfied exactly as on
/// the Claude Code path. Env-overridable for tests.
pub fn chatgpt_upstream_from_env() -> String {
    std::env::var("PARSEC_CHATGPT_UPSTREAM")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".to_string())
}

/// Which wire namespace a routed path belongs to, and how to reach its
/// upstream: `(prefix-stripped path, upstream base, model-call path)`. The
/// two prefixes carry the SAME Responses wire — only the upstream host and
/// the path Codex appends `responses` under differ (`{base}/v1/responses`
/// on the platform API, `{base}/responses` on the ChatGPT backend).
fn split_wire<'a>(path_q: &'a str, st: &'a AppState) -> (&'a str, &'a str, &'static str) {
    if let Some(p) = path_q.strip_prefix("/chatgpt") {
        (p, st.chatgpt_upstream.as_str(), "/responses")
    } else {
        (
            path_q.strip_prefix("/openai").unwrap_or(path_q),
            st.openai_upstream.as_str(),
            "/v1/responses",
        )
    }
}

/// Hop-by-hop / transport headers that must not be forwarded on a proxied
/// request. `accept-encoding` is stripped so the upstream answers identity —
/// the usage scan (and, later, the Responses adapter) must be able to read
/// the wire; the client asked *us* in plaintext, so it gets plaintext back.
const HOP_BY_HOP: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "expect",
    "accept-encoding",
];

/// Header forwarding for the OpenAI path. Unlike the Anthropic allowlist
/// (auth surface only), the Responses passthrough forwards the request
/// headers VERBATIM minus hop-by-hop — Codex sends provider-specific headers
/// (`openai-beta`, `openai-organization`, session ids, the installer's
/// `http_headers` map) that a narrow allowlist would silently break.
/// `to_upstream` strips the parsec-internal `x-parsec-*` surface: the
/// attribution tag terminates at the worker's ledger and never reaches
/// OpenAI (same guarantee `forward_auth_headers` gives on the Anthropic
/// path). The supervisor→worker hop passes `to_upstream = false` so the tag
/// survives to the ledger.
pub(crate) fn forward_headers(inbound: &HeaderMap, to_upstream: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) {
            continue;
        }
        if to_upstream && n.starts_with("x-parsec") {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Conversation identity for the ledger row: sha256 of Codex's per-session
/// `prompt_cache_key`, hex[..24] — an id derived from an id, so the row can
/// never carry raw text through this field. Absent key ⇒ empty (the row
/// still lands, ungrouped).
fn conv_id_from(body: Option<&Value>) -> String {
    let Some(pck) = body
        .and_then(|b| b.get("prompt_cache_key"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    else {
        return String::new();
    };
    let mut h = Sha256::new();
    h.update(b"openai-responses:");
    h.update(pck.trim().as_bytes());
    format!("{:x}", h.finalize())[..24].to_string()
}

/// OpenAI usage → savings-ledger billed fields. OpenAI's `input_tokens`
/// INCLUDES the cached prefix (Anthropic's excludes it), so the cached share
/// is split out: billed_input + billed_cache_read sums exactly to what
/// OpenAI billed as input — no token is counted twice and none invented.
/// Prefix-cache writes are not billed separately on OpenAI ⇒ 0.
fn ledger_usage(u: &Value) -> Value {
    let input = u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
    let cached = u
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(0, input.max(0));
    let out = u.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
    json!({
        "input_tokens": input - cached,
        "cache_read_input_tokens": cached,
        "cache_creation_input_tokens": 0,
        "output_tokens": out,
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Decode an encoded request body for parsing/curation. Codex 0.147.0 sends
/// `content-encoding: zstd` on its Responses bodies (observed live; magic
/// 28 b5 2f fd); gzip/deflate covered for other clients. None = unknown
/// encoding or corrupt payload — the caller fails open to verbatim
/// forwarding of the ORIGINAL bytes, so a new upstream encoding can never
/// break traffic, only curation.
fn decode_body(raw: &[u8], encoding: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    match encoding {
        "" | "identity" => Some(raw.to_vec()),
        "zstd" => zstd::stream::decode_all(raw).ok(),
        "gzip" => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw)
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        "deflate" => {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(raw)
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        _ => None,
    }
}

/// Conversation-memo key: the ledger conv id (prompt_cache_key hash) when
/// Codex sent one, else a hash of the internal head — the same
/// task-head-stability idea as the Anthropic `conversation_id`.
fn conv_key(body: &Value, internal: &[Value]) -> String {
    let pck = conv_id_from(Some(body));
    if !pck.is_empty() {
        return pck;
    }
    let head: Vec<Value> = internal
        .iter()
        .take(2)
        .map(|m| m.get("content").cloned().unwrap_or(Value::Null))
        .collect();
    let mut h = Sha256::new();
    h.update(b"openai-responses:");
    h.update(py_json_dumps(&Value::Array(head)).as_bytes());
    format!("{:x}", h.finalize())[..24].to_string()
}

/// The curation plan for one Responses model call (Milestone B): curated
/// bytes to send instead of the original, plus the ledger telemetry.
struct RespPlan {
    conv_id: String,
    out_bytes: Vec<u8>,
    stats: PlanStats,
}

/// Curate a Responses body through the SAME deterministic Freezer as the
/// Anthropic path (the proxy is the determinism boundary; the brain only
/// scores). Mirrors `server::curate` minus the Anthropic-only stages —
/// no cache_control breakpoints (OpenAI's prefix cache is automatic: the
/// fold map keeps served bytes stable instead), no tool pruning, no
/// governor (follow-ups, docs/codex-integration.md).
///
/// `Ok(None)` = legal wire we do not curate (e.g. `input` as a plain
/// string) — verbatim passthrough, NOT a fail-open. `Err` = fail-open.
async fn curate_responses(st: &Arc<AppState>, body: &Value) -> anyhow::Result<Option<RespPlan>> {
    let t_curate = std::time::Instant::now();
    if body.get("input").and_then(Value::as_array).is_none() {
        return Ok(None);
    }
    let internal = crate::responses::to_internal(body);
    // Human-authored entries the curator may score but must never rewrite
    // (protect.rs). On this wire that is EVERY `role: "user"` item: tool
    // output arrives as `function_call_output`, so nothing else is.
    let protected = crate::responses::protected_mask(body);
    let conv_id = conv_key(body, &internal);
    let mut stats = PlanStats {
        turn: internal
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .count() as i64,
        ..PlanStats::default()
    };
    lock(&st.convs).entry(conv_id.clone()).or_default().touched = std::time::Instant::now();

    // New-run reset (sessions.py semantics, as on the Anthropic path): no
    // assistant entry means turn 1 of a fresh run — stale memos from an
    // earlier run with the same key must not leak in.
    let has_assistant = internal
        .iter()
        .any(|m| m.get("role").and_then(Value::as_str) == Some("assistant"));
    if !has_assistant && lock(&st.convs).remove(&conv_id).is_some() {
        tracing::debug!(
            conv = %conv_id,
            "openai: no assistant turn — fresh run, stale memo reset"
        );
    }

    // Freezer path — taken from the memo, driven on a blocking thread
    // (BrainScorer is a blocking HTTP client), put back after. No brain ⇒
    // passthrough curation: internal text unchanged, the wire freeze (fold
    // map) still records and replays served bytes.
    let curated_internal: Vec<Value> = if let Some(bcfg) = &st.brain {
        let protect_current = st.protect_current;
        let taken = lock(&st.convs)
            .entry(conv_id.clone())
            .or_default()
            .freezer
            .take();
        let internal_in = internal.clone();
        let bcfg2 = bcfg.clone();
        // v3: the Freezer computes the HS re-request columns (see server.rs).
        let rereq = bcfg2.contract == crate::brain::BrainContract::V3;
        let conv2 = conv_id.clone();
        let (fz, served, fails_before, calls_before, insists_before) =
            tokio::task::spawn_blocking(move || {
                let mut fz = taken.unwrap_or_else(|| {
                    // Responses/Codex: tool output only. Reasoning is opaque
                    // (`encrypted_content`) and tool calls are textless, so
                    // assistant prose would be half the cuttable surface here
                    // and the plan for the turn lives in it.
                    let cfg = FreezeConfig {
                        cut_assistant: false,
                        protect_current,
                        rereq,
                        ..FreezeConfig::default()
                    };
                    Freezer::new(cfg, BrainScorer::new(bcfg2, conv2))
                });
                fz.scorer.attach_gf = false;
                fz.scorer.stats.last_doom_q = None;
                let fails_before = fz.scorer_fail_opens;
                let calls_before = (fz.scorer.stats.trace_calls, fz.scorer.stats.brain_ms);
                let insists_before = fz.insists;
                let served = fz.serve(&internal_in);
                (fz, served, fails_before, calls_before, insists_before)
            })
            .await
            .map_err(|e| anyhow::anyhow!("freezer task panicked: {e}"))?;
        stats.scorer_fail_opens = fz.scorer_fail_opens - fails_before;
        // Per-request DELTA (same cumulative-copy bug as the Anthropic lane —
        // docs/perf-research-2026-09-02.md §0).
        stats.brain_ms = fz.scorer.stats.brain_ms - calls_before.1;
        stats.checkpoint_id = fz.scorer.stats.checkpoint_id.clone();
        stats.births_scored = fz.scorer.stats.trace_calls - calls_before.0;
        stats.curator_insists = fz.insists.saturating_sub(insists_before);
        lock(&st.convs).entry(conv_id.clone()).or_default().freezer = Some(fz);
        if stats.curator_insists > 0 {
            tracing::info!(
                conv = %conv_id,
                insists = stats.curator_insists,
                "openai curator: insist valve — agent re-asked cut content (over-cut signal)"
            );
        }
        match served {
            Ok(mut c) => {
                stats.freeze_cut_protected_tokens =
                    crate::protect::restore_protected(&internal, &mut c, &protected);
                if stats.freeze_cut_protected_tokens > 0 {
                    tracing::warn!(
                        conv = %conv_id,
                        refused_tokens = stats.freeze_cut_protected_tokens,
                        "openai curator: cut refused on human-authored content — served verbatim"
                    );
                }
                stats.freeze_cut_roles = crate::protect::cut_by_role(&internal, &c);
                stats.freeze_cut_tokens = (crate::server::internal_mass(&internal)
                    - crate::server::internal_mass(&c))
                .max(0);
                c
            }
            Err(e) => anyhow::bail!("freeze rejected internal view: {e}"),
        }
    } else {
        internal.clone()
    };

    // Fold-back onto the original body. Folds commit before the send (they
    // memoize served bytes and replay idempotently — same discipline as the
    // Anthropic path; there are no fingerprints to defer here because this
    // wire places no cache anchors).
    let mut folds = {
        let mut convs = lock(&st.convs);
        convs.entry(conv_id.clone()).or_default().folds.clone()
    };
    let folds_before = folds.len();
    let curated = crate::responses::apply_curation(body, &curated_internal, Some(&mut folds));
    // Same as the Anthropic path: the current turn (everything after the
    // last assistant-authored item) is served full under
    // FreezeConfig::protect_current and re-folds once it is history.
    if st.protect_current {
        crate::splice::unfreeze_current_turn(body.get("input"), &mut folds, |it| {
            it.get("role").and_then(Value::as_str) == Some("assistant")
                || matches!(
                    it.get("type").and_then(Value::as_str),
                    Some("function_call") | Some("reasoning")
                )
        });
    }
    stats.folds_total = folds.len();
    stats.folds_new = folds.len().saturating_sub(folds_before);
    lock(&st.convs).entry(conv_id.clone()).or_default().folds = folds;
    // §8.4 hole-filler: OpenAI publishes no count_tokens endpoint, so the
    // savings for this request are measured directly — the o200k_base token
    // delta between the body that arrived and the one being sent. See
    // counterfact.rs for why this is a measurement and not a baseline.
    stats.counterfactual_local = crate::counterfact::responses_delta(body, &curated);
    stats.curate_ms = t_curate.elapsed().as_secs_f64() * 1000.0;
    tracing::debug!(
        conv = %conv_id,
        turn = stats.turn,
        cut_tokens = stats.freeze_cut_tokens,
        folds_total = stats.folds_total,
        folds_new = stats.folds_new,
        births_scored = stats.births_scored,
        brain_ms = stats.brain_ms,
        "openai curate done"
    );

    Ok(Some(RespPlan {
        conv_id,
        out_bytes: serde_json::to_vec(&curated)?,
        stats,
    }))
}

/// 426 for any Upgrade (WebSocket) attempt: answered locally, never
/// forwarded. Codex treats a failed WS connect as "fall back to HTTP SSE"
/// (session-scoped), which is exactly the transport this passthrough speaks.
pub(crate) fn reject_upgrade() -> Response {
    (
        StatusCode::UPGRADE_REQUIRED,
        "parsec proxy: WebSocket transport is not supported — retry over HTTP (SSE)",
    )
        .into_response()
}

/// Incremental scan of a Responses SSE stream for the authoritative usage:
/// the `response.completed` event carries `response.usage`. Fed chunk-by-
/// chunk so the relay stays unbuffered; nothing else is parsed or altered.
#[derive(Default)]
struct SseScan {
    buf: String,
    usage: Option<Value>,
    /// Distinct event types seen (bounded) — the no-usage diagnostic: when a
    /// stream ends without a row, this names what the backend actually sent.
    types: Vec<String>,
}

impl SseScan {
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
        if let Some(t) = evt.get("type").and_then(Value::as_str) {
            if self.types.len() < 24 && !self.types.iter().any(|s| s == t) {
                self.types.push(t.to_string());
            }
        }
        if evt.get("type").and_then(Value::as_str) == Some("response.completed") {
            if let Some(u) = evt.pointer("/response/usage") {
                if !u.is_null() {
                    self.usage = Some(u.clone());
                }
            }
        }
    }

    fn finish(&mut self) -> Option<Value> {
        let rest = std::mem::take(&mut self.buf);
        self.line(rest.trim());
        self.usage.take()
    }
}

/// Stream-end bookkeeping that survives a client abort (same Drop pattern as
/// the Anthropic path's StreamFinalize): only what the scan actually OBSERVED
/// is written — an aborted stream that never saw `response.completed` writes
/// NO row (§8.4: nothing is fabricated).
struct Finalize {
    st: Arc<AppState>,
    ctx: RowCtx,
    stats: PlanStats,
    scan: Arc<Mutex<SseScan>>,
    done: bool,
}

impl Finalize {
    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        let (usage, types) = {
            let mut s = lock(&self.scan);
            (s.finish(), std::mem::take(&mut s.types))
        };
        match usage {
            Some(u) => crate::server::write_ledger(
                &self.st,
                &self.ctx,
                None,
                Some(&ledger_usage(&u)),
                &self.stats,
            ),
            // A completed model stream with no usage means no ledger row —
            // name what the backend actually sent (event types only, never
            // content) so a shape drift is diagnosable from the log alone.
            None => tracing::info!(
                conv = %&self.ctx.conv_id[..self.ctx.conv_id.len().min(12)],
                event_types = %types.join(","),
                "openai stream ended without response.completed usage — no row written"
            ),
        }
    }
}

impl Drop for Finalize {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The passthrough handler for everything under `/openai/`. Method-agnostic:
/// Codex hits GET (`models`) and POST (`responses`, `responses/compact`,
/// `search`, `session`, …) under one base_url and ALL of it must relay.
pub async fn relay(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let _guard = InFlight::enter(&st);
    let (parts, body) = req.into_parts();
    if parts.headers.contains_key(header::UPGRADE) {
        tracing::debug!("openai: Upgrade request rejected 426 — client falls back to SSE");
        return reject_upgrade();
    }
    let raw = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "request body too large").into_response(),
    };
    let path_q = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let is_chatgpt = path_q.starts_with("/chatgpt");
    let (upstream_path, upstream_base, model_path) = split_wire(&path_q, &st);
    let url = format!("{}{}", upstream_base.trim_end_matches('/'), upstream_path);
    // The model call itself; sibling endpoints (incl. responses/compact)
    // relay without curation or a ledger row.
    let is_model_call =
        parts.method == Method::POST && upstream_path.split('?').next() == Some(model_path);
    let content_encoding = parts
        .headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let body_json: Option<Value> = if is_model_call {
        decode_body(&raw, &content_encoding).and_then(|b| serde_json::from_slice(&b).ok())
    } else {
        None
    };
    tracing::debug!(
        method = %parts.method,
        path = %upstream_path,
        chatgpt = is_chatgpt,
        bytes = raw.len(),
        model_call = is_model_call,
        "inbound OpenAI-wire request"
    );

    // Curation (Milestone B): entitled model calls run through the Freezer;
    // ANY error fails open to forwarding the original bytes verbatim,
    // counted, per §8.3. Unentitled or non-model traffic never curates.
    let mut plan: Option<RespPlan> = None;
    let mut fail_open = false;
    if is_model_call && st.entitled {
        match body_json.as_ref() {
            Some(b) => match curate_responses(&st, b).await {
                Ok(p) => plan = p,
                Err(e) => {
                    crate::server::note_fail_open(&st, &format!("responses curation failed: {e}"));
                    fail_open = true;
                }
            },
            None => {
                // Diagnostic (structure only, never content): a non-JSON model
                // body is almost always an encoded one — log what encoding.
                let enc = parts
                    .headers
                    .get(header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-");
                let magic: String = raw
                    .iter()
                    .take(4)
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                crate::server::note_fail_open(
                    &st,
                    &format!(
                        "inbound responses body is not JSON \
                         (content-encoding={enc}, len={}, magic=[{magic}])",
                        raw.len()
                    ),
                );
                fail_open = true;
            }
        }
    }
    let send_body: Bytes = plan
        .as_ref()
        .map(|p| Bytes::from(p.out_bytes.clone()))
        .unwrap_or_else(|| raw.clone());
    if is_model_call {
        tracing::debug!(
            in_bytes = raw.len(),
            out_bytes = send_body.len(),
            curated = plan.is_some(),
            fail_open,
            "forwarding /v1/responses upstream"
        );
    }
    let stats = plan.as_ref().map(|p| p.stats.clone()).unwrap_or_default();

    let mut fwd_headers = forward_headers(&parts.headers, true);
    if plan.is_some() {
        // Curated bytes are identity JSON — the original body's
        // content-encoding (codex sends zstd) no longer describes them.
        // Fail-open keeps the original bytes AND the original header.
        fwd_headers.remove(header::CONTENT_ENCODING);
    }
    let resp = match st
        .client
        .request(parts.method.clone(), url)
        .headers(fwd_headers)
        .body(send_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return crate::server::bad_gateway(&e),
    };
    let status = resp.status();
    let ct = resp.headers().get(header::CONTENT_TYPE).cloned();
    // SSE detection: the REQUEST's stream flag is authoritative — the
    // ChatGPT backend serves its event stream with NO content-type header
    // (observed live), so header sniffing alone buffers the stream, which
    // both blinds the usage scan and breaks incremental token display.
    let is_sse = body_json
        .as_ref()
        .and_then(|b| b.get("stream"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || ct
            .as_ref()
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
    // We strip accept-encoding upstream precisely so the usage scan can read
    // the wire; a backend that compresses anyway blinds it — say so.
    if is_model_call {
        if let Some(re) = resp.headers().get(header::CONTENT_ENCODING) {
            tracing::info!(
                encoding = ?re,
                "upstream compressed the response despite identity accept-encoding — \
                 usage scan is blind, no ledger row"
            );
        }
    }

    // Attribution rows only for successful, entitled model calls — the
    // unentitled proxy is INERT here exactly as on the Anthropic path.
    if !(is_model_call && st.entitled && status.is_success()) {
        return crate::server::respond(status, ct, Body::from_stream(resp.bytes_stream()));
    }

    let row_ctx = RowCtx {
        conv_id: plan
            .as_ref()
            .map(|p| p.conv_id.clone())
            .unwrap_or_else(|| conv_id_from(body_json.as_ref())),
        session_id: None,
        // The base-URL override (subscription mode) cannot carry the
        // installer's http_headers tag, but the /chatgpt namespace exists
        // only for Codex — attribution is inferred from the route.
        tool: crate::server::tool_from_headers(&parts.headers)
            .or_else(|| is_chatgpt.then(|| "codex".to_string())),
        model: body_json
            .as_ref()
            .and_then(|b| b.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string),
        cache_prefix_sha8: crate::server::sha8_of_fps(std::iter::empty()),
        fail_open,
        queued_ms: 0.0,
        probe_ms: 0.0,
        upstream_ttfb_ms: 0.0,
    };

    tracing::debug!(
        conv = %row_ctx.conv_id,
        status = %status,
        content_type = ?ct,
        sse = is_sse,
        "openai model response — relaying"
    );
    if is_sse {
        let scan = Arc::new(Mutex::new(SseScan::default()));
        let scan_tee = scan.clone();
        let tee = resp.bytes_stream().map(move |r| {
            if let Ok(b) = &r {
                lock(&scan_tee).feed(b);
            }
            r.map_err(io::Error::other)
        });
        // The stream outlives this handler; its own guard keeps the drain
        // accounting honest until the last SSE byte.
        let stream_guard = InFlight::enter(&st);
        let mut fin = Finalize {
            st: st.clone(),
            ctx: row_ctx,
            stats,
            scan,
            done: false,
        };
        let finalize = futures_util::stream::once(async move {
            let _guard = stream_guard;
            fin.finish();
            Ok::<Bytes, io::Error>(Bytes::new())
        });
        return crate::server::respond(status, ct, Body::from_stream(tee.chain(finalize)));
    }

    // Non-streaming: buffer, relay as-is, read .usage from the response body.
    let bytes = resp.bytes().await.unwrap_or_default();
    let parsed = serde_json::from_slice::<Value>(&bytes).ok();
    let usage = parsed.as_ref().and_then(|v| v.get("usage").cloned());
    match &usage {
        Some(u) => crate::server::write_ledger(&st, &row_ctx, None, Some(&ledger_usage(u)), &stats),
        // No row ⇒ say why, structurally (top-level key names are schema,
        // never content) — a shape drift must be diagnosable from the log.
        None => tracing::info!(
            conv = %&row_ctx.conv_id[..row_ctx.conv_id.len().min(12)],
            bytes = bytes.len(),
            json = parsed.is_some(),
            keys = %parsed
                .as_ref()
                .and_then(|v| v.as_object())
                .map(|o| o.keys().take(10).cloned().collect::<Vec<_>>().join(","))
                .unwrap_or_default(),
            "openai buffered model response without usage — no row written"
        ),
    }
    crate::server::respond(status, ct, Body::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn forward_headers_strips_hop_by_hop_and_gates_parsec_tag() {
        let mut h = HeaderMap::new();
        for (k, v) in [
            ("authorization", "Bearer sk-test"),
            ("openai-beta", "responses=experimental"),
            ("content-type", "application/json"),
            ("x-parsec-tool", "codex"),
            ("host", "127.0.0.1:8082"),
            ("content-length", "42"),
            ("connection", "keep-alive"),
            ("upgrade", "websocket"),
            ("accept-encoding", "gzip"),
        ] {
            h.insert(k, HeaderValue::from_static(v));
        }
        // To upstream: auth + provider headers survive, hop-by-hop and the
        // parsec tag do not — the tag must never reach OpenAI.
        let up = forward_headers(&h, true);
        assert_eq!(up.get("authorization").unwrap(), "Bearer sk-test");
        assert_eq!(up.get("openai-beta").unwrap(), "responses=experimental");
        assert_eq!(up.get("content-type").unwrap(), "application/json");
        for gone in [
            "x-parsec-tool",
            "host",
            "content-length",
            "connection",
            "upgrade",
            "accept-encoding",
        ] {
            assert!(up.get(gone).is_none(), "{gone} must be stripped");
        }
        // Supervisor→worker hop: the tag survives to reach the ledger.
        let to_worker = forward_headers(&h, false);
        assert_eq!(to_worker.get("x-parsec-tool").unwrap(), "codex");
        assert!(to_worker.get("host").is_none());
    }

    #[test]
    fn ledger_usage_splits_cached_share_without_double_count() {
        // OpenAI input_tokens INCLUDES cached; the split must re-sum exactly.
        let u = json!({
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 40},
            "output_tokens": 7,
            "total_tokens": 107
        });
        let m = ledger_usage(&u);
        assert_eq!(m["input_tokens"], 60);
        assert_eq!(m["cache_read_input_tokens"], 40);
        assert_eq!(m["cache_creation_input_tokens"], 0);
        assert_eq!(m["output_tokens"], 7);
        // No details object ⇒ everything is uncached input.
        let bare = ledger_usage(&json!({"input_tokens": 10, "output_tokens": 1}));
        assert_eq!(bare["input_tokens"], 10);
        assert_eq!(bare["cache_read_input_tokens"], 0);
        // A cached figure larger than input (malformed upstream) clamps
        // rather than going negative.
        let odd = ledger_usage(&json!({
            "input_tokens": 5,
            "input_tokens_details": {"cached_tokens": 9}
        }));
        assert_eq!(odd["input_tokens"], 0);
        assert_eq!(odd["cache_read_input_tokens"], 5);
    }

    #[test]
    fn conv_id_hashes_prompt_cache_key_never_text() {
        let a = conv_id_from(Some(&json!({"prompt_cache_key": "sess-1"})));
        let b = conv_id_from(Some(&json!({"prompt_cache_key": "sess-2"})));
        assert_eq!(a.len(), 24);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        // Stable across calls (pure function of the key).
        assert_eq!(
            a,
            conv_id_from(Some(&json!({"prompt_cache_key": "sess-1"})))
        );
        // Absent/blank key ⇒ ungrouped, never a fabricated id.
        assert_eq!(conv_id_from(Some(&json!({}))), "");
        assert_eq!(conv_id_from(Some(&json!({"prompt_cache_key": " "}))), "");
        assert_eq!(conv_id_from(None), "");
    }

    #[test]
    fn decode_body_handles_codex_encodings() {
        let body = br#"{"model":"gpt-5.6-codex","input":[]}"#;
        // zstd — what codex 0.147.0 actually sends.
        let z = zstd::stream::encode_all(&body[..], 3).unwrap();
        assert_eq!(decode_body(&z, "zstd").as_deref(), Some(&body[..]));
        // gzip, for other clients.
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(body).unwrap();
        let g = gz.finish().unwrap();
        assert_eq!(decode_body(&g, "gzip").as_deref(), Some(&body[..]));
        // Identity passes through; unknown encodings and corrupt payloads
        // return None (caller fails open to verbatim forwarding).
        assert_eq!(decode_body(body, "").as_deref(), Some(&body[..]));
        assert_eq!(decode_body(body, "br"), None);
        assert_eq!(decode_body(b"\x28\xb5\x2f\xfdgarbage", "zstd"), None);
    }

    #[test]
    fn sse_scan_reads_completed_usage_only() {
        let mut s = SseScan::default();
        s.feed(b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n");
        s.feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n");
        assert!(s.usage.is_none());
        // Split a completed event across chunk boundaries.
        let completed = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":100,\"input_tokens_details\":{\"cached_tokens\":40},\"output_tokens\":7}}}\n\n";
        let (a, b) = completed.split_at(50);
        s.feed(a.as_bytes());
        s.feed(b.as_bytes());
        let u = s.finish().expect("usage from response.completed");
        assert_eq!(u["input_tokens"], 100);
        // An aborted stream that never completed yields nothing.
        let mut empty = SseScan::default();
        empty.feed(b"data: {\"type\":\"response.created\"}\n\n");
        assert!(empty.finish().is_none());
    }
}
