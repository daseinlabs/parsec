//! Governor (L2) tests — two layers:
//!
//! 1. Reference parity: replay fixtures generated from
//!    adaptive-context-clean's governor.py/doom.py (regenerate with
//!    `python3 parity/gen_governor_fixtures.py`) against the exact ports in
//!    `governor.rs`. Floats compare EXACTLY (Python repr round-trips to the
//!    same f64 bits; the arithmetic is identical).
//!
//! 2. Integration (mock brain + mock upstream, contract Track B item 8):
//!    rule fire appends exactly one [SUPERVISOR] turn with no anchor on it
//!    and dedupes next turn; the kill latch injects the kill directive every
//!    turn; advise mode is byte-identical to off on the served wire while
//!    the ledger carries the governor seams; a failing rules endpoint
//!    fail-opens (counted) without touching the request.

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
use parsec_proxy::governor::{
    self, action_of, budget_features, edited_src_files, is_test_path, loop_feats,
    loop_fracs_default, struct_edited_src, GovMode, GovernorConfig,
};
use parsec_proxy::server::{router, AppState};
use parsec_proxy::splice::strip_cache_control;

// ═══════════════════════════ 1. reference parity ═══════════════════════════

fn fixtures() -> Value {
    let path = format!(
        "{}/parity/fixtures/governor.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing governor fixtures — run: python3 parity/gen_governor_fixtures.py")
    });
    serde_json::from_str(&data).expect("fixture parse")
}

fn f64s(v: &Value) -> Vec<f64> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap())
        .collect()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn parity_loop_fracs() {
    for c in fixtures()["loop_fracs"].as_array().unwrap() {
        let cmds = strs(&c["cmds"]);
        let got = loop_fracs_default(&cmds);
        assert_eq!(got, f64s(&c["expected"]), "loop_fracs '{}'", c["name"]);
    }
}

#[test]
fn parity_loop_feats() {
    for c in fixtures()["loop_feats"].as_array().unwrap() {
        let lf = f64s(&c["lf"]);
        let got = loop_feats(&lf, c["t"].as_i64().unwrap(), 0.34);
        assert_eq!(
            got.to_vec(),
            f64s(&c["expected"]),
            "loop_feats '{}'",
            c["name"]
        );
    }
}

#[test]
fn parity_is_test_path() {
    for c in fixtures()["is_test_path"].as_array().unwrap() {
        let got = is_test_path(c["path"].as_str().unwrap());
        assert_eq!(
            got,
            c["expected"].as_bool().unwrap(),
            "is_test_path '{}'",
            c["name"]
        );
    }
}

#[test]
fn parity_edited_src_files() {
    for c in fixtures()["edited_src_files"].as_array().unwrap() {
        let got: Vec<String> = edited_src_files(c["action"].as_str().unwrap())
            .into_iter()
            .collect();
        assert_eq!(
            got,
            strs(&c["expected"]),
            "edited_src_files '{}'",
            c["name"]
        );
    }
}

#[test]
fn parity_struct_edited_src() {
    for c in fixtures()["struct_edited_src"].as_array().unwrap() {
        let got: Vec<String> = struct_edited_src(&c["message"]).into_iter().collect();
        assert_eq!(
            got,
            strs(&c["expected"]),
            "struct_edited_src '{}'",
            c["name"]
        );
    }
}

#[test]
fn parity_budget_features() {
    for c in fixtures()["budget_features"].as_array().unwrap() {
        let msgs = c["messages"].as_array().unwrap().clone();
        let (cum, n) = budget_features(&msgs);
        let exp = c["expected"].as_array().unwrap();
        assert_eq!(cum, exp[0].as_f64().unwrap(), "budget cum '{}'", c["name"]);
        assert_eq!(
            n as i64,
            exp[1].as_i64().unwrap(),
            "budget nsteps '{}'",
            c["name"]
        );
    }
}

