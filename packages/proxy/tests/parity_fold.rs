//! Fold-back parity vs service/anthropic_shapes.py. Regenerate fixtures with:
//!   python3 parity/gen_fold_fixtures.py

use dasein_proxy::splice::{
    apply_curation, content_text, orig_fingerprint, place_cache_breakpoint, rewrite_first_text,
    strip_cache_control, system_to_text, FoldMap,
};
use serde_json::Value;

fn fixtures() -> Value {
    let path = format!("{}/parity/fixtures/fold.json", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing fold fixtures — run: python3 parity/gen_fold_fixtures.py")
    });
    serde_json::from_str(&data).expect("fixture parse")
}

#[test]
fn parity_fold_primitives() {
    let fx = fixtures();
    for c in fx["system_to_text"].as_array().unwrap() {
        let got = system_to_text(Some(&c["system"]));
        assert_eq!(
            Value::String(got),
            c["expected"],
            "system_to_text '{}'",
            c["name"]
        );
    }
    for c in fx["content_text"].as_array().unwrap() {
        let got = content_text(Some(&c["content"]));
        assert_eq!(
            Value::String(got),
            c["expected"],
            "content_text '{}'",
            c["name"]
        );
    }
    for c in fx["strip_cache_control"].as_array().unwrap() {
        assert_eq!(
            strip_cache_control(&c["content"]),
            c["expected"],
            "strip_cache_control '{}'",
            c["name"]
        );
    }
    for c in fx["fingerprint"].as_array().unwrap() {
        let got = orig_fingerprint(&c["content"]);
        assert_eq!(
            Value::String(got),
            c["expected"],
            "fingerprint '{}'",
            c["name"]
        );
    }
    for c in fx["rewrite_first_text"].as_array().unwrap() {
        let got = rewrite_first_text(Some(&c["content"]), c["new_text"].as_str().unwrap());
        assert_eq!(got, c["expected"], "rewrite_first_text '{}'", c["name"]);
    }
}

#[test]
fn parity_place_cache_breakpoint() {
    let fx = fixtures();
    let mut prior: Option<Vec<String>> = None;
    for c in fx["place_cache_breakpoint"].as_array().unwrap() {
        let directive = c["directive"].as_bool().unwrap();
        let (got, fps) = place_cache_breakpoint(&c["input"], directive, prior.as_deref());
        assert_eq!(
            got, c["expected"],
            "breakpoint body (directive={directive})"
        );
        let want_fps: Vec<String> = c["expected_fps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(fps, want_fps, "breakpoint fps (directive={directive})");
        prior = Some(fps);
    }
}

#[test]
fn parity_apply_curation() {
    let fx = fixtures();
    for c in fx["apply_curation"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let curated: Vec<Value> = c["curated"].as_array().unwrap().clone();
        if c["state"].is_null() {
            let got = apply_curation(&c["body"], &curated, None);
            assert_eq!(got, c["expected"], "apply_curation '{name}'");
        } else {
            // The reference mutates state["served_folds"]; reproduce that shape.
            let mut state = c["state"].as_object().unwrap().clone();
            let mut folds: FoldMap = state
                .get("served_folds")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let got = apply_curation(&c["body"], &curated, Some(&mut folds));
            assert_eq!(got, c["expected"], "apply_curation '{name}'");
            state.insert("served_folds".into(), Value::Object(folds));
            assert_eq!(
                Value::Object(state),
                c["expected_state"],
                "state after '{name}'"
            );
        }
    }
}
