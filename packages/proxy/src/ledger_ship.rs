//! Ship savings-ledger rows to the platform for per-account attribution.
//!
//! The proxy already persists every row locally (`server::write_ledger` →
//! `ledger.jsonl`, the source of the statusline / `dasein savings` figures).
//! This ADDITIONALLY posts the same row to the platform's `/ledger` endpoint,
//! authenticated with the user's per-account `dsn_` key, so savings show up
//! per user in the dashboard (DIRECTION §7c savings-ledger ingest → per-account
//! reporting). Identity lives here, off the brain: the brain stays a stateless
//! coarse scorer; the platform does the per-request key lookup ("time delay is
//! fine") and attributes the row to an account.
//!
//! Best-effort and fail-open by construction: shipping is a fire-and-forget
//! background POST that never blocks or fails a request, and the row is always
//! on disk regardless. Enabled only when BOTH the platform URL and the user's
//! API key are configured; otherwise the proxy stays local-only exactly as
//! before. Platform ingest is idempotent on `request_id`, so a retried or
//! duplicated row is harmless.

use serde_json::Value;

/// Release-baked platform base URL: `DASEIN_DEFAULT_PLATFORM_URL` at BUILD time
/// (release.yml stamps it, like the brain URL). Runtime `DASEIN_PLATFORM_URL`
/// wins; an empty value is the off switch.
const BAKED_PLATFORM_URL: Option<&str> = option_env!("DASEIN_DEFAULT_PLATFORM_URL");

/// Where + how to ship rows. Cloned into each fire-and-forget task.
#[derive(Debug, Clone)]
pub struct LedgerSink {
    /// `{base}/ledger` receives one POST per row.
    url: String,
    /// The account's opaque `dsn_` key (sent as `X-Dasein-Key`). Never logged.
    api_key: String,
}

/// Resolve the sink live at ship time: `Some` only when a platform URL AND an
/// API key are both configured. Precedence — URL: `DASEIN_PLATFORM_URL` env →
/// baked default → `~/.dasein/credentials.json`; key: `DASEIN_API_KEY` env →
/// credentials file. Reading the (tiny) credentials file per shipped row is
/// what lets `dasein key set …` take effect immediately, with no proxy restart.
pub fn resolve() -> Option<LedgerSink> {
    let creds = crate::credentials::load();
    resolve_parts(
        std::env::var("DASEIN_PLATFORM_URL").ok().as_deref(),
        BAKED_PLATFORM_URL,
        creds.platform_url.as_deref(),
        std::env::var("DASEIN_API_KEY").ok().as_deref(),
        creds.api_key.as_deref(),
    )
}

/// Pure resolver, split out for tests. Empty values are off switches, mirroring
/// the brain-URL/key semantics in `brain.rs`.
fn resolve_parts(
    env_url: Option<&str>,
    baked_url: Option<&str>,
    file_url: Option<&str>,
    env_key: Option<&str>,
    file_key: Option<&str>,
) -> Option<LedgerSink> {
    let url = env_url
        .or(baked_url)
        .or(file_url)
        .map(|u| u.trim().trim_end_matches('/'))
        .filter(|u| !u.is_empty())?;
    let api_key = env_key
        .or(file_key)
        .map(str::trim)
        .filter(|k| !k.is_empty())?;
    Some(LedgerSink {
        url: url.to_string(),
        api_key: api_key.to_string(),
    })
}

/// Fire-and-forget POST of one ledger row to the platform. Resolves the sink
/// live (env → credentials file), so a key set mid-session ships the very next
/// row. Returns immediately; the request runs on a detached task and a failure
/// is a debug log, never an error the caller sees (the row is already on disk).
/// No-op when shipping is unconfigured, or when not inside a Tokio runtime
/// (e.g. a sync test harness), so `write_ledger` stays callable from any
/// context.
pub fn ship(client: &reqwest::Client, row: &Value) {
    let Some(sink) = resolve() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let client = client.clone();
    let url = format!("{}/ledger", sink.url);
    let api_key = sink.api_key.clone();
    let body = row.clone();
    handle.spawn(async move {
        match client
            .post(&url)
            .header("X-Dasein-Key", api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            // 4xx/5xx and transport errors are non-fatal: the row is on disk,
            // ingest is idempotent, so a later backfill can re-ship it.
            Ok(r) => tracing::debug!(status = %r.status(), "ledger ship: platform rejected row"),
            Err(e) => tracing::debug!("ledger ship: platform unreachable: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_both_url_and_key() {
        assert!(
            resolve_parts(Some("https://p.example"), None, None, Some("dsn_abc"), None).is_some()
        );
        // missing key (env and file) -> no sink
        assert!(resolve_parts(Some("https://p.example"), None, None, None, None).is_none());
        // missing url (env, baked, file) -> no sink
        assert!(resolve_parts(None, None, None, Some("dsn_abc"), None).is_none());
        // empty strings are off switches
        assert!(
            resolve_parts(Some(""), Some("https://baked"), None, Some("dsn_abc"), None).is_none()
        );
        assert!(resolve_parts(Some("https://p.example"), None, None, Some("  "), None).is_none());
    }

    #[test]
    fn url_precedence_env_baked_file_and_key_from_file() {
        // env URL beats baked; trailing slash trimmed
        let s = resolve_parts(
            Some("https://env.example/"),
            Some("https://baked"),
            None,
            Some("dsn_x"),
            None,
        )
        .expect("both present");
        assert_eq!(s.url, "https://env.example");
        // baked used only when env is absent
        let b = resolve_parts(
            None,
            Some("https://baked.example/"),
            None,
            Some("dsn_x"),
            None,
        )
        .unwrap();
        assert_eq!(b.url, "https://baked.example");
        // credentials-file URL is the last fallback; the key can come from the file
        let f = resolve_parts(
            None,
            None,
            Some("https://file.example/"),
            None,
            Some("dsn_file"),
        )
        .expect("file supplies both");
        assert_eq!(f.url, "https://file.example");
        assert_eq!(f.api_key, "dsn_file");
        // env key beats file key
        let k = resolve_parts(
            Some("https://p"),
            None,
            None,
            Some("dsn_env"),
            Some("dsn_file"),
        )
        .unwrap();
        assert_eq!(k.api_key, "dsn_env");
    }
}
