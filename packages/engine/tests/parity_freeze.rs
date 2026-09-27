//! Freeze parity + determinism suite.
//!
//! Parity: Freezer with the stub scorer must reproduce the REAL Python
//! curate() (decision + render machinery, GNN stubbed identically) call-for-
//! call over growing conversations, across fixed-tau and per-pool-qhat
//! suites. Regenerate fixtures with:
//!   python3 parity/gen_freeze_fixtures.py
//!
//! Determinism (CI invariant §8.1, the reason this port exists): a COLD
//! Freezer serving only the final prefix must produce byte-identical output
//! to the incrementally driven one — the reference's session dicts cannot do
//! this (restart served different bytes); the pure fold must. Also asserted:
//! idempotent re-serve, and the purity guard resetting on edited history.

use parsec_engine::chunking::ChunkMode;
use parsec_engine::freeze::{
    BirthQuery, ChunkScorer, FreezeConfig, FreezeError, Freezer, ScoreError, ScoreResult,
    StubScorer,
};
use serde_json::{json, Value};

fn load(name: &str) -> Value {
    let path = format!(
        "{}/parity/fixtures/{}.json",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    let data = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing {name} fixtures — run: python3 parity/gen_freeze_fixtures.py")
    });
    serde_json::from_str(&data).expect("fixture parse")
}

fn config_of(fx: &Value) -> (FreezeConfig, Option<i64>) {
    let tau_q = fx["config"]["tau_q"].as_i64();
    let cfg = FreezeConfig {
        read_lines: fx["config"]["read_lines"].as_u64().map(|g| g as usize),
        mode: ChunkMode::Fixed,
        tau_fixed_q: tau_q,
        min_run_tokens: 10,
        // Reference behaviour: the Python freezer cuts assistant prose.
        cut_assistant: true,
        // Reference behaviour: births are decided at their own step and the
        // markers carry the terse "re-read" pointer (both frozen in the
        // fixtures). Product defaults differ; see FreezeConfig.
        protect_current: false,
        product_markers: false,
    };
    (cfg, tau_q)
}

fn run_suite(name: &str) {
    let fx = load(name);
    let (cfg, tau_q) = config_of(&fx);
    for conv in fx["conversations"].as_array().unwrap() {
        let cname = conv["name"].as_str().unwrap();
        let messages: Vec<Value> = conv["messages"].as_array().unwrap().clone();

        // Incremental: one freezer across all calls (the reference session shape).
        let mut inc = Freezer::new(cfg.clone(), StubScorer { tau_q });
        for call in conv["calls"].as_array().unwrap() {
            let k = call["prefix_len"].as_u64().unwrap() as usize;
            let got = inc.serve(&messages[..k]).unwrap();
            assert_eq!(
                serde_json::to_value(&got).unwrap(),
                call["expected"],
                "incremental serve parity, {name}/{cname} prefix_len={k}"
            );

            // Cold replay: a fresh freezer seeing ONLY this prefix must match
            // the incremental output byte-for-byte (restart-safety — the
            // property the reference lacks).
            let mut cold = Freezer::new(cfg.clone(), StubScorer { tau_q });
            let cold_got = cold.serve(&messages[..k]).unwrap();
            assert_eq!(
                serde_json::to_value(&cold_got).unwrap(),
                serde_json::to_value(&got).unwrap(),
                "cold == incremental, {name}/{cname} prefix_len={k}"
            );
        }

        // Registries after the full conversation match the Python curator's.
        assert_eq!(
            inc.registry_snapshot(),
            conv["registries"],
            "registries, {name}/{cname}"
        );

        // Re-serving the final prefix is idempotent (frozen forever) — note
        // this is a DOCUMENTED deviation from the reference, which re-serves
        // the untouched original on a repeat call at the same cur_step.
        let k = conv["calls"].as_array().unwrap().last().unwrap()["prefix_len"]
            .as_u64()
            .unwrap() as usize;
        let again = inc.serve(&messages[..k]).unwrap();
        assert_eq!(
            serde_json::to_value(&again).unwrap(),
            conv["calls"].as_array().unwrap().last().unwrap()["expected"],
            "idempotent re-serve, {name}/{cname}"
        );

        // Purity guard: editing a consumed message resets the memo; the warm
        // freezer must equal a cold one on the edited history.
        let mut edited = messages[..k].to_vec();
        if let Some(m0) = edited.first_mut() {
            let mut o = m0.as_object().cloned().unwrap_or_default();
            o.insert("content".into(), json!("EDITED TASK: different bytes"));
            *m0 = Value::Object(o);
        }
        let warm_edited = inc.serve(&edited).unwrap();
        let mut cold2 = Freezer::new(cfg.clone(), StubScorer { tau_q });
        let cold_edited = cold2.serve(&edited).unwrap();
        assert_eq!(
            serde_json::to_value(&warm_edited).unwrap(),
            serde_json::to_value(&cold_edited).unwrap(),
            "warm == cold after client edit, {name}/{cname}"
        );
    }
}

#[test]
fn parity_freeze_tau15() {
    run_suite("freeze_tau15");
}

#[test]
fn parity_freeze_tau85() {
    run_suite("freeze_tau85");
}

#[test]
fn parity_freeze_tauq() {
    run_suite("freeze_tauq");
}

/// Fails its first `fails_left` calls (brain down), then delegates to the
/// always-healthy StubScorer.
struct FlakyScorer {
    fails_left: usize,
    inner: StubScorer,
}
impl ChunkScorer for FlakyScorer {
    fn score(&mut self, q: &BirthQuery) -> Result<ScoreResult, ScoreError> {
        if self.fails_left > 0 {
            self.fails_left -= 1;
            return Err(ScoreError("brain unreachable".into()));
        }
        self.inner.score(q)
    }
}

