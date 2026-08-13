//! `parsec proxy` — the SUPERVISOR that owns the routed port.
//!
//! DIRECTION.md §4 puts the data plane on the user's machine; this process is
//! what makes that plane *survivable*. Claude Code routes `ANTHROPIC_BASE_URL`
//! here, at a fixed loopback port it read once at launch and cannot change.
//! The supervisor is deliberately DUMB — pure transport, no curation — so it
//! (nearly) cannot crash, and it holds the socket for its whole life:
//!
//! - It spawns the curating **worker** (`parsec proxy-worker`) on an ephemeral
//!   loopback port and **restarts** it whenever it exits or wedges (bugs:
//!   "proxy crashed mid-session", "proxy didn't auto restart").
//! - While the worker is down/unhealthy it forwards requests **straight to
//!   api.anthropic.com** with the user's own auth headers, so a worker crash
//!   degrades to a plain passthrough instead of a wedged session ("fall back
//!   to regular Anthropic URLs when the proxy is down").
//! - There is no idle self-shutdown: the port stays alive until `/shutdown`
//!   (or the machine reboots). The SessionStart hook / `parsec up` revive the
//!   supervisor if it ever dies.
//!
//! Why a second process rather than one resilient server: only a separate
//! address space survives the faults a `catch_unwind` cannot (OOM, a segfault
//! in a native TLS/ONNX dep, a deadlock, `abort`). The split is the whole
//! point — putting curation *in here* would give it the worker's crash
//! profile and take the fallback down with it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::setup::{default_port, parsec_home};

/// Request bodies are whole conversations — a few MB in practice. Cap high
/// enough never to reject a real Claude Code turn, low enough to bound a
/// hostile local caller (the bind is 127.0.0.1-only regardless).
const MAX_BODY: usize = 256 * 1024 * 1024;

/// The supervisor's liveness beacon. The worker watches this file and exits
/// if it goes stale — the portable orphan guard (no `PR_SET_PDEATHSIG` on
/// Windows/macOS; a heartbeat file behaves identically on every platform).
pub fn heartbeat_path() -> PathBuf {
    parsec_home().join("supervisor.heartbeat")
}

fn epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct SupState {
    /// Fallback upstream when the worker is down (default api.anthropic.com).
    upstream: String,
    /// Fallback upstreams for the OpenAI-wire namespaces (`/openai/*` BYOK,
    /// `/chatgpt/*` subscription) — the Anthropic fallback would 404 them.
    openai_upstream: String,
    chatgpt_upstream: String,
    client: reqwest::Client,
    /// Ephemeral loopback port of a HEALTHY worker; 0 = none (go direct).
    worker_port: AtomicU16,
    /// The worker process handle — held so we can `wait()`/`kill()` it.
    child: Mutex<Option<std::process::Child>>,
    shutting_down: AtomicBool,
    /// Requests served by the direct-to-Anthropic fallback (worker down).
    fallbacks: AtomicU64,
    heartbeat: PathBuf,
}

impl SupState {
    fn new(upstream: String) -> Self {
        SupState {
            upstream,
            openai_upstream: crate::openai::upstream_from_env(),
            chatgpt_upstream: crate::openai::chatgpt_upstream_from_env(),
            client: reqwest::Client::new(),
            worker_port: AtomicU16::new(0),
            child: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            fallbacks: AtomicU64::new(0),
            heartbeat: heartbeat_path(),
        }
    }