#[test]
fn parity_action_of() {
    for c in fixtures()["action_of"].as_array().unwrap() {
        let got = action_of(&c["message"]);
        assert_eq!(
            Value::String(got),
            c["expected"],
            "action_of '{}'",
            c["name"]
        );
    }
}

// ═══════════════════════════ 2. integration ════════════════════════════════

/// Grid tau the mock brain hands out (the real ckpt's 0.70-cov tau).
const TAU_Q: i64 = 315_265;

#[derive(Clone, Default)]
struct Upstream {
    reqs: Arc<Mutex<Vec<Value>>>,
    raws: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Answer the NEXT /v1/messages with a 529 (overloaded) — the retry-loss
    /// scenario for the one-shot latch commit gating.
    fail_next: Arc<AtomicBool>,
}

async fn upstream_messages(State(u): State<Upstream>, _h: HeaderMap, raw: Bytes) -> Response {
    u.reqs
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&raw).unwrap_or(Value::Null));
    u.raws.lock().unwrap().push(raw.to_vec());
    if u.fail_next.swap(false, Ordering::SeqCst) {
        return Response::builder()
            .status(StatusCode::from_u16(529).unwrap())
            .body(Body::from("overloaded"))
            .unwrap();
    }
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

/// Mock brain: trace/tools like proxy_brain.rs, plus the governor endpoints
/// (/v1/score/rules with a fixed-fire_step roster + /v1/neighbors).
#[derive(Clone, Default)]
struct MockBrain {
    trace_reqs: Arc<Mutex<Vec<Value>>>,
    rules_reqs: Arc<Mutex<Vec<Value>>>,
    neighbors_reqs: Arc<Mutex<Vec<Value>>>,
    /// doom_q attached to trace responses when the request carried `gf`.
    doom_q: Arc<Mutex<Option<i64>>>,
    /// rule1 scores above tau (p_q 900k) when true, below (100k) when false.
    rules_fire: Arc<AtomicBool>,
    /// /v1/score/rules answers 500 when true.
    rules_fail: Arc<AtomicBool>,
    /// /v1/neighbors median (None ⇒ null median: hoods inert).
    nbr_median: Arc<Mutex<Option<f64>>>,
    /// score/trace answers 500 with a multi-byte UTF-8 body straddling byte
    /// 200 when true (the byte-slice panic regression, finding 3).
    trace_fail_utf8: Arc<AtomicBool>,
    /// /v1/bundle advertises a served doom head (set true in setup_with —
    /// the finding-6 gate needs it for gf to ride the v1 wire).
    bundle_doom: Arc<AtomicBool>,
}

async fn brain_trace(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.trace_reqs.lock().unwrap().push(req.clone());
    if b.trace_fail_utf8.load(Ordering::SeqCst) {
        // 199 ASCII bytes then multi-byte chars: byte 200 falls INSIDE the
        // first 'é' — a byte-slice clip of this body panics.
        let detail = format!("{}{}", "a".repeat(199), "é".repeat(5));
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(detail))
            .unwrap();
    }
    // Dev requests carry live_gi (one score per live chunk); v1 requests
    // carry featurized nodes (one score per node) — same convention as the
    // proxy_brain_v1.rs mock.
    let n_live = req["live_gi"]
        .as_array()
        .or_else(|| req["nodes"].as_array())
        .unwrap()
        .len();
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
    let mut resp = json!({
        "scores_q": scores, "tau_q": TAU_Q,
        "checkpoint_id": "c".repeat(64),
        "timings_ms": {"embed": 1.0, "forward": 1.0}
    });
    if req.get("gf").is_some() {
        if let Some(dq) = *b.doom_q.lock().unwrap() {
            resp["doom_q"] = json!(dq);
        }
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(resp.to_string()))
        .unwrap()
}

