//! Rust side of the embedder parity gate: embeds a battery of texts via
//! OnnxEmbedder and prints `{"vectors": [...]}` to stdout for
//! scripts/embed/parity_gate.py --rust-vectors to diff against Python
//! onnxruntime on the same export.
//!
//!   cargo run -p dasein-engine --features onnx --example onnx_battery -- \
//!       <model_dir> <battery.json> [model_file]

use dasein_engine::embed::{Embedder, OnnxEmbedder};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: onnx_battery <model_dir> <battery.json> [model_file]");
        std::process::exit(2);
    }
    let model_file = args.get(3).map(String::as_str).unwrap_or("model.onnx");
    let texts: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(&args[2]).expect("read battery"))
            .expect("battery must be a JSON list of strings");
    let embedder =
        OnnxEmbedder::load_model(Path::new(&args[1]), model_file, 8).expect("load model");
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let vectors = embedder.embed(&refs).expect("embed");
    println!(
        "{}",
        serde_json::json!({ "vectors": vectors, "dim": embedder.dim() })
    );
}
