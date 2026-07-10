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

use dasein_engine::chunking::ChunkMode;
use dasein_engine::freeze::{FreezeConfig, FreezeError, Freezer, StubScorer};
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

#[test]
fn malformed_input_fails_open() {
    let cfg = FreezeConfig {
        read_lines: Some(10),
        mode: ChunkMode::Fixed,
        tau_fixed_q: Some(500_000),
        min_run_tokens: 10,
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
