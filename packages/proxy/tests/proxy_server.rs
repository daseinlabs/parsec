//! Integration tests for `dasein proxy` (server.rs): a mock Anthropic
//! upstream records what the proxy actually sends and serves canned
//! responses, so every wire invariant — auth passthrough, anchor placement,
//! the fps-commit-on-success ordering, ledger honesty, count_tokens
//! statelessness, byte-identical SSE relay — is asserted end to end.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};

use dasein_proxy::server::{router, AppState};

// ── mock upstream ───────────────────────────────────────────────────────────

const SSE_BODY: &[u8] = b"event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_creation_input_tokens\":3,\"cache_read_input_tokens\":90}}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
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
    fn body(&self) -> Value {
        serde_json::from_slice(&self.raw).unwrap()
    }
}

#[derive(Clone, Default)]
struct MockState {
    reqs: Arc<Mutex<Vec<Recorded>>>,
    fail_next: Arc<AtomicBool>,
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
    fn messages(&self) -> Vec<Recorded> {
        self.reqs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/v1/messages")
            .cloned()
            .collect()
    }
    fn count_tokens(&self) -> Vec<Recorded> {
        self.reqs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/v1/messages/count_tokens")
            .cloned()
            .collect()
    }
}

async fn mock_messages(State(m): State<MockState>, headers: HeaderMap, raw: Bytes) -> Response {
    m.record("/v1/messages", &headers, &raw);
    if m.fail_next.swap(false, Ordering::SeqCst) {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("upstream boom"))
            .unwrap();
    }
    let v: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    if v.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(SSE_BODY))
            .unwrap();
    }
    let msg = json!({
        "id": "msg_mock", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 100, "output_tokens": 5,
            "cache_creation_input_tokens": 7, "cache_read_input_tokens": 50
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(msg.to_string()))
        .unwrap()
}

async fn mock_count_tokens(State(m): State<MockState>, headers: HeaderMap, raw: Bytes) -> Response {
    m.record("/v1/messages/count_tokens", &headers, &raw);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"input_tokens": 1234}).to_string()))
        .unwrap()
}

// ── harness ─────────────────────────────────────────────────────────────────

struct Ctx {
    url: String,
    mock: MockState,
    state: Arc<AppState>,
    ledger: PathBuf,
    http: reqwest::Client,
}

async fn setup() -> Ctx {
    setup_entitled(true).await
}

async fn setup_entitled(entitled: bool) -> Ctx {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mock = MockState::default();
    let mock_router = Router::new()
        .route("/v1/messages", post(mock_messages))
        .route("/v1/messages/count_tokens", post(mock_count_tokens))
        .with_state(mock.clone());
    let ml = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = ml.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ml, mock_router).await.unwrap() });

    let ledger = std::env::temp_dir().join(format!(
        "dasein-proxy-test-{}-{}.jsonl",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&ledger);
    let mut st = AppState::new(format!("http://{mock_addr}"), ledger.clone());
    st.entitled = entitled;
    let state = Arc::new(st);
    let pl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = pl.local_addr().unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(pl, app).await.unwrap() });

    Ctx {
        url: format!("http://{proxy_addr}"),
        mock,
        state,
        ledger,
        http: reqwest::Client::new(),
    }
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

fn assistant(text: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
}

/// A realistic body: Claude Code traffic always carries a system prompt —
/// it is what keeps the conversation-id head stable while the tail grows.
fn body(messages: Vec<Value>) -> Value {
    json!({
        "model": "claude-test",
        "max_tokens": 16,
        "system": "be terse",
        "messages": messages
    })
}

/// True when any block of the message's content carries cache_control.
fn has_anchor(msg: &Value) -> bool {
    match msg.get("content") {
        Some(Value::Array(blocks)) => blocks.iter().any(|b| b.get("cache_control").is_some()),
        _ => false,
    }
}

