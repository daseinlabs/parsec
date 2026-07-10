//! Integration tests for the brain-api/v1 CLIENT mode (DASEIN_BRAIN_CONTRACT
//! =v1): a mock brain serves /v1/bundle + v1-shaped scores so the wired
//! behavior — client featurization (no raw text on the wire), the bundle
//! handshake, per-step fail-open on handshake failure, the v1 tool path — is
//! asserted end to end on the actual proxy. Dev-mode coverage stays in
//! tests/proxy_brain.rs, untouched: the default contract remains dev.
//!
//! Also here: `live_brain_dev_v1_score_parity` (#[ignore]) — one BirthQuery
//! scored through BOTH contracts against a REAL running brain. Run it with a
//! hash-backend brain up (see scripts/parity_v1.sh, which prints the exact
//! command), e.g.:
//!   DASEIN_BRAIN_URL=http://127.0.0.1:8093 \
//!     cargo test -p dasein-proxy --test proxy_brain_v1 -- --ignored

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

use dasein_proxy::brain::{BrainConfig, BrainContract};
use dasein_proxy::server::{router, AppState};

/// Grid tau the mock brain hands out (the real ckpt's 0.70-cov tau).
const TAU_Q: i64 = 315_265;
const CKPT: fn() -> String = || "c".repeat(64);

// ── mock upstream (canned Anthropic) ────────────────────────────────────────

#[derive(Clone, Default)]
struct Upstream {
    reqs: Arc<Mutex<Vec<Value>>>,
}

async fn upstream_messages(State(u): State<Upstream>, _h: HeaderMap, raw: Bytes) -> Response {
    u.reqs
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&raw).unwrap_or(Value::Null));
    let msg = json!({
        "id": "msg_mock", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 100, "output_tokens": 5,
                  "cache_creation_input_tokens": 7, "cache_read_input_tokens": 50}
    });
    ok_json(msg.to_string())
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

// ── mock v1 brain ───────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct MockBrain {
    bundle_calls: Arc<AtomicU64>,
    trace_reqs: Arc<Mutex<Vec<Value>>>,
    tools_reqs: Arc<Mutex<Vec<Value>>>,
    /// Fails the HANDSHAKE (GET /v1/bundle) — the v1-specific outage mode.
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
            "contracts": ["brain-api-dev/v0", "brain-api/v1"],
            "checkpoint_id": CKPT(),
            "tau_q": TAU_Q,
            "target_cov": "0.70",
            "grid": 1_000_000,
        })
        .to_string(),
    )
}

/// v1 trace scoring: mask rows LOW (cut), the rest never-cut — mirrors the
/// dev mock so the freezer machinery behaves identically.
async fn brain_trace(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.trace_reqs.lock().unwrap().push(req.clone());
    assert_eq!(
        req["contract"], "brain-api/v1",
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
            "timings_ms": {"embed": 0.0, "forward": 1.0}
        })
        .to_string(),
    )
}

