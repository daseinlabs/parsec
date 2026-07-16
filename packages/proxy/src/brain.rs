//! Brain-API client — the remote scorer behind `engine::freeze`'s
//! `ChunkScorer` seam, plus the tool-schema keep-set machinery
//! (docs/brain-serving-v0.md; contracts brain-api-dev/v0 + brain-api/v1).
//!
//! Two contracts, selected by `DASEIN_BRAIN_CONTRACT` (default `dev`):
//! - **dev** (brain-api-dev/v0): the request carries the INTERNAL MESSAGE
//!   VIEW (raw text) to OUR cluster, so this path additionally requires the
//!   explicit `DASEIN_BRAIN_DEV_RAW=1` opt-in — the same data-plane exception
//!   STATUS.md blesses for the dev embed fallback, never for real users.
//! - **v1** (brain-api/v1): the client featurizes where the text lives
//!   (`featurize.rs` — local embedder + engine node/readout structs + rel-4
//!   pairs); the wire carries vectors + features + salted opaque ids, raw
//!   text UNREPRESENTABLE. No raw-text opt-in needed. On first score() the
//!   scorer handshakes `GET /v1/bundle` for the checkpoint_id (the §8.2
//!   matched-pair guard rides in every payload); an unreachable brain is a
//!   per-step fail-open exactly like a failed score, retried next serve.
//!
//! Invariants owned here:
//! - Scorer failure returns `Err(ScoreError)` — the Freezer leaves the birth
//!   step undecided and retries on a later call (§8.3; brain downtime costs
//!   savings, never correctness).
//! - One HTTP round trip per birth step: tau is constant per (checkpoint,
//!   target_cov), so the Freezer's per-owner-pool tau calls are served from
//!   the same-live-set response cache.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use dasein_engine::embed::{Embedder, HashEmbedder};
use dasein_engine::freeze::{BirthQuery, ChunkScorer, ScoreError, ScoreResult};
use dasein_engine::pystr::char_prefix;

use crate::featurize;

/// Which wire contract the scorer speaks (DASEIN_BRAIN_CONTRACT).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrainContract {
    /// brain-api-dev/v0 — raw internal view, dev machines only.
    #[default]
    Dev,
    /// brain-api/v1 — client-side featurization, data-plane clean.
    V1,
}

impl BrainContract {
    /// `DASEIN_BRAIN_CONTRACT` parsing: only the exact string "v1" selects
    /// the v1 contract; anything else (unset, "dev", typos) stays dev — the
    /// conservative default while v1 is being proven out.
    pub fn from_env_value(v: Option<&str>) -> BrainContract {
        match v.map(str::trim) {
            Some("v1") => BrainContract::V1,
            _ => BrainContract::Dev,
        }
    }
}

/// Config surface (docs/brain-serving-v0.md "Config surface").
#[derive(Debug, Clone)]
pub struct BrainConfig {
    pub url: String,
    pub key: Option<String>,
    pub timeout: Duration,
    pub target_cov: String,
    /// AC_TOOL_CUT equivalent; reference default 0.70.
    pub tool_cut: f64,
    /// DASEIN_TOOL_PRUNE — defaults on when the brain is configured.
    pub tool_prune: bool,
    /// DASEIN_TOOL_STUB — serve pruned tools as name+note stubs instead of
    /// dropping them (default on); "off" restores the reference hard-drop.
    pub tool_stub: bool,
    /// DASEIN_BRAIN_CONTRACT: dev (default) | v1.
    pub contract: BrainContract,
    /// v1 client embedder: DASEIN_EMBED_BACKEND = hash | remote | onnx.
    pub embed_backend: String,
    /// DASEIN_EMBED_URL — required for the remote backend (dev fallback:
    /// raw text rides to the embed service; our machines only).
    pub embed_url: Option<String>,
    /// DASEIN_ONNX_DIR — the local bge-large export directory (onnx backend).
    pub onnx_dir: Option<String>,
}

/// Release-baked default brain URL: `DASEIN_DEFAULT_BRAIN_URL` at BUILD time
/// (release.yml stamps the production Cloud Run URL so a published plugin
/// reaches the brain with zero configuration). Dev/CI builds bake nothing.
/// Runtime `DASEIN_BRAIN_URL` always wins, and setting it to an EMPTY string
/// is the off switch even when a default is baked.
const BAKED_BRAIN_URL: Option<&str> = option_env!("DASEIN_DEFAULT_BRAIN_URL");

/// URL + contract resolution, pure for testability. Env URL beats baked.
/// A BAKED url defaults the contract to v1 (a released binary must be
/// data-plane-clean by default; dev's raw-text wire is opt-in only), while
/// an env-supplied URL keeps the conservative dev default. An explicit
/// `DASEIN_BRAIN_CONTRACT` always wins. Returns (url, contract, baked).
fn resolve_url_contract(
    env_url: Option<&str>,
    baked_url: Option<&str>,
    env_contract: Option<&str>,
) -> Option<(String, BrainContract, bool)> {
    let (url, baked) = match env_url {
        Some(u) => (u.trim(), false),
        None => (baked_url.unwrap_or("").trim(), true),
    };
    if url.is_empty() {
        return None;
    }
    let contract = match env_contract {
        Some(v) => BrainContract::from_env_value(Some(v)),
        None if baked => BrainContract::V1,
        None => BrainContract::Dev,
    };
    Some((url.trim_end_matches('/').to_string(), contract, baked))
}

