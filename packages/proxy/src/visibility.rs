//! Curator → hook visibility channel (docs/NOREREAD_HOOK_DEFECT.md defect 3).
//!
//! The no-reread hook's denial premise — "you already have this content in
//! the messages above" — is FALSE for content the curator elided from the
//! served bytes. The proxy is the only party that knows what survived
//! curation, so after each curated serve it exports the freezer's cut
//! registries for the request's Claude Code session to
//! `~/.parsec/sessions/<sid>.elided.json`. The hook consults the export
//! before denying: a re-read that touches dropped-and-never-restored content
//! is recovery, not a habit, and passes.
//!
//! This closes the deny → workaround → insist-restore loop the measured runs
//! showed: elided content re-reads pass the hook, the freezer's insist valve
//! serves them full once (moving the range into `served_ranges`), after
//! which the content really IS above and the hook denies again — correctly.
//!
//! Fail-open at every seam: a missing/corrupt export means "nothing elided"
//! (pre-channel behavior), and the escape grant in `noreread` remains the
//! valve of last resort. File keys are chunk BASENAMES (all the chunker
//! records), so matching is by basename — a collision across directories can
//! only over-allow, never over-deny.
//!
//! One export file PER CONVERSATION (`<sid>.elided.<conv12>.json`): subagents
//! mint their own conv_ids inside a session, and per-conv files make each
//! writer the sole owner of its file — no read-modify-write race between
//! concurrent subagent serves. `load` merges all of a session's conv files.
//!
//! Deliberately NOT exported from the openai/codex path (`openai.rs`): those
//! requests carry no Claude Code session identity (`metadata.user_id`) to
//! key the export by, and no Claude Code hook reads on that path — there is
//! neither a key nor a consumer. Revisit if a codex-side re-read gate lands.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::noreread::{sessions_dir, LineRange};

/// Cut-registry state for one file (basename): what the curator dropped
/// from the served context, and what it later restored in full.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FileVis {
    pub dropped: Vec<(i64, i64)>,
    pub served: Vec<(i64, i64)>,
}

/// Merged view the hook consumes: lowercase basename -> visibility.
pub type Elided = BTreeMap<String, FileVis>;

/// On-disk export: one file per (session, conversation), holding that
/// conversation's FULL current registries — each serve rewrites it whole.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ElidedExport {
    version: u32,
    files: BTreeMap<String, FileVis>,
    /// The freezer's override registry: action texts (Bash command strings
    /// as the model issued them, Read tool calls in their `sed -n` internal
    /// rendering) whose direct result currently has elided chunks. The
    /// override protocol asks the model to repeat exactly such a call once;
    /// the hook's loop-breaker must not count that repeat. Absent in v1
    /// exports (older proxy) — deserializes empty, which only over-counts.
    #[serde(default)]
    cmds: Vec<String>,
    /// Unix seconds, lifecycle metadata only — never a decision input.
    updated_unix: u64,
}

const EXPORT_VERSION: u32 = 1;

fn safe_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `<sid>.elided.` — the session's file-name prefix `load` scans for.
fn session_prefix(session_id: &str) -> String {
    format!("{}.elided.", safe_id(session_id))
}

/// This (session, conversation)'s export file. conv_ids are sha256 hex;
/// 12 chars is collision-safe within one session's handful of conversations.
pub fn conv_path(session_id: &str, conv_id: &str) -> PathBuf {
    let conv = safe_id(conv_id);
    let conv12 = &conv[..conv.len().min(12)];
    sessions_dir().join(format!("{}{}.json", session_prefix(session_id), conv12))
}

/// Proxy side: persist `snapshot` (this conversation's FULL current cut
/// registries, basenames lowercased). Sole-writer file + atomic rename;
/// every error path returns silently (the export is an optimization for the
/// hook, never worth failing a serve over).
pub fn record(
    session_id: &str,
    conv_id: &str,
    snapshot: BTreeMap<String, FileVis>,
    cmds: Vec<String>,
) {
    let path = conv_path(session_id, conv_id);
    if snapshot.is_empty() && cmds.is_empty() {
        // Registry reset (client edit / fresh run): drop the stale export.
        let _ = std::fs::remove_file(&path);
        return;
    }
    let export = ElidedExport {
        version: EXPORT_VERSION,
        files: snapshot,
        cmds,
        updated_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(&export).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Hook side: the merged elided view across all of a session's conversation
/// exports (empty on any error — fail toward the pre-channel behavior).
pub fn load(session_id: &str) -> Elided {
    let prefix = session_prefix(session_id);
    let mut out = Elided::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".json") {
            continue;
        }
        let Some(export) = std::fs::read_to_string(e.path())
            .ok()
            .and_then(|s| serde_json::from_str::<ElidedExport>(&s).ok())
        else {
            continue;
        };
        for (base, vis) in export.files {
            let m = out.entry(base).or_default();
            m.dropped.extend(vis.dropped);
            m.served.extend(vis.served);
        }
    }
    // read_dir order is fs-dependent — sort so the merged view (and any
    // logging of it) is deterministic.
    for vis in out.values_mut() {
        vis.dropped.sort_unstable();
        vis.dropped.dedup();
        vis.served.sort_unstable();
        vis.served.dedup();
    }
    out
}