    /// GET /health on `port` and confirm the responder is a parsec proxy (ours
    /// or the worker's) — the identity check that distinguishes a live worker
    /// from a foreign squatter reusing the port.
    async fn health_is_parsec(&self, port: u16) -> bool {
        match self
            .client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r
                .text()
                .await
                .map(|t| t.contains("parsec-proxy"))
                .unwrap_or(false),
            _ => false,
        }
    }

    fn write_heartbeat(&self) {
        if let Some(dir) = self.heartbeat.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.heartbeat, epoch_s().to_string());
    }

    fn kill_worker(&self) {
        if let Some(mut ch) = lock(&self.child).take() {
            let _ = ch.kill();
            let _ = ch.wait();
        }
    }

    /// Spawn the worker on a fresh ephemeral port and wait for it to answer a
    /// health probe. Returns the port once healthy. The worker is a DIRECT
    /// child (not detached) so we remain its parent — able to reap and kill
    /// it — and it inherits our env (brain config, upstream) verbatim.
    async fn spawn_worker(&self) -> anyhow::Result<u16> {
        let port = pick_free_port()?;
        let exe = std::env::current_exe()?;
        let log_dir = parsec_home();
        std::fs::create_dir_all(&log_dir)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("proxy.log"))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("proxy-worker")
            .env("PARSEC_PROXY_PORT", port.to_string())
            .env("PARSEC_SUPERVISOR_HEARTBEAT", &self.heartbeat)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log.try_clone()?))
            .stderr(std::process::Stdio::from(log));
        #[cfg(windows)]
        {
            // `parsec.exe` is a console-subsystem binary. The supervisor that
            // spawns this worker is itself detached (no console — see
            // `setup::spawn_detached`), so without a flag here Windows would
            // allocate a fresh, VISIBLE console window for the worker. Suppress
            // it: stdout/stderr already go to proxy.log, so the worker never
            // needs a console. CREATE_NO_WINDOW (not DETACHED_PROCESS) because
            // the worker stays our managed child — we remain its parent to reap
            // and kill it.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
        }
        let child = cmd.spawn()?;
        *lock(&self.child) = Some(child);

        // Poll for liveness up to ~5s; bail early if it died on startup (e.g.
        // its ephemeral port got stolen in the bind race — the caller retries
        // with a new port).
        for _ in 0..50 {
            if self.health_is_parsec(port).await {
                return Ok(port);
            }
            if let Some(ch) = lock(&self.child).as_mut() {
                if matches!(ch.try_wait(), Ok(Some(_))) {
                    anyhow::bail!("worker exited during startup");
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("worker never became healthy on 127.0.0.1:{port}")
    }

    fn teardown(&self, reason: &str) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.kill_worker();
        let _ = std::fs::remove_file(&self.heartbeat);
        tracing::info!(
            reason,
            fallbacks = self.fallbacks.load(Ordering::Relaxed),
            "parsec supervisor shutting down (worker stopped)"
        );
    }
}

fn pick_free_port() -> anyhow::Result<u16> {
    // Bind :0, read the assigned port, drop. The worker rebinds it a beat
    // later; the TOCTOU window is negligible on loopback and self-heals via
    // respawn if lost.
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

/// `parsec proxy` entrypoint. Bind the routed port and run forever.
pub fn run() -> anyhow::Result<()> {
    let port = default_port();
    let upstream = std::env::var("PARSEC_UPSTREAM")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let state = Arc::new(SupState::new(upstream));

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                // Idempotent double-start: if a parsec supervisor already owns
                // the port, this spawn is a no-op success (the SessionStart
                // hook and `parsec up` both fire spawns that may race).
                if state.health_is_parsec(port).await {
                    tracing::info!("a parsec proxy already owns 127.0.0.1:{port} — nothing to do");
                    return Ok(());
                }
                // Foreign squatter on the routed port. We cannot move — Claude
                // Code is already pointed here — so fail loudly; `parsec setup`
                // is what re-routes to a free port for the next session.
                anyhow::bail!(
                    "cannot bind 127.0.0.1:{port}: {e} — a non-parsec process holds the routed \
                     port, so curation is off until it frees. Run `parsec setup` to re-route to \
                     a free port (takes effect on the next Claude Code launch)."
                );
            }
        };
        tracing::info!(
            "parsec supervisor listening on 127.0.0.1:{port} — owns the routed port; \
             worker curates, fallback is direct-to-Anthropic"
        );
        tokio::spawn(monitor(state.clone()));
        let on_exit = state.clone();
        axum::serve(listener, router(state))
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        on_exit.teardown("signal");
        Ok(())
    })
}

