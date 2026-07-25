//! The unified API-key gate — the ONE place that answers "is dasein entitled
//! to run?" and renders the "get a key" messaging.
//!
//! Everything a missing key turns off checks [`enabled`] / [`resolve_key`]
//! here, and every "get a key" line renders from [`gate_banner`], so the policy
//! and the copy live in a single file (DIRECTION §7 entitlement). Today three
//! call sites gate on this:
//! - the hook (no-reread / loop-breaker savings) — `hook::run` PreToolUse/Post,
//! - the backend/GNN curation — `brain::BrainConfig::from_env`,
//! - the proxy serve path — `server::messages` (pure passthrough when off).
//!
//! Flip the whole product on/off from `enabled()`; change the URL/copy from
//! `SIGNUP_URL` / `gate_banner`. Nothing else should re-derive the key.

use crate::credentials;

/// Where the user mints their per-account `dsn_` key.
pub const SIGNUP_URL: &str = "app.getparsec.ai";

/// First non-empty (trimmed) candidate — the shared precedence primitive.
fn first_key(cands: &[Option<&str>]) -> Option<String> {
    cands
        .iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The per-account `dsn_` key from the dashboard: `DASEIN_API_KEY` env →
/// `~/.dasein/credentials.json`. This is the entitlement + attribution key — it
/// authenticates to the platform brain AND rides savings-ledger rows. It is NOT
/// the self-host shared `DASEIN_BRAIN_KEY`, which must never reach the platform.
pub fn account_key() -> Option<String> {
    let creds = credentials::load();
    first_key(&[
        std::env::var("DASEIN_API_KEY").ok().as_deref(),
        creds.api_key.as_deref(),
    ])
}

/// The bearer the proxy authenticates to the brain with: an explicit shared
/// `DASEIN_BRAIN_KEY` (dev/self-host) wins, else the per-account key. Read fresh
/// each call so `dasein key set` applies without a proxy restart.
pub fn resolve_key() -> Option<String> {
    let creds = credentials::load();
    first_key(&[
        std::env::var("DASEIN_BRAIN_KEY").ok().as_deref(),
        std::env::var("DASEIN_API_KEY").ok().as_deref(),
        creds.api_key.as_deref(),
    ])
}

/// THE gate. `false` ⇒ dasein saves NOTHING: the hook does not gate re-reads or
/// loops, the backend is never called, and the proxy is pure passthrough. A key
/// of EITHER kind (self-host `DASEIN_BRAIN_KEY` or the account key) enables it.
pub fn enabled() -> bool {
    resolve_key().is_some()
}

/// The prominent, top-of-session banner shown while unentitled. Pure so it is
/// testable; [`gate_banner`] supplies the live `enabled`/`muted` inputs.
fn banner_text() -> String {
    format!(
        "⚠️  dasein: NO API KEY — savings are OFF.\n\
         Claude Code runs normally, but dasein will not curate context or block \
         re-reads/loops until you add a key.\n\
         → Get your key at {SIGNUP_URL}, then run:  dasein key set <dsn_…>\n\
         (silence this reminder with DASEIN_API_KEY_NOTE=0)"
    )
}

/// The banner to show at SessionStart (and install), or `None` when entitled or
/// muted. Split from [`banner_text`] so the decision is unit-testable.
fn banner_for(enabled: bool, muted: bool) -> Option<String> {
    (!enabled && !muted).then(banner_text)
}

/// Live gate banner: `Some` iff unentitled and not muted via
/// `DASEIN_API_KEY_NOTE=0`.
pub fn gate_banner() -> Option<String> {
    banner_for(
        enabled(),
        std::env::var("DASEIN_API_KEY_NOTE").ok().as_deref() == Some("0"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_precedence_first_nonempty_wins() {
        // Brain key wins (self-host override).
        assert_eq!(
            first_key(&[Some(" braink "), Some("acct"), None]).as_deref(),
            Some("braink")
        );
        // Falls through empties to the account key.
        assert_eq!(
            first_key(&[Some("  "), None, Some("dsn_file")]).as_deref(),
            Some("dsn_file")
        );
        // All empty/absent -> no key -> gate closed.
        assert_eq!(first_key(&[None, Some(""), Some("   ")]), None);
    }

    #[test]
    fn banner_shows_only_when_unentitled_and_unmuted() {
        // Unentitled and not muted -> banner, and it points at the signup URL.
        let b = banner_for(false, false).expect("banner when unentitled");
        assert!(b.contains(SIGNUP_URL));
        assert!(b.contains("dasein key set"));
        // Entitled -> never.
        assert_eq!(banner_for(true, false), None);
        // Muted -> suppressed even when unentitled.
        assert_eq!(banner_for(false, true), None);
    }
}
