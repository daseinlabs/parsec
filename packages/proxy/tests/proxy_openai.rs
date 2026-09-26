//! Integration tests for the `/openai/*` Responses passthrough (openai.rs,
//! Codex Milestone A): a mock OpenAI upstream records what the proxy
//! actually sends, so the wire invariants — verbatim body relay, header
//! forwarding (auth in, x-parsec-tool never out), WebSocket deflection,
//! SSE byte-identity, ledger attribution with a locally-measured
//! counterfactual — are asserted end to end.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use parsec_proxy::brain::{BrainConfig, BrainContract};
use parsec_proxy::server::{router, AppState};

// A Responses SSE stream: usage rides ONLY the response.completed event.
const SSE_BODY: &[u8] = b"event: response.created\n\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\
\n\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\
\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":100,\"input_tokens_details\":{\"cached_tokens\":40},\"output_tokens\":7,\"total_tokens\":107}}}\n\
\n";

#[derive(Clone)]
struct Recorded {
    path: String,
    headers: Vec<(String, String)>,
    raw: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Default)]
struct MockState {
    reqs: Arc<Mutex<Vec<Recorded>>>,
}

impl MockState {
    fn record(&self, path: &str, headers: &HeaderMap, raw: &[u8]) {
        self.reqs.lock().unwrap().push(Recorded {
            path: path.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect(),
            raw: raw.to_vec(),
        });
    }
    fn all(&self) -> Vec<Recorded> {
        self.reqs.lock().unwrap().clone()
    }
}

async fn mock_responses(State(m): State<MockState>, headers: HeaderMap, raw: Bytes) -> Response {
    respond_model_call(m, "/v1/responses", headers, raw)
}

/// The ChatGPT backend shape: codex appends `responses` directly under the
/// base (`…/backend-api/codex/responses`), no `/v1`.
async fn mock_responses_chatgpt(
    State(m): State<MockState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    respond_model_call(m, "/responses", headers, raw)
}

fn respond_model_call(m: MockState, path: &str, headers: HeaderMap, raw: Bytes) -> Response {
    m.record(path, &headers, &raw);
    let v: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    if v.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(SSE_BODY))
            .unwrap();
    }
    let resp = json!({
        "id": "resp_mock", "object": "response", "status": "completed",
        "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}]}],
        "usage": {
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 40},
            "output_tokens": 7, "total_tokens": 107
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(resp.to_string()))
        .unwrap()
}

async fn mock_models(State(m): State<MockState>, headers: HeaderMap) -> Response {
    m.record("/v1/models", &headers, b"");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"object": "list", "data": []}).to_string(),
        ))
        .unwrap()
}

async fn mock_compact(State(m): State<MockState>, headers: HeaderMap, raw: Bytes) -> Response {
    m.record("/v1/responses/compact", &headers, &raw);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"ok": true}).to_string()))
        .unwrap()
}

// ── mock brain (canned grid scores, as in proxy_brain.rs) ───────────────────

/// Grid tau the mock brain hands out (the real ckpt's 0.70-cov tau).
const TAU_Q: i64 = 315_265;

#[derive(Clone, Default)]
struct MockBrain {
    trace_reqs: Arc<Mutex<Vec<Value>>>,
    fail: Arc<AtomicBool>,
}

