//! Integration tests for the real-scorer path (docs/brain-serving-v0.md):
//! a mock brain serves canned grid scores next to the mock Anthropic
//! upstream, so the wired behavior — freeze trimming, per-step scorer
//! fail-open, the frozen tool keep-set, metadata bypass, resident-turn byte
//! stability — is asserted end to end on the actual proxy.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};

use dasein_proxy::brain::{self, BrainConfig, BrainContract};
use dasein_proxy::server::{router, AppState};
use dasein_proxy::splice::strip_cache_control;

/// Grid tau the mock brain hands out (the real ckpt's 0.70-cov tau).
const TAU_Q: i64 = 315_265;

// ── mock upstream (canned Anthropic) ────────────────────────────────────────

#[derive(Clone, Default)]
struct Upstream {
    reqs: Arc<Mutex<Vec<Value>>>,
    raws: Arc<Mutex<Vec<Vec<u8>>>>,
}

async fn upstream_messages(State(u): State<Upstream>, _h: HeaderMap, raw: Bytes) -> Response {
    u.reqs
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&raw).unwrap_or(Value::Null));
    u.raws.lock().unwrap().push(raw.to_vec());
    let msg = json!({
        "id": "msg_mock", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 100, "output_tokens": 5,
                  "cache_creation_input_tokens": 7, "cache_read_input_tokens": 50}
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(msg.to_string()))
        .unwrap()
}

async fn upstream_count(_h: HeaderMap, _raw: Bytes) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"input_tokens": 1234}).to_string()))
        .unwrap()
}

// ── mock brain ──────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct MockBrain {
    trace_reqs: Arc<Mutex<Vec<Value>>>,
    tools_reqs: Arc<Mutex<Vec<Value>>>,
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

/// Tool head: first name scores high, the rest low — with equal token mass
/// and target_cut 0.70, prune keeps exactly the first.
async fn brain_tools(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.tools_reqs.lock().unwrap().push(req.clone());
    if b.fail.load(Ordering::SeqCst) {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("brain boom"))
            .unwrap();
    }
    let names: Vec<String> = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let scores: Vec<i64> = (0..names.len())
        .map(|i| if i == 0 { 990_000 } else { 100_000 + i as i64 })
        .collect();
    let tokens: Vec<i64> = names.iter().map(|_| 100).collect();
    let resp = json!({
        "names": names, "scores_q": scores, "tokens": tokens,
        "checkpoint_id": "c".repeat(64)
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
        .route("/v1/score/trace", post(brain_trace))
        .route("/v1/score/tools", post(brain_tools))
        .with_state(brain.clone());
    let bl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let brain_addr = bl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(bl, brain_router).await.unwrap() });

    let ledger = std::env::temp_dir().join(format!(
        "dasein-brain-test-{}-{}.jsonl",
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
        contract: BrainContract::Dev,
        embed_backend: "hash".into(),
        embed_url: None,
        onnx_dir: None,
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

/// A CC-shaped turn: task, assistant Read tool_use, long tool_result.
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

#[tokio::test]
async fn brain_scores_trim_the_observation() {
    let ctx = setup().await;
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 200);

    // The brain was consulted with the dev contract...
    let treqs = ctx.brain.trace_reqs.lock().unwrap().clone();
    assert!(!treqs.is_empty(), "no brain trace call");
    assert_eq!(treqs[0]["contract"], "brain-api-dev/v0");
    assert_eq!(treqs[0]["target_cov"], "0.70");
    assert!(treqs[0]["chunk_checksum"].as_str().unwrap().len() == 64);

    // ...and the forwarded tool_result was digested (low scores → cut).
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let fwd = sent.last().unwrap();
    let obs = serde_json::to_string(&fwd["messages"][2]).unwrap();
    assert!(obs.contains("omitted"), "observation not trimmed: {obs}");
    assert!(
        obs.contains("re-read parser.py"),
        "no recovery pointer: {obs}"
    );
    // Small assistant text (< 10-token run floor) survives marker economics.
    let asst = serde_json::to_string(&fwd["messages"][1]).unwrap();
    assert!(
        asst.contains("Let me read the file."),
        "assistant text lost"
    );
    // tool_use block rides through untouched.
    assert!(asst.contains("toolu_1"));

    let rows = ledger_rows(&ctx);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["fail_open"], false);
    assert_eq!(rows[0]["checkpoint_id"], "c".repeat(64));
    assert!(rows[0]["freeze_cut_tokens"].as_i64().unwrap() > 0);
    assert!(rows[0].get("scorer_fail_opens").is_none());
}

#[tokio::test]
async fn resident_turns_stay_byte_identical_across_turns() {
    let ctx = setup().await;
    post_messages(&ctx, &body(convo_turn1())).await;

    let mut msgs = convo_turn1();
    msgs.push(json!({"role": "assistant", "content": "The bug is on line 12."}));
    msgs.push(json!({"role": "user", "content": "apply the fix"}));
    post_messages(&ctx, &body(msgs)).await;

    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    #[allow(clippy::needless_range_loop)] // j indexes BOTH sends in parallel
    for j in 0..3 {
        // Anchors move between calls; served CONTENT must not.
        let a = strip_cache_control(&sent[0]["messages"][j]);
        let b = strip_cache_control(&sent[1]["messages"][j]);
        assert_eq!(a, b, "resident turn {j} changed bytes across calls");
    }
}

