//! Integration tests for the brain-api/v3 CLIENT mode (PARSEC_BRAIN_CONTRACT
//! =v3, the HS curator): a mock brain serves /v1/bundle + v3-shaped scores,
//! and the wire is checked against the committed brain-api-v3 schema — the
//! 27-wide node struct carrying the Freezer's re-request columns, the `ext`
//! class, the 104-col decided rows with the brain's blocks left zero — plus
//! the per-kind keep rule and the handshake refusal of a non-v3 brain.

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

const CKPT: fn() -> String = || "d".repeat(64);
/// Mask rows score this; `read` sits above it (cut), everything else below.
const MASK_SCORE: i64 = 300_000;

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
                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
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

#[derive(Clone, Default)]
struct MockBrain {
    bundle_calls: Arc<AtomicU64>,
    trace_reqs: Arc<Mutex<Vec<Value>>>,
    tools_reqs: Arc<Mutex<Vec<Value>>>,
    /// Advertise a bge (v2-only) brain instead of an HS one.
    legacy: Arc<AtomicBool>,
}

async fn brain_bundle(State(b): State<MockBrain>) -> Response {
    b.bundle_calls.fetch_add(1, Ordering::SeqCst);
    if b.legacy.load(Ordering::SeqCst) {
        return ok_json(
            json!({
                "contract": "brain-api-dev/v0",
                "contracts": ["brain-api-dev/v0", "brain-api/v1", "brain-api/v2"],
                "checkpoint_id": CKPT(), "tau_q": 315_265, "accept_gzip": true,
            })
            .to_string(),
        );
    }
    ok_json(
        json!({
            "contracts": ["brain-api/v2", "brain-api/v3"],
            "checkpoint_id": CKPT(),
            "read_struct": 104, "node_struct": 106, "spec_version": "v6828t3",
            "tau_q_by_kind": {"read": 400_000, "other": 200_000, "grep": 200_000,
                              "reasoning": 200_000},
            "tau_coverage": "0.90", "encoder": "Qwen/Qwen3.5-0.8B-Base",
            "accept_gzip": false, "doom": {"gf": 4, "served": false},
        })
        .to_string(),
    )
}

async fn brain_trace(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.trace_reqs.lock().unwrap().push(req.clone());
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
                MASK_SCORE
            } else {
                1_000_000
            }
        })
        .collect();
    ok_json(
        json!({
            "scores_q": scores, "checkpoint_id": CKPT(),
            "tau_q_by_kind": {"read": 400_000, "other": 200_000, "grep": 200_000,
                              "reasoning": 200_000},
            "timings_ms": {"encode": 12.0, "forward": 1.0}
        })
        .to_string(),
    )
}

async fn brain_tools(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.tools_reqs.lock().unwrap().push(req.clone());
    // Returning an empty roster is the fail-open shape: full tools served.
    ok_json(json!({"names": [], "scores_q": [], "tokens": [], "checkpoint_id": CKPT()}).to_string())
}

struct Ctx {
    url: String,
    upstream: Upstream,
    brain: MockBrain,
    _ledger: PathBuf,
    http: reqwest::Client,
}

async fn setup(legacy_brain: bool) -> Ctx {
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
    brain.legacy.store(legacy_brain, Ordering::SeqCst);
    let brain_router = Router::new()
        .route("/v1/bundle", get(brain_bundle))
        .route("/v1/score/trace", post(brain_trace))
        .route("/v1/score/tools", post(brain_tools))
        .with_state(brain.clone());
    let bl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let brain_addr = bl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(bl, brain_router).await.unwrap() });

    let ledger = std::env::temp_dir().join(format!(
        "parsec-brain-v3-test-{}-{}.jsonl",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&ledger);
    let cfg = BrainConfig {
        url: format!("http://{brain_addr}"),
        key: None,
        timeout: Duration::from_secs(5),
        target_cov: "0.90".into(),
        tool_cut: 0.70,
        tool_prune: true,
        tool_stub: true,
        contract: BrainContract::V3,
        score_memo_dir: None,
    };
    // Decide births on the request that carries them (reference cut-at-birth),
    // as the v2 suite does; protect_current is covered by golden_conversation.
    std::env::set_var("PARSEC_PROTECT_CURRENT", "off");
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
        _ledger: ledger,
        http: reqwest::Client::new(),
    }
}