async fn brain_tools(_h: HeaderMap, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    let names: Vec<String> = req["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let scores: Vec<i64> = names.iter().map(|_| 990_000).collect();
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

async fn brain_rules(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.rules_reqs.lock().unwrap().push(req);
    if b.rules_fail.load(Ordering::SeqCst) {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("rules boom"))
            .unwrap();
    }
    let p_q: i64 = if b.rules_fire.load(Ordering::SeqCst) {
        900_000
    } else {
        100_000
    };
    // Fixed fire_step (the nearest-earlier clamp in the reference): the same
    // (eid, fire_step) across turns exercises the dedupe.
    let resp = json!({
        "scores_q": {"rule1": p_q},
        "rules": [{"eid": "rule1",
                   "text": "After implementing a fix, you MUST run the existing test suite \
                            for the modified module BEFORE declaring success.",
                   "p_q": p_q, "fire_step": 1}],
        "tau_hint_q": 150_000,
        "checkpoint_id": "c".repeat(64)
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(resp.to_string()))
        .unwrap()
}

/// GET /v1/bundle — the v1 handshake: checkpoint id + the doom capability
/// the client gates `gf` on (mirrors the brain app.py bundle_info shape:
/// `"doom": {"gf": int, "served": bool}`).
async fn brain_bundle(State(b): State<MockBrain>) -> Response {
    let served = b.bundle_doom.load(Ordering::SeqCst);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "checkpoint_id": "c".repeat(64),
                "doom": {"gf": if served { 4 } else { 0 }, "served": served}
            })
            .to_string(),
        ))
        .unwrap()
}

async fn brain_neighbors(State(b): State<MockBrain>, raw: Bytes) -> Response {
    let req: Value = serde_json::from_slice(&raw).unwrap();
    b.neighbors_reqs.lock().unwrap().push(req);
    let median = *b.nbr_median.lock().unwrap();
    let resp = json!({
        "nbr_cost_median": median,
        "nbr_count": if median.is_some() { 16 } else { 0 },
        "neighbors_active": median.is_some(),
        "checkpoint_id": "c".repeat(64)
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(resp.to_string()))
        .unwrap()
}

struct Ctx {
    url: String,
    upstream: Upstream,
    brain: MockBrain,
    ledger: PathBuf,
    state: Arc<AppState>,
    http: reqwest::Client,
}

/// `serve_rules_route=false` leaves /v1/score/rules unrouted (404) — the
/// endpoint-missing flavor of the rules fail-open.
async fn setup(gov: GovernorConfig, serve_rules_route: bool) -> Ctx {
    setup_with(gov, serve_rules_route, BrainContract::Dev).await
}

async fn setup_with(gov: GovernorConfig, serve_rules_route: bool, contract: BrainContract) -> Ctx {
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
    brain.bundle_doom.store(true, Ordering::SeqCst); // doom-capable by default
    let mut brain_router = Router::new()
        .route("/v1/bundle", get(brain_bundle))
        .route("/v1/score/trace", post(brain_trace))
        .route("/v1/score/tools", post(brain_tools))
        .route("/v1/neighbors", post(brain_neighbors));
    if serve_rules_route {
        brain_router = brain_router.route("/v1/score/rules", post(brain_rules));
    }
    let brain_router = brain_router.with_state(brain.clone());
    let bl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let brain_addr = bl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(bl, brain_router).await.unwrap() });

    let ledger = std::env::temp_dir().join(format!(
        "parsec-governor-test-{}-{}.jsonl",
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
        contract,
        score_memo_dir: None,
    };
    let state = Arc::new(AppState::with_brain_governor(
        format!("http://{up_addr}"),
        ledger.clone(),
        Some(cfg),
        gov,
    ));
    let pl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = pl.local_addr().unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(pl, app).await.unwrap() });

    Ctx {
        url: format!("http://{proxy_addr}"),
        upstream,
        brain,
        ledger,
        state,
        http: reqwest::Client::new(),
    }
}

