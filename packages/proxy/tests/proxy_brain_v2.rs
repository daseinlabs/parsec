//! Integration tests for the brain-api/v2 CLIENT mode (PARSEC_BRAIN_CONTRACT
//! =v2): a mock brain serves /v1/bundle + v2-shaped scores so the wired
//! behavior is asserted end to end on the actual proxy — chunk TEXT on the
//! wire, NO vectors, the bundle handshake and its checkpoint_id still riding,
//! and the dupcos handoff (readout cols 47-48 zeroed for the server to fill).
//!
//! v1 coverage stays in tests/proxy_brain_v1.rs and dev in tests/proxy_brain.rs;
//! all three contracts are served simultaneously (docs/server-side-embedding.md).

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

const TAU_Q: i64 = 315_265;
const CKPT: fn() -> String = || "c".repeat(64);

#[derive(Clone, Default)]
struct Upstream {
    reqs: Arc<Mutex<Vec<Value>>>,
}

async fn upstream_messages(State(u): State<Upstream>, _h: HeaderMap, raw: Bytes) -> Response {
    u.reqs
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&raw).unwrap_or(Value::Null));
    ok_json(
        json!({
            "id": "msg_mock", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 5,
                      "cache_creation_input_tokens": 7, "cache_read_input_tokens": 50}
        })
        .to_string(),
    )
}

async fn upstream_count(_h: HeaderMap, _raw: Bytes) -> Response {
    ok_json(json!({"input_tokens": 1234}).to_string())
}

fn ok_json(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn boom() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("brain boom"))
        .unwrap()
}

#[derive(Clone, Default)]
struct MockBrain {
    bundle_calls: Arc<AtomicU64>,
    trace_reqs: Arc<Mutex<Vec<Value>>>,
    tools_reqs: Arc<Mutex<Vec<Value>>>,
    fail_bundle: Arc<AtomicBool>,
}

async fn brain_bundle(State(b): State<MockBrain>) -> Response {
    b.bundle_calls.fetch_add(1, Ordering::SeqCst);
    if b.fail_bundle.load(Ordering::SeqCst) {
        return boom();
    }
    ok_json(
        json!({
            "contract": "brain-api-dev/v0",
            "contracts": ["brain-api-dev/v0", "brain-api/v1", "brain-api/v2"],
            "checkpoint_id": CKPT(),
            "tau_q": TAU_Q,
            "target_cov": "0.70",
            "grid": 1_000_000,
        })
        .to_string(),
    )
}

async fn brain_trace(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.trace_reqs.lock().unwrap().push(req.clone());
    assert_eq!(
        req["contract"], "brain-api/v2",
        "wrong contract on the wire"
    );
    let n = req["nodes"].as_array().unwrap().len();
    let mask: Vec<usize> = req["mask"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    let scores: Vec<i64> = (0..n)
        .map(|j| {
            if mask.contains(&j) {
                100_000
            } else {
                1_000_000
            }
        })
        .collect();
    ok_json(
        json!({
            "scores_q": scores, "tau_q": TAU_Q, "checkpoint_id": CKPT(),
            // unlike v1, embed is a REAL number on v2 — the server embedded
            "timings_ms": {"embed": 4.2, "forward": 1.0}
        })
        .to_string(),
    )
}

async fn brain_tools(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.tools_reqs.lock().unwrap().push(req.clone());
    assert_eq!(req["contract"], "brain-api/v2");
    let names: Vec<String> = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    let scores: Vec<i64> = (0..names.len())
        .map(|i| if i == 0 { 990_000 } else { 100_000 + i as i64 })
        .collect();
    let tokens: Vec<i64> = names.iter().map(|_| 100).collect();
    ok_json(
        json!({"names": names, "scores_q": scores, "tokens": tokens,
               "checkpoint_id": CKPT()})
        .to_string(),
    )
}

struct Ctx {
    url: String,
    upstream: Upstream,
    brain: MockBrain,
    ledger: PathBuf,
    http: reqwest::Client,
}

async fn setup() -> Ctx {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let upstream = Upstream::default();
    let up_router = Router::new()
        .route("/v1/messages", post(upstream_messages))
        .route("/v1/messages/count_tokens", post(upstream_count))
        .with_state(upstream.clone());
    let ul = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = ul.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ul, up_router).await.unwrap() });

    let brain = MockBrain::default();
    let brain_router = Router::new()
        .route("/v1/bundle", get(brain_bundle))
        .route("/v1/score/trace", post(brain_trace))
        .route("/v1/score/tools", post(brain_tools))
        .with_state(brain.clone());
    let bl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let brain_addr = bl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(bl, brain_router).await.unwrap() });

    let ledger = std::env::temp_dir().join(format!(
        "parsec-brain-v2-test-{}-{}.jsonl",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&ledger);
    let cfg = BrainConfig {
        url: format!("http://{brain_addr}"),
        key: None,
        timeout: Duration::from_secs(5),
        target_cov: "0.70".into(),
        tool_cut: 0.70,
        tool_prune: true,
        tool_stub: true,
        contract: BrainContract::V2,
        score_memo_dir: None,
        // deliberately "hash": v2 must never construct a client embedder, so
        // the backend value is inert. If it ever regressed to building one,
        // the payload assertions below would still pass — but the embedder
        // would be dead weight, which is what needs_embedder() pins.
    };
    let state = Arc::new(AppState::with_brain(
        format!("http://{up_addr}"),
        ledger.clone(),
        Some(cfg),
    ));
    let pl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = pl.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move { axum::serve(pl, app).await.unwrap() });

    Ctx {
        url: format!("http://{proxy_addr}"),
        upstream,
        brain,
        ledger,
        http: reqwest::Client::new(),
    }
}