/// Keep exactly one healthy worker alive, and refresh the heartbeat every
/// tick. Respawn on exit; kill+respawn on a failed health probe (a wedged
/// worker never `exit`s, so process-liveness alone would miss a deadlock).
async fn monitor(st: Arc<SupState>) {
    let mut health_fails = 0u32;
    let mut backoff = 1u64;
    st.write_heartbeat(); // fresh before the worker's first check
    loop {
        if st.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        st.write_heartbeat();

        // Reap a dead worker (crash or clean exit) so the block below respawns.
        let running = {
            let mut guard = lock(&st.child);
            match guard.as_mut() {
                Some(ch) => match ch.try_wait() {
                    Ok(Some(status)) => {
                        tracing::warn!(%status, "worker exited — respawning");
                        st.worker_port.store(0, Ordering::Relaxed);
                        *guard = None;
                        false
                    }
                    _ => true, // still running (or try_wait errored — assume alive)
                },
                None => false,
            }
        };

        if !running {
            match st.spawn_worker().await {
                Ok(p) => {
                    st.worker_port.store(p, Ordering::Relaxed);
                    health_fails = 0;
                    backoff = 1;
                    tracing::info!("worker healthy on 127.0.0.1:{p}");
                }
                Err(e) => {
                    st.kill_worker();
                    st.worker_port.store(0, Ordering::Relaxed);
                    tracing::warn!(
                        "worker spawn failed ({e}) — retrying in {backoff}s \
                         (requests fall back to Anthropic meanwhile)"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(30);
                    continue;
                }
            }
        } else {
            // Liveness probe on the running worker.
            let port = st.worker_port.load(Ordering::Relaxed);
            if port != 0 && !st.health_is_parsec(port).await {
                health_fails += 1;
                tracing::warn!(health_fails, "worker health probe failed");
                if health_fails >= 2 {
                    tracing::warn!("worker unhealthy — killing for respawn");
                    st.kill_worker();
                    st.worker_port.store(0, Ordering::Relaxed);
                    health_fails = 0;
                }
            } else {
                health_fails = 0;
            }
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

pub fn router(state: Arc<SupState>) -> Router {
    Router::new()
        // Identity + liveness of the SUPERVISOR (answered locally). `service`
        // carries "parsec-proxy" so `parsec uninstall`'s stop probe and the
        // hook's identity check recognize us.
        .route(
            "/health",
            get(|| async {
                Json(json!({
                    "ok": true,
                    "service": "parsec-proxy",
                    "role": "supervisor",
                    "version": env!("CARGO_PKG_VERSION"),
                }))
            }),
        )
        // Localhost kill-switch (parsec uninstall): stop the worker, then the
        // supervisor. Reply first, exit off the response path so the 200
        // flushes.
        .route("/shutdown", post(shutdown_handler))
        // Everything else is forwarded: to the worker when healthy, else
        // straight to Anthropic.
        .fallback(forward)
        .with_state(state)
}

async fn shutdown_handler(State(st): State<Arc<SupState>>) -> Response {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        st.teardown("/shutdown");
        std::process::exit(0);
    });
    Json(json!({
        "ok": true,
        "service": "parsec-proxy",
        "role": "supervisor",
        "shutting_down": true,
    }))
    .into_response()
}

/// The reverse-proxy path. Forward to the worker when one is healthy; on a
/// worker transport error (it died between the probe and now), or when no
/// worker is up, forward the SAME request straight to the wire's upstream —
/// api.anthropic.com for the Anthropic paths, api.openai.com for `/openai/*`
/// (the Anthropic fallback would 404 that wire, wedging Codex exactly when
/// fail-open matters). Pure byte transport — no curation, no ledger (an
/// un-curated fallback has nothing to measure; §8.4 forbids fabricating a
/// row).
async fn forward(State(st): State<Arc<SupState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let is_openai =
        parts.uri.path().starts_with("/openai/") || parts.uri.path().starts_with("/chatgpt/");
    // WebSocket deflection belongs at the routed port too: a wedged worker
    // must not turn an Upgrade attempt into a confusing forwarded error —
    // 426 here means the client falls back to HTTP SSE either way. The
    // subscription path NEEDS this: Codex's built-in provider tries
    // Responses-over-WebSocket first.
    if is_openai && parts.headers.contains_key(header::UPGRADE) {
        return crate::openai::reject_upgrade();
    }
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "request body too large").into_response(),
    };
    let path_q = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();

    let worker_port = st.worker_port.load(Ordering::Relaxed);
    if worker_port != 0 {
        let base = format!("http://127.0.0.1:{worker_port}");
        match send(
            &st,
            &base,
            &parts,
            &path_q,
            worker_headers(&parts.headers, is_openai),
            bytes.clone(),
        )
        .await
        {
            Ok(resp) => return stream_back(resp),
            Err(e) => {
                tracing::warn!(
                    "worker at {base} unreachable ({e}) — falling back to upstream; \
                     monitor will respawn it"
                );
                st.worker_port.store(0, Ordering::Relaxed);
            }
        }
    }

    st.fallbacks.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(path = %path_q, "worker down — forwarding direct to upstream");
    // Direct fallback: pick the wire's upstream and, for the OpenAI-wire
    // namespaces, drop our routing prefix and any x-parsec-* internals
    // (they never leave the machine's parsec hops).
    let (base, path, headers) = if let Some(p) = path_q.strip_prefix("/chatgpt") {
        (
            st.chatgpt_upstream.as_str(),
            p.to_string(),
            crate::openai::forward_headers(&parts.headers, true),
        )
    } else if is_openai {
        (
            st.openai_upstream.as_str(),
            path_q
                .strip_prefix("/openai")
                .unwrap_or(path_q.as_str())
                .to_string(),
            crate::openai::forward_headers(&parts.headers, true),
        )
    } else {
        (
            st.upstream.as_str(),
            path_q.clone(),
            crate::server::forward_auth_headers(&parts.headers),
        )
    };
    match send(&st, base, &parts, &path, headers, bytes).await {
        Ok(resp) => stream_back(resp),
        Err(e) => bad_gateway(&e),
    }
}

