//! `parsec trim` — det+dir compaction staging (DASEIN_TRIM_HANDOFF.md).
//!
//! Compute flow: read the session transcript (full fidelity, uncapped) →
//! `engine::needed::det_trim` (the deterministic needed-set keep) → render
//! (the measured det arm's plain join) → stage a pending payload under
//! `~/.parsec/trim/<project-key>.json`. The agent then generates the
//! STANDING DIRECTIVES block itself (the skill's job — no LLM call in this
//! binary, ever) and runs `parsec trim --finalize` with the directives on
//! stdin. On the next `/clear` (or fresh startup) the SessionStart hook
//! injects the composed payload as additionalContext, one-shot, and deletes
//! the file (see `consume_pending`).
//!
//! A never-finalized `"det"` payload is still injectable — det-only is the
//! spec's deterministic fallback (fail-open).
//!
//! Token numbers here are chars/4 ESTIMATES (the handoff §4 sanctions the
//! approximation) and are always labeled as estimates — they never enter the
//! count_tokens-backed savings ledger (measurement honesty).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use parsec_engine::needed::{det_trim_with, NeedCfg};
use parsec_engine::pystr::char_len;

use crate::setup::{home_dir, parsec_home};

pub const TRIM_VERSION: u32 = 1;
/// Pending payloads expire after this long (the user is told to /clear
/// immediately; a stale trim injected into unrelated work is worse than none).
pub const DEFAULT_TTL_SECS: u64 = 1800;

#[derive(Debug, Serialize, Deserialize)]
pub struct TrimPayload {
    pub version: u32,
    /// cwd at trim time — consume_pending verifies it still matches.
    pub project_dir: String,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Unix seconds, hook-lifecycle metadata only — NEVER a label input.
    pub created_at: u64,
    /// "det" (computed) | "ready" (directives attached).
    pub status: String,
    pub body: String,
    #[serde(default)]
    pub directives: Option<String>,
    /// chars/4 estimates: total = all session chunks, body = the trim.
    pub tokens_total_est: i64,
    pub tokens_body_est: i64,
    /// Aggressiveness level 1..=5 the keep-set was computed with. 3 =
    /// `NeedCfg::det()`, the measured/parity-locked configuration (also the
    /// serde default, so pre-level payloads read back correctly).
    #[serde(default = "default_level")]
    pub level: u8,
}

fn default_level() -> u8 {
    3
}

/// Resolve the trim level: explicit flag > PARSEC_TRIM_LEVEL env > 3 (det).
/// Loud on bad input — this is a user-set CLI knob, not a serving path.
fn resolve_level(flag: Option<u8>) -> anyhow::Result<u8> {
    let level = match flag {
        Some(l) => l,
        None => match std::env::var("PARSEC_TRIM_LEVEL") {
            Ok(v) => v
                .trim()
                .parse::<u8>()
                .map_err(|_| anyhow::anyhow!("PARSEC_TRIM_LEVEL={v:?} is not a number (1-5)"))?,
            Err(_) => return Ok(3),
        },
    };
    anyhow::ensure!(
        (1..=5).contains(&level),
        "trim level must be between 1 (low trimming) and 5 (very high), got {level}"
    );
    Ok(level)
}

pub struct TrimArgs {
    pub transcript: Option<PathBuf>,
    pub session_id: Option<String>,
    pub out: Option<PathBuf>,
    pub patch_file: Option<PathBuf>,
    pub json: bool,
    pub finalize: bool,
    /// Aggressiveness 1..=5 (None = PARSEC_TRIM_LEVEL env, then 3).
    pub level: Option<u8>,
}

