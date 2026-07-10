//! to_internal parity vs service/anthropic_shapes.py + trace_contract.py.
//! Regenerate fixtures with:
//!   python3 parity/gen_internal_fixtures.py

use dasein_engine::pystr::py_json_dumps;
use dasein_proxy::internal::{bash_twin_command, derive_command, derive_query, to_internal};
use serde_json::Value;

fn fixtures() -> Value {
    let path = format!(
        "{}/parity/fixtures/internal.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing internal fixtures — run: python3 parity/gen_internal_fixtures.py")
    });
    serde_json::from_str(&data).expect("fixture parse")
}

#[test]
fn parity_to_internal() {
    let fx = fixtures();
    for c in fx["to_internal"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let got = Value::Array(to_internal(&c["body"]));
        // Compare Python-dumped bytes: catches key-order drift Value eq
        // would miss, and diffs stably.
        assert_eq!(
            py_json_dumps(&got),
            py_json_dumps(&c["expected"]),
            "to_internal '{name}'"
        );
    }
}

#[test]
fn parity_bash_twin_command() {
    let fx = fixtures();
    for c in fx["bash_twin_command"].as_array().unwrap() {
        let got = bash_twin_command(c["tool"].as_str().unwrap(), &c["args"])
            .map(Value::String)
            .unwrap_or(Value::Null);
        assert_eq!(got, c["expected"], "bash_twin_command '{}'", c["name"]);
    }
}

#[test]
fn parity_derive_command() {
    let fx = fixtures();
    for c in fx["derive_command"].as_array().unwrap() {
        let got = derive_command(c["tool"].as_str().unwrap(), &c["args"]);
        assert_eq!(
            Value::String(got),
            c["expected"],
            "derive_command '{}'",
            c["name"]
        );
    }
}

#[test]
fn parity_derive_query() {
    let fx = fixtures();
    for c in fx["derive_query"].as_array().unwrap() {
        let got = derive_query(c["tool"].as_str().unwrap(), &c["args"]);
        assert_eq!(
            Value::String(got),
            c["expected"],
            "derive_query '{}'",
            c["name"]
        );
    }
}