fn gov(mode: GovMode) -> GovernorConfig {
    GovernorConfig {
        mode,
        ..GovernorConfig::default()
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

fn convo_turn2() -> Vec<Value> {
    let mut msgs = convo_turn1();
    msgs.push(json!({"role": "assistant", "content": "The bug is on line 12."}));
    msgs.push(json!({"role": "user", "content": "apply the fix"}));
    msgs
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

fn has_cache_control(msg: &Value) -> bool {
    msg.get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn off_mode_makes_no_governor_calls_and_no_gf() {
    let ctx = setup(gov(GovMode::Off), true).await;
    ctx.brain.rules_fire.store(true, Ordering::SeqCst);
    assert_eq!(
        post_messages(&ctx, &body(convo_turn1())).await.status(),
        200
    );
    assert_eq!(
        post_messages(&ctx, &body(convo_turn2())).await.status(),
        200
    );

    assert!(ctx.brain.rules_reqs.lock().unwrap().is_empty());
    assert!(ctx.brain.neighbors_reqs.lock().unwrap().is_empty());
    for t in ctx.brain.trace_reqs.lock().unwrap().iter() {
        assert!(t.get("gf").is_none(), "gf attached with governor off: {t}");
    }
    for r in ledger_rows(&ctx) {
        assert!(r.get("governor_mode").is_none(), "gov seam with mode off");
        assert!(r.get("gov_rule_fires").is_none());
    }
    // No [SUPERVISOR] anywhere on the wire.
    for s in ctx.upstream.reqs.lock().unwrap().iter() {
        assert!(!serde_json::to_string(s).unwrap().contains("[SUPERVISOR]"));
    }
    assert_eq!(ctx.state.gov_fail_open_count.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn advise_mode_wire_identical_to_off_with_ledger_seams() {
    let off = setup(gov(GovMode::Off), true).await;
    let adv = setup(gov(GovMode::Advise), true).await;
    adv.brain.rules_fire.store(true, Ordering::SeqCst); // would fire — must not touch the wire
    for ctx in [&off, &adv] {
        assert_eq!(post_messages(ctx, &body(convo_turn1())).await.status(), 200);
        assert_eq!(post_messages(ctx, &body(convo_turn2())).await.status(), 200);
    }
    // Served bytes: byte-identical to off (advise never touches the wire).
    let off_raws = off.upstream.raws.lock().unwrap().clone();
    let adv_raws = adv.upstream.raws.lock().unwrap().clone();
    assert_eq!(off_raws, adv_raws, "advise altered the served bytes");

    // ...while gf rode the BRAIN wire and the ledger carries the seams.
    assert!(adv
        .brain
        .trace_reqs
        .lock()
        .unwrap()
        .iter()
        .any(|t| t.get("gf").is_some()));
    let gf = adv.brain.trace_reqs.lock().unwrap()[0]["gf"].clone();
    if !gf.is_null() {
        assert_eq!(gf.as_array().unwrap().len(), 4);
    }
    assert!(!adv.brain.rules_reqs.lock().unwrap().is_empty());
    let rows = ledger_rows(&adv);
    assert_eq!(rows[0]["governor_mode"], "advise");
    assert_eq!(rows[0]["gov_directive_injected"], false);
    assert_eq!(rows[0]["gov_rule_fires"], 1); // recorded, never injected
    assert!(rows[0].get("gov_cum_tok").is_some());
    assert!(rows[0].get("gov_n_src").is_some());
    // The off proxy's rows carry none of it.
    assert!(ledger_rows(&off)[0].get("governor_mode").is_none());
}

#[tokio::test]
async fn rule_fire_appends_one_supervisor_turn_and_dedupes() {
    let ctx = setup(gov(GovMode::On), true).await;
    ctx.brain.rules_fire.store(true, Ordering::SeqCst);
    assert_eq!(
        post_messages(&ctx, &body(convo_turn1())).await.status(),
        200
    );

    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let msgs1 = sent[0]["messages"].as_array().unwrap().clone();
    assert_eq!(msgs1.len(), 4, "expected exactly one appended turn");
    let d = &msgs1[3];
    assert_eq!(d["role"], "user");
    let dtext = d["content"][0]["text"].as_str().unwrap();
    assert!(
        dtext.starts_with("[SUPERVISOR] MANDATORY RULE (learned from previous failed runs"),
        "directive text: {dtext}"
    );
    assert!(dtext.contains("run the existing test suite"));
    // Breakpoint discipline: the moving directive tail is NEVER an anchor.
    assert!(!has_cache_control(d), "anchor on the directive turn");
    assert!(
        msgs1[..3].iter().any(has_cache_control),
        "no anchor on the real messages"
    );

    // Next turn: same (eid, fire_step) from the brain — deduped, no append,
    // and every previously-served turn stays byte-identical.
    assert_eq!(
        post_messages(&ctx, &body(convo_turn2())).await.status(),
        200
    );
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let msgs2 = sent[1]["messages"].as_array().unwrap().clone();
    assert_eq!(msgs2.len(), 5, "dedupe failed: turn re-appended");
    assert!(!serde_json::to_string(&msgs2)
        .unwrap()
        .contains("[SUPERVISOR]"));
    for j in 0..3 {
        assert_eq!(
            strip_cache_control(&msgs1[j]),
            strip_cache_control(&msgs2[j]),
            "resident turn {j} changed bytes"
        );
    }

    let rows = ledger_rows(&ctx);
    assert_eq!(rows[0]["governor_mode"], "on");
    assert_eq!(rows[0]["gov_rule_fires"], 1);
    assert_eq!(rows[0]["gov_directive_injected"], true);
    assert_eq!(rows[1]["gov_rule_fires"], 0);
    assert_eq!(rows[1]["gov_directive_injected"], false);
}

#[tokio::test]
async fn kill_latch_injects_kill_directive_every_turn() {
    // doom_k=1 + kill floor 0: one high doom score latches the kill.
    let cfg = GovernorConfig {
        mode: GovMode::On,
        doom_k: 1,
        kill_floor_tok: 0,
        ..GovernorConfig::default()
    };
    let ctx = setup(cfg, true).await;
    *ctx.brain.doom_q.lock().unwrap() = Some(900_000);
    ctx.brain.rules_fire.store(true, Ordering::SeqCst); // must be short-circuited by the kill

    assert_eq!(
        post_messages(&ctx, &body(convo_turn1())).await.status(),
        200
    );
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let msgs = sent[0]["messages"].as_array().unwrap();
    let d = msgs.last().unwrap();
    assert_eq!(d["role"], "user");
    assert_eq!(
        d["content"][0]["text"].as_str().unwrap(),
        governor::KILL_DIRECTIVE
    );
    assert!(!has_cache_control(d));
    // A killed run gets NO rule delivery (reference early return).
    assert!(ctx.brain.rules_reqs.lock().unwrap().is_empty());

    // Latched: the directive rides EVERY subsequent turn.
    assert_eq!(
        post_messages(&ctx, &body(convo_turn2())).await.status(),
        200
    );
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let msgs2 = sent[1]["messages"].as_array().unwrap();
    assert_eq!(
        msgs2.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap(),
        governor::KILL_DIRECTIVE
    );
    let rows = ledger_rows(&ctx);
    assert_eq!(rows[0]["gov_directive_injected"], true);
    assert_eq!(rows[0]["gov_doom_q"], 900_000);
    assert_eq!(rows[1]["gov_directive_injected"], true);
}

#[tokio::test]
async fn runaway_kill_via_neighbor_median_fetched_once() {
    let cfg = GovernorConfig {
        mode: GovMode::On,
        kill_floor_tok: 0,
        ..GovernorConfig::default()
    };
    let ctx = setup(cfg, true).await;
    *ctx.brain.nbr_median.lock().unwrap() = Some(10.0);

    // Turn 1: billed_in_cum is 0 — runaway 0.0, nothing fires.
    assert_eq!(
        post_messages(&ctx, &body(convo_turn1())).await.status(),
        200
    );
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    assert!(!serde_json::to_string(&sent[0])
        .unwrap()
        .contains("MANDATORY:"));

    // Turn 2: usage accrued 157 billed-side tokens ⇒ runaway 15.7 > 3.25.
    assert_eq!(
        post_messages(&ctx, &body(convo_turn2())).await.status(),
        200
    );
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let msgs2 = sent[1]["messages"].as_array().unwrap();
    assert_eq!(
        msgs2.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap(),
        governor::KILL_DIRECTIVE
    );
    // Neighbour baseline fetched ONCE per conversation.
    assert_eq!(ctx.brain.neighbors_reqs.lock().unwrap().len(), 1);
    let nb = &ctx.brain.neighbors_reqs.lock().unwrap()[0];
    assert_eq!(nb["contract"], "brain-api-dev/v0");
    assert_eq!(
        nb["task_text"].as_str().unwrap(),
        "Fix the failing test in parser.py"
    );
    let rows = ledger_rows(&ctx);
    assert_eq!(rows[1]["nbr_cost_median"], 10.0);
    assert!(rows[1]["gov_runaway_factor"].as_f64().unwrap() > 3.25);
}

#[tokio::test]
async fn rules_endpoint_failure_fails_open_and_is_counted() {
    // 500 flavor.
    let ctx = setup(gov(GovMode::On), true).await;
    ctx.brain.rules_fail.store(true, Ordering::SeqCst);
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 200);
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    assert_eq!(
        sent[0]["messages"].as_array().unwrap().len(),
        3,
        "500 injected a turn"
    );
    assert!(!serde_json::to_string(&sent[0])
        .unwrap()
        .contains("[SUPERVISOR]"));
    assert!(ctx.state.gov_fail_open_count.load(Ordering::Relaxed) >= 1);
    let rows = ledger_rows(&ctx);
    assert_eq!(
        rows[0]["fail_open"], false,
        "request-level fail-open is wrong here"
    );
    assert_eq!(rows[0]["gov_rule_fires"], 0);

    // 404 flavor (endpoint not routed at all — an older brain).
    let ctx = setup(gov(GovMode::On), false).await;
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 200);
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    assert_eq!(sent[0]["messages"].as_array().unwrap().len(), 3);
    assert!(ctx.state.gov_fail_open_count.load(Ordering::Relaxed) >= 1);
}

/// Cross-track seam guard: the governed v1 wire bodies the PROXY builds must
/// validate against the BRAIN's committed schema (extra=forbid server-side —
/// an unknown key is a 422 that silently fail-opens the feature forever, the
/// exact bug class this test pins). Asserts every outbound key is a schema
/// property and every required property is present, for the three governed
/// v1 requests: score/rules, /v1/neighbors, score/trace (+gf).
#[tokio::test]
async fn v2_governor_bodies_match_brain_schema() {
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/../contracts/schemas/brain-api-v2.schema.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("brain-api schema present"),
    )
    .unwrap();
    let check = |body: &Value, def: &str| {
        let d = &schema["$defs"][def];
        let props = d["properties"].as_object().unwrap();
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        for k in &keys {
            assert!(
                props.contains_key(k.as_str()),
                "{def}: outbound key {k:?} is not a schema property (server is \
                 extra=forbid — this 422s live)"
            );
        }
        for r in d["required"].as_array().unwrap() {
            assert!(
                body.get(r.as_str().unwrap()).is_some(),
                "{def}: required {r} missing from outbound body"
            );
        }
    };

    let ctx = setup_with(gov(GovMode::On), true, BrainContract::V2).await;
    ctx.brain.rules_fire.store(true, Ordering::SeqCst);
    *ctx.brain.doom_q.lock().unwrap() = Some(120_000);
    *ctx.brain.nbr_median.lock().unwrap() = Some(50_000.0);

    let resp = post_messages(&ctx, &body(convo_turn2())).await;
    assert_eq!(resp.status(), 200);

    let rules = ctx.brain.rules_reqs.lock().unwrap().clone();
    assert_eq!(rules.len(), 1, "one rules call per governed turn");
    assert_eq!(rules[0]["contract"], "brain-api/v2");
    check(&rules[0], "score_rules_request");
    for gone in ["mask", "decided_struct", "target_cov"] {
        assert!(
            rules[0].get(gone).is_none(),
            "rules body must not carry {gone}"
        );
    }
    assert!(!rules[0]["nodes"].as_array().unwrap().is_empty());

    let nbrs = ctx.brain.neighbors_reqs.lock().unwrap().clone();
    assert_eq!(nbrs.len(), 1, "neighbors fetched once per conversation");
    assert_eq!(nbrs[0]["contract"], "brain-api/v2");
    check(&nbrs[0], "neighbors_request");
    assert!(nbrs[0]["task_text"].is_string()); // v2: the server embeds

    let traces = ctx.brain.trace_reqs.lock().unwrap().clone();
    assert!(!traces.is_empty(), "trace scored");
    for t in &traces {
        assert_eq!(t["contract"], "brain-api/v2");
        check(t, "score_trace_request");
        let gf = t["gf"].as_array().expect("governed v2 trace carries gf");
        assert_eq!(gf.len(), 4);
    }

    // The full v2 loop fires on the wire: exactly one appended [SUPERVISOR]
    // turn rode upstream.
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let msgs = sent[0]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), convo_turn2().len() + 1);
    let last = serde_json::to_string(&msgs[msgs.len() - 1]).unwrap();
    assert!(last.contains("[SUPERVISOR]") && last.contains("MANDATORY RULE"));
}

/// Finding 2: one-shot latches (rule-fire dedupe, horizon) commit only after
/// upstream 2xx — a 529 the model never saw must NOT burn them; the retry
/// re-fires the identical directives, and only the delivered turn dedupes.
#[tokio::test]
async fn upstream_failure_does_not_burn_one_shot_latches() {
    let cfg = GovernorConfig {
        mode: GovMode::On,
        horizon_step: 1, // fires on turn 1 — a one-shot latch to lose
        ..GovernorConfig::default()
    };
    let ctx = setup(cfg, true).await;
    ctx.brain.rules_fire.store(true, Ordering::SeqCst);

    // Attempt 1: upstream 529 — the directive rode a request the model
    // never saw.
    ctx.upstream.fail_next.store(true, Ordering::SeqCst);
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 529);
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let d1 =
        serde_json::to_string(sent[0]["messages"].as_array().unwrap().last().unwrap()).unwrap();
    assert!(
        d1.contains("[HARNESS DIRECTIVE — MANDATORY]"),
        "no horizon on attempt 1"
    );
    assert!(d1.contains("[SUPERVISOR]"), "no rule fire on attempt 1");

    // Retry (client re-sends the same turn): latches were NOT burned — the
    // exact same directives fire again.
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 200);
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let d2 =
        serde_json::to_string(sent[1]["messages"].as_array().unwrap().last().unwrap()).unwrap();
    assert!(
        d2.contains("[HARNESS DIRECTIVE — MANDATORY]"),
        "horizon lost to the 529"
    );
    assert!(d2.contains("[SUPERVISOR]"), "rule fire lost to the 529");

    // Turn 2 after the 2xx: both consumables committed — nothing appended.
    let resp = post_messages(&ctx, &body(convo_turn2())).await;
    assert_eq!(resp.status(), 200);
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let msgs = sent[2]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5, "directive re-appended after commit");
    let all = serde_json::to_string(msgs).unwrap();
    assert!(!all.contains("[SUPERVISOR]"));
    assert!(!all.contains("[HARNESS DIRECTIVE"));
}