impl BrainConfig {
    /// None unless a brain URL is configured — `DASEIN_BRAIN_URL`, or the
    /// release-baked [`BAKED_BRAIN_URL`] fallback — and `DASEIN_FREEZE` isn't
    /// "off". The DEV contract additionally requires the explicit raw-text
    /// opt-in `DASEIN_BRAIN_DEV_RAW=1`; the v1 contract sends no raw text and
    /// needs no opt-in.
    pub fn from_env() -> Option<BrainConfig> {
        let env_url = std::env::var("DASEIN_BRAIN_URL").ok();
        let env_contract = std::env::var("DASEIN_BRAIN_CONTRACT").ok();
        let (url, contract, baked) =
            resolve_url_contract(env_url.as_deref(), BAKED_BRAIN_URL, env_contract.as_deref())?;
        if contract == BrainContract::Dev
            && std::env::var("DASEIN_BRAIN_DEV_RAW").ok().as_deref() != Some("1")
        {
            tracing::warn!(
                "DASEIN_BRAIN_URL is set but DASEIN_BRAIN_DEV_RAW=1 is not — the v0 \
                 contract sends raw text to the brain; refusing without the explicit \
                 opt-in (passthrough curation stays active). Set \
                 DASEIN_BRAIN_CONTRACT=v1 for the data-plane-clean contract."
            );
            return None;
        }
        if std::env::var("DASEIN_FREEZE").ok().as_deref() == Some("off") {
            return None;
        }
        let timeout_ms: u64 = std::env::var("DASEIN_BRAIN_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10_000);
        let embed_backend = std::env::var("DASEIN_EMBED_BACKEND").unwrap_or_else(|_| "hash".into());
        if contract == BrainContract::V1 && embed_backend == "hash" {
            if baked {
                // The baked release default must never DEGRADE anyone: hash
                // vectors are not the trained bge embeddings, and unlike an
                // HTTP failure, garbage scores don't fail open. Stay off
                // until a real embedder is configured.
                tracing::info!(
                    "release brain URL is baked in, but DASEIN_EMBED_BACKEND is 'hash' \
                     (test vectors) — brain stays OFF until a real embedder is \
                     configured (DASEIN_EMBED_BACKEND=remote|onnx)"
                );
                return None;
            }
            tracing::warn!(
                "v1 contract with the HASH embed backend — deterministic test vectors, \
                 NOT the checkpoint's trained bge embeddings (set DASEIN_EMBED_BACKEND=\
                 remote|onnx for real scores)"
            );
        }
        Some(BrainConfig {
            url,
            key: std::env::var("DASEIN_BRAIN_KEY")
                .ok()
                .filter(|k| !k.is_empty()),
            timeout: Duration::from_millis(timeout_ms),
            target_cov: std::env::var("DASEIN_TARGET_COV").unwrap_or_else(|_| "0.70".into()),
            tool_cut: std::env::var("DASEIN_TOOL_CUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.70),
            tool_prune: std::env::var("DASEIN_TOOL_PRUNE").ok().as_deref() != Some("off"),
            tool_stub: std::env::var("DASEIN_TOOL_STUB").ok().as_deref() != Some("off"),
            contract,
            embed_backend,
            embed_url: std::env::var("DASEIN_EMBED_URL")
                .ok()
                .filter(|u| !u.is_empty()),
            onnx_dir: std::env::var("DASEIN_ONNX_DIR")
                .ok()
                .filter(|d| !d.is_empty()),
        })
    }
}

/// Build the v1 client embedder from config. Deterministic backends only;
/// `onnx` requires the engine `onnx` cargo feature (compile-gated — the
/// default binary stays light) and errors otherwise.
pub fn build_embedder(cfg: &BrainConfig) -> Result<Box<dyn Embedder + Send>, ScoreError> {
    match cfg.embed_backend.as_str() {
        "hash" => Ok(Box::new(HashEmbedder::default())),
        "remote" => {
            let url = cfg.embed_url.clone().ok_or_else(|| {
                ScoreError("DASEIN_EMBED_URL required for DASEIN_EMBED_BACKEND=remote".into())
            })?;
            Ok(Box::new(
                dasein_engine::embed::RemoteEmbedder::new(url)
                    .map_err(|e| ScoreError(format!("remote embedder: {e}")))?,
            ))
        }
        "onnx" => {
            #[cfg(feature = "onnx")]
            {
                let dir = cfg.onnx_dir.clone().unwrap_or_else(|| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                    format!("{home}/.dasein/models/bge-large-onnx")
                });
                Ok(Box::new(
                    dasein_engine::embed::OnnxEmbedder::load(std::path::Path::new(&dir))
                        .map_err(|e| ScoreError(format!("onnx embedder: {e}")))?,
                ))
            }
            #[cfg(not(feature = "onnx"))]
            {
                Err(ScoreError(
                    "DASEIN_EMBED_BACKEND=onnx but this binary was built without the \
                     `onnx` cargo feature (cargo build --features onnx)"
                        .into(),
                ))
            }
        }
        other => Err(ScoreError(format!(
            "unknown DASEIN_EMBED_BACKEND {other:?} (hash | remote | onnx)"
        ))),
    }
}

#[derive(Deserialize)]
struct BundleInfo {
    checkpoint_id: String,
    /// `{"gf": int, "served": bool}` — whether this bundle scores a doom
    /// head. Absent on pre-doom brains (their v1 models are extra=forbid, so
    /// sending `gf` to them would 422 every trace: version-skew guard).
    #[serde(default)]
    doom: Option<DoomInfo>,
}

#[derive(Deserialize)]
struct DoomInfo {
    #[serde(default)]
    served: bool,
}

impl BundleInfo {
    fn doom_served(&self) -> bool {
        self.doom.as_ref().is_some_and(|d| d.served)
    }
}

/// `GET /v1/bundle` — the v1 handshake: the checkpoint the brain serves,
/// which every v1 payload must name (§8.2 matched-pair guard, 409 on drift),
/// plus the capability flags the client gates optional fields on (doom/gf).
fn fetch_bundle(
    http: &reqwest::blocking::Client,
    cfg: &BrainConfig,
) -> Result<BundleInfo, ScoreError> {
    let mut req = http.get(format!("{}/v1/bundle", cfg.url));
    if let Some(k) = &cfg.key {
        req = req.bearer_auth(k);
    }
    let resp = req
        .send()
        .map_err(|e| ScoreError(format!("brain bundle handshake: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ScoreError(format!("brain bundle handshake: {status}")));
    }
    let info: BundleInfo = resp
        .json()
        .map_err(|e| ScoreError(format!("brain bundle decode: {e}")))?;
    tracing::debug!(
        checkpoint = %info.checkpoint_id,
        doom_served = info.doom_served(),
        "brain /v1/bundle handshake ok"
    );
    Ok(info)
}

/// Per-conversation id salt: deliberately NON-derivable server-side (the
/// point of salting — equality rides the wire, the names don't). Never on
/// the served-bytes path: ids only join rows within one request.
fn fresh_salt(conv_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(conv_id.as_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    format!("{:x}", h.finalize())
}

#[derive(Deserialize)]
struct TraceResponse {
    scores_q: Vec<i64>,
    tau_q: i64,
    checkpoint_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    timings_ms: Value,
    /// Doom head on the 1e-6 grid — present iff the request carried `gf`
    /// AND the checkpoint has a doom head (wire contract addition).
    #[serde(default)]
    doom_q: Option<i64>,
}

/// Telemetry the proxy reads back after a serve (ledger capture seams).
#[derive(Debug, Default, Clone)]
pub struct BrainStats {
    pub brain_ms: f64,
    pub checkpoint_id: Option<String>,
    /// HTTP trace-score round trips (birth steps scored) — tracing seam.
    pub trace_calls: u64,
    /// Latest doom_q seen (governor consumer resets it per serve).
    pub last_doom_q: Option<i64>,
}

/// (cur_step, live-set fingerprint, scores_q, tau_q) of the last response.
type ScoreCache = Mutex<Option<(i64, String, Vec<i64>, i64)>>;

/// The remote `ChunkScorer`. One instance per conversation (owned by that
/// conversation's Freezer); the blocking client is cheap and self-contained —
/// `Freezer::serve` always runs inside `spawn_blocking`.
pub struct BrainScorer {
    cfg: BrainConfig,
    conv_id: String,
    http: reqwest::blocking::Client,
    /// The multi-owner birth case re-calls score() per owner pool wanting
    /// only tau; tau is constant on calibrated checkpoints, so replaying the
    /// cached scores is byte-equivalent to the reference (which discards
    /// per-pool scores too).
    cache: ScoreCache,
    pub stats: BrainStats,
    /// Attach the governor's 4-float `gf` (loop_feats) to score/trace bodies
    /// — set by the server when `DASEIN_GOVERNOR != off`; false = today's
    /// wire, byte-identical (doom head not scored).
    pub attach_gf: bool,
    // ── v1-contract state (unused in dev mode) ─────────────────────────────
    /// /v1/bundle handshake result; None until the first successful GET
    /// (retried per score() — an unreachable brain is a per-step fail-open).
    v1_checkpoint: Option<String>,
    /// Did the handshaken bundle advertise a served doom head? `gf` is
    /// attached on the v1 wire only when true (pre-gf brains are
    /// extra=forbid: an unknown key is a 422 = total curation outage).
    /// Refreshed with every handshake (incl. the 409 re-pair).
    v1_doom: bool,
    /// Client embedder, built lazily on the first v1 score (blocking
    /// context — Freezer::serve always runs inside spawn_blocking).
    v1_embedder: Option<Box<dyn Embedder + Send>>,
    /// Exact-text embed cache (keys are the CLIPPED strings — the
    /// reference's two-layer cache collapsed to one, curator L281-286).
    v1_embed_cache: HashMap<String, Vec<f32>>,
    /// Per-conversation opaque-id salt (never leaves this process).
    conv_salt: String,
}

impl BrainScorer {
    pub fn new(cfg: BrainConfig, conv_id: String) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .expect("reqwest blocking client");
        let conv_salt = fresh_salt(&conv_id);
        BrainScorer {
            cfg,
            conv_id,
            http,
            cache: Mutex::new(None),
            stats: BrainStats::default(),
            attach_gf: false,
            v1_checkpoint: None,
            v1_doom: false,
            v1_embedder: None,
            v1_embed_cache: HashMap::new(),
            conv_salt,
        }
    }

    fn live_fp(q: &BirthQuery) -> String {
        let mut h = Sha256::new();
        for (gi, c) in q.live_gi.iter().zip(q.live.iter()) {
            h.update(format!("{}:{}:{}:{}\n", gi, c.step, c.kind, c.tokens).as_bytes());
        }
        format!("{:x}", h.finalize())
    }

    /// v1 request body: handshake (cached) + client featurization. Any
    /// failure surfaces as ScoreError = per-step fail-open, retried whole
    /// (including the handshake) on the next serve.
    fn v1_body(&mut self, q: &BirthQuery) -> Result<Value, ScoreError> {
        if self.v1_checkpoint.is_none() {
            let info = fetch_bundle(&self.http, &self.cfg)?;
            // Finding 6: gate `gf` on the handshake's doom capability — a
            // pre-gf v1 brain (extra=forbid) would 422 every trace body.
            self.v1_doom = info.doom_served();
            self.v1_checkpoint = Some(info.checkpoint_id);
        }
        if self.v1_embedder.is_none() {
            self.v1_embedder = Some(build_embedder(&self.cfg)?);
        }
        let checkpoint_id = self.v1_checkpoint.clone().expect("handshake done");
        let embedder: &dyn Embedder = &**self.v1_embedder.as_ref().expect("embedder built");
        featurize::build_v1_trace_payload(
            q,
            embedder,
            &mut self.v1_embed_cache,
            featurize::changeprone(),
            &self.conv_salt,
            &self.conv_id,
            &checkpoint_id,
            &self.cfg.target_cov,
        )
    }
}

impl ChunkScorer for BrainScorer {
    fn score(&mut self, q: &BirthQuery) -> Result<ScoreResult, ScoreError> {
        let fp = Self::live_fp(q);
        if let Some((step, cached_fp, scores, tau)) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            if *step == q.cur_step && *cached_fp == fp {
                tracing::debug!(
                    conv = %self.conv_id,
                    cur_step = q.cur_step,
                    "brain score served from same-live-set cache"
                );
                return Ok(ScoreResult {
                    scores_q: scores.clone(),
                    tau_q: *tau,
                });
            }
        }

        let mut body = match self.cfg.contract {
            BrainContract::Dev => json!({
                "contract": "brain-api-dev/v0",
                "conv_id": self.conv_id,
                "messages": q.messages,
                "live_gi": q.live_gi,
                "mask": q.mask,
                "cur_step": q.cur_step,
                "chunk_checksum": q.chunk_checksum,
                "target_cov": self.cfg.target_cov,
            }),
            BrainContract::V1 => self.v1_body(q)?,
        };
        // Governor doom head input (wire contract addition, both contracts):
        // loop_feats over the SAME internal view being scored. Omitted when
        // the governor is off or there are no commands — today's behavior.
        // v1 additionally requires the handshaken bundle to SERVE a doom head
        // (pre-gf v1 servers are extra=forbid — sending gf would 422 every
        // trace, a total curation outage on version skew). Dev models ignore
        // extras, so dev attaches unconditionally.
        let gf_capable = match self.cfg.contract {
            BrainContract::Dev => true,
            BrainContract::V1 => self.v1_doom, // set by the handshake above
        };
        if self.attach_gf && gf_capable {
            if let Some(gf) = crate::governor::gf_of(q.messages) {
                if let Some(o) = body.as_object_mut() {
                    o.insert("gf".into(), json!(gf));
                }
            }
        }
        let t0 = Instant::now();
        let mut req = self
            .http
            .post(format!("{}/v1/score/trace", self.cfg.url))
            .json(&body);
        if let Some(k) = &self.cfg.key {
            req = req.bearer_auth(k);
        }
        let resp = req
            .send()
            .map_err(|e| ScoreError(format!("brain unreachable: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            if status.as_u16() == 409 && self.cfg.contract == BrainContract::V1 {
                // checkpoint drift (brain redeployed mid-conversation):
                // forget the handshake so the retry re-pairs features+weights.
                self.v1_checkpoint = None;
            }
            let detail = resp.text().unwrap_or_default();
            // char-clip, never byte-slice: a multi-byte char straddling the
            // boundary panicked inside spawn_blocking and dropped the taken
            // freezer (memo loss) — same pattern as the count_tokens probe.
            return Err(ScoreError(format!(
                "brain {status}: {}",
                detail.chars().take(200).collect::<String>()
            )));
        }
        let r: TraceResponse = resp
            .json()
            .map_err(|e| ScoreError(format!("brain response decode: {e}")))?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.stats.brain_ms += elapsed_ms;
        // the whole GNN score vector, index-aligned to the live chunk set —
        // model outputs only, no text (data-plane rule)
        tracing::debug!(
            conv = %self.conv_id,
            cur_step = q.cur_step,
            scores_q = ?r.scores_q,
            tau_q = r.tau_q,
            "GNN trace score vector"
        );
        tracing::debug!(
            conv = %self.conv_id,
            cur_step = q.cur_step,
            live = q.live.len(),
            scores = r.scores_q.len(),
            tau_q = r.tau_q,
            checkpoint = %r.checkpoint_id,
            elapsed_ms = elapsed_ms as u64,
            "brain /v1/score/trace ok"
        );
        self.stats.checkpoint_id = Some(r.checkpoint_id);
        self.stats.trace_calls += 1;
        if let Some(dq) = r.doom_q {
            self.stats.last_doom_q = Some(dq);
        }
        if r.scores_q.len() != q.live.len() {
            return Err(ScoreError(format!(
                "brain returned {} scores for {} live chunks",
                r.scores_q.len(),
                q.live.len()
            )));
        }
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((q.cur_step, fp, r.scores_q.clone(), r.tau_q));
        Ok(ScoreResult {
            scores_q: r.scores_q,
            tau_q: r.tau_q,
        })
    }
}

// ── tool-schema keep-set (reference: ToolSchemaPruner + curating_proxy) ─────

#[derive(Deserialize)]
pub struct ToolScores {
    pub names: Vec<String>,
    pub scores_q: Vec<i64>,
    pub tokens: Vec<i64>,
    #[serde(default)]
    pub checkpoint_id: String,
}

/// Score the request roster via the brain's tool head, on whichever contract
/// the config selects. None = anything went wrong or the roster is
/// ineligible — serve the FULL tools array and retry on the next request
/// (reference retry-until-success semantics).
pub async fn score_tools(
    client: &reqwest::Client,
    cfg: &BrainConfig,
    conv_id: &str,
    internal: &[Value],
    tools: &Value,
) -> Option<ToolScores> {
    let body = match cfg.contract {
        BrainContract::Dev => json!({
            "contract": "brain-api-dev/v0",
            "conv_id": conv_id,
            "messages": internal,
            "tools": tools,
        }),
        BrainContract::V1 => {
            // Handshake + featurization are blocking work (blocking HTTP
            // handshake, possible remote/onnx embedder) — off the async
            // runtime. Once per conversation: the keep-set freezes after the
            // first successful prune.
            let cfg2 = cfg.clone();
            let conv2 = conv_id.to_string();
            let internal2 = internal.to_vec();
            let tools2 = tools.clone();
            let built = tokio::task::spawn_blocking(move || {
                build_v1_tools_body(&cfg2, &conv2, &internal2, &tools2)
            })
            .await
            .ok()?;
            match built {
                Ok(Some(body)) => body,
                Ok(None) => return None, // ineligible roster/view: full roster
                Err(e) => {
                    tracing::warn!("v1 tools featurization failed ({e}): full roster served");
                    return None;
                }
            }
        }
    };
    let mut req = client
        .post(format!("{}/v1/score/tools", cfg.url))
        .timeout(cfg.timeout)
        .json(&body);
    if let Some(k) = &cfg.key {
        req = req.bearer_auth(k);
    }
    let t0 = Instant::now();
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::warn!("brain score/tools {}: full roster served", resp.status());
        return None;
    }
    let ts: ToolScores = resp.json().await.ok()?;
    if ts.names.is_empty()
        || ts.names.len() != ts.scores_q.len()
        || ts.names.len() != ts.tokens.len()
    {
        tracing::debug!(
            names = ts.names.len(),
            scores = ts.scores_q.len(),
            tokens = ts.tokens.len(),
            "brain score/tools response malformed — full roster served"
        );
        return None;
    }
    tracing::debug!(
        conv = %conv_id,
        names = ?ts.names,
        scores_q = ?ts.scores_q,
        tokens = ?ts.tokens,
        "GNN tool score vector"
    );
    tracing::debug!(
        conv = %conv_id,
        tools = ts.names.len(),
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "brain /v1/score/tools ok"
    );
    Some(ts)
}

/// Blocking v1 tools-request assembly: bundle handshake + client
/// featurization. Ok(None) = ineligible (fail-open, full roster).
fn build_v1_tools_body(
    cfg: &BrainConfig,
    conv_id: &str,
    internal: &[Value],
    tools: &Value,
) -> Result<Option<Value>, ScoreError> {
    let http = reqwest::blocking::Client::builder()
        .timeout(cfg.timeout)
        .build()
        .map_err(|e| ScoreError(format!("blocking client: {e}")))?;
    let checkpoint_id = fetch_bundle(&http, cfg)?.checkpoint_id;
    let embedder = build_embedder(cfg)?;
    let mut cache = HashMap::new();
    featurize::build_v1_tools_payload(
        internal,
        tools,
        &*embedder,
        &mut cache,
        &fresh_salt(conv_id),
        conv_id,
        &checkpoint_id,
    )
}

// ── governor consumers: /v1/score/rules + /v1/neighbors ────────────────────

/// task_text per the reference query rule: the FIRST user message's internal
/// text, clipped to 2000 chars.
fn first_user_text(internal: &[Value]) -> String {
    for m in internal {
        if m.get("role").and_then(Value::as_str) == Some("user") {
            let t = match m.get("content") {
                Some(Value::String(s)) => s.as_str(),
                _ => "",
            };
            return char_prefix(t, 2000).to_string();
        }
    }
    String::new()
}

/// `/v1/score/rules` response (wire contract: existing fields plus the
/// `rules` roster scored at the CURRENT step). A response without `rules`
/// (an older brain) carries no text to deliver — the caller fires nothing.
#[derive(Deserialize)]
pub struct RulesResponse {
    #[serde(default)]
    pub rules: Vec<crate::governor::RuleScore>,
    #[serde(default)]
    pub tau_hint_q: Option<i64>,
    #[serde(default)]
    pub checkpoint_id: String,
}

/// Score the server-side rule roster at `cur_step` (dev + v1 bodies, the
/// score_tools pattern). None = anything failed (transport, non-2xx incl.
/// the 501 v1-unsupported detail, decode) — the governor skips rules this
/// turn and retries next request (fail-open).
pub async fn score_rules(
    client: &reqwest::Client,
    cfg: &BrainConfig,
    conv_id: &str,
    internal: &[Value],
    tools: &Value,
    cur_step: i64,
) -> Option<RulesResponse> {
    let body = match cfg.contract {
        BrainContract::Dev => json!({
            "contract": "brain-api-dev/v0",
            "conv_id": conv_id,
            "messages": internal,
            "tools": tools.as_array().cloned().unwrap_or_default(),
            "step": cur_step,
        }),
        BrainContract::V1 => {
            let cfg2 = cfg.clone();
            let conv2 = conv_id.to_string();
            let internal2 = internal.to_vec();
            let built = tokio::task::spawn_blocking(move || {
                build_v1_rules_body(&cfg2, &conv2, &internal2, cur_step)
            })
            .await
            .ok()?;
            match built {
                Ok(Some(body)) => body,
                Ok(None) => return None, // nothing chunkable: skip rules
                Err(e) => {
                    tracing::warn!("v1 rules featurization failed ({e}): rules skipped");
                    return None;
                }
            }
        }
    };
    let mut req = client
        .post(format!("{}/v1/score/rules", cfg.url))
        .timeout(cfg.timeout)
        .json(&body);
    if let Some(k) = &cfg.key {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::warn!(
            "brain score/rules {}: rules skipped (fail-open)",
            resp.status()
        );
        return None;
    }
    resp.json().await.ok()
}

/// Blocking v1 rules-request assembly: the SAME v1 trace payload shape as
/// score/trace (chunks/steps as opaque ids + vectors, `cur_step` included)
/// built over the full internal view with an empty decided mask. Ok(None) =
/// ineligible (fail-open, rules skipped).
fn build_v1_rules_body(
    cfg: &BrainConfig,
    conv_id: &str,
    internal: &[Value],
    cur_step: i64,
) -> Result<Option<Value>, ScoreError> {
    use dasein_engine::chunking::{accumulated_chunks, ChunkMode};
    use dasein_engine::messages::{assistant_chunks_of, reasoning_chunks_of, steps_of};

    let http = reqwest::blocking::Client::builder()
        .timeout(cfg.timeout)
        .build()
        .map_err(|e| ScoreError(format!("blocking client: {e}")))?;
    let checkpoint_id = fetch_bundle(&http, cfg)?.checkpoint_id;
    let embedder = build_embedder(cfg)?;
    let steps = steps_of(internal);
    let t_last = steps.len().max(1) - 1;
    let mut chunks = accumulated_chunks(&steps, t_last, Some(10), ChunkMode::Fixed);
    chunks.extend(
        assistant_chunks_of(internal)
            .into_iter()
            .filter(|c| c.step <= t_last as i64),
    );
    chunks.extend(
        reasoning_chunks_of(internal)
            .into_iter()
            .filter(|c| c.step <= t_last as i64),
    );
    chunks.sort_by_key(|c| c.step);
    if chunks.is_empty() {
        return Ok(None);
    }
    let live_owner = vec![0usize; chunks.len()];
    let q = BirthQuery {
        cur_step,
        task_text: first_user_text(internal),
        recent_cmds: String::new(),
        live: &chunks,
        live_owner: &live_owner,
        live_gi: (0..chunks.len()).collect(),
        messages: internal,
        chunk_checksum: String::new(),
        mask: Vec::new(),
    };
    let mut cache = HashMap::new();
    let mut payload = featurize::build_v1_trace_payload(
        &q,
        &*embedder,
        &mut cache,
        featurize::changeprone(),
        &fresh_salt(conv_id),
        conv_id,
        &checkpoint_id,
        &cfg.target_cov,
    )?;
    // The rules request is the TRACE payload shape minus the decision-side
    // fields: the brain's ScoreRulesV1Request is extra=forbid, so carrying
    // mask/decided_struct/target_cov 422s live (guarded by
    // tests/proxy_governor.rs::v1_governor_bodies_match_brain_schema).
    if let Some(o) = payload.as_object_mut() {
        o.remove("mask");
        o.remove("decided_struct");
        o.remove("target_cov");
    }
    Ok(Some(payload))
}

/// `/v1/neighbors` response: null median when hoods are off or fewer than 4
/// cost-bearing neighbours exist (reference: runaway stays inert).
#[derive(Debug, Clone, Deserialize)]
pub struct NeighborsInfo {
    pub nbr_cost_median: Option<f64>,
    #[serde(default)]
    pub nbr_count: i64,
    #[serde(default)]
    pub neighbors_active: bool,
    #[serde(default)]
    pub checkpoint_id: String,
}

/// Fetch the per-task neighbour-cost baseline ONCE per conversation (the
/// neighbour set is per-task constant; the caller caches the result in
/// ConvState). None = failure — retried on the next request (fail-open,
/// runaway stays inert meanwhile).
pub async fn fetch_neighbors(
    client: &reqwest::Client,
    cfg: &BrainConfig,
    conv_id: &str,
    internal: &[Value],
) -> Option<NeighborsInfo> {
    let task_text = first_user_text(internal);
    if task_text.is_empty() {
        return None; // reference: no task text, no neighbours
    }
    let body = match cfg.contract {
        BrainContract::Dev => json!({
            "contract": "brain-api-dev/v0",
            "conv_id": conv_id,
            "task_text": task_text,
        }),
        BrainContract::V1 => {
            // The client embeds the task head locally; raw text stays
            // unrepresentable on the v1 wire.
            let cfg2 = cfg.clone();
            let built = tokio::task::spawn_blocking(move || -> Result<Value, ScoreError> {
                let http = reqwest::blocking::Client::builder()
                    .timeout(cfg2.timeout)
                    .build()
                    .map_err(|e| ScoreError(format!("blocking client: {e}")))?;
                let checkpoint_id = fetch_bundle(&http, &cfg2)?.checkpoint_id;
                let embedder = build_embedder(&cfg2)?;
                let vecs = embedder
                    .embed(&[task_text.as_str()])
                    .map_err(|e| ScoreError(format!("task embed: {e}")))?;
                let task_vec = vecs.into_iter().next().unwrap_or_default();
                Ok(json!({"checkpoint_id": checkpoint_id, "task_vec": task_vec}))
            })
            .await
            .ok()?;
            match built {
                Ok(mut b) => {
                    if let Some(o) = b.as_object_mut() {
                        o.insert("contract".into(), json!("brain-api/v1"));
                        o.insert("conv_id".into(), json!(conv_id));
                    }
                    b
                }
                Err(e) => {
                    tracing::warn!("v1 neighbors featurization failed ({e}): neighbors skipped");
                    return None;
                }
            }
        }
    };
    let mut req = client
        .post(format!("{}/v1/neighbors", cfg.url))
        .timeout(cfg.timeout)
        .json(&body);
    if let Some(k) = &cfg.key {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::warn!(
            "brain neighbors {}: neighbors skipped (fail-open)",
            resp.status()
        );
        return None;
    }
    resp.json().await.ok()
}

#[derive(Debug, Clone, PartialEq)]
pub struct PruneResult {
    /// Names to KEEP, original roster order.
    pub keep: Vec<String>,
    pub tokens_saved: i64,
    pub tokens_kept: i64,
    pub cut_frac: f64,
}

/// tool_pruner.ToolSchemaPruner.prune, exact port: walk schemas ascending by
/// score, cutting until >= target_cut of the roster's token mass is removed
/// (check BEFORE each cut, so the boundary tool overshoots by its own mass).
/// No protected list, no minimum keep — keep=[] is a legal output, exactly as
/// in the reference. Ties sort by original index (deterministic; numpy's
/// unstable quicksort only differs on ties).
pub fn prune(scores_q: &[i64], tokens: &[i64], names: &[String], target_cut: f64) -> PruneResult {
    let n = names.len();
    if n == 0 {
        return PruneResult {
            keep: Vec::new(),
            tokens_saved: 0,
            tokens_kept: 0,
            cut_frac: 0.0,
        };
    }
    let tot: i64 = tokens.iter().sum();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| (scores_q[i], i));
    let mut cut = vec![false; n];
    let mut cum: f64 = 0.0;
    for &i in &order {
        if cum >= target_cut * tot as f64 {
            break;
        }
        cut[i] = true;
        cum += tokens[i] as f64;
    }
    let keep: Vec<String> = (0..n)
        .filter(|&i| !cut[i])
        .map(|i| names[i].clone())
        .collect();
    let tokens_saved: i64 = (0..n).filter(|&i| cut[i]).map(|i| tokens[i]).sum();
    PruneResult {
        keep,
        tokens_saved,
        tokens_kept: tot - tokens_saved,
        cut_frac: if tot > 0 { cum / tot as f64 } else { 0.0 },
    }
}

/// Model-facing note appended to every stub description: the tool still
/// exists and one call by name brings its full schema back (the reactive
/// unfreeze in server.rs promotes it from the next request on).
pub const STUB_NOTE: &str = "[This tool is available but its full schema was elided to save \
context. To use it, call it by name with your best-guess arguments; its complete schema will \
be provided from the next turn onward.]";

/// Chars of the original description carried into a stub (char-safe cap).
pub const STUB_DESC_CHARS: usize = 200;

/// Deviation from the reference (like the reactive unfreeze it feeds): a
/// pruned tool is served as a minimal stub — name + truncated description +
/// [`STUB_NOTE`] + an accept-anything object schema — instead of vanishing,
/// so the model KNOWS the tool exists and can reach for it; the resulting
/// prefix tool_use then triggers the unfreeze. Stub bytes are a pure
/// function of the original schema (no clock/RNG), so a frozen keep-set
/// still serves a byte-stable roster. Returns None for provider-typed tools
/// (e.g. web_search_20250305): re-declaring one as a custom stub would
/// change its execution semantics, so those keep the reference hard-drop.
pub fn stub_tool(t: &Value) -> Option<Value> {
    match t.get("type").and_then(Value::as_str) {
        None | Some("custom") => {}
        Some(_) => return None,
    }
    t.get("input_schema")?;
    let name = t.get("name").and_then(Value::as_str)?;
    let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
    let head = char_prefix(desc, STUB_DESC_CHARS);
    let description = if head.is_empty() {
        STUB_NOTE.to_string()
    } else {
        format!("{head}\n\n{STUB_NOTE}")
    };
    Some(json!({
        "name": name,
        "description": description,
        "input_schema": {"type": "object", "additionalProperties": true}
    }))
}

/// sha8 of the pre-prune roster identity (sorted names) — the ledger capture
/// seam that joins served traffic back to the full roster without storing
/// schemas (TRAINING_CAPTURE_GAPS #1).
pub fn roster_sha8(tools: &Value) -> String {
    let mut names: Vec<&str> = tools
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    names.sort_unstable();
    let mut h = Sha256::new();
    h.update(names.join("\n").as_bytes());
    format!("{:x}", h.finalize())[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prune_rank_to_target() {
        // 4 tools, equal 100-token mass; cut 70% => walk ascending scores,
        // cut until cum >= 280: cuts the three lowest (cum 300 overshoots).
        let r = prune(
            &[900_000, 100_000, 200_000, 300_000],
            &[100, 100, 100, 100],
            &names(&["a", "b", "c", "d"]),
            0.70,
        );
        assert_eq!(r.keep, names(&["a"]));
        assert_eq!(r.tokens_saved, 300);
        assert_eq!(r.tokens_kept, 100);
    }

    #[test]
    fn prune_boundary_overshoot_and_order() {
        // Check-before-cut: with target 0.5 of 300 tokens (150), cutting the
        // lowest (200 tok) already overshoots — exactly one cut.
        let r = prune(
            &[100_000, 500_000, 900_000],
            &[200, 50, 50],
            &names(&["x", "y", "z"]),
            0.50,
        );
        assert_eq!(r.keep, names(&["y", "z"])); // original order, not score order
        assert_eq!(r.tokens_saved, 200);
    }

    #[test]
    fn prune_can_cut_everything() {
        // Reference legal output: the top scorer holding > (1-target) of the
        // mass still gets cut because cum stays below threshold until after.
        let r = prune(&[100_000, 900_000], &[10, 990], &names(&["a", "b"]), 0.70);
        assert_eq!(r.keep, Vec::<String>::new());
    }

    #[test]
    fn prune_empty() {
        let r = prune(&[], &[], &[], 0.70);
        assert!(r.keep.is_empty());
        assert_eq!(r.cut_frac, 0.0);
    }

    #[test]
    fn stub_tool_keeps_name_and_notes_restorability() {
        let t = json!({
            "name": "Grep",
            "description": "searches file contents with regex",
            "input_schema": {"type": "object", "properties": {"pattern": {"type": "string"}}}
        });
        let s = stub_tool(&t).unwrap();
        assert_eq!(s["name"], "Grep");
        let d = s["description"].as_str().unwrap();
        assert!(d.starts_with("searches file contents with regex"));
        assert!(d.contains(STUB_NOTE));
        // The detailed schema is gone; what remains accepts any arguments.
        assert_eq!(
            s["input_schema"],
            json!({"type": "object", "additionalProperties": true})
        );
        // Deterministic bytes: the frozen keep-set must serve a stable roster.
        assert_eq!(stub_tool(&t).unwrap(), s);
    }

    #[test]
    fn stub_tool_truncates_description_char_safe() {
        let long: String = "é".repeat(STUB_DESC_CHARS + 50);
        let t = json!({"name": "X", "description": long, "input_schema": {"type": "object"}});
        let d = stub_tool(&t).unwrap()["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(d.starts_with(&"é".repeat(STUB_DESC_CHARS)));
        assert!(!d.starts_with(&"é".repeat(STUB_DESC_CHARS + 1)));
        assert!(d.ends_with(STUB_NOTE));
    }

    #[test]
    fn stub_tool_refuses_provider_typed_tools() {
        // A provider-executed tool re-declared as a custom stub would change
        // execution semantics — those keep the reference hard-drop.
        let t = json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5});
        assert_eq!(stub_tool(&t), None);
        // Explicit type "custom" is still a plain client tool.
        let c = json!({"type": "custom", "name": "T", "input_schema": {"type": "object"}});
        assert!(stub_tool(&c).is_some());
        // No input_schema at all → not a stubbable client tool.
        let odd = json!({"name": "odd"});
        assert_eq!(stub_tool(&odd), None);
    }

    #[test]
    fn baked_brain_url_resolution() {
        // Env URL beats the baked default and keeps the conservative dev
        // contract default (existing behavior, byte-identical).
        assert_eq!(
            resolve_url_contract(Some("http://x:1/"), Some("https://baked.example"), None),
            Some(("http://x:1".into(), BrainContract::Dev, false))
        );
        // Unset env falls back to the baked release URL, which defaults the
        // contract to v1 — a released binary is data-plane-clean by default.
        assert_eq!(
            resolve_url_contract(None, Some("https://baked.example/"), None),
            Some(("https://baked.example".into(), BrainContract::V1, true))
        );
        // An explicit contract always wins over the baked v1 default.
        assert_eq!(
            resolve_url_contract(None, Some("https://baked.example"), Some("dev")),
            Some(("https://baked.example".into(), BrainContract::Dev, true))
        );
        // Explicitly EMPTY DASEIN_BRAIN_URL is the off switch even when a
        // default is baked in.
        assert_eq!(
            resolve_url_contract(Some(""), Some("https://baked.example"), None),
            None
        );
        // Dev/CI builds bake nothing: unset env means no brain, as before.
        assert_eq!(resolve_url_contract(None, None, None), None);
    }

    #[test]
    fn contract_defaults_to_dev() {
        // dev stays the default: only the exact "v1" opts into the new wire.
        assert_eq!(BrainContract::from_env_value(None), BrainContract::Dev);
        assert_eq!(
            BrainContract::from_env_value(Some("dev")),
            BrainContract::Dev
        );
        assert_eq!(BrainContract::from_env_value(Some("")), BrainContract::Dev);
        assert_eq!(
            BrainContract::from_env_value(Some("V1")),
            BrainContract::Dev
        );
        assert_eq!(BrainContract::from_env_value(Some("v1")), BrainContract::V1);
        assert_eq!(
            BrainContract::from_env_value(Some(" v1 ")),
            BrainContract::V1
        );
        assert_eq!(BrainContract::default(), BrainContract::Dev);
    }
}
