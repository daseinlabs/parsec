//! Client-side embedder stack (v1 groundwork).
//!
//! ⚠️ Checkpoints are matched to the embedder that produced their training
//! vectors — for `curator_v4_prod.pt` that is UN-PREFIXED bge-large-en-v1.5
//! (1024-d, L2-normalized, CLS pooling, 512-token truncation; chunk text
//! clipped to 2000 chars and observation heads to 240 BEFORE embedding).
//! Never add the bge "Represent this sentence…" query instruction: the
//! service never applied it and the trained vectors don't carry it.
//!
//! v0 (inference-first, docs/brain-serving-v0.md) embeds SERVER-SIDE in
//! `packages/brain`. This module is the client-side stack that unlocks the
//! brain-api/v1 vectors+features contract:
//! - [`HashEmbedder`]: exact port of the vendored Python `_hash_embed` test
//!   backend (bit-parity pinned by `tests/parity_embed.rs`).
//! - [`RemoteEmbedder`]: dev fallback speaking the dasein-embed `/embed`
//!   JSON path (raw text on the wire — dev-raw posture only, never users).
//! - [`OnnxEmbedder`] (cargo feature `onnx`): local bge-large ONNX export
//!   (`scripts/embed/export_bge_onnx.py`), gated by the vector-parity gate
//!   (`scripts/embed/parity_gate.py`) against the live service before it may
//!   ship. Gate result numbers live in docs/brain-serving-v0.md appendix.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// bge-large-en-v1.5 output width; the checkpoint's expected vector dim.
pub const BGE_DIM: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// Network/IO failure reaching a backend.
    #[error("embed transport: {0}")]
    Transport(String),
    /// Backend answered, but not with what the protocol promises
    /// (bad status, wrong count, wrong dim).
    #[error("embed protocol: {0}")]
    Protocol(String),
    /// Local model failure (ONNX session, tokenizer).
    #[error("embed model: {0}")]
    Model(String),
}

/// One embedding backend. Vectors are f32, L2-normalized, `dim()` wide;
/// output order matches input order. Implementations must be deterministic
/// for a fixed (backend weights, input) — no RNG, no wall clock.
pub trait Embedder {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
    fn dim(&self) -> usize;
}

/// Shared backends: an expensive embedder (the ~1.3GB ONNX session) is
/// loaded once per process and handed out as `Arc` clones.
impl<T: Embedder + ?Sized> Embedder for std::sync::Arc<T> {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        (**self).embed(texts)
    }

    fn dim(&self) -> usize {
        (**self).dim()
    }
}

// ---------------------------------------------------------------------------
// Hash backend (deterministic pseudo-embeddings for tests/offline dev)
// ---------------------------------------------------------------------------

/// Exact port of `dasein_brain.vendored.embedding._hash_embed`: sha256 of
/// `"{counter}:{text}"`, 4-byte big-endian uints scaled to `/2^32 - 0.5`,
/// counter-extended until `dim`, L2-normalized. All arithmetic stays f64
/// (Python float) and narrows to f32 only at the boundary, so outputs are
/// bit-equal to `np.float32(reference)` — pinned by tests/parity_embed.rs.
pub fn hash_embed(text: &str, dim: usize) -> Vec<f32> {
    let mut out: Vec<f64> = Vec::with_capacity(dim);
    let mut counter: u64 = 0;
    while out.len() < dim {
        let h = Sha256::digest(format!("{counter}:{text}").as_bytes());
        for word in h.chunks_exact(4) {
            out.push(u32::from_be_bytes(word.try_into().unwrap()) as f64 / 4294967296.0 - 0.5);
            if out.len() >= dim {
                break;
            }
        }
        counter += 1;
    }
    // Python: sqrt(sum(x*x)) or 1.0 — sequential f64 fold, zero-guarded.
    let mut sq = 0.0f64;
    for x in &out {
        sq += x * x;
    }
    let norm = if sq.sqrt() == 0.0 { 1.0 } else { sq.sqrt() };
    out.iter().map(|x| (x / norm) as f32).collect()
}

/// Deterministic test backend (no network). Mirrors the brain's
/// `DASEIN_EMBED_BACKEND=hash` mode so client-side fixtures can be
/// generated hermetically with vectors the brain would agree on.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new(BGE_DIM)
    }
}

impl Embedder for HashEmbedder {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| hash_embed(t, self.dim)).collect())
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

// ---------------------------------------------------------------------------
// Remote backend (dasein-embed /embed JSON path — dev fallback)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model_id: &'a str,
    texts: &'a [&'a str],
}