/// Finding 3: a multi-byte UTF-8 brain error body straddling byte 200 used
/// to panic the byte-slice clip inside spawn_blocking, dropping the taken
/// freezer. Now: per-step scorer fail-open, freezer memo intact next turn.
#[tokio::test]
async fn utf8_brain_error_body_fails_open_without_panic() {
    let ctx = setup(gov(GovMode::Off), true).await;
    ctx.brain.trace_fail_utf8.store(true, Ordering::SeqCst);
    let resp = post_messages(&ctx, &body(convo_turn1())).await;
    assert_eq!(resp.status(), 200);
    let rows = ledger_rows(&ctx);
    assert_eq!(
        rows[0]["fail_open"], false,
        "panic escalated a per-step fail-open to request level"
    );
    assert_eq!(rows[0]["scorer_fail_opens"], 1);
    // Served FULL (birth step undecided), not trimmed.
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    assert!(!serde_json::to_string(&sent[0]).unwrap().contains("omitted"));

    // Heal: the same conversation's freezer survived — the birth step is
    // retried and the resident turns replay byte-identically.
    ctx.brain.trace_fail_utf8.store(false, Ordering::SeqCst);
    let calls_before = ctx.brain.trace_reqs.lock().unwrap().len();
    let resp = post_messages(&ctx, &body(convo_turn2())).await;
    assert_eq!(resp.status(), 200);
    assert!(
        ctx.brain.trace_reqs.lock().unwrap().len() > calls_before,
        "healed brain never re-consulted"
    );
    let rows = ledger_rows(&ctx);
    assert!(rows[1].get("scorer_fail_opens").is_none());
    let sent = ctx.upstream.reqs.lock().unwrap().clone();
    let first = sent[0]["messages"].as_array().unwrap();
    let second = sent[1]["messages"].as_array().unwrap();
    for (j, (a, b)) in first.iter().zip(second.iter()).take(3).enumerate() {
        assert_eq!(
            strip_cache_control(a),
            strip_cache_control(b),
            "resident turn {j} changed bytes after the error"
        );
    }
}