fn file_lines(n: usize) -> String {
    (1..=n)
        .map(|i| format!("{i}: let value_{i} = compute_step_{i}(input_{i});"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn convo_turn1() -> Vec<Value> {
    vec![
        json!({"role": "user", "content": "Fix the failing test in parser.py"}),
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "Let me read the file."},
            {"type": "tool_use", "id": "toolu_1", "name": "Read",
             "input": {"file_path": "src/parser.py"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": file_lines(40)}
        ]}),
    ]
}

fn body(messages: Vec<Value>) -> Value {
    json!({
        "model": "claude-sonnet-5",
        "max_tokens": 512,
        "system": "be terse",
        "messages": messages
    })
}

async fn post_messages(ctx: &Ctx, b: &Value) -> reqwest::Response {
    ctx.http
        .post(format!("{}/v1/messages", ctx.url))
        .header("x-api-key", "k")
        .json(b)
        .send()
        .await
        .unwrap()
}

fn ledger_rows(ctx: &Ctx) -> Vec<Value> {
    match std::fs::read_to_string(&ctx.ledger) {
        Ok(s) => s
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

/// The v2 payload carries chunk TEXT and structural features — and no vectors
/// anywhere — while the scores still drive the freezer's trim exactly like v1.
#[tokio::test]
async fn v2_payload_carries_text_not_vectors_and_trims() {
    let ctx = setup().await;
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 200);

    let treqs = ctx.brain.trace_reqs.lock().unwrap().clone();
    assert!(!treqs.is_empty(), "no v2 trace call");
    let p = &treqs[0];
    assert_eq!(p["contract"], "brain-api/v2");
    assert_eq!(p["checkpoint_id"].as_str().unwrap(), CKPT());

    let nodes = p["nodes"].as_array().unwrap();
    assert!(!nodes.is_empty(), "no nodes on the wire");
    for n in nodes {
        // text slots present...
        assert!(n["text"].is_string(), "node.text must be a string");
        assert!(n["cmd"].is_string(), "node.cmd must be a string");
        assert!(n["head"].is_string(), "node.head must be a string");
        // ...and the v1 vector slots absent entirely
        assert!(n.get("emb_text").is_none(), "v1 vector slot leaked into v2");
        assert!(n.get("emb_cmd").is_none(), "v1 vector slot leaked into v2");
        assert!(n.get("emb_head").is_none(), "v1 vector slot leaked into v2");
        // structural featurization still rides
        assert_eq!(n["struct"].as_array().unwrap().len(), 21);
        // text is client-clipped per the contract
        assert!(n["text"].as_str().unwrap().chars().count() <= 2000);
        assert!(n["head"].as_str().unwrap().chars().count() <= 240);
    }
    assert!(p["task_text"].is_string());
    assert!(p.get("task_emb").is_none(), "v1 task vector leaked into v2");

    // the whole point: the observation actually got trimmed on the wire
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let out = serde_json::to_string(&sent[0]).unwrap();
    assert!(
        out.len() < serde_json::to_string(&body(convo_turn1())).unwrap().len(),
        "v2 scoring did not trim the forwarded body"
    );
}

/// The dupcos handoff: a client with no embedder cannot compute readout cols
/// 47-48, so it MUST send them zeroed for the brain to refill. If this ever
/// emits non-zero values the client has grown an embedder again, and the
/// server's fill would silently overwrite real numbers.
#[tokio::test]
async fn v2_decided_struct_zeroes_dupcos_for_the_server_to_fill() {
    let ctx = setup().await;
    assert_eq!(
        post_messages(&ctx, &body(convo_turn1())).await.status(),
        200
    );

    let treqs = ctx.brain.trace_reqs.lock().unwrap().clone();
    let ds = treqs[0]["decided_struct"].as_array().unwrap();
    assert!(!ds.is_empty(), "no decided rows to check");
    for row in ds {
        let r = row.as_array().unwrap();
        assert_eq!(r.len(), 49, "readout width must stay 49 on v2");
        assert_eq!(r[47].as_f64().unwrap(), 0.0, "dupcos col 47 must ride zero");
        assert_eq!(r[48].as_f64().unwrap(), 0.0, "dupcos col 48 must ride zero");
        // ...and the rest is genuinely populated, so the zeros above are a
        // deliberate handoff rather than an all-zero readout bug.
        assert!(
            r[..47].iter().any(|v| v.as_f64().unwrap() != 0.0),
            "the whole readout row is zero — structural featurization is broken"
        );
    }
}

/// v2 handshakes exactly like v1 — the §8.2 matched-pair guard is the reason
/// v2 derives from v1 rather than from the guardless dev contract.
#[tokio::test]
async fn v2_handshake_once_per_conversation() {
    let ctx = setup().await;
    assert_eq!(
        post_messages(&ctx, &body(convo_turn1())).await.status(),
        200
    );
    let after_first = ctx.brain.bundle_calls.load(Ordering::SeqCst);
    assert!(after_first >= 1, "no bundle handshake on v2");

    let mut msgs = convo_turn1();
    msgs.push(json!({"role": "assistant", "content": "done"}));
    msgs.push(json!({"role": "user", "content": "now fix the other one"}));
    assert_eq!(post_messages(&ctx, &body(msgs)).await.status(), 200);
    assert_eq!(
        ctx.brain.bundle_calls.load(Ordering::SeqCst),
        after_first,
        "v2 re-handshook mid-conversation (the checkpoint is cached per conv)"
    );
}

/// Handshake failure = per-step fail-open, and the request still succeeds.
#[tokio::test]
async fn v2_handshake_failure_fails_open() {
    let ctx = setup().await;
    ctx.brain.fail_bundle.store(true, Ordering::SeqCst);
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(
        resp.status(),
        200,
        "brain outage must not break the request"
    );
    assert!(
        ctx.brain.trace_reqs.lock().unwrap().is_empty(),
        "scored despite a failed handshake"
    );
    let rows = ledger_rows(&ctx);
    assert!(!rows.is_empty(), "no ledger row written");
}

/// Tool head on v2: names and the serialized schema TEXT ride; no vectors.
#[tokio::test]
async fn v2_tools_payload_carries_schema_text() {
    let ctx = setup().await;
    let mut b = body(convo_turn1());
    b["tools"] = json!([
        {"name": "Read", "description": "read a file",
         "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}},
        {"name": "Bash", "description": "run a command", "input_schema": {"type": "object"}},
    ]);
    assert_eq!(post_messages(&ctx, &b).await.status(), 200);

    let treqs = ctx.brain.tools_reqs.lock().unwrap().clone();
    assert!(!treqs.is_empty(), "no v2 tools call");
    let tools = treqs[0]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    for t in tools {
        assert!(t["name"].is_string());
        assert!(t["schema_text"].is_string(), "schema text must ride on v2");
        assert!(t.get("emb").is_none(), "v1 tool vector leaked into v2");
        assert!(t["tokens"].as_i64().unwrap() >= 1);
    }
    // the schema text is the exact serialized form the brain re-embeds
    let first = tools[0]["schema_text"].as_str().unwrap();
    assert!(first.starts_with('{') && first.contains("\"name\""));
}