#[derive(Deserialize)]
struct EmbedResponse {
    vectors: Vec<Vec<f32>>,
}

/// JSON client for the dasein-embed CPU proxy `POST /embed`
/// (`{"model_id", "texts"}` → `{"vectors"}`, fp32 lists, L2-normalized).
/// Dev-raw posture: raw text rides to OUR cluster — dev machines only.
/// Batches serially (reference client behavior: concurrency against the
/// single-GPU coalescing encoder buys ~1.2x and adds transport pressure).
pub struct RemoteEmbedder {
    url: String,
    model_id: String,
    batch: usize,
    dim: usize,
    client: reqwest::blocking::Client,
}

impl RemoteEmbedder {
    /// `url` is the full endpoint, e.g. `http://127.0.0.1:18080/embed`.
    /// The 300s default timeout rides the GPU scale-from-zero cold start.
    pub fn new(url: impl Into<String>) -> Result<Self, EmbedError> {
        Self::with_options(url, "bge-large-en-v1.5", 512, Duration::from_secs(300))
    }

    pub fn with_options(
        url: impl Into<String>,
        model_id: impl Into<String>,
        batch: usize,
        timeout: Duration,
    ) -> Result<Self, EmbedError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| EmbedError::Transport(e.to_string()))?;
        Ok(Self {
            url: url.into(),
            model_id: model_id.into(),
            batch: batch.max(1),
            dim: BGE_DIM,
            client,
        })
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let body = EmbedRequest {
            model_id: &self.model_id,
            texts,
        };
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .map_err(|e| EmbedError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            let snippet: String = text.chars().take(200).collect();
            return Err(EmbedError::Protocol(format!(
                "{status} from {}: {snippet}",
                self.url
            )));
        }
        let parsed: EmbedResponse = resp
            .json()
            .map_err(|e| EmbedError::Protocol(e.to_string()))?;
        if parsed.vectors.len() != texts.len() {
            return Err(EmbedError::Protocol(format!(
                "count mismatch: sent {} texts, got {} vectors",
                texts.len(),
                parsed.vectors.len()
            )));
        }
        for v in &parsed.vectors {
            if v.len() != self.dim {
                return Err(EmbedError::Protocol(format!(
                    "dim mismatch: expected {}, got {}",
                    self.dim,
                    v.len()
                )));
            }
        }
        Ok(parsed.vectors)
    }
}

