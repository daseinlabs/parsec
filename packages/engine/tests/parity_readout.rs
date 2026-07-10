//! READOUT parity suite (DIRECTION.md §7b): replays conversations through the
//! Rust 49-col decided_struct port and asserts f32 BIT-equality against the
//! vendored brain reference (dasein_brain.scorer._het_readout — the serving
//! truth for curator_v4_prod.pt). Regenerate fixtures with:
//!   ../brain/.venv/bin/python parity/gen_readout_fixtures.py
//!
//! One documented epsilon: dupcos col 47 (max cosine). The reference computes
//! it through numpy float32 norm + BLAS matvec whose summation order is
//! SIMD/machine-dependent (two numpy hosts already disagree in the last ulps);
//! the Rust port accumulates sequentially in f32. Col 48 (1/(1+Δstep) to the
//! argmax match) stays BIT-exact — a tolerance there would hide an argmax
//! flip, which is a real divergence, not rounding.

use dasein_engine::chunking::Chunk;
use dasein_engine::readout::{decided_struct, Changeprone, ReadoutCtx, READ_STRUCT};
use serde_json::Value;

const DUPCOS_COS_COL: usize = 47;
const DUPCOS_EPS: f32 = 5e-7;

fn load(name: &str) -> Value {
    let path = format!("{}/parity/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing readout parity fixtures — run: ../brain/.venv/bin/python parity/gen_readout_fixtures.py"
        )
    });
    serde_json::from_str(&data).expect("fixture parse")
}

#[test]
fn parity_decided_struct() {
    let changeprone =
        Changeprone::from_json(&load("changeprone.json").to_string()).expect("changeprone parse");
    let fx = load("readout.json");
    for case in fx["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let chunks: Vec<Chunk> = serde_json::from_value(case["chunks"].clone()).unwrap();
        let alive: Vec<usize> = serde_json::from_value(case["alive"].clone()).unwrap();
        let decided: Vec<usize> = serde_json::from_value(case["decided"].clone()).unwrap();
        let steps: Vec<(String, String)> = serde_json::from_value(case["steps"].clone()).unwrap();
        let embs: Vec<Vec<f32>> = case["embs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                row.as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_f64().unwrap() as f32)
                    .collect()
            })
            .collect();
        let ctx = ReadoutCtx {
            chunks: &chunks,
            alive: &alive,
            decided: &decided,
            task_text: case["task_text"].as_str().unwrap(),
            recent: case["recent"].as_str().unwrap(),
            steps: &steps,
            cur_step: case["cur_step"].as_i64().unwrap(),
            t_total: case["t_total"].as_i64().unwrap(),
            age: case["age"].as_f64().unwrap(),
            changeprone: case["use_changeprone"]
                .as_bool()
                .unwrap()
                .then_some(&changeprone),
            content_embs: Some(&embs),
        };
        let got = decided_struct(&ctx);
        let want = case["expected"].as_array().unwrap();
        assert_eq!(got.len(), want.len(), "{name}: row count");
        for (r, (g, w)) in got.iter().zip(want).enumerate() {
            let w = w.as_array().unwrap();
            assert_eq!(w.len(), READ_STRUCT, "{name}: width at row {r}");
            for (k, (gv, wv)) in g.iter().zip(w).enumerate() {
                let want32 = wv.as_f64().unwrap() as f32;
                if k == DUPCOS_COS_COL {
                    assert!(
                        (gv - want32).abs() <= DUPCOS_EPS,
                        "{name}: row {r} col {k}: got {gv:e} want {want32:e} (eps {DUPCOS_EPS:e})"
                    );
                } else {
                    assert_eq!(
                        gv.to_bits(),
                        want32.to_bits(),
                        "{name}: row {r} col {k}: got {gv:e} want {want32:e}"
                    );
                }
            }
        }
    }
}