#[tokio::test]
async fn brain_down_fails_open_per_step_and_retries() {
    let ctx = setup().await;
    ctx.brain.fail.store(true, Ordering::SeqCst);
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 200);

    // Request succeeded; the observation was served FULL (no digest).
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let obs = serde_json::to_string(&sent[0]["messages"][2]).unwrap();
    assert!(!obs.contains("omitted"), "trimmed despite brain down");
    let rows = ledger_rows(&ctx);
    assert_eq!(
        rows[0]["fail_open"], false,
        "request-level fail-open is wrong here"
    );
    assert_eq!(rows[0]["scorer_fail_opens"], 1);

    // Heal the brain: the same conversation's next call RETRIES the skipped
    // birth step (quantized replay) — the wire keeps the already-served turn
    // frozen, but the scorer is consulted again.
    ctx.brain.fail.store(false, Ordering::SeqCst);
    let calls_before = ctx.brain.trace_reqs.lock().unwrap().len();
    let mut msgs = convo_turn1();
    msgs.push(json!({"role": "assistant", "content": "hm."}));
    msgs.push(json!({"role": "user", "content": "continue"}));
    post_messages(&ctx, &body(msgs)).await;
    let calls_after = ctx.brain.trace_reqs.lock().unwrap().len();
    assert!(
        calls_after > calls_before,
        "healed brain never re-consulted"
    );
    let rows = ledger_rows(&ctx);
    assert!(rows[1].get("scorer_fail_opens").is_none());
}

#[tokio::test]
async fn tool_keepset_frozen_once_per_conversation() {
    let ctx = setup().await;
    let tools = json!([
        {"name": "Read", "description": "read", "input_schema": {"type": "object"}},
        {"name": "Bash", "description": "run", "input_schema": {"type": "object"}},
        {"name": "Grep", "description": "grep", "input_schema": {"type": "object"}},
        {"name": "Glob", "description": "glob", "input_schema": {"type": "object"}}
    ]);
    let mut b = body(convo_turn1());
    b["tools"] = tools.clone();
    post_messages(&ctx, &b).await;

    // Equal mass, first name scored high, cut 0.70 of 4 ⇒ keep exactly Read
    // full; the pruned three ride as name+note stubs (DASEIN_TOOL_STUB
    // default) so the model knows they can be called back.
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let fwd_tools = sent[0]["tools"].as_array().unwrap();
    assert_eq!(fwd_tools.len(), 4);
    assert_eq!(fwd_tools[0]["name"], "Read");
    assert_eq!(fwd_tools[0]["description"], "read"); // kept = untouched bytes
    for t in &fwd_tools[1..] {
        let d = t["description"].as_str().unwrap();
        assert!(d.contains(brain::STUB_NOTE), "stub missing the note: {t}");
        assert_eq!(
            t["input_schema"],
            json!({"type": "object", "additionalProperties": true})
        );
    }

    // Second request: keep-set FROZEN — no second scoring round trip, and
    // the stub roster is byte-stable.
    let mut msgs = convo_turn1();
    msgs.push(json!({"role": "assistant", "content": "ok"}));
    msgs.push(json!({"role": "user", "content": "go on"}));
    let mut b2 = body(msgs);
    b2["tools"] = tools.clone();
    post_messages(&ctx, &b2).await;
    assert_eq!(ctx.brain.tools_reqs.lock().unwrap().len(), 1);
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    assert_eq!(sent[1]["tools"], sent[0]["tools"]);

    let rows = ledger_rows(&ctx);
    assert_eq!(rows[0]["tools_total"], 4);
    assert_eq!(rows[0]["tools_kept"], 1);
    assert_eq!(rows[0]["tools_stubbed"], 3);
    assert_eq!(rows[0]["tools_pre_prune_sha8"].as_str().unwrap().len(), 8);
}

#[tokio::test]
async fn forced_tool_choice_serves_full_roster() {
    let ctx = setup().await;
    let mut b = body(convo_turn1());
    b["tools"] = json!([
        {"name": "Read", "description": "r", "input_schema": {"type": "object"}},
        {"name": "Bash", "description": "b", "input_schema": {"type": "object"}},
        {"name": "Grep", "description": "g", "input_schema": {"type": "object"}},
        {"name": "Task", "description": "t", "input_schema": {"type": "object"}}
    ]);
    b["tool_choice"] = json!({"type": "tool", "name": "Task"});
    post_messages(&ctx, &b).await;
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    // The keep-set may freeze, but a forced tool_choice must never be pruned
    // into an upstream 400 — full roster this call.
    assert_eq!(sent[0]["tools"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn metadata_calls_bypass_everything() {
    let ctx = setup().await;
    let b = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 30,
        "system": "You are tasked to generate a concise title for this coding session.",
        "messages": [{"role": "user", "content": "the transcript"}]
    });
    let raw_sent = serde_json::to_vec(&b).unwrap();
    let resp = ctx
        .http
        .post(format!("{}/v1/messages", ctx.url))
        .header("x-api-key", "k")
        .body(raw_sent.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Forwarded byte-verbatim: no anchors added, no curation.
    let raws = ctx.upstream.raws.lock().unwrap().clone();
    assert_eq!(raws[0], raw_sent);
    // No brain consultation, no ledger row.
    assert!(ctx.brain.trace_reqs.lock().unwrap().is_empty());
    assert!(ledger_rows(&ctx).is_empty());
}