/// Mask rows score LOW (cut), everything else 1.0 — the freezer's own
/// budget-cut machinery decides what actually drops.
async fn brain_trace(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.trace_reqs.lock().unwrap().push(req.clone());
    if b.fail.load(Ordering::SeqCst) {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("brain boom"))
            .unwrap();
    }
    let n_live = req["live_gi"].as_array().unwrap().len();
    let mask: Vec<usize> = req["mask"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    let scores: Vec<i64> = (0..n_live)
        .map(|j| {
            if mask.contains(&j) {
                100_000
            } else {
                1_000_000
            }
        })
        .collect();
    let resp = json!({
        "scores_q": scores, "tau_q": TAU_Q,
        "checkpoint_id": "c".repeat(64),
        "timings_ms": {"embed": 1.0, "forward": 1.0}
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(resp.to_string()))
        .unwrap()
}

// ── harness ─────────────────────────────────────────────────────────────────

struct Ctx {
    url: String,
    mock: MockState,
    brain: MockBrain,
    ledger: PathBuf,
    http: reqwest::Client,
}

async fn setup_full(entitled: bool, with_brain: bool) -> Ctx {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mock = MockState::default();
    let mock_router = Router::new()
        .route("/v1/responses", post(mock_responses))
        .route("/v1/responses/compact", post(mock_compact))
        .route("/v1/models", get(mock_models))
        .route("/responses", post(mock_responses_chatgpt))
        .with_state(mock.clone());
    let ml = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = ml.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ml, mock_router).await.unwrap() });

    let brain = MockBrain::default();
    let brain_router = Router::new()
        .route("/v1/score/trace", post(brain_trace))
        .with_state(brain.clone());
    let bl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let brain_addr = bl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(bl, brain_router).await.unwrap() });

    let ledger = std::env::temp_dir().join(format!(
        "parsec-openai-test-{}-{}.jsonl",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&ledger);
    let bcfg = with_brain.then(|| BrainConfig {
        url: format!("http://{brain_addr}"),
        key: None,
        timeout: Duration::from_secs(5),
        target_cov: "0.70".into(),
        tool_cut: 0.70,
        tool_prune: false,
        tool_stub: false,
        contract: BrainContract::Dev,
        score_memo_dir: None,
    });
    // Anthropic upstream is irrelevant here; only the OpenAI one is hit.
    // These suites assert a cut on the very request that births the
    // observation — the reference cut-at-birth policy. The product default
    // (FreezeConfig::protect_current) serves the current turn in full and
    // is covered end-to-end by tests/golden_conversation.rs.
    std::env::set_var("PARSEC_PROTECT_CURRENT", "off");
    let mut st = AppState::with_brain("http://127.0.0.1:9".to_string(), ledger.clone(), bcfg);
    st.openai_upstream = format!("http://{mock_addr}");
    st.chatgpt_upstream = format!("http://{mock_addr}");
    st.entitled = entitled;
    let state = Arc::new(st);
    let pl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = pl.local_addr().unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(pl, app).await.unwrap() });

    Ctx {
        url: format!("http://{proxy_addr}"),
        mock,
        brain,
        ledger,
        http: reqwest::Client::new(),
    }
}

async fn setup_entitled(entitled: bool) -> Ctx {
    setup_full(entitled, false).await
}

async fn setup() -> Ctx {
    setup_entitled(true).await
}

fn responses_body(stream: bool) -> Value {
    json!({
        "model": "gpt-5.6-codex",
        "instructions": "be terse",
        "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "hello"}]}],
        "prompt_cache_key": "sess-abc",
        "store": false,
        "stream": stream,
        "include": ["reasoning.encrypted_content"]
    })
}

fn ledger_rows(ctx: &Ctx) -> Vec<Value> {
    std::fs::read_to_string(&ctx.ledger)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

async fn wait_rows(ctx: &Ctx, n: usize) -> Vec<Value> {
    // Streaming rows land at stream end from the finalizer — poll briefly.
    for _ in 0..50 {
        let rows = ledger_rows(ctx);
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    ledger_rows(ctx)
}

// ── tests ───────────────────────────────────────────────────────────────────

/// The body must cross VERBATIM (passthrough = zero rewrite), auth and
/// provider headers must reach upstream, and the parsec attribution tag must
/// NOT — it terminates at the ledger.
#[tokio::test]
async fn responses_body_and_headers_relay_verbatim_tag_stays_local() {
    let ctx = setup().await;
    let sent = responses_body(false);
    let resp = ctx
        .http
        .post(format!("{}/openai/v1/responses", ctx.url))
        .header("authorization", "Bearer sk-test")
        .header("openai-beta", "responses=experimental")
        .header("x-parsec-tool", "codex")
        .json(&sent)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "resp_mock");

    let reqs = ctx.mock.all();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.path, "/v1/responses");
    let forwarded: Value = serde_json::from_slice(&r.raw).unwrap();
    assert_eq!(forwarded, sent, "passthrough must forward byte-verbatim");
    assert_eq!(r.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(r.header("openai-beta"), Some("responses=experimental"));
    assert_eq!(
        r.header("x-parsec-tool"),
        None,
        "attribution tag must never reach OpenAI"
    );

    // Ledger: tool-tagged, null counterfactual (no OpenAI count_tokens —
    // §8.4 forbids estimating), billed split re-sums to OpenAI's input.
    let rows = wait_rows(&ctx, 1).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["tool"], "codex");
    assert_eq!(row["model"], "gpt-5.6-codex");
    // Unentitled/uncurated on this wire still MEASURES: the counterfactual is
    // the local o200k_base count, which equals the billed input side when
    // nothing was cut. It used to be null forever — OpenAI ships no
    // count_tokens endpoint (counterfact.rs).
    assert_eq!(row["counterfactual_source"], "local_bpe");
    assert_eq!(row["counterfactual_input_tokens"], 100);
    assert_eq!(row["billed_input_tokens"], 60);
    assert_eq!(row["billed_cache_read_tokens"], 40);
    assert_eq!(row["billed_cache_write_tokens"], 0);
    assert_eq!(row["billed_output_tokens"], 7);
    assert_eq!(row["fail_open"], false);
    assert_eq!(row["conv_id"].as_str().unwrap().len(), 24);
}

