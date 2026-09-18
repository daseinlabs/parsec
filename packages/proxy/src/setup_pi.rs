//! `parsec setup pi` — route the pi coding agent through the local proxy.
//!
//! pi speaks Anthropic Messages, so it rides the proxy's EXISTING anthropic
//! wire; nothing on the serving path changes. Two managed artifacts, both
//! ownership-gated and non-destructive:
//!
//! 1. **`<pi dir>/models.json`**: pi's built-in `anthropic` provider is
//!    re-pointed by supplying only a new `baseUrl` — every built-in Anthropic
//!    model stays available and the user's existing OAuth or API-key auth
//!    keeps working. The document is parsed, two keys are set, and it is
//!    re-serialized; `serde_json`'s `preserve_order` keeps every other
//!    provider, model and key in its original position (DIRECTION.md's
//!    determinism rule is why that feature is non-negotiable).
//!    - `providers.anthropic.baseUrl = "http://127.0.0.1:PORT"` — a BARE
//!      ORIGIN with no `/v1`. pi hands `baseUrl` to the Anthropic SDK as
//!      `baseURL` (packages/ai/src/api/anthropic-messages.ts) and the SDK
//!      appends `/v1/messages` itself, exactly as the built-in
//!      `https://api.anthropic.com` does. Writing `/v1` here would produce
//!      `/v1/v1/messages`.
//!    - `providers.anthropic.headers["x-parsec-tool"] = "pi"` — ledger
//!      attribution, so savings can be reported per tool. pi resolves header
//!      values with `$ENV` and `!cmd` forms; a literal needs neither.
//! 2. **`<pi dir>/extensions/parsec.ts`**: the embedded extension, a managed
//!    file drop owned by [`SENTINEL`] on exactly the opencode shim's terms —
//!    a file we wrote may be refreshed or removed, a file without the
//!    sentinel is the user's and is never touched.
//!
//! Deliberately NOT touched: `~/.parsec/setup_state.json`. That file drives
//! Claude Code's auto-setup phase machine, and marking it `ready` from a
//! pi-only install would make a later Claude Code setup skip its routing.
//! This command and the extension both fall back to the default port when no
//! state exists, so they agree without sharing state.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The extension, embedded verbatim. include_str! keeps the shipped source
/// and the installed file drop byte-identical, so [`decide`] can compare them.
const EXTENSION_TS: &str = include_str!("../../pi-extension/parsec.ts");

/// Marks an extension file as parsec-managed. Present in every version we
/// have ever shipped, so ownership survives extension updates the way the
/// opencode shim's sentinel does.
const SENTINEL: &str = "parsec-managed-extension";

/// Ledger attribution tag; must match `TOOL` in packages/pi-extension/parsec.ts.
const TOOL: &str = "pi";

/// The wire a pi install rides. Setup refuses to call a listener compatible
/// unless it advertises it.
const REQUIRED_WIRES: &[&str] = &["anthropic"];

/// pi's agent dir. `PI_CODING_AGENT_DIR` is pi's own override for where the
/// agent keeps `models.json`, `extensions/` and friends; an empty or
/// whitespace value is treated as unset, matching the `CODEX_HOME` rule in
/// [`crate::setup_codex`].
fn pi_agent_dir() -> PathBuf {
    std::env::var("PI_CODING_AGENT_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::setup::home_dir().join(".pi").join("agent"))
}

pub fn models_path() -> PathBuf {
    pi_agent_dir().join("models.json")
}

pub fn extension_path() -> PathBuf {
    pi_agent_dir().join("extensions").join("parsec.ts")
}

/// Whether a parsec-managed extension is installed: the install report's
/// "configured harness" test (`install::configured_harnesses`), the pi
/// counterpart of `setup_opencode::plugin_path().exists()`. Ownership-gated
/// like everything else here — a foreign extension file does not count.
pub fn is_configured() -> bool {
    std::fs::read_to_string(extension_path()).is_ok_and(|c| c.contains(SENTINEL))
}

/// What a write attempt of the extension file should do.
#[derive(Debug, PartialEq, Eq)]
enum WriteDecision {
    /// No file, or an older managed one → (re)write.
    Write,
    /// Byte-identical managed file already there.
    Current,
    /// A file without our sentinel: the user's, refuse to touch.
    Foreign,
}

fn decide(existing: Option<&str>) -> WriteDecision {
    match existing {
        None => WriteDecision::Write,
        Some(cur) if cur == EXTENSION_TS => WriteDecision::Current,
        Some(cur) if cur.contains(SENTINEL) => WriteDecision::Write,
        Some(_) => WriteDecision::Foreign,
    }
}

// ── models.json plumbing (format-preserving JSON edits) ─────────────────────

/// The document's top-level object. An absent or blank file is an empty one;
/// anything that is not a JSON object is refused rather than overwritten.
fn parse_root(existing: Option<&str>) -> Result<Map<String, Value>, String> {
    let text = existing.unwrap_or("").trim();
    if text.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err("is not a JSON object — move it aside, then re-run".to_string()),
        Err(e) => Err(format!(
            "is not valid JSON ({e}) — fix or move it aside, then re-run"
        )),
    }
}