/// Finding 6: on the v1 contract, `gf` rides only when the handshaken
/// bundle SERVES a doom head — a pre-gf brain (extra=forbid) must never be
/// sent the unknown key (it would 422 every trace: total curation outage).
#[tokio::test]
async fn v1_gf_gated_on_bundle_doom_capability() {
    // Doom-less bundle: no gf on any trace body.
    let ctx = setup_with(gov(GovMode::On), true, BrainContract::V2).await;
    ctx.brain.bundle_doom.store(false, Ordering::SeqCst);
    let resp = post_messages(&ctx, &body(convo_turn2())).await;
    assert_eq!(resp.status(), 200);
    let traces = ctx.brain.trace_reqs.lock().unwrap().clone();
    assert!(!traces.is_empty());
    for t in &traces {
        assert!(
            t.get("gf").is_none(),
            "gf sent to a doom-less v1 brain: {t}"
        );
    }

    // Doom-capable bundle (the setup default): gf rides every trace.
    let ctx = setup_with(gov(GovMode::On), true, BrainContract::V2).await;
    let resp = post_messages(&ctx, &body(convo_turn2())).await;
    assert_eq!(resp.status(), 200);
    let traces = ctx.brain.trace_reqs.lock().unwrap().clone();
    assert!(!traces.is_empty());
    for t in &traces {
        assert_eq!(t["gf"].as_array().map(Vec::len), Some(4), "gf missing: {t}");
    }
}

/// Finding 5: a curation fail-open (parseable body) must still accrue the
/// billed usage into the conversation's governor memo — the kill floor
/// otherwise undercounts on exactly the blow-out conversations.
#[tokio::test]
async fn fail_open_request_still_accrues_billed_tokens() {
    let ctx = setup(gov(GovMode::Advise), true).await;
    // messages is not an array: curation bails, the request fail-opens.
    let bad = json!({
        "model": "claude-sonnet-5",
        "max_tokens": 16,
        "system": "be terse",
        "messages": 42
    });
    let resp = post_messages(&ctx, &bad).await;
    assert_eq!(resp.status(), 200);
    let rows = ledger_rows(&ctx);
    assert_eq!(rows[0]["fail_open"], true);
    assert_ne!(
        rows[0]["conv_id"], "",
        "fail-open row lost its conversation id"
    );
    // The billed side (100 + 50 read + 7 write) landed in the memo anyway.
    let convs = ctx.state.convs.lock().unwrap();
    assert_eq!(convs.len(), 1, "expected exactly the accrual-created memo");
    let cs = convs.values().next().unwrap();
    assert_eq!(cs.gov.billed_in_cum, 157);
}
