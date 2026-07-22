//! Per-user platform credentials at `~/.dasein/credentials.json`.
//!
//! Holds the account's `dsn_` API key (and optionally a platform URL) the proxy
//! ships savings-ledger rows with — so a user configures it once from inside
//! Claude (`dasein key set …` / the `/dasein-key` skill) instead of editing env
//! vars or settings.json. Written here, read by `ledger_ship::resolve` at ship
//! time (no proxy restart needed — it re-reads the file per shipped row).
//!
//! Runtime env still wins: `DASEIN_API_KEY` / `DASEIN_PLATFORM_URL` override the
//! file, so CI and self-host deployments keep the env path. The file is the
//! end-user path.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The stored credential blob. Fields optional so a partial file is tolerated.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Optional platform base URL override (released builds bake one; a dev
    /// build has none, so a self-hoster can point it here without env).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_url: Option<String>,
}

pub fn path() -> PathBuf {
    crate::setup::dasein_home().join("credentials.json")
}

/// Best-effort load — a missing or malformed file yields empty credentials
/// (shipping simply stays off), never an error on the serving path.
pub fn load() -> Credentials {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic write (tmp + rename), 0600 — the key is a bearer secret.
pub fn store(c: &Credentials) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(c).unwrap_or_default())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &p)
}

/// Remove the file (idempotent — absent is success).
pub fn clear() -> std::io::Result<()> {
    match std::fs::remove_file(path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `dsn_ab…wxyz` — masked form for confirmations/logs (never the full key).
pub fn mask(key: &str) -> String {
    let n = key.chars().count();
    if n <= 8 {
        return "…".to_string();
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}