/// Borrow `key` as an object, creating an empty one when absent. A non-object
/// there is the user's data in a shape we cannot merge into: refuse.
fn entry_object<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<&'a mut Map<String, Value>, String> {
    let slot = map
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    match slot {
        Value::Object(m) => Ok(m),
        _ => Err(format!(
            "has a `{what}` that is not a JSON object — fix it, then re-run"
        )),
    }
}

/// Two-space pretty JSON with a trailing newline — what pi's own docs show and
/// what an editor will leave alone.
fn render(root: &Map<String, Value>) -> Result<String, String> {
    serde_json::to_string_pretty(&Value::Object(root.clone()))
        .map(|s| format!("{s}\n"))
        .map_err(|e| format!("could not be re-serialized ({e})"))
}

/// Set our two keys, preserving every other byte in JSON terms.
///
/// Ownership: an existing `providers.anthropic.baseUrl` that is not a loopback
/// parsec-shaped URL is the user's own gateway and setup refuses rather than
/// stomping it. A loopback one (any port) is ours to re-point.
fn upsert_models(existing: Option<&str>, port: u16) -> Result<String, String> {
    let mut root = parse_root(existing)?;
    {
        let providers = entry_object(&mut root, "providers", "providers")?;
        let anthropic = entry_object(providers, "anthropic", "providers.anthropic")?;
        if let Some(base) = anthropic.get("baseUrl") {
            let ours = base
                .as_str()
                .is_some_and(|s| crate::hook::local_proxy_port(s).is_some());
            if !ours {
                return Err(format!(
                    "already points `providers.anthropic.baseUrl` at {base} — that is \
                     your own gateway, not ours to repoint. Remove that key (or point \
                     it at the parsec proxy yourself), then re-run"
                ));
            }
        }
        anthropic.insert(
            "baseUrl".to_string(),
            Value::String(format!("http://127.0.0.1:{port}")),
        );
        let headers = entry_object(anthropic, "headers", "providers.anthropic.headers")?;
        headers.insert("x-parsec-tool".to_string(), Value::String(TOOL.to_string()));
    }
    render(&root)
}

/// Remove exactly what [`upsert_models`] added. `None` = nothing of ours was
/// there. A foreign baseUrl is left untouched, and the user's own keys under
/// `providers.anthropic` keep the object alive.
fn remove_from_models(existing: &str) -> Result<Option<String>, String> {
    let mut root = parse_root(Some(existing))?;
    let mut changed = false;
    {
        let Some(Value::Object(providers)) = root.get_mut("providers") else {
            return Ok(None);
        };
        let Some(Value::Object(anthropic)) = providers.get_mut("anthropic") else {
            return Ok(None);
        };
        let ours = anthropic
            .get("baseUrl")
            .and_then(Value::as_str)
            .is_some_and(|s| crate::hook::local_proxy_port(s).is_some());
        if ours {
            anthropic.remove("baseUrl");
            changed = true;
        }
        if let Some(Value::Object(headers)) = anthropic.get_mut("headers") {
            if headers.get("x-parsec-tool").and_then(Value::as_str) == Some(TOOL) {
                headers.remove("x-parsec-tool");
                changed = true;
            }
            if headers.is_empty() {
                anthropic.remove("headers");
            }
        }
        if anthropic.is_empty() {
            providers.remove("anthropic");
        }
    }
    if !changed {
        return Ok(None);
    }
    render(&root).map(Some)
}

