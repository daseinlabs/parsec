//! Anonymous install identity + fleet registration (docs/install-tracking.md).
//!
//! Mints a random machine-scoped id into `~/.parsec/install.json` and posts an
//! install-report/v0 ping (contracts/schemas/install-report.schema.json) to
//! the platform's `/installs` — the fleet-accounting complement of the
//! per-request savings ledger. The id is random, never derived from hardware
//! identifiers, and dies with `parsec uninstall`/purge (a reinstall is a new
//! install, which is the honest reading).
//!
//! Same posture as `ledger_ship`: best-effort and fail-open — a report never
//! blocks setup or serving, and a keyless ping is still sent (the platform
//! accepts anonymous registrations; a later keyed ping links the account).
//! Reports are deduped locally: at most one per day unless the fingerprint
//! (binary version + configured harnesses + key identity) changed.
//!
//! The serving path only READS the id (`current_id` → the ledger row's
//! `install_id`); minting happens off the serving path, in the setup/key CLI
//! and at worker startup.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

pub const STATE_CONTRACT: &str = "install/v0";
pub const REPORT_CONTRACT: &str = "install-report/v0";

/// Re-report after this long even when nothing changed — keeps `last_seen`
/// honest for the fleet's activity windows without chatty pings.
const REPORT_INTERVAL_SECS: u64 = 86_400;

/// `~/.parsec/install.json` — the id plus local report-dedupe state.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct InstallState {
    pub contract_version: String,
    /// `ins_<32 hex>` — random, minted once per install.
    pub install_id: String,
    pub created_unix: u64,
    /// Last successful report (0 = never reported).
    #[serde(default)]
    pub reported_unix: u64,
    /// What that report described (version|harnesses|key identity) — a change
    /// re-reports before the daily window elapses.
    #[serde(default)]
    pub reported_fingerprint: String,
}

pub fn path() -> PathBuf {
    crate::setup::parsec_home().join("install.json")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mint_id() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    h.update(t.as_nanos().to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(crate::setup::home_dir().to_string_lossy().as_bytes());
    format!("ins_{}", &format!("{:x}", h.finalize())[..32])
}

fn valid(st: &InstallState) -> bool {
    st.contract_version == STATE_CONTRACT
        && st.install_id.len() == 4 + 32
        && st.install_id.starts_with("ins_")
}

fn load() -> Option<InstallState> {
    let st: InstallState = serde_json::from_str(&std::fs::read_to_string(path()).ok()?).ok()?;
    valid(&st).then_some(st)
}

/// Atomic write (tmp + rename), like the credentials/setup-state siblings.
fn save(st: &InstallState) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(st).unwrap_or_default())?;
    std::fs::rename(&tmp, &p)
}

/// The id if this install has minted one — read-only, for the serving path
/// (`write_ledger` stamps it on rows). None until setup/key/proxy-start mints.
pub fn current_id() -> Option<String> {
    load().map(|st| st.install_id)
}

/// Get-or-mint. None only when the state can't be persisted (an id that
/// changes every process would corrupt the fleet count, so no id then).
fn ensure_state() -> Option<InstallState> {
    if let Some(st) = load() {
        return Some(st);
    }
    let st = InstallState {
        contract_version: STATE_CONTRACT.into(),
        install_id: mint_id(),
        created_unix: now_unix(),
        ..Default::default()
    };
    save(&st).ok()?;
    Some(st)
}

/// Harnesses parsec is CONFIGURED to intercept on this machine right now —
/// the install-level complement of the ledger's per-request `tool` field.
pub fn configured_harnesses() -> Vec<String> {
    let mut h = Vec::new();
    if crate::setup::load_state().is_some_and(|s| s.phase == "ready") {
        h.push("claude-code".to_string());
    }
    if crate::setup_desktop::load_desktop_state().is_some_and(|s| s.enabled) {
        h.push("claude-desktop".to_string());
    }
    if std::fs::read_to_string(crate::setup_codex::config_path())
        .is_ok_and(|c| c.contains("parsec-managed codex"))
    {
        h.push("codex".to_string());
    }
    if crate::setup_opencode::plugin_path().exists() {
        h.push("opencode".to_string());
    }
    h
}

/// What the ping describes; a change forces a re-report inside the daily
/// window. The key rides as a sha8, never the key itself — set/clear/rotate
/// each flip the fingerprint (a keyed ping is what links the account).
fn fingerprint(version: &str, harnesses: &[String], key: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let key_id = key
        .map(|k| {
            let mut h = Sha256::new();
            h.update(k.as_bytes());
            format!("{:x}", h.finalize())[..8].to_string()
        })
        .unwrap_or_default();
    format!("{version}|{}|{key_id}", harnesses.join(","))
}