/// SSE relays byte-identically; the row lands at stream end from the
/// response.completed usage.
#[tokio::test]
async fn sse_stream_relays_byte_identical_and_writes_row_at_end() {
    let ctx = setup().await;
    let resp = ctx
        .http
        .post(format!("{}/openai/v1/responses", ctx.url))
        .header("authorization", "Bearer sk-test")
        .header("x-parsec-tool", "codex")
        .json(&responses_body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let relayed = resp.bytes().await.unwrap();
    assert_eq!(&relayed[..], SSE_BODY, "SSE must relay byte-identical");

    let rows = wait_rows(&ctx, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["billed_input_tokens"], 60);
    assert_eq!(rows[0]["billed_cache_read_tokens"], 40);
    assert_eq!(rows[0]["billed_output_tokens"], 7);
    assert_eq!(rows[0]["tool"], "codex");
}

/// Sibling endpoints under the same base_url relay untouched and write NO
/// rows — they are not model turns.
#[tokio::test]
async fn sibling_endpoints_relay_without_rows() {
    let ctx = setup().await;
    let models = ctx
        .http
        .get(format!("{}/openai/v1/models", ctx.url))
        .header("authorization", "Bearer sk-test")
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    let compact = ctx
        .http
        .post(format!("{}/openai/v1/responses/compact", ctx.url))
        .header("authorization", "Bearer sk-test")
        .json(&json!({"whatever": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(compact.status(), 200);

    let paths: Vec<String> = ctx.mock.all().iter().map(|r| r.path.clone()).collect();
    assert_eq!(paths, vec!["/v1/models", "/v1/responses/compact"]);
    assert_eq!(
        ctx.mock.all()[0].header("authorization"),
        Some("Bearer sk-test")
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(ledger_rows(&ctx).is_empty(), "no rows for non-model calls");
}

/// A WebSocket Upgrade attempt is answered 426 locally (never forwarded) so
/// the client falls back to HTTP SSE.
#[tokio::test]
async fn websocket_upgrade_rejected_locally() {
    let ctx = setup().await;
    let resp = ctx
        .http
        .get(format!("{}/openai/v1/responses", ctx.url))
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    assert!(ctx.mock.all().is_empty(), "upgrade must not reach upstream");
}

/// Unentitled: traffic still relays (never wedge the user's session) but the
/// proxy is INERT — no ledger row, exactly like the Anthropic path.
#[tokio::test]
async fn unentitled_relays_without_rows() {
    let ctx = setup_entitled(false).await;
    let resp = ctx
        .http
        .post(format!("{}/openai/v1/responses", ctx.url))
        .header("authorization", "Bearer sk-test")
        .json(&responses_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(ctx.mock.all().len(), 1, "still forwarded");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(ledger_rows(&ctx).is_empty());
}

// ── Milestone B: curation through the Freezer ───────────────────────────────

fn file_lines(n: usize) -> String {
    (1..=n)
        .map(|i| format!("{i}: let value_{i} = compute_step_{i}(input_{i});"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A Codex-shaped turn: task, reasoning, shell function_call, long output.
fn codex_turn1() -> Vec<Value> {
    vec![
        json!({"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "Fix the failing test in parser.py"}]}),
        json!({"type": "reasoning", "id": "rs_1",
               "encrypted_content": "gAAAAABencrypted", "summary": []}),
        json!({"type": "function_call", "name": "shell", "call_id": "call_1",
               "arguments": "{\"command\":[\"bash\",\"-lc\",\"cat src/parser.py\"]}"}),
        json!({"type": "function_call_output", "call_id": "call_1",
               "output": file_lines(40)}),
    ]
}

fn brain_body(input: Vec<Value>) -> Value {
    json!({
        "model": "gpt-5.6-codex",
        "instructions": "You are Codex. Be terse.",
        "input": input,
        "prompt_cache_key": "sess-brain",
        "store": false,
        "include": ["reasoning.encrypted_content"]
    })
}

async fn post_responses(ctx: &Ctx, body: &Value) -> reqwest::Response {
    ctx.http
        .post(format!("{}/openai/v1/responses", ctx.url))
        .header("authorization", "Bearer sk-test")
        .header("x-parsec-tool", "codex")
        .json(body)
        .send()
        .await
        .unwrap()
}

/// Brain scores trim the Codex tool output — while reasoning
/// (encrypted_content), the function_call pairing, and every top-level field
/// (prompt_cache_key above all) ride through byte-identical.
#[tokio::test]
async fn brain_scores_trim_codex_tool_output() {
    let ctx = setup_full(true, true).await;
    let sent = brain_body(codex_turn1());
    let resp = post_responses(&ctx, &sent).await;
    assert_eq!(resp.status(), 200);

    let treqs = ctx.brain.trace_reqs.lock().unwrap().clone();
    assert!(!treqs.is_empty(), "no brain trace call");
    assert_eq!(treqs[0]["contract"], "brain-api-dev/v0");

    let reqs = ctx.mock.all();
    assert_eq!(reqs.len(), 1);
    let fwd: Value = serde_json::from_slice(&reqs[0].raw).unwrap();
    // The observation was digested (low scores → cut)…
    let obs = serde_json::to_string(&fwd["input"][3]).unwrap();
    assert!(obs.contains(" omitted"), "observation not trimmed: {obs}");
    assert!(
        obs.len() < serde_json::to_string(&sent["input"][3]).unwrap().len(),
        "trimmed output should be smaller"
    );
    assert_eq!(fwd["input"][3]["call_id"], "call_1", "tool pairing intact");
    // …and the hazard surfaces are byte-identical.
    assert_eq!(fwd["input"][1], sent["input"][1], "reasoning item changed");
    assert_eq!(fwd["input"][2], sent["input"][2], "function_call changed");
    assert_eq!(fwd["prompt_cache_key"], "sess-brain");
    assert_eq!(fwd["include"], sent["include"]);
    assert_eq!(fwd["instructions"], sent["instructions"]);
    assert_eq!(fwd["input"][0], sent["input"][0], "task message changed");

    let rows = wait_rows(&ctx, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["fail_open"], false);
    assert_eq!(rows[0]["tool"], "codex");
    assert_eq!(rows[0]["checkpoint_id"], "c".repeat(64));
    assert!(rows[0]["freeze_cut_tokens"].as_i64().unwrap() > 0);
    assert!(rows[0].get("scorer_fail_opens").is_none());

    // §8.4 hole closed: OpenAI has no count_tokens endpoint, so the row's
    // counterfactual is the local o200k_base count — labeled as such, and
    // anchored to the billed input side so savings = the measured delta.
    assert_eq!(rows[0]["counterfactual_source"], "local_bpe");
    let cf = rows[0]["counterfactual_input_tokens"].as_i64().unwrap();
    let billed = rows[0]["billed_input_tokens"].as_i64().unwrap()
        + rows[0]["billed_cache_read_tokens"].as_i64().unwrap()
        + rows[0]["billed_cache_write_tokens"].as_i64().unwrap();
    assert!(
        cf > billed,
        "a request that cut content must measure above what it billed: {cf} vs {billed}"
    );
    // Role-aware accounting: the cut came out of tool output, never a user turn.
    let roles = rows[0]["freeze_cut_roles"].as_object().unwrap();
    assert!(
        roles.get("tool").is_some(),
        "tool output should show as cut: {roles:?}"
    );
    assert!(
        roles.get("user").is_none(),
        "no user turn may be cut: {roles:?}"
    );
}

/// A passthrough (no brain) cut nothing, so it measures exactly zero saved —
/// a measured zero, not the null a missing endpoint used to force.
#[tokio::test]
async fn passthrough_openai_row_measures_zero_saved_not_null() {
    let ctx = setup_full(true, false).await;
    let sent = brain_body(codex_turn1());
    assert_eq!(post_responses(&ctx, &sent).await.status(), 200);
    let rows = wait_rows(&ctx, 1).await;
    assert_eq!(rows[0]["counterfactual_source"], "local_bpe");
    let cf = rows[0]["counterfactual_input_tokens"].as_i64().unwrap();
    let billed = rows[0]["billed_input_tokens"].as_i64().unwrap()
        + rows[0]["billed_cache_read_tokens"].as_i64().unwrap()
        + rows[0]["billed_cache_write_tokens"].as_i64().unwrap();
    assert_eq!(cf, billed, "nothing was cut, so nothing was saved");
}

/// Resident turns replay byte-identically across calls — the fold map is
/// what stands in for cache_control on this wire: OpenAI's automatic prefix
/// cache only hits if the served prefix bytes never move.
#[tokio::test]
async fn resident_codex_turns_stay_byte_identical_across_turns() {
    let ctx = setup_full(true, true).await;
    post_responses(&ctx, &brain_body(codex_turn1())).await;

    let mut input = codex_turn1();
    input.push(json!({"type": "message", "role": "assistant",
                      "content": [{"type": "output_text", "text": "The bug is on line 12."}]}));
    input.push(json!({"type": "message", "role": "user",
                      "content": [{"type": "input_text", "text": "apply the fix"}]}));
    post_responses(&ctx, &brain_body(input)).await;

    let reqs = ctx.mock.all();
    assert_eq!(reqs.len(), 2);
    let a: Value = serde_json::from_slice(&reqs[0].raw).unwrap();
    let b: Value = serde_json::from_slice(&reqs[1].raw).unwrap();
    assert_eq!(a["instructions"], b["instructions"]);
    for j in 0..4 {
        assert_eq!(
            a["input"][j], b["input"][j],
            "resident turn {j} changed bytes across calls"
        );
    }
}

/// Brain down: per-step fail-open inside the freezer — the request succeeds,
/// the observation serves FULL, and the row records the scorer fail-open
/// (not a request-level one).
#[tokio::test]
async fn brain_down_fails_open_and_serves_full() {
    let ctx = setup_full(true, true).await;
    ctx.brain.fail.store(true, Ordering::SeqCst);
    let resp = post_responses(&ctx, &brain_body(codex_turn1())).await;
    assert_eq!(resp.status(), 200);

    let reqs = ctx.mock.all();
    let fwd: Value = serde_json::from_slice(&reqs[0].raw).unwrap();
    let obs = serde_json::to_string(&fwd["input"][3]).unwrap();
    assert!(!obs.contains(" omitted"), "trimmed despite brain down");

    let rows = wait_rows(&ctx, 1).await;
    assert_eq!(
        rows[0]["fail_open"], false,
        "request-level fail-open is wrong here"
    );
    assert_eq!(rows[0]["scorer_fail_opens"], 1);
}

/// `input` as a plain string is legal wire we do not curate: verbatim
/// passthrough, no fail-open counted.
#[tokio::test]
async fn string_input_relays_verbatim_without_fail_open() {
    let ctx = setup_full(true, true).await;
    let sent = json!({"model": "gpt-5.6-codex", "input": "hello", "stream": false});
    let resp = post_responses(&ctx, &sent).await;
    assert_eq!(resp.status(), 200);
    let fwd: Value = serde_json::from_slice(&ctx.mock.all()[0].raw).unwrap();
    assert_eq!(fwd, sent);
    let rows = wait_rows(&ctx, 1).await;
    assert_eq!(rows[0]["fail_open"], false);
}

// ── ChatGPT-subscription mode (/chatgpt/*) ──────────────────────────────────

/// Subscription traffic reaches the ChatGPT backend shape (`/responses`, no
/// /v1), the OAuth surface (Bearer + chatgpt-account-id) crosses intact, and
/// rows attribute tool=codex from the route — no header tag exists in this
/// mode.
#[tokio::test]
async fn chatgpt_route_relays_oauth_surface_and_infers_attribution() {
    let ctx = setup().await;
    let sent = responses_body(false);
    let resp = ctx
        .http
        .post(format!("{}/chatgpt/responses", ctx.url))
        .header("authorization", "Bearer eyJ-oauth-token")
        .header("chatgpt-account-id", "acct_123")
        .json(&sent)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let reqs = ctx.mock.all();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.path, "/responses", "must hit the ChatGPT backend shape");
    assert_eq!(r.header("authorization"), Some("Bearer eyJ-oauth-token"));
    assert_eq!(r.header("chatgpt-account-id"), Some("acct_123"));
    let forwarded: Value = serde_json::from_slice(&r.raw).unwrap();
    assert_eq!(forwarded, sent);

    let rows = wait_rows(&ctx, 1).await;
    assert_eq!(rows[0]["tool"], "codex", "attribution inferred from route");
    assert_eq!(rows[0]["counterfactual_source"], "local_bpe");
    assert_eq!(rows[0]["counterfactual_input_tokens"], 100);
    assert_eq!(rows[0]["billed_input_tokens"], 60);
}

/// Curation is namespace-agnostic: the same Freezer trims subscription
/// traffic, and the hazard surfaces stay byte-identical.
#[tokio::test]
async fn chatgpt_route_curates_identically() {
    let ctx = setup_full(true, true).await;
    let sent = brain_body(codex_turn1());
    let resp = ctx
        .http
        .post(format!("{}/chatgpt/responses", ctx.url))
        .header("authorization", "Bearer eyJ-oauth-token")
        .json(&sent)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let reqs = ctx.mock.all();
    let fwd: Value = serde_json::from_slice(&reqs[0].raw).unwrap();
    let obs = serde_json::to_string(&fwd["input"][3]).unwrap();
    assert!(obs.contains(" omitted"), "observation not trimmed: {obs}");
    assert_eq!(fwd["input"][1], sent["input"][1], "reasoning item changed");
    assert_eq!(fwd["prompt_cache_key"], "sess-brain");

    let rows = wait_rows(&ctx, 1).await;
    assert_eq!(rows[0]["tool"], "codex");
    assert!(rows[0]["freeze_cut_tokens"].as_i64().unwrap() > 0);
}

/// Codex sends zstd-compressed request bodies: the proxy must decode them
/// for curation and forward identity JSON with the stale content-encoding
/// header dropped — while an undecodable body still relays verbatim.
#[tokio::test]
async fn zstd_body_is_decoded_curated_and_forwarded_identity() {
    let ctx = setup_full(true, true).await;
    let sent = brain_body(codex_turn1());
    let compressed = zstd::stream::encode_all(sent.to_string().as_bytes(), 3).unwrap();
    let resp = ctx
        .http
        .post(format!("{}/chatgpt/responses", ctx.url))
        .header("authorization", "Bearer eyJ-oauth-token")
        .header("content-encoding", "zstd")
        .header("content-type", "application/json")
        .body(compressed)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let reqs = ctx.mock.all();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(
        r.header("content-encoding"),
        None,
        "stale zstd header must not describe identity bytes"
    );
    let fwd: Value = serde_json::from_slice(&r.raw).expect("forwarded body must be plain JSON");
    // Curation ran on the decoded body: observation trimmed, hazards intact.
    let obs = serde_json::to_string(&fwd["input"][3]).unwrap();
    assert!(obs.contains(" omitted"), "observation not trimmed: {obs}");
    assert_eq!(fwd["input"][1], sent["input"][1], "reasoning item changed");
    assert_eq!(fwd["prompt_cache_key"], "sess-brain");

    let rows = wait_rows(&ctx, 1).await;
    assert_eq!(rows[0]["fail_open"], false);
    assert!(rows[0]["freeze_cut_tokens"].as_i64().unwrap() > 0);
}

/// The built-in provider tries Responses-over-WebSocket first in
/// subscription mode — the deflection must hold on this namespace too.
#[tokio::test]
async fn chatgpt_route_rejects_websocket_upgrade() {
    let ctx = setup().await;
    let resp = ctx
        .http
        .get(format!("{}/chatgpt/responses", ctx.url))
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    assert!(ctx.mock.all().is_empty(), "upgrade must not reach upstream");
}

/// Anthropic routes are untouched by the new wire: an unknown path outside
/// both namespaces still 404s.
#[tokio::test]
async fn unknown_paths_still_404() {
    let ctx = setup().await;
    let resp = ctx
        .http
        .post(format!("{}/v1/responses", ctx.url)) // no /openai prefix
        .json(&responses_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(ctx.mock.all().is_empty());
}