/// Claude Code's own project-directory munge: every non-alphanumeric byte
/// becomes '-' (so `/Users/x/code` -> `-Users-x-code`, `C:\Users\x` ->
/// `C--Users-x`). Byte-level, path-separator-agnostic — Windows-safe.
pub fn project_key(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn trim_dir() -> PathBuf {
    parsec_home().join("trim")
}

pub fn pending_path(cwd: &str) -> PathBuf {
    trim_dir().join(format!("{}.json", project_key(cwd)))
}

/// Newest session JSONL for this project under ~/.claude/projects/<key>/.
/// Subagent transcripts live in subdirectories and are not matched.
fn discover_transcript(cwd: &str) -> Option<PathBuf> {
    let dir = home_dir()
        .join(".claude")
        .join("projects")
        .join(project_key(cwd));
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn ttl_secs() -> u64 {
    std::env::var("PARSEC_TRIM_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TTL_SECS)
}

fn save_atomic(path: &Path, payload: &TrimPayload) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("pending path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("trim")
    ));
    std::fs::write(&tmp, serde_json::to_vec(payload)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn fmt_k(tokens: i64) -> String {
    if tokens >= 1000 {
        format!("~{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("~{tokens}")
    }
}

pub fn run(args: TrimArgs) -> anyhow::Result<()> {
    if args.finalize {
        return finalize(&args);
    }
    compute(&args)
}

/// Stats of a successfully staged det trim (chars/4 estimates throughout).
pub struct StagedStats {
    pub kept: usize,
    pub total_chunks: usize,
    pub steps: usize,
    pub tokens_total_est: i64,
    pub tokens_body_est: i64,
    pub level: u8,
    pub path: PathBuf,
}

/// A stage attempt that completed without a hard error.
pub enum StageOutcome {
    Staged(StagedStats),
    /// Nothing worth staging (no transcript / too short / empty keep-set) —
    /// an expected outcome, not an error: CLI exit 2, MCP plain text.
    Nothing(String),
}

/// The compute flow, callable from both the CLI and the MCP server. Hard
/// errors (bad level, I/O, over the PARSEC_TRIM_MAX_EST_TOKENS cap) are
/// anyhow errors; everything expected is a StageOutcome.
pub fn stage(
    transcript: Option<&Path>,
    session_id: Option<&str>,
    out: Option<&Path>,
    patch: &str,
    level_flag: Option<u8>,
) -> anyhow::Result<StageOutcome> {
    let level = resolve_level(level_flag)?;
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let transcript = match transcript
        .map(Path::to_path_buf)
        .or_else(|| discover_transcript(&cwd))
    {
        Some(t) => t,
        None => {
            return Ok(StageOutcome::Nothing(format!(
                "no session transcript found for this project (looked under ~/.claude/projects/{}/) — pass the transcript path explicitly",
                project_key(&cwd)
            )));
        }
    };
    let messages = crate::adjudicator::messages_from_transcript_full(&transcript.to_string_lossy());
    let d = match det_trim_with(&messages, patch, &NeedCfg::level(level)) {
        Ok(d) => d,
        Err(skip) => {
            return Ok(StageOutcome::Nothing(format!(
                "{skip} — nothing worth compacting yet"
            )));
        }
    };
    if d.kept() == 0 {
        // An empty keep-set is a legitimate label (a session with no precise
        // re-reads / re-used content), but staging it would inject a useless
        // empty payload — refuse like the other nothing-to-do paths.
        return Ok(StageOutcome::Nothing(format!(
            "the needed-set is empty (0 of {} chunks re-used later) — nothing to stage",
            d.chunks.len()
        )));
    }
    let body = d.render();
    let body_est = char_len(&body) as i64 / 4;
    if let Ok(cap) = std::env::var("PARSEC_TRIM_MAX_EST_TOKENS") {
        if let Ok(cap) = cap.parse::<i64>() {
            // Refuse, never truncate — a truncated needed-set betrays the method.
            anyhow::ensure!(
                body_est <= cap,
                "trim body is {} est. tokens > PARSEC_TRIM_MAX_EST_TOKENS={cap}; refusing (not truncating)",
                body_est
            );
        }
    }
    let payload = TrimPayload {
        version: TRIM_VERSION,
        project_dir: cwd.clone(),
        session_id: session_id.map(str::to_string),
        created_at: now_secs(),
        status: "det".into(),
        body,
        directives: None,
        tokens_total_est: d.tokens_total,
        tokens_body_est: body_est,
        level,
    };
    let path = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| pending_path(&cwd));
    save_atomic(&path, &payload)?;
    Ok(StageOutcome::Staged(StagedStats {
        kept: d.kept(),
        total_chunks: d.chunks.len(),
        steps: d.steps.len(),
        tokens_total_est: d.tokens_total,
        tokens_body_est: body_est,
        level,
        path,
    }))
}

fn compute(args: &TrimArgs) -> anyhow::Result<()> {
    let patch = match &args.patch_file {
        Some(p) => std::fs::read_to_string(p)?,
        None => String::new(),
    };
    let s = match stage(
        args.transcript.as_deref(),
        args.session_id.as_deref(),
        args.out.as_deref(),
        &patch,
        args.level,
    )? {
        StageOutcome::Staged(s) => s,
        StageOutcome::Nothing(msg) => {
            println!("{msg}");
            std::process::exit(2);
        }
    };
    if args.json {
        println!(
            "{}",
            json!({
                "kept_chunks": s.kept,
                "total_chunks": s.total_chunks,
                "steps": s.steps,
                "tokens_before_est": s.tokens_total_est,
                "tokens_body_est": s.tokens_body_est,
                "pending_path": s.path.to_string_lossy(),
                "status": "det",
                "level": s.level,
            })
        );
    } else {
        println!(
            "kept {}/{} chunks · {} of {} tokens (est., chars/4) · level {}",
            s.kept,
            s.total_chunks,
            fmt_k(s.tokens_body_est),
            fmt_k(s.tokens_total_est),
            s.level,
        );
        println!("staged: {} (status: det)", s.path.to_string_lossy());
        println!("next: pipe STANDING DIRECTIVES into `parsec trim --finalize`, then run /clear");
    }
    Ok(())
}

/// Stats of a finalized (directives-attached) trim.
pub struct FinalizedStats {
    pub tokens_body_est: i64,
    pub tokens_directives_est: i64,
    pub path: PathBuf,
}

/// Attach the STANDING DIRECTIVES to the staged payload and mark it ready.
/// Shared by the CLI (directives from stdin) and the MCP server (argument).
pub fn finalize_directives(directives: &str, out: Option<&Path>) -> anyhow::Result<FinalizedStats> {
    let directives = directives.trim().to_string();
    anyhow::ensure!(
        !directives.is_empty(),
        "no directives given — the staged det-only trim remains usable as-is"
    );
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let path = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| pending_path(&cwd));
    let data = std::fs::read_to_string(&path).map_err(|e| {
        anyhow::anyhow!(
            "no staged trim at {} ({e}) — run `parsec trim` first",
            path.display()
        )
    })?;
    let mut payload: TrimPayload = serde_json::from_str(&data)?;
    anyhow::ensure!(
        payload.version == TRIM_VERSION,
        "staged trim has version {} (expected {TRIM_VERSION}) — re-run `parsec trim`",
        payload.version
    );
    let dir_est = char_len(&directives) as i64 / 4;
    payload.directives = Some(directives);
    payload.status = "ready".into();
    payload.created_at = now_secs();
    save_atomic(&path, &payload)?;
    Ok(FinalizedStats {
        tokens_body_est: payload.tokens_body_est,
        tokens_directives_est: dir_est,
        path,
    })
}

fn finalize(args: &TrimArgs) -> anyhow::Result<()> {
    use std::io::Read as _;
    let mut directives = String::new();
    std::io::stdin().read_to_string(&mut directives)?;
    let f = finalize_directives(&directives, args.out.as_deref())?;
    if args.json {
        println!(
            "{}",
            json!({
                "status": "ready",
                "tokens_body_est": f.tokens_body_est,
                "tokens_directives_est": f.tokens_directives_est,
                "pending_path": f.path.to_string_lossy(),
            })
        );
    } else {
        println!(
            "ready: {} body + {} directives tokens (est.) staged at {}",
            fmt_k(f.tokens_body_est),
            fmt_k(f.tokens_directives_est),
            f.path.to_string_lossy()
        );
        println!(
            "run /clear now — the trim is injected into the next session (expires in {} min)",
            ttl_secs() / 60
        );
    }
    Ok(())
}

/// One-shot pickup for the SessionStart hook. Returns the composed context
/// when a fresh, matching pending trim exists — and DELETES the file before
/// returning it (if the delete fails, nothing is injected: a hook that can't
/// consume must not replay the same context into every future session).
/// Every error path returns None — the hook never fails the session.
pub fn consume_pending(cwd: &str) -> Option<String> {
    let path = pending_path(cwd);
    let data = std::fs::read_to_string(&path).ok()?;
    let payload: TrimPayload = match serde_json::from_str(&data) {
        Ok(p) => p,
        Err(_) => {
            let _ = std::fs::remove_file(&path); // corrupt: drop silently
            return None;
        }
    };
    if payload.version != TRIM_VERSION || payload.project_dir != cwd {
        return None;
    }
    let age = now_secs().saturating_sub(payload.created_at);
    if age > ttl_secs() {
        let _ = std::fs::remove_file(&path); // stale: drop silently
        return None;
    }
    std::fs::remove_file(&path).ok()?; // one-shot: consume BEFORE injecting
    let mut out = format!(
        "The following is a deterministic trim of your previous session in this \
         project, staged by /parsec:trim — the parts of that session that were \
         actually used later ({} tokens est.):\n\n{}",
        fmt_k(payload.tokens_body_est),
        payload.body
    );
    if let Some(directives) = payload
        .directives
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        out.push_str("\n\nSTANDING DIRECTIVES:\n");
        out.push_str(directives);
    }
    Some(out)
}

/// Drop pending trims older than `max_age_secs` (file mtime), best-effort.
pub fn prune_pending(max_age_secs: u64) {
    let Ok(entries) = std::fs::read_dir(trim_dir()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .map(|d| d.as_secs());
        if age.is_some_and(|a| a > max_age_secs) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_munges_every_non_alphanumeric_byte() {
        assert_eq!(project_key("/Users/surya/code"), "-Users-surya-code");
        assert_eq!(
            project_key(r"C:\Users\nicks\Desktop"),
            "C--Users-nicks-Desktop"
        );
        assert_eq!(project_key("/a/b.c_d e"), "-a-b-c-d-e");
        assert_eq!(project_key(""), "");
    }

    #[test]
    fn fmt_k_labels() {
        assert_eq!(fmt_k(58_012), "~58.0k");
        assert_eq!(fmt_k(950), "~950");
    }

    #[test]
    fn resolve_level_flag_path() {
        // env-fallback behavior is covered by the CLI tests (env mutation is
        // racy under the parallel in-process test runner)
        assert_eq!(resolve_level(Some(1)).unwrap(), 1);
        assert_eq!(resolve_level(Some(5)).unwrap(), 5);
        assert!(resolve_level(Some(0)).is_err());
        assert!(resolve_level(Some(6)).is_err());
    }
}