// Content chosen so the stub scores fall below each step's pool qhat — every
// step trims something under a healthy scorer (asserted for step 2 below).
fn read_obs(tag: &str, n: usize) -> String {
    (1..=n)
        .map(|i| format!("def {tag}_helper_{i}(): return parse_{tag}(node={i}, strict=True)"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Three birth steps: task, then three read->observation turns (each step is
/// multi-owner — assistant text pool + read pool — so recovery exercises the
/// per-owner-pool tau calls too).
fn flaky_conv() -> Vec<Value> {
    vec![
        json!({"role": "user", "content": "TASK: audit the parser helpers"}),
        json!({"role": "assistant", "content": "Reading foo.py to map the helpers.",
               "extra": {"actions": [{"command": "cat foo.py"}]}}),
        json!({"role": "user", "content": read_obs("foo", 30)}),
        json!({"role": "assistant", "content": "Now bar.py for the callers.",
               "extra": {"actions": [{"command": "cat bar.py"}]}}),
        json!({"role": "user", "content": read_obs("bar", 30)}),
        json!({"role": "assistant", "content": "Finally baz.py for the tests.",
               "extra": {"actions": [{"command": "cat baz.py"}]}}),
        json!({"role": "user", "content": read_obs("baz", 30)}),
    ]
}

/// Scorer failure = per-step fail-open (docs/brain-serving-v0.md): the failed
/// step renders FULL and stays undecided; once the scorer heals, the retried
/// steps decide exactly as if they had been current — recovery == cold
/// replay, the core determinism claim.
#[test]
fn scorer_failure_defers_step_and_recovery_equals_cold_replay() {
    let messages = flaky_conv();
    let cfg = FreezeConfig::default();
    let mut fz = Freezer::new(
        cfg.clone(),
        FlakyScorer {
            fails_left: 0,
            inner: StubScorer { tau_q: None },
        },
    );

    // Turn 1: healthy — step 1 decided and committed.
    let first = fz.serve(&messages[..3]).unwrap();
    assert_eq!(fz.scorer_fail_opens, 0);

    // Brain goes down: the first serve of the full conversation fails at
    // step 2 (its first score call — steps 0/1 are already replayed).
    fz.scorer.fails_left = 1;
    let degraded = fz.serve(&messages).unwrap(); // (a) still Ok
    assert_eq!(fz.scorer_fail_opens, 1, "one fail-open counted"); // (b)
    for i in 3..7 {
        assert_eq!(
            degraded[i], messages[i],
            "step-2+ turns render FULL while the scorer is down (msg {i})"
        );
    }
    // Prior decisions still render (the fail-open is per-step, not per-request).
    assert_eq!(
        degraded[2], first[2],
        "decided step 1 keeps its frozen form"
    );

    // Healthy again: the deferred steps replay as if current.
    let recovered = fz.serve(&messages).unwrap();
    assert_eq!(fz.scorer_fail_opens, 1, "recovery adds no fail-opens");
    let mut fresh = Freezer::new(cfg, StubScorer { tau_q: None });
    let cold = fresh.serve(&messages).unwrap();
    assert_eq!(recovered, cold, "recovery == cold replay"); // (c)
    assert_eq!(
        fz.registry_snapshot(),
        fresh.registry_snapshot(),
        "recovered registries == fresh registries"
    ); // (d)
       // The full render above was a deferral, not a no-op: the healthy path
       // does trim the step that failed.
    assert_ne!(recovered[4], messages[4], "healthy scorer trims step 2");
}

#[test]
fn malformed_input_fails_open() {
    let cfg = FreezeConfig {
        read_lines: Some(10),
        mode: ChunkMode::Fixed,
        tau_fixed_q: Some(500_000),
        min_run_tokens: 10,
        // Reference behaviour: the Python freezer cuts assistant prose.
        cut_assistant: true,
        // Reference behaviour: births are decided at their own step and the
        // markers carry the terse "re-read" pointer (both frozen in the
        // fixtures). Product defaults differ; see FreezeConfig.
        protect_current: false,
        product_markers: false,
    };
    let mut fz = Freezer::new(cfg, StubScorer { tau_q: None });
    // Non-string text in a content part: the reference raises out of curate()
    // before any commit; serve must Err without advancing state.
    let bad = vec![
        json!({"role": "user", "content": "task"}),
        json!({"role": "assistant", "content": [{"type": "text", "text": 42}],
               "extra": {"actions": [{"command": "ls"}]}}),
        json!({"role": "user", "content": "obs"}),
    ];
    assert!(matches!(fz.serve(&bad), Err(FreezeError::Invalid(_))));
    // Truthy non-string command likewise.
    let bad2 = vec![
        json!({"role": "user", "content": "task"}),
        json!({"role": "assistant", "content": "x",
               "extra": {"actions": [{"command": 5}]}}),
        json!({"role": "user", "content": "obs"}),
    ];
    assert!(matches!(fz.serve(&bad2), Err(FreezeError::Invalid(_))));
    // Falsy non-string command falls through Python's `or` chain — valid.
    let ok = vec![
        json!({"role": "user", "content": "task"}),
        json!({"role": "assistant", "content": "x",
               "extra": {"actions": [{"command": 0, "query": "grep foo"}]}}),
        json!({"role": "user", "content": "obs text here"}),
    ];
    assert!(fz.serve(&ok).is_ok());
}
