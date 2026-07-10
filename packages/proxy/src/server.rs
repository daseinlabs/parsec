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

use std::collections::HashMap;
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

use dasein_engine::pystr::py_json_dumps;

use crate::splice::{self, FoldMap};

/// Per-conversation memo. Strictly a cache (see module doc): `folds` are the
/// exact bytes already served per turn (splice::FoldMap), `last_fps` the
/// message fingerprints of the last SUCCESSFUL upstream call — the proof of
/// what Anthropic's cache actually holds.
#[derive(Default)]
pub struct ConvState {
    pub folds: FoldMap,
    pub last_fps: Vec<String>,
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
}

impl AppState {
    pub fn new(upstream_base: String, ledger_path: PathBuf) -> Self {
        Self {
            upstream_base,
            client: reqwest::Client::new(),
            convs: Mutex::new(HashMap::new()),
            fail_open_count: AtomicU64::new(0),
            ledger_path,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.upstream_base.trim_end_matches('/'), path)
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
    let state = Arc::new(AppState::new(upstream, ledger));
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
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
/// auth surface — x-api-key, authorization, anthropic-* — verbatim. These
/// values are never logged or stored (§3: user's own auth headers pass
/// through, subscription tokens never touch our cloud).
fn forward_auth_headers(inbound: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        let n = name.as_str();
        if n == "x-api-key" || n == "authorization" || n.starts_with("anthropic-") {
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

// ── curation plan (spec steps b-d) ──────────────────────────────────────────

struct Plan {
    conv_id: String,
    out_bytes: Vec<u8>,
    /// This call's message fingerprints — committed to ConvState only after
    /// upstream 2xx (the anthropic_shapes.py:484 fix).
    cur_fps: Vec<String>,
    /// cachePrefixSha8 bust attribution (§4.3): sha8 over the leading run of
    /// cur_fps proven byte-identical to the prior successful call.
    cache_prefix_sha8: String,
}

/// v0 curation: PASSTHROUGH-shaped, but through the real machinery. The
/// internal view is exactly `to_internal`'s text view ([system] + one entry
/// per message), so apply_curation's `cur_text == content_text(orig)` branch
/// forwards every turn verbatim while recording folds — the wire freeze is
/// live from day one even before a curator cuts anything.
fn curate(st: &AppState, headers: &HeaderMap, body: &Value) -> anyhow::Result<Plan> {
    let msgs = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("body has no messages array"))?;

    let mut internal: Vec<Value> = Vec::new();
    let sys_text = splice::system_to_text(body.get("system"));
    if !sys_text.is_empty() {
        internal.push(json!({ "content": sys_text }));
    }
    for m in msgs {
        internal.push(json!({ "content": splice::content_text(m.get("content")) }));
    }
    let conv_id = conversation_id(headers, &internal);

    let (mut folds, prior_fps) = {
        let mut convs = lock(&st.convs);
        let cs = convs.entry(conv_id.clone()).or_default();
        (cs.folds.clone(), cs.last_fps.clone())
    };
    let curated = splice::apply_curation(body, &internal, Some(&mut folds));
    // Folds MAY commit before the send: they memoize served bytes and replay
    // idempotently — unlike fingerprints, which assert "upstream has cached
    // these bytes" and must wait for the 2xx (see below).
    lock(&st.convs).entry(conv_id.clone()).or_default().folds = folds;

    let prior = if prior_fps.is_empty() {
        None
    } else {
        Some(prior_fps.as_slice())
    };
    let (out, cur_fps) = splice::place_cache_breakpoint(&curated, false, prior);

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
    })
}

// ── §8.4 count_tokens counterfactual ────────────────────────────────────────

/// Free probe: the ORIGINAL inbound body against upstream count_tokens. Any
/// failure returns None — the ledger records null, never an estimate (§8.4).
async fn count_tokens_probe(st: &AppState, headers: &HeaderMap, raw: Bytes) -> Option<i64> {
    let resp = st
        .client
        .post(st.url("/v1/messages/count_tokens"))
        .headers(forward_auth_headers(headers))
        .body(raw)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    v.get("input_tokens").and_then(Value::as_i64)
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
fn write_ledger(
    st: &AppState,
    conv_id: &str,
    counterfactual: Option<i64>,
    usage: Option<&Value>,
    cache_prefix_sha8: &str,
    fail_open: bool,
) {
    let g = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    let row = json!({
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
    let body: Option<Value> = serde_json::from_slice(&raw).ok();

    // (b-d) conversation id, v0 curation, breakpoint placement — any error
    // here means forwarding the ORIGINAL body verbatim (fail-open, counted).
    let plan = match body.as_ref() {
        Some(b) => match curate(&st, &headers, b) {
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

    let conv_id = plan.as_ref().map(|p| p.conv_id.clone()).unwrap_or_default();
    let sha8 = plan
        .as_ref()
        .map(|p| p.cache_prefix_sha8.clone())
        .unwrap_or_else(|| sha8_of_fps(std::iter::empty()));

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
        let finalize = futures_util::stream::once(async move {
            let usage = lock(&scan).finish();
            write_ledger(
                &st2,
                &conv_id,
                counterfactual,
                usage.as_ref(),
                &sha8,
                fail_open,
            );
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
    write_ledger(
        &st,
        &conv_id,
        counterfactual,
        usage.as_ref(),
        &sha8,
        fail_open,
    );
    respond(status, ct, Body::from(bytes))
}