async fn post_messages(ctx: &Ctx, body: &Value, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut req = ctx.http.post(format!("{}/v1/messages", ctx.url)).json(body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.send().await.unwrap()
}

fn ledger_rows(ctx: &Ctx) -> Vec<Value> {
    std::fs::read_to_string(&ctx.ledger)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// ── tests ───────────────────────────────────────────────────────────────────

/// Unentitled (no API key ⇒ `AppState::entitled = false`): the serve path is a
/// PURE PASSTHROUGH. The body is forwarded byte-verbatim (no cache_control
/// anchors, no fold/curation), auth still flows, and NO count_tokens probe is
/// made — dasein saves nothing until a key is set. Contrast with
/// `auth_headers_forwarded_and_anchors_added`, which asserts the entitled path
/// DOES add anchors and probe.
#[tokio::test]
async fn unentitled_serve_is_pure_passthrough() {
    let ctx = setup_entitled(false).await;
    // A 2-turn conversation that WOULD get anchors + a probe when entitled.
    let sent = body(vec![user("hello"), assistant("hi"), user("again")]);
    let resp = post_messages(&ctx, &sent, &[("x-api-key", "sk-test")]).await;
    assert_eq!(resp.status(), 200);

    let msgs = ctx.mock.messages();
    assert_eq!(msgs.len(), 1, "exactly one upstream forward");
    assert_eq!(
        msgs[0].body(),
        sent,
        "unentitled forward must be byte-verbatim — no anchors, no curation"
    );
    assert_eq!(
        msgs[0].header("x-api-key"),
        Some("sk-test"),
        "auth header still passes through (Claude Code runs normally)"
    );
    assert_eq!(
        ctx.mock.count_tokens().len(),
        0,
        "no counterfactual probe when unentitled"
    );
}

/// (1) auth headers reach the mock verbatim; our server adds cache_control
/// anchors to the forwarded body (system + tail on a first call).
#[tokio::test]
async fn auth_headers_forwarded_and_anchors_added() {
    let ctx = setup().await;
    let mut b = body(vec![user("hello")]);
    b["system"] = json!("be terse");
    let resp = post_messages(
        &ctx,
        &b,
        &[
            ("x-api-key", "sk-ant-test-123"),
            ("authorization", "Bearer tok-456"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "prompt-caching-2024"),
            ("x-ccb-run-id", "bench-run-42"),
            ("x-unrelated", "should-not-forward"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 200);

    let msgs = ctx.mock.messages();
    assert_eq!(msgs.len(), 1);
    let r = &msgs[0];
    assert_eq!(r.header("x-api-key"), Some("sk-ant-test-123"));
    assert_eq!(r.header("authorization"), Some("Bearer tok-456"));
    assert_eq!(r.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(r.header("anthropic-beta"), Some("prompt-caching-2024"));
    // the bench run-id tag rides upstream: the usage gateway below the proxy
    // keys its per-request rows by it (the reference forwarded it; a SHARED
    // gateway cannot isolate the dasein arm's rows without it)
    assert_eq!(r.header("x-ccb-run-id"), Some("bench-run-42"));
    assert_eq!(r.header("x-unrelated"), None);
    // the counterfactual probe carried the same auth + run-id tag
    let ct = ctx.mock.count_tokens();
    assert_eq!(ct.len(), 1);
    assert_eq!(ct[0].header("x-api-key"), Some("sk-ant-test-123"));
    assert_eq!(ct[0].header("x-ccb-run-id"), Some("bench-run-42"));

    let sent = r.body();
    // system anchored (run-stable block) and the tail message anchored
    let sys = &sent["system"];
    assert!(
        sys.as_array()
            .unwrap()
            .iter()
            .any(|b| b.get("cache_control").is_some()),
        "system should carry a cache_control anchor: {sys}"
    );
    let ms = sent["messages"].as_array().unwrap();
    assert!(
        has_anchor(&ms[0]),
        "first-call tail should be anchored: {sent}"
    );
}

/// (2) a second identical-prefix call anchors the FROZEN prefix — the
/// cache_control lands on the last message of the matching prefix (index 0),
/// not only the tail.
#[tokio::test]
async fn second_call_anchors_frozen_prefix() {
    let ctx = setup().await;
    let b1 = body(vec![user("hello")]);
    assert_eq!(post_messages(&ctx, &b1, &[]).await.status(), 200);

    let b2 = body(vec![
        user("hello"),
        assistant("hi there"),
        user("next step"),
    ]);
    assert_eq!(post_messages(&ctx, &b2, &[]).await.status(), 200);

    let msgs = ctx.mock.messages();
    assert_eq!(msgs.len(), 2);
    let sent = msgs[1].body();
    let ms = sent["messages"].as_array().unwrap();
    assert!(
        has_anchor(&ms[0]),
        "frozen-prefix anchor missing on messages[0]: {sent}"
    );
    assert!(
        !has_anchor(&ms[1]),
        "middle message must not be anchored: {sent}"
    );
    assert!(
        has_anchor(&ms[2]),
        "tail anchor missing on messages[2]: {sent}"
    );
}

/// (3) fingerprints are committed ONLY after upstream success: a call the
/// upstream 500s must not advance last_fps, so the retry behaves exactly
/// like the failed call (same anchors, byte-identical body).
#[tokio::test]
async fn fps_not_committed_on_upstream_500() {
    let ctx = setup().await;
    let b1 = body(vec![user("hello")]);
    assert_eq!(post_messages(&ctx, &b1, &[]).await.status(), 200);

    ctx.mock.fail_next.store(true, Ordering::SeqCst);
    let b2 = body(vec![
        user("hello"),
        assistant("hi there"),
        user("next step"),
    ]);
    let r2 = post_messages(&ctx, &b2, &[]).await;
    assert_eq!(r2.status(), 500, "upstream error must relay as-is");
    assert_eq!(r2.text().await.unwrap(), "upstream boom");

    // third call, same body: must behave like the (failed) second call —
    // frozen prefix is still just [msg0] from call 1.
    assert_eq!(post_messages(&ctx, &b2, &[]).await.status(), 200);
    let msgs = ctx.mock.messages();
    assert_eq!(msgs.len(), 3);
    let sent2 = msgs[1].body();
    let sent3 = msgs[2].body();
    assert_eq!(
        sent2, sent3,
        "retry after 500 must send byte-identical anchors"
    );
    let ms = sent3["messages"].as_array().unwrap();
    assert!(
        has_anchor(&ms[0]),
        "fps leaked from the failed call: anchor should still sit on messages[0]"
    );
}

/// (4) a savings-ledger row is written with the billed usage and the
/// count_tokens counterfactual, schema-aligned field names.
#[tokio::test]
async fn ledger_row_written_with_counterfactual_and_billed() {
    let ctx = setup().await;
    let mut b = body(vec![user("hello")]);
    // The CC 2.1.x metadata shape: user_id is a JSON-encoded string.
    b["metadata"] = serde_json::json!({
        "user_id": r#"{"device_id":"d1fe","account_uuid":"","session_id":"8068d98c-4176-4b0e-8e2b-a543aa24f204"}"#
    });
    assert_eq!(post_messages(&ctx, &b, &[]).await.status(), 200);

    let rows = ledger_rows(&ctx);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["contract_version"], "savings-ledger/v0");
    assert_eq!(row["session_id"], "8068d98c-4176-4b0e-8e2b-a543aa24f204");
    assert!(row["request_id"].as_str().unwrap().starts_with("req_"));
    assert_eq!(row["request_id"].as_str().unwrap().len(), 4 + 32);
    assert_eq!(row["counterfactual_input_tokens"], 1234);
    assert_eq!(row["billed_input_tokens"], 100);
    assert_eq!(row["billed_output_tokens"], 5);
    assert_eq!(row["billed_cache_read_tokens"], 50);
    assert_eq!(row["billed_cache_write_tokens"], 7);
    assert_eq!(row["cachePrefixSha8"].as_str().unwrap().len(), 8);
    assert_eq!(row["fail_open"], false);
    assert!(row["ts"].as_str().unwrap().contains('T'));
}

/// (5) count_tokens is a pure verbatim passthrough: the exact inbound bytes
/// reach upstream, the response relays, and NO conversation state is created
/// (the reference's count-tokens-advances-the-curator bug).
#[tokio::test]
async fn count_tokens_verbatim_and_stateless() {
    let ctx = setup().await;
    let raw = br#"{"model":"claude-test","messages":[{"role":"user","content":"hello"}]}"#;
    let resp = ctx
        .http
        .post(format!("{}/v1/messages/count_tokens", ctx.url))
        .header("x-api-key", "sk-ant-test-123")
        .header(header::CONTENT_TYPE, "application/json")
        .body(raw.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["input_tokens"], 1234);

    let ct = ctx.mock.count_tokens();
    assert_eq!(ct.len(), 1);
    assert_eq!(
        ct[0].raw,
        raw.to_vec(),
        "body must pass through byte-verbatim"
    );
    assert_eq!(ct[0].header("x-api-key"), Some("sk-ant-test-123"));
    assert!(ctx.mock.messages().is_empty());
    assert_eq!(
        ctx.state.convs.lock().unwrap().len(),
        0,
        "count_tokens must not touch conversation state"
    );
    assert!(
        ledger_rows(&ctx).is_empty(),
        "count_tokens writes no ledger row"
    );

    // and anything else 404s
    let r = ctx
        .http
        .get(format!("{}/v1/models", ctx.url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

/// (6) streaming: the SSE bytes relay byte-identical while usage is scanned
/// out of message_start + the last message_delta for the ledger.
#[tokio::test]
async fn sse_relayed_byte_identical_with_usage_ledger() {
    let ctx = setup().await;
    let mut b = body(vec![user("hello")]);
    b["stream"] = json!(true);
    let resp = post_messages(&ctx, &b, &[]).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(bytes.as_ref(), SSE_BODY, "SSE relay must be byte-identical");

    let rows = ledger_rows(&ctx);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["billed_input_tokens"], 10);
    assert_eq!(row["billed_output_tokens"], 42, "last message_delta wins");
    assert_eq!(row["billed_cache_read_tokens"], 90);
    assert_eq!(row["billed_cache_write_tokens"], 3);
    assert_eq!(row["counterfactual_input_tokens"], 1234);
    assert_eq!(row["fail_open"], false);
}