fn due(st: &InstallState, fp: &str, now: u64) -> bool {
    st.reported_unix == 0
        || st.reported_fingerprint != fp
        || now.saturating_sub(st.reported_unix) > REPORT_INTERVAL_SECS
}

/// Everything a report needs, resolved live — None when the platform URL is
/// unconfigured, the state can't persist, or the last report still stands.
fn prepare() -> Option<(String, Option<String>, InstallState, String, Value)> {
    let base = crate::ledger_ship::platform_base_url()?;
    let st = ensure_state()?;
    let key = crate::apikey::account_key();
    let harnesses = configured_harnesses();
    let fp = fingerprint(env!("CARGO_PKG_VERSION"), &harnesses, key.as_deref());
    if !due(&st, &fp, now_unix()) {
        return None;
    }
    let body = json!({
        "contract_version": REPORT_CONTRACT,
        "install_id": st.install_id,
        "ts": crate::server::rfc3339_now(),
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "harnesses": harnesses,
    });
    Some((format!("{base}/installs"), key, st, fp, body))
}

fn commit(mut st: InstallState, fp: String) {
    st.reported_unix = now_unix();
    st.reported_fingerprint = fp;
    if let Err(e) = save(&st) {
        tracing::debug!("install report sent but dedupe state not saved: {e}");
    }
}

/// Blocking report for the sync CLI paths (`parsec setup`, `parsec key set`,
/// harness setups). Short timeout, every failure a debug line — registration
/// must never make setup look broken. Must NOT be called inside a Tokio
/// runtime (reqwest::blocking panics there); worker code uses [`spawn_reporter`].
pub fn report_blocking() {
    let Some((url, key, st, fp, body)) = prepare() else {
        return;
    };
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    else {
        return;
    };
    let mut req = client.post(&url).json(&body);
    if let Some(k) = key {
        req = req.header("X-Parsec-Key", k);
    }
    match req.send() {
        Ok(r) if r.status().is_success() => commit(st, fp),
        Ok(r) => tracing::debug!(status = %r.status(), "install report rejected"),
        Err(e) => tracing::debug!("install report failed: {e}"),
    }
}

async fn report_async(client: &reqwest::Client) {
    let Some((url, key, st, fp, body)) = prepare() else {
        return;
    };
    let mut req = client.post(&url).json(&body);
    if let Some(k) = key {
        req = req.header("X-Parsec-Key", k);
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => commit(st, fp),
        Ok(r) => tracing::debug!(status = %r.status(), "install report rejected"),
        Err(e) => tracing::debug!("install report failed: {e}"),
    }
}

/// Long-lived worker path: report at startup, then re-check every 6h so a
/// proxy that runs for weeks still refreshes last_seen (the daily dedupe in
/// [`due`] keeps the actual wire traffic to at most one ping per day) and
/// picks up harness/key changes made while it was already running. No-op
/// outside a Tokio runtime.
pub fn spawn_reporter(client: &reqwest::Client) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let client = client.clone();
    handle.spawn(async move {
        loop {
            report_async(&client).await;
            tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_id_shape() {
        let id = mint_id();
        assert!(id.starts_with("ins_"));
        assert_eq!(id.len(), 4 + 32);
        assert!(id[4..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn due_on_first_fingerprint_change_and_age() {
        let mut st = InstallState {
            contract_version: STATE_CONTRACT.into(),
            install_id: mint_id(),
            created_unix: 1000,
            ..Default::default()
        };
        let fp = fingerprint("0.2.6", &["claude-code".into()], None);
        assert!(due(&st, &fp, 1000), "never reported -> due");
        st.reported_unix = 1000;
        st.reported_fingerprint = fp.clone();
        assert!(!due(&st, &fp, 1000 + 60), "fresh + unchanged -> not due");
        assert!(
            due(&st, &fp, 1000 + REPORT_INTERVAL_SECS + 1),
            "stale -> due"
        );
        let keyed = fingerprint("0.2.6", &["claude-code".into()], Some("psc_x"));
        assert_ne!(fp, keyed, "key identity is part of the fingerprint");
        assert!(due(&st, &keyed, 1000 + 60), "changed fingerprint -> due");
    }

    #[test]
    fn fingerprint_never_carries_the_key() {
        let fp = fingerprint("0.2.6", &[], Some("psc_supersecret"));
        assert!(!fp.contains("supersecret"));
    }
}