impl Embedder for RemoteEmbedder {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        // Empty input short-circuits (the proxy would too — spare the call).
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.batch) {
            out.extend(self.embed_batch(chunk)?);
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

// ---------------------------------------------------------------------------
// ONNX backend (cargo feature "onnx") — local bge-large, no torch
// ---------------------------------------------------------------------------

#[cfg(feature = "onnx")]
mod onnx {
    use super::{EmbedError, Embedder, BGE_DIM};
    use ort::session::Session;
    use ort::value::Tensor;
    use std::path::Path;
    use std::sync::Mutex;
    use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

    /// bge-large max_seq_length; the service tokenizes with truncation=True
    /// which resolves to this.
    const MAX_TOKENS: usize = 512;

    /// Local bge-large-en-v1.5 via ONNX Runtime: HF tokenizer (truncation
    /// 512, batch-longest padding), CLS pooling (first token of
    /// last_hidden_state), L2 norm, f32. Loads the layout written by
    /// scripts/embed/export_bge_onnx.py (model.onnx + tokenizer.json).
    pub struct OnnxEmbedder {
        session: Mutex<Session>,
        tokenizer: Tokenizer,
        batch: usize,
    }

    impl OnnxEmbedder {
        /// `dir` = the export directory (default ~/.dasein/models/bge-large-onnx).
        pub fn load(dir: &Path) -> Result<Self, EmbedError> {
            Self::load_model(dir, "model.onnx", 32)
        }

        /// `model_file` selects the variant (model.onnx / model.int8.onnx).
        pub fn load_model(dir: &Path, model_file: &str, batch: usize) -> Result<Self, EmbedError> {
            let session = Session::builder()
                .and_then(|mut b| b.commit_from_file(dir.join(model_file)))
                .map_err(|e| EmbedError::Model(e.to_string()))?;
            let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
                .map_err(|e| EmbedError::Model(e.to_string()))?;
            tokenizer
                .with_truncation(Some(TruncationParams {
                    max_length: MAX_TOKENS,
                    ..Default::default()
                }))
                .map_err(|e| EmbedError::Model(e.to_string()))?;
            tokenizer.with_padding(Some(PaddingParams::default())); // BatchLongest, pad_id 0
            Ok(Self {
                session: Mutex::new(session),
                tokenizer,
                batch: batch.max(1),
            })
        }

        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            let encodings = self
                .tokenizer
                .encode_batch(texts.to_vec(), true)
                .map_err(|e| EmbedError::Model(e.to_string()))?;
            let n = encodings.len();
            let seq = encodings
                .iter()
                .map(|e| e.get_ids().len())
                .max()
                .unwrap_or(0);
            let mut ids = Vec::with_capacity(n * seq);
            let mut mask = Vec::with_capacity(n * seq);
            let mut types = Vec::with_capacity(n * seq);
            for e in &encodings {
                ids.extend(e.get_ids().iter().map(|&v| v as i64));
                mask.extend(e.get_attention_mask().iter().map(|&v| v as i64));
                types.extend(e.get_type_ids().iter().map(|&v| v as i64));
            }
            let shape = [n as i64, seq as i64];
            let to_tensor = |data: Vec<i64>| {
                Tensor::from_array((shape, data)).map_err(|e| EmbedError::Model(e.to_string()))
            };
            let mut session = self
                .session
                .lock()
                .map_err(|e| EmbedError::Model(e.to_string()))?;
            let outputs = session
                .run(ort::inputs![
                    "input_ids" => to_tensor(ids)?,
                    "attention_mask" => to_tensor(mask)?,
                    "token_type_ids" => to_tensor(types)?,
                ])
                .map_err(|e| EmbedError::Model(e.to_string()))?;
            let (out_shape, data) = outputs["last_hidden_state"]
                .try_extract_tensor::<f32>()
                .map_err(|e| EmbedError::Model(e.to_string()))?;
            let dims: Vec<i64> = out_shape.to_vec();
            if dims.len() != 3 || dims[0] != n as i64 || dims[2] != BGE_DIM as i64 {
                return Err(EmbedError::Model(format!(
                    "unexpected last_hidden_state shape {dims:?}"
                )));
            }
            let seq_out = dims[1] as usize;
            let mut vectors = Vec::with_capacity(n);
            for i in 0..n {
                // CLS pooling: first token of the last hidden state.
                let row = &data[i * seq_out * BGE_DIM..i * seq_out * BGE_DIM + BGE_DIM];
                let norm = row.iter().map(|x| x * x).sum::<f32>().sqrt();
                let norm = if norm == 0.0 { 1.0 } else { norm };
                vectors.push(row.iter().map(|x| x / norm).collect());
            }
            Ok(vectors)
        }
    }

    impl Embedder for OnnxEmbedder {
        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            let mut out = Vec::with_capacity(texts.len());
            for chunk in texts.chunks(self.batch) {
                out.extend(self.embed_batch(chunk)?);
            }
            Ok(out)
        }

        fn dim(&self) -> usize {
            BGE_DIM
        }
    }
}