/// Hook side: the merged override registry across a session's conversation
/// exports — every action text whose direct result is currently elided.
/// Empty on any error (fail toward counting, which at worst denies a
/// third identical run, never the override's single repeat).
pub fn load_cmds(session_id: &str) -> std::collections::BTreeSet<String> {
    let prefix = session_prefix(session_id);
    let mut out = std::collections::BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".json") {
            continue;
        }
        if let Some(export) = std::fs::read_to_string(e.path())
            .ok()
            .and_then(|s| serde_json::from_str::<ElidedExport>(&s).ok())
        {
            out.extend(export.cmds);
        }
    }
    out
}

/// True when reading `rng` of `path` would touch content the curator
/// dropped from the served context and never restored in full — i.e. the
/// "reuse it from the messages above" premise is false. Basename match;
/// a served range must COVER the dropped∩requested intersection to count
/// as restored (partial restores stay elided — bias toward allowing).
pub fn is_elided(elided: &Elided, path: &str, rng: LineRange) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    let Some(vis) = elided.get(&base) else {
        return false;
    };
    let (lo, hi) = rng.bounds();
    for &(dlo, dhi) in &vis.dropped {
        let ilo = lo.max(dlo);
        let ihi = hi.min(dhi);
        if ilo > ihi {
            continue;
        }
        let restored = vis
            .served
            .iter()
            .any(|&(slo, shi)| slo <= ilo && shi >= ihi);
        if !restored {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elision_overlap_semantics() {
        let mut e = Elided::new();
        e.insert(
            "app.py".into(),
            FileVis {
                dropped: vec![(10, 50)],
                served: vec![(30, 60)],
            },
        );
        // requested range inside dropped, NOT covered by served -> elided
        assert!(is_elided(&e, "/repo/src/app.py", LineRange::Lines(12, 20)));
        // intersection fully covered by a served (restored) range -> visible
        assert!(!is_elided(&e, "/repo/src/app.py", LineRange::Lines(35, 45)));
        // no intersection with any dropped range -> visible
        assert!(!is_elided(&e, "/repo/src/app.py", LineRange::Lines(60, 90)));
        // tail read overlaps everything -> elided while any cut is unrestored
        assert!(is_elided(&e, "/repo/src/app.py", LineRange::Tail(5)));
        // different basename -> not tracked
        assert!(!is_elided(
            &e,
            "/repo/src/other.py",
            LineRange::Lines(12, 20)
        ));
        // basename match is case-insensitive on the path side
        assert!(is_elided(&e, "/repo/src/App.py", LineRange::Lines(12, 20)));
    }

    #[test]
    fn record_load_roundtrip_merges_convs() {
        let sid = "visibility-test-3d1c7a";
        let _ = std::fs::remove_file(conv_path(sid, "conv-a"));
        let _ = std::fs::remove_file(conv_path(sid, "conv-b"));
        let mut a = BTreeMap::new();
        a.insert(
            "app.py".to_string(),
            FileVis {
                dropped: vec![(1, 9)],
                served: vec![],
            },
        );
        record(sid, "conv-a", a, Vec::new());
        let mut b = BTreeMap::new();
        b.insert(
            "app.py".to_string(),
            FileVis {
                dropped: vec![(20, 30)],
                served: vec![(20, 30)],
            },
        );
        record(sid, "conv-b", b, Vec::new());
        let merged = load(sid);
        let vis = merged.get("app.py").unwrap();
        assert_eq!(vis.dropped, vec![(1, 9), (20, 30)]);
        assert_eq!(vis.served, vec![(20, 30)]);
        // an empty snapshot removes the conv's stale export file
        record(sid, "conv-a", BTreeMap::new(), Vec::new());
        assert!(!conv_path(sid, "conv-a").exists());
        assert_eq!(load(sid).get("app.py").unwrap().dropped, vec![(20, 30)]);
        let _ = std::fs::remove_file(conv_path(sid, "conv-b"));
    }

    #[test]
    fn missing_or_corrupt_export_is_empty() {
        assert!(load("visibility-test-never-written").is_empty());
        let sid = "visibility-test-corrupt";
        let p = conv_path(sid, "conv-x");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"not json").unwrap();
        assert!(load(sid).is_empty());
        let _ = std::fs::remove_file(p);
    }
}