fn file_lines(n: usize) -> String {
    (1..=n)
        .map(|i| format!("{i}: let value_{i} = compute_step_{i}(input_{i});"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn convo() -> Vec<Value> {
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
        "tools": [{"name": "Read", "description": "read a file",
                   "input_schema": {"type": "object"}}],
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

fn schema() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/../contracts/schemas/brain-api-v3.schema.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("brain-api-v3 schema present"),
    )
    .unwrap()
}

/// Every key is a schema property and every required property is present
/// (the brain's models are extra="forbid": an unknown key is a 422).
fn check_keys(schema: &Value, def: &str, obj: &Value) {
    let d = &schema["$defs"][def];
    let props = d["properties"].as_object().unwrap();
    for k in obj.as_object().unwrap().keys() {
        assert!(
            props.contains_key(k),
            "{def}: key {k:?} not in the v3 schema"
        );
    }
    for r in d["required"].as_array().unwrap() {
        let r = r.as_str().unwrap();
        assert!(obj.get(r).is_some(), "{def}: required {r:?} missing");
    }
}

fn served_tool_result(ctx: &Ctx) -> String {
    let reqs = ctx.upstream.reqs.lock().unwrap().clone();
    let last = reqs.last().expect("upstream saw the request");
    last["messages"][2]["content"][0]["content"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| last["messages"][2]["content"][0]["content"].to_string())
}

// ── tests ───────────────────────────────────────────────────────────────────

/// The v3 score/trace body matches the committed schema key-for-key, carries
/// the re-request columns and the extension class, leaves the brain's decided
/// blocks zero — and the per-kind reply drives the cut.
#[tokio::test]
async fn v3_payload_matches_schema_and_the_kind_rule_cuts() {
    let ctx = setup(false).await;
    let resp = post_messages(&ctx, &body(convo())).await;
    assert_eq!(resp.status(), 200);

    let treqs = ctx.brain.trace_reqs.lock().unwrap().clone();
    assert!(!treqs.is_empty(), "no v3 trace call");
    let p = &treqs[0];
    let sc = schema();
    check_keys(&sc, "score_trace_request", p);
    assert_eq!(p["contract"], "brain-api/v3");
    assert_eq!(p["checkpoint_id"].as_str().unwrap(), CKPT());

    let exts: Vec<&str> = sc["$defs"]["ext_class"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let nodes = p["nodes"].as_array().unwrap();
    assert!(!nodes.is_empty());
    for n in nodes {
        check_keys(&sc, "node", n);
        assert_eq!(n["struct"].as_array().unwrap().len(), 27, "21 + 6 rereq");
        assert!(
            exts.contains(&n["ext"].as_str().unwrap()),
            "ext {:?}",
            n["ext"]
        );
    }
    let reads: Vec<&Value> = nodes.iter().filter(|n| n["kind"] == "read").collect();
    assert!(!reads.is_empty(), "the Read result chunks as reads");
    assert!(reads.iter().all(|n| n["ext"] == "py"), "parser.py -> py");

    let rows = p["decided_struct"].as_array().unwrap();
    assert_eq!(rows.len(), p["mask"].as_array().unwrap().len());
    for r in rows {
        let r: Vec<f64> = r
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        assert_eq!(r.len(), 104);
        assert!(
            r[22..72].iter().all(|&x| x == 0.0),
            "read columns are the brain's"
        );
        assert!(r[102..].iter().all(|&x| x == 0.0), "dupcos is the brain's");
    }

    // Mask rows score 300k: below read's 400k tau, so the read is cut.
    let served = served_tool_result(&ctx);
    assert_ne!(
        served,
        file_lines(40),
        "read below its kind tau must be cut"
    );
    assert!(served.contains("omitted"), "{served}");
}

/// A brain that does not advertise brain-api/v3 at the client's widths is
/// never sent a v3 body: every step fails open and the request is served
/// uncut. The refusal is not cached — each serve re-checks the bundle.
#[tokio::test]
async fn v3_client_refuses_a_non_v3_brain() {
    let ctx = setup(true).await;
    for _ in 0..2 {
        let resp = post_messages(&ctx, &body(convo())).await;
        assert_eq!(resp.status(), 200);
    }
    assert!(
        ctx.brain.trace_reqs.lock().unwrap().is_empty(),
        "no score/trace against a v2-only brain"
    );
    assert!(
        ctx.brain.bundle_calls.load(Ordering::SeqCst) >= 2,
        "re-checked per serve"
    );
    assert_eq!(served_tool_result(&ctx), file_lines(40), "served uncut");
}

/// score/tools keeps the brain-api/v2 shape under the v3 client.
#[tokio::test]
async fn v3_client_keeps_v2_tools_bodies() {
    let ctx = setup(false).await;
    let resp = post_messages(&ctx, &body(convo())).await;
    assert_eq!(resp.status(), 200);
    let tools = ctx.brain.tools_reqs.lock().unwrap().clone();
    assert!(!tools.is_empty(), "no tools call");
    assert_eq!(tools[0]["contract"], "brain-api/v2");
}