#[cfg(feature = "onnx")]
pub use onnx::OnnxEmbedder;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn hash_embedder_unit_norm_and_deterministic() {
        let e = HashEmbedder::default();
        assert_eq!(e.dim(), 1024);
        let a = e.embed(&["hello", "hello", "world"]).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0], a[1]);
        assert_ne!(a[0], a[2]);
        let norm: f64 = a[0].iter().map(|&x| (x as f64) * (x as f64)).sum();
        assert!((norm.sqrt() - 1.0).abs() < 1e-6, "norm {}", norm.sqrt());
    }

    #[test]
    fn hash_embedder_empty_input() {
        assert!(HashEmbedder::new(8).embed(&[]).unwrap().is_empty());
    }

    /// Single-thread HTTP/1.1 stub: answers each connection with the JSON
    /// produced by `respond(texts)` (status 200) or a fixed error status.
    fn mock_embed_server(
        n_conns: usize,
        respond: impl Fn(&str, Vec<String>) -> (u16, String) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<Vec<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/embed", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..n_conns {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let body_start = loop {
                    let k = sock.read(&mut tmp).unwrap();
                    buf.extend_from_slice(&tmp[..k]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..body_start]).to_string();
                let clen: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                while buf.len() < body_start + clen {
                    let k = sock.read(&mut tmp).unwrap();
                    buf.extend_from_slice(&tmp[..k]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&buf[body_start..body_start + clen]).unwrap();
                let model_id = body["model_id"].as_str().unwrap().to_string();
                let texts: Vec<String> = body["texts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_string())
                    .collect();
                seen.push(texts.clone());
                let (status, json) = respond(&model_id, texts);
                let reason = if status == 200 { "OK" } else { "Bad Request" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
                    json.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
            seen
        });
        (url, handle)
    }

    /// One 1024-d vector whose first component encodes the text length —
    /// enough to prove order preservation across batches.
    fn tagged_vec(t: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; 1024];
        v[0] = t.len() as f32;
        v
    }

    #[test]
    fn remote_embedder_round_trip_and_batching() {
        let (url, handle) = mock_embed_server(3, |model_id, texts| {
            assert_eq!(model_id, "bge-large-en-v1.5");
            let vectors: Vec<Vec<f32>> = texts.iter().map(|t| tagged_vec(t)).collect();
            (
                200,
                serde_json::json!({
                    "vectors": vectors,
                    "model_id": model_id,
                    "count": texts.len(),
                    "token_count": 0,
                })
                .to_string(),
            )
        });
        let e = RemoteEmbedder::with_options(&url, "bge-large-en-v1.5", 2, Duration::from_secs(5))
            .unwrap();
        assert_eq!(e.dim(), 1024);
        let texts = ["a", "bb", "ccc", "dddd", "eeeee"];
        let got = e.embed(&texts).unwrap();
        assert_eq!(got.len(), 5);
        for (t, v) in texts.iter().zip(&got) {
            assert_eq!(v[0], t.len() as f32);
            assert_eq!(v.len(), 1024);
        }
        // batch=2 over 5 texts → 3 serial requests, order preserved
        let seen = handle.join().unwrap();
        assert_eq!(
            seen,
            vec![
                vec!["a".to_string(), "bb".to_string()],
                vec!["ccc".to_string(), "dddd".to_string()],
                vec!["eeeee".to_string()],
            ]
        );
    }

    #[test]
    fn remote_embedder_empty_input_no_network() {
        // Bound port with no listener thread — any request would hang/fail.
        let e = RemoteEmbedder::new("http://127.0.0.1:1/embed").unwrap();
        assert!(e.embed(&[]).unwrap().is_empty());
    }

    #[test]
    fn remote_embedder_http_error_is_protocol() {
        let (url, handle) = mock_embed_server(1, |_, _| {
            (400, r#"{"detail":"Unknown model: nope"}"#.to_string())
        });
        let e = RemoteEmbedder::with_options(&url, "nope", 512, Duration::from_secs(5)).unwrap();
        let err = e.embed(&["x"]).unwrap_err();
        assert!(matches!(err, EmbedError::Protocol(_)), "{err}");
        assert!(err.to_string().contains("400"), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn remote_embedder_count_mismatch_is_protocol() {
        let (url, handle) = mock_embed_server(1, |model_id, _| {
            (
                200,
                serde_json::json!({"vectors": [], "model_id": model_id,
                "count": 0, "token_count": 0})
                .to_string(),
            )
        });
        let e =
            RemoteEmbedder::with_options(&url, "bge-large-en-v1.5", 512, Duration::from_secs(5))
                .unwrap();
        let err = e.embed(&["x"]).unwrap_err();
        assert!(matches!(err, EmbedError::Protocol(_)), "{err}");
        assert!(err.to_string().contains("count mismatch"), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn remote_embedder_dim_mismatch_is_protocol() {
        let (url, handle) = mock_embed_server(1, |model_id, texts| {
            let vectors: Vec<Vec<f32>> = texts.iter().map(|_| vec![0.0; 768]).collect();
            (
                200,
                serde_json::json!({"vectors": vectors, "model_id": model_id,
                "count": 1, "token_count": 0})
                .to_string(),
            )
        });
        let e =
            RemoteEmbedder::with_options(&url, "bge-large-en-v1.5", 512, Duration::from_secs(5))
                .unwrap();
        let err = e.embed(&["x"]).unwrap_err();
        assert!(err.to_string().contains("dim mismatch"), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn transport_error_is_transport() {
        // RFC 5737 TEST-NET address: unroutable, fails fast on macOS.
        let e = RemoteEmbedder::with_options(
            "http://192.0.2.1:9/embed",
            "bge-large-en-v1.5",
            512,
            Duration::from_millis(200),
        )
        .unwrap();
        let err = e.embed(&["x"]).unwrap_err();
        assert!(matches!(err, EmbedError::Transport(_)), "{err}");
    }
}