/// Headers for the supervisor→worker hop. Anthropic paths keep the narrow
/// auth allowlist plus the charset-gated `x-parsec-tool` attribution tag —
/// without the re-add, a shim's tag died at this hop and every routed
/// opencode request lost its ledger attribution. `/openai/*` forwards the
/// verbatim-minus-hop-by-hop set the passthrough needs (Codex sends
/// provider headers an allowlist would break); the WORKER strips x-parsec-*
/// before anything goes upstream.
fn worker_headers(inbound: &axum::http::HeaderMap, is_openai: bool) -> axum::http::HeaderMap {
    if is_openai {
        return crate::openai::forward_headers(inbound, false);
    }
    let mut h = crate::server::forward_auth_headers(inbound);
    if let Some(tag) = crate::server::tool_from_headers(inbound) {
        // The gate guarantees a slug ⇒ always a valid header value.
        if let Ok(v) = axum::http::HeaderValue::from_str(&tag) {
            h.insert("x-parsec-tool", v);
        }
    }
    h
}

async fn send(
    st: &SupState,
    base: &str,
    parts: &axum::http::request::Parts,
    path_q: &str,
    // Prepared by the caller per hop: the worker hop keeps the attribution
    // tag; upstream hops carry the wire's auth surface only, never logged or
    // stored (§3).
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> reqwest::Result<reqwest::Response> {
    let url = format!("{}{}", base.trim_end_matches('/'), path_q);
    st.client
        .request(parts.method.clone(), url)
        .headers(headers)
        .body(body)
        .send()
        .await
}

/// Relay an upstream response unmodified, streaming the body (SSE stays
/// unbuffered so tokens arrive as they are produced).
fn stream_back(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let ct = resp.headers().get(header::CONTENT_TYPE).cloned();
    let mut b = Response::builder().status(status);
    if let Some(ct) = ct {
        b = b.header(header::CONTENT_TYPE, ct);
    }
    b.body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn bad_gateway(e: &reqwest::Error) -> Response {
    tracing::warn!("upstream unreachable: {e}");
    (
        StatusCode::BAD_GATEWAY,
        format!("parsec proxy: upstream unreachable: {e}"),
    )
        .into_response()
}

/// Is the heartbeat file's timestamp within `max_stale_s` of `now`? A missing
/// or unparseable file reads as NOT fresh — the supervisor is presumed gone.
fn heartbeat_fresh(raw: Option<String>, now: u64, max_stale_s: u64) -> bool {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .map(|ts| now.saturating_sub(ts) <= max_stale_s)
        .unwrap_or(false)
}

/// Worker-side orphan guard (called from `server::run`). When spawned under a
/// supervisor, watch its heartbeat file and exit if it goes stale — so a
/// crashed/killed supervisor never leaves an unreachable worker running
/// forever (there is no idle-exit backstop any more). A hand-run worker has
/// no `PARSEC_SUPERVISOR_HEARTBEAT` and is left alone.
pub fn arm_orphan_guard() {
    let Ok(hb) = std::env::var("PARSEC_SUPERVISOR_HEARTBEAT") else {
        return;
    };
    let hb = hb.trim().to_string();
    if hb.is_empty() {
        return;
    }
    let path = PathBuf::from(hb);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(15));
        // Supervisor writes every ~3s; 45s stale ⇒ it is gone.
        let fresh = heartbeat_fresh(std::fs::read_to_string(&path).ok(), epoch_s(), 45);
        if !fresh {
            tracing::info!("supervisor heartbeat stale/missing — worker exiting (orphan guard)");
            std::process::exit(0);
        }
    });
}

/// Resolves on Ctrl-C or (unix) SIGTERM so axum drains in-flight relays before
/// `serve` returns. Mirrors the worker's handler.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_freshness_window() {
        // Within the window ⇒ fresh; past it ⇒ stale.
        assert!(heartbeat_fresh(Some("1000".into()), 1000, 45));
        assert!(heartbeat_fresh(Some("1000".into()), 1044, 45));
        assert!(!heartbeat_fresh(Some("1000".into()), 1046, 45));
        // A clock that went backwards (suspend/resume) reads as fresh, not a
        // false orphan-kill (saturating_sub ⇒ 0).
        assert!(heartbeat_fresh(Some("2000".into()), 1000, 45));
        // Missing / garbage ⇒ supervisor presumed gone.
        assert!(!heartbeat_fresh(None, 1000, 45));
        assert!(!heartbeat_fresh(Some("  ".into()), 1000, 45));
        assert!(!heartbeat_fresh(Some("nope".into()), 1000, 45));
    }

    #[test]
    fn pick_free_port_is_bindable() {
        let p = pick_free_port().expect("a free ephemeral port");
        assert_ne!(p, 0);
        // Nothing should hold it in the instant after we released it.
        assert!(std::net::TcpListener::bind(("127.0.0.1", p)).is_ok());
    }
}