/// v1 tool head: first roster name high, the rest low (equal mass + cut 0.70
/// keeps exactly the first).
async fn brain_tools(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.tools_reqs.lock().unwrap().push(req.clone());
    assert_eq!(req["contract"], "brain-api/v1");
    let names: Vec<String> = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    let scores: Vec<i64> = (0..names.len())
        .map(|i| if i == 0 { 990_000 } else { 100_000 + i as i64 })
        .collect();
    // equal masses (like the dev mock) so the 0.70 cut keeps exactly the top
    // scorer — the client's real chars//4 masses are asserted on the payload
    // side, not echoed back.
    let tokens: Vec<i64> = names.iter().map(|_| 100).collect();
    ok_json(
        json!({"names": names, "scores_q": scores, "tokens": tokens,
               "checkpoint_id": CKPT()})
        .to_string(),
    )
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
        .route("/v1/bundle", get(brain_bundle))
        .route("/v1/score/trace", post(brain_trace))
        .route("/v1/score/tools", post(brain_tools))
        .with_state(brain.clone());
    let bl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let brain_addr = bl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(bl, brain_router).await.unwrap() });

    let ledger = std::env::temp_dir().join(format!(
        "dasein-brain-v1-test-{}-{}.jsonl",
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
        contract: BrainContract::V1,
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

/// The v1 payload carries vectors + features + ids ONLY — no internal view,
/// no chunk text, no command/file/system strings — and the scores still
/// drive the freezer's trim exactly like the dev contract.
#[tokio::test]
async fn v1_payload_is_textless_and_trims() {
    let ctx = setup().await;
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 200);

    let treqs = ctx.brain.trace_reqs.lock().unwrap().clone();
    assert!(!treqs.is_empty(), "no v1 trace call");
    let p = &treqs[0];
    assert_eq!(p["contract"], "brain-api/v1");
    assert_eq!(p["checkpoint_id"].as_str().unwrap(), CKPT());
    assert_eq!(p["target_cov"], "0.70");

    // v1 structure: nodes + decided_struct + rel-4 pairs, mask-aligned.
    let nodes = p["nodes"].as_array().unwrap();
    assert!(!nodes.is_empty());
    for n in nodes {
        assert_eq!(n["emb_text"].as_array().unwrap().len(), 1024);
        assert_eq!(n["struct"].as_array().unwrap().len(), 21);
    }
    let mask = p["mask"].as_array().unwrap();
    assert!(!mask.is_empty());
    assert_eq!(p["decided_struct"].as_array().unwrap().len(), mask.len());
    for row in p["decided_struct"].as_array().unwrap() {
        assert_eq!(row.as_array().unwrap().len(), 49);
    }
    assert!(
        p.get("edges_supersession").is_some(),
        "rel-4 field is REQUIRED"
    );
    assert!(
        p.get("sys_emb").is_some(),
        "system prompt rides as a vector"
    );
    // the READ chunks carry salted ids + coordinates, never names
    let read = nodes
        .iter()
        .find(|n| n["kind"] == "read")
        .expect("a read node");
    assert_eq!(read["file_id"].as_str().unwrap().len(), 16);
    assert!(read["lo"].is_i64() && read["hi"].is_i64());
    assert_eq!(read["cmd_id"].as_str().unwrap().len(), 16);

    // NO raw-text slot: the dev contract's fields are absent...
    for k in ["messages", "live_gi", "chunk_checksum"] {
        assert!(
            p.get(k).is_none(),
            "dev field {k} leaked into the v1 payload"
        );
    }
    // ...and none of the conversation's strings appear ANYWHERE in the body.
    let wire = serde_json::to_string(p).unwrap();
    for leak in [
        "parser.py",            // file basename (and the bash-twin command)
        "compute_step_1",       // observation content
        "Fix the failing test", // task text
        "be terse",             // system prompt
        "Let me read the file", // assistant text
    ] {
        assert!(
            !wire.contains(leak),
            "raw text {leak:?} leaked into the v1 payload"
        );
    }

    // the trim happened exactly as under the dev contract
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let obs = serde_json::to_string(&sent.last().unwrap()["messages"][2]).unwrap();
    assert!(obs.contains("omitted"), "observation not trimmed: {obs}");
    assert!(
        obs.contains("re-read parser.py"),
        "no recovery pointer: {obs}"
    );

    let rows = ledger_rows(&ctx);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["fail_open"], false);
    assert_eq!(rows[0]["checkpoint_id"].as_str().unwrap(), CKPT());
    assert!(rows[0]["freeze_cut_tokens"].as_i64().unwrap() > 0);
    assert!(rows[0].get("scorer_fail_opens").is_none());
}

/// The /v1/bundle handshake happens ONCE per conversation (cached on the
/// scorer) — later turns reuse the checkpoint id.
#[tokio::test]
async fn v1_handshake_once_per_conversation() {
    let ctx = setup().await;
    post_messages(&ctx, &body(convo_turn1())).await;
    let mut msgs = convo_turn1();
    msgs.push(json!({"role": "assistant", "content": "The bug is on line 12."}));
    msgs.push(json!({"role": "user", "content": "apply the fix"}));
    post_messages(&ctx, &body(msgs)).await;

    assert_eq!(ctx.brain.bundle_calls.load(Ordering::SeqCst), 1);
    assert_eq!(ctx.brain.trace_reqs.lock().unwrap().len(), 2);
}