// ── file plumbing ───────────────────────────────────────────────────────────

fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Atomic-ish write: temp file + rename.
fn write_via_tmp(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("parsec-tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Bring a proxy that serves the anthropic wire up on `port`, replacing an
/// older parsec build that holds it. A foreign listener is never touched and
/// never reported as success — the user has to free the port themselves.
fn warm_proxy(port: u16) {
    use crate::setup::PortOccupant;
    let spawn = |what: &str| match crate::setup::spawn_proxy_detached(port, &[]) {
        Ok(()) => println!("proxy {what} on 127.0.0.1:{port}"),
        Err(e) => println!("proxy pre-warm failed ({e}) — run `parsec up` before pi"),
    };
    match crate::setup::classify_port(port, REQUIRED_WIRES) {
        PortOccupant::Compatible => println!("proxy already listening on 127.0.0.1:{port}"),
        PortOccupant::Free => spawn("starting"),
        PortOccupant::StaleParsec => {
            println!(
                "an older parsec proxy holds 127.0.0.1:{port} and does not serve the \
                 anthropic route — replacing it"
            );
            if !crate::setup::shutdown_parsec_on(port) {
                println!(
                    "it refused to stop — run `parsec up --restart`, then re-run \
                     `parsec setup pi`"
                );
                return;
            }
            for _ in 0..40 {
                if !crate::hook::port_listening(port) {
                    spawn("restarted");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            println!(
                "it acked the stop but kept the port — run `parsec up --restart`, then \
                 re-run `parsec setup pi`"
            );
        }
        PortOccupant::Foreign => println!(
            "127.0.0.1:{port} is held by something that is not the parsec proxy — \
             left alone. pi traffic will fail until that port is free; stop it and \
             re-run `parsec setup pi`."
        ),
    }
}

/// `parsec setup pi` — repoint pi's anthropic provider, drop the extension,
/// refresh the binary alias the extension probes, warm the proxy.
pub fn setup() -> anyhow::Result<()> {
    let dir = pi_agent_dir();
    if !dir.exists() {
        println!(
            "note: {} does not exist yet — is pi installed? Creating it anyway.",
            dir.display()
        );
    }

    // Port: state-file port when Claude Code setup ran, else the default.
    //
    // `choose_free_port` must run BEFORE models.json is written, for the same
    // reason it does in `setup_codex`: the port is baked into the file as
    // literal text and nothing re-validates it at request time (unlike the
    // opencode shim, which re-probes /health before it routes). A port held by
    // a FOREIGN process is therefore not a dead route but a live one pointed
    // at a stranger — carrying the user's Anthropic OAuth token or API key on
    // every request. `warm_proxy` below warns about a foreign listener, but by
    // then the file already names it.
    let preferred = crate::setup::load_state()
        .map(|st| st.port)
        .filter(|p| *p > 0)
        .unwrap_or_else(crate::setup::default_port);
    let port = crate::setup::choose_free_port(preferred);
    if port != preferred {
        println!(
            "127.0.0.1:{preferred} is held by a non-parsec process — routing pi at \
             {port} instead. If Claude Code is routed at {preferred}, re-run \
             `parsec setup` so both land on one proxy."
        );
    }

    let models = models_path();
    let existing = read_optional(&models)?;
    match upsert_models(existing.as_deref(), port) {
        Err(msg) => anyhow::bail!("{} {msg}", models.display()),
        Ok(next) => {
            if existing.as_deref() == Some(next.as_str()) {
                println!("pi already routed in {}", models.display());
            } else {
                write_via_tmp(&models, &next)?;
                println!(
                    "routed pi's anthropic provider at http://127.0.0.1:{port} in {}",
                    models.display()
                );
            }
        }
    }

    let ext = extension_path();
    match decide(read_optional(&ext)?.as_deref()) {
        WriteDecision::Foreign => {
            anyhow::bail!(
                "{} exists and is not parsec-managed — move it aside, then re-run",
                ext.display()
            );
        }
        WriteDecision::Current => {
            println!("pi extension already current at {}", ext.display());
        }
        WriteDecision::Write => {
            write_via_tmp(&ext, EXTENSION_TS)?;
            println!("pi extension written to {}", ext.display());
        }
    }

    // Give the extension a stable binary path to probe, exactly as the
    // opencode shim gets one.
    if let Err(e) = crate::setup_opencode::refresh_bin_alias() {
        println!(
            "could not refresh the {} alias ({e}) — the extension will look for \
             `parsec` on PATH instead",
            crate::setup_opencode::bin_alias_path().display()
        );
    }

    warm_proxy(port);

    println!("done. Restart pi to activate curation. Undo anytime: parsec disable pi");
    Ok(())
}

/// `parsec disable pi` — remove exactly (and only) what setup added.
pub fn disable() -> anyhow::Result<()> {
    let models = models_path();
    match read_optional(&models)? {
        None => println!(
            "no pi models.json at {} — nothing to undo",
            models.display()
        ),
        Some(cur) => match remove_from_models(&cur) {
            Err(msg) => println!("{} {msg} — left alone", models.display()),
            Ok(None) => println!(
                "{} carries no parsec routing — left alone",
                models.display()
            ),
            Ok(Some(next)) => {
                write_via_tmp(&models, &next)?;
                println!("removed the parsec route from {}", models.display());
            }
        },
    }

    let ext = extension_path();
    match std::fs::read_to_string(&ext) {
        Ok(cur) if cur.contains(SENTINEL) => {
            std::fs::remove_file(&ext)?;
            println!(
                "removed {} — restart pi to route directly again",
                ext.display()
            );
        }
        Ok(_) => println!("{} is not parsec-managed — left alone", ext.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no pi extension at {} — nothing to undo", ext.display());
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Uninstall hook: same removal as [`disable`], quiet when absent — called
/// from `parsec uninstall` so a full cleanup never leaves pi pointing at a
/// proxy that no longer exists.
pub fn remove_if_managed() {
    let models = models_path();
    if let Ok(cur) = std::fs::read_to_string(&models) {
        if let Ok(Some(next)) = remove_from_models(&cur) {
            match write_via_tmp(&models, &next) {
                Ok(()) => println!("removed the parsec route from {}", models.display()),
                Err(e) => eprintln!(
                    "could not rewrite {}: {e} — remove the parsec baseUrl by hand",
                    models.display()
                ),
            }
        }
    }
    let ext = extension_path();
    if let Ok(cur) = std::fs::read_to_string(&ext) {
        if cur.contains(SENTINEL) {
            match std::fs::remove_file(&ext) {
                Ok(()) => println!("removed pi extension {}", ext.display()),
                Err(e) => eprintln!(
                    "could not remove pi extension {}: {e} — delete by hand",
                    ext.display()
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_MODELS: &str = r#"{
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "models": [
        {
          "id": "llama3.1:8b"
        }
      ]
    },
    "anthropic": {
      "headers": {
        "x-portkey-api-key": "$PORTKEY_API_KEY"
      }
    }
  }
}
"#;

    #[test]
    fn upsert_preserves_user_providers_and_adds_only_our_two_keys() {
        let out = upsert_models(Some(USER_MODELS), 8082).expect("upsert");
        let before: Value = serde_json::from_str(USER_MODELS).unwrap();
        let after: Value = serde_json::from_str(&out).unwrap();

        // Ours, exactly.
        assert_eq!(
            after["providers"]["anthropic"]["baseUrl"],
            Value::String("http://127.0.0.1:8082".to_string())
        );
        assert_eq!(
            after["providers"]["anthropic"]["headers"]["x-parsec-tool"],
            Value::String("pi".to_string())
        );

        // The user's bytes, in JSON terms, untouched.
        assert_eq!(after["providers"]["ollama"], before["providers"]["ollama"]);
        assert_eq!(
            after["providers"]["anthropic"]["headers"]["x-portkey-api-key"],
            before["providers"]["anthropic"]["headers"]["x-portkey-api-key"]
        );
        assert!(out.ends_with("}\n"), "trailing newline");
        assert!(out.contains("\n  \"providers\""), "two-space indent");
    }

    #[test]
    fn upsert_is_idempotent() {
        let once = upsert_models(Some(USER_MODELS), 8082).expect("first");
        let twice = upsert_models(Some(&once), 8082).expect("second");
        assert_eq!(once, twice);
    }

    #[test]
    fn a_foreign_base_url_is_refused_and_a_loopback_one_is_ours() {
        let foreign = r#"{"providers":{"anthropic":{"baseUrl":"https://gw.example.com"}}}"#;
        let err = upsert_models(Some(foreign), 8082).expect_err("must refuse");
        assert!(err.contains("your own gateway"), "{err}");

        // A loopback route (even on another port) is a parsec one: re-point it.
        let ours = r#"{"providers":{"anthropic":{"baseUrl":"http://127.0.0.1:9999"}}}"#;
        let out = upsert_models(Some(ours), 8082).expect("repoint");
        assert!(out.contains("http://127.0.0.1:8082"), "{out}");
    }

    #[test]
    fn unparsable_json_is_refused_rather_than_overwritten() {
        let err = upsert_models(Some("{not json"), 8082).expect_err("must refuse");
        assert!(err.contains("is not valid JSON"), "{err}");
    }

    #[test]
    fn disable_roundtrips_the_users_json_and_drops_an_anthropic_we_created() {
        // A file that had no anthropic provider at all: setup creates the
        // object, disable removes it again.
        let user = r#"{"providers":{"ollama":{"baseUrl":"http://localhost:11434/v1"}}}"#;
        let installed = upsert_models(Some(user), 8082).expect("upsert");
        let removed = remove_from_models(&installed)
            .expect("remove")
            .expect("something of ours was there");
        let before: Value = serde_json::from_str(user).unwrap();
        let after: Value = serde_json::from_str(&removed).unwrap();
        assert_eq!(after, before, "disable must restore the user's document");

        // The user's own anthropic keys survive; only ours go.
        let installed = upsert_models(Some(USER_MODELS), 8082).expect("upsert");
        let removed = remove_from_models(&installed)
            .expect("remove")
            .expect("ours was there");
        let after: Value = serde_json::from_str(&removed).unwrap();
        let before: Value = serde_json::from_str(USER_MODELS).unwrap();
        assert_eq!(after, before);

        // Nothing of ours left → nothing to do.
        assert_eq!(remove_from_models(USER_MODELS).expect("remove"), None);
    }

    #[test]
    fn extension_carries_the_sentinel_the_tool_tag_and_the_command_surface() {
        // The ownership rule is only sound if every shipped extension embeds
        // the sentinel, and attribution only works if it sets the header.
        assert!(EXTENSION_TS.contains(SENTINEL));
        assert!(EXTENSION_TS.contains("x-parsec-tool"));
        assert!(EXTENSION_TS.contains(&format!("\"{TOOL}\"")));

        // One revival contract across every client: the Codex SessionStart
        // hook, the opencode shim and this extension all run the same thing.
        assert!(
            EXTENSION_TS.contains("\"up\", \"--session-start\""),
            "extension does not run `parsec up --session-start`"
        );

        // The hook subcommands it bridges must be the ones hook.rs answers.
        for event in ["PreToolUse", "PostToolUse"] {
            assert!(
                EXTENSION_TS.contains(event),
                "{event} missing from extension"
            );
        }

        // The installer writes the alias at bin_alias_path(); the extension
        // probes BIN_NAME. If those disagree the extension finds no binary.
        assert!(
            EXTENSION_TS.contains("parsec.exe"),
            "extension has no Windows binary name"
        );
        let expected = crate::setup_opencode::bin_alias_path();
        let name = expected
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(
            EXTENSION_TS.contains(&format!("\"{name}\"")),
            "extension does not probe {name}"
        );
    }

    #[test]
    fn decide_writes_fresh_refreshes_ours_refuses_foreign() {
        assert_eq!(decide(None), WriteDecision::Write);
        assert_eq!(decide(Some(EXTENSION_TS)), WriteDecision::Current);
        // An older managed version (sentinel present, bytes differ).
        let old = format!("// {SENTINEL}\nexport default function () {{}}\n");
        assert_eq!(decide(Some(&old)), WriteDecision::Write);
        // A user's own extension: never ours to overwrite.
        assert_eq!(
            decide(Some("export default function (pi) {}")),
            WriteDecision::Foreign
        );
    }
}