/// Handshake failure = per-step fail-open (serve full, count it, keep the
/// request 200) and the next healthy call retries the skipped step.
#[tokio::test]
async fn v1_handshake_failure_fails_open_per_step_and_retries() {
    let ctx = setup().await;
    ctx.brain.fail_bundle.store(true, Ordering::SeqCst);
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 200);

    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let obs = serde_json::to_string(&sent[0]["messages"][2]).unwrap();
    assert!(
        !obs.contains("omitted"),
        "trimmed despite handshake failure"
    );
    assert!(
        ctx.brain.trace_reqs.lock().unwrap().is_empty(),
        "scored without a handshake"
    );
    let rows = ledger_rows(&ctx);
    assert_eq!(
        rows[0]["fail_open"], false,
        "request-level fail-open is wrong here"
    );
    assert_eq!(rows[0]["scorer_fail_opens"], 1);

    // heal: the same conversation's next call re-handshakes and replays the
    // skipped birth step.
    ctx.brain.fail_bundle.store(false, Ordering::SeqCst);
    let mut msgs = convo_turn1();
    msgs.push(json!({"role": "assistant", "content": "hm."}));
    msgs.push(json!({"role": "user", "content": "continue"}));
    post_messages(&ctx, &body(msgs)).await;
    assert!(ctx.brain.bundle_calls.load(Ordering::SeqCst) >= 2);
    assert!(
        !ctx.brain.trace_reqs.lock().unwrap().is_empty(),
        "healed brain never consulted"
    );
    let rows = ledger_rows(&ctx);
    assert!(rows[1].get("scorer_fail_opens").is_none());
}

/// v1 tool path: the roster rides as (name, schema-embedding, tokens) — the
/// serialized schema TEXT stays home — and the keep-set applies as usual.
#[tokio::test]
async fn v1_tools_payload_names_ride_schemas_do_not() {
    let ctx = setup().await;
    let tools = json!([
        {"name": "Read", "description": "reads files from the local filesystem",
         "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}}},
        {"name": "Bash", "description": "executes shell commands in a sandbox",
         "input_schema": {"type": "object"}},
        {"name": "Grep", "description": "searches file contents with regex",
         "input_schema": {"type": "object"}},
        {"name": "Glob", "description": "fast file pattern matching tool",
         "input_schema": {"type": "object"}}
    ]);
    let mut b = body(convo_turn1());
    b["tools"] = tools.clone();
    post_messages(&ctx, &b).await;

    let toolsr = ctx.brain.tools_reqs.lock().unwrap().clone();
    assert_eq!(toolsr.len(), 1);
    let p = &toolsr[0];
    let names: Vec<&str> = p["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Read", "Bash", "Grep", "Glob"]);
    for t in p["tools"].as_array().unwrap() {
        assert_eq!(t["emb"].as_array().unwrap().len(), 1024);
        assert!(t["tokens"].as_i64().unwrap() >= 1);
    }
    assert!(
        !p["nodes"].as_array().unwrap().is_empty(),
        "tool graph needs obs nodes"
    );
    let wire = serde_json::to_string(p).unwrap();
    for leak in [
        "reads files from the local filesystem", // schema descriptions stay home
        "input_schema",
        "file_path",
        "parser.py",
    ] {
        assert!(
            !wire.contains(leak),
            "raw text {leak:?} leaked into the v1 tools payload"
        );
    }

    // equal mass, first name scored high, cut 0.70 of 4 ⇒ keep exactly Read.
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let fwd_tools = sent[0]["tools"].as_array().unwrap();
    assert_eq!(fwd_tools.len(), 1);
    assert_eq!(fwd_tools[0]["name"], "Read");

    let rows = ledger_rows(&ctx);
    assert_eq!(rows[0]["tools_total"], 4);
    assert_eq!(rows[0]["tools_kept"], 1);
}

// ── live cross-contract parity (#[ignore]: needs a running brain) ───────────

/// One BirthQuery scored through BOTH contracts against a REAL brain —
/// the fast per-query twin of scripts/parity_v1.sh. The brain must run the
/// HASH embed backend (the client featurizes with hash here), e.g. the one
/// golden_replay.sh / parity_v1.sh starts:
///   cd packages/brain && DASEIN_EMBED_BACKEND=hash DASEIN_CKPT=~/.dasein/brain/curator_v4_prod.pt \
///     .venv/bin/python -m uvicorn --factory dasein_brain.app:create_app --port 8093
///   DASEIN_BRAIN_URL=http://127.0.0.1:8093 \
///     cargo test -p dasein-proxy --test proxy_brain_v1 -- --ignored
#[test]
#[ignore = "needs a running hash-backend brain (DASEIN_BRAIN_URL)"]
fn live_brain_dev_v1_score_parity() {
    use dasein_engine::chunking::{chunk_assistant, chunk_observation, ChunkMode, DEFAULT_WIN};
    use dasein_engine::freeze::{BirthQuery, ChunkScorer};
    use dasein_proxy::brain::BrainScorer;
    use sha2::{Digest, Sha256};

    let url = std::env::var("DASEIN_BRAIN_URL").unwrap_or_else(|_| "http://127.0.0.1:8093".into());
    let cfg = |contract| BrainConfig {
        url: url.clone(),
        key: std::env::var("DASEIN_BRAIN_KEY")
            .ok()
            .filter(|k| !k.is_empty()),
        timeout: Duration::from_secs(120),
        target_cov: "0.70".into(),
        tool_cut: 0.70,
        tool_prune: false,
        contract,
        embed_backend: "hash".into(),
        embed_url: None,
        onnx_dir: None,
    };

    // A strict-alternation internal view whose freezer parse we can mirror
    // by hand (task = step 0; one observation step 1).
    let src = (1..=24)
        .map(|i| format!("{i}: def handler_{i}(x):"))
        .collect::<Vec<_>>()
        .join("\n");
    let obs_text = format!("returncode: 0\n{src}");
    let messages = vec![
        json!({"role": "system", "content": "be terse"}),
        json!({"role": "user", "content": "Fix handler_3 in q.py"}),
        json!({"role": "assistant", "content": "Read it.",
               "extra": {"actions": [{"tool": "Read", "command": "cat q.py"}]}}),
        json!({"role": "tool", "content": obs_text.clone()}),
    ];
    // freezer-parse mirror: obs chunks (step 1) then the assistant chunk.
    let mut chunks = chunk_observation(
        "cat q.py",
        &obs_text,
        1,
        DEFAULT_WIN,
        Some(10),
        ChunkMode::Fixed,
    );
    chunks.extend(chunk_assistant("Read it.", 1, DEFAULT_WIN));
    chunks.sort_by_key(|c| (c.step, i64::from(c.kind == "reasoning")));
    let checksum = {
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
    let live_gi: Vec<usize> = (0..chunks.len()).collect();
    let live_owner = vec![0usize; chunks.len()];
    let mask: Vec<usize> = (0..chunks.len())
        .filter(|&j| chunks[j].step == 1 && chunks[j].evict != "provider")
        .collect();
    let q = BirthQuery {
        cur_step: 1,
        task_text: "Fix handler_3 in q.py".into(),
        recent_cmds: "cat q.py".into(),
        live: &chunks,
        live_owner: &live_owner,
        live_gi: live_gi.clone(),
        messages: &messages,
        chunk_checksum: checksum,
        mask: mask.clone(),
    };
    assert!(!mask.is_empty());

    let dev = BrainScorer::new(cfg(BrainContract::Dev), "live-parity".into())
        .score(&q)
        .expect("dev contract score");
    let v1 = BrainScorer::new(cfg(BrainContract::V1), "live-parity".into())
        .score(&q)
        .expect("v1 contract score");
    assert_eq!(dev.tau_q, v1.tau_q, "tau diverged across contracts");
    assert_eq!(
        dev.scores_q, v1.scores_q,
        "scores diverged across contracts (dev vs v1)"
    );
    println!(
        "live parity OK: {} live rows, {} decided, tau_q {}",
        chunks.len(),
        mask.len(),
        dev.tau_q
    );
}
