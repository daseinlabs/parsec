//! Adjudicator SUBMIT (L3) — deterministic Stop-hook twin of
//! `meta/adjudicator_submit.py` + `meta/adjudicator_v3.py` +
//! `service/harness_runners.py`'s transcript parsing.
//!
//! Advisory-first: the Stop hook computes the reference's pre-LLM views
//! (recent actions/observations, has-edit ladder, loop fractions) from the
//! session transcript and logs a verdict row to ~/.parsec/adjudicator.jsonl.
//! Blocking the stop is env-gated OFF by default — the reference CUT
//! block-on-CONTINUE after it overrode correct stops (bench arms/parsec.py).
//!
//! Ports (exact semantics; every cap counts CHARS like the Python slices):
//! - [`messages_from_transcript`] = harness_runners._messages_from_transcript
//!   (max_msgs=80, obs_cap=2000) over the Claude Code Stop-hook transcript
//!   JSONL.
//! - [`build_views`] = adjudicator_v3.build_views (recent_actions 14@160,
//!   recent_observations 4@last-400, history_tail 22 {act@140, think@240},
//!   current_diff via _find_diff capped 3000).
//! - has-edit ladder = adjudicator_submit.adjudicate lines 89-105:
//!   _edit_on_disk (the agent's OWN latest `git diff` result — ground truth,
//!   an empty one overrides everything) > shown diff OR structured source
//!   edit (`governor::struct_edited_src`, the governor's exact do-no-harm
//!   gate), with the reverted-edit short-circuit verbatim.
//! - verdict = the deterministic no-LLM twin (DESIGN_CONTRACT Track C):
//!   SUBMIT iff has_edit AND the mech-stall gate (governor.py:516-517 —
//!   `loop_fracs` over the assistant action stream, computed only at >= 6
//!   commands, fire when lf[-1] >= 0.5 and the last 3 are all >= 0.34).
//!   At Stop time SUBMIT means ALLOW the stop and note the bank.
//!
//! Documented deviations from the reference:
//! - No LLM call anywhere (global contract decision #4); the JSONL row
//!   records everything a future bench-side adjudicator would need.
//! - Parsed actions carry the tool_use `input` fields (file_path, ...) in
//!   addition to the reference's {tool, name, command} triple. The harness
//!   parser dropped them, which made the struct-edit rung unreachable from a
//!   transcript alone — the harness compensated with an on-disk
//!   `git add -A; git diff --cached` probe that STAGES the user's index and
//!   is unacceptable in a plugin hook. Merging `input` reproduces exactly
//!   the dict `governor._struct_edited_src` builds for native tool_use
//!   blocks, so the rung reads the same files the proxy path would.
//! - The live working-tree probe is `git diff HEAD --stat` (side-effect
//!   free, 2s clamp) and is TELEMETRY ONLY (`disk_probe` row field), never a
//!   verdict input — the verdict stays a pure function of the transcript
//!   bytes (+ env dials).

use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Value};

use parsec_engine::pystr::{char_len, char_prefix, py_has_content, py_is_space, py_strip};

use crate::governor;
use crate::splice::py_truthy;

/// harness_runners._messages_from_transcript defaults.
pub const MAX_MSGS: usize = 80;
pub const OBS_CAP: usize = 2000;

/// adjudicator_v3.build_views defaults.
const N_ACTIONS: usize = 14;
const N_OBS: usize = 4;
const N_TAIL: usize = 22;
/// _find_diff slice: `t[idx:idx + 3000]`.
const DIFF_CAP: usize = 3000;

// ── Python string helpers ───────────────────────────────────────────────────

/// `s[-n:]` — a char-count suffix, never a byte slice.
fn char_suffix(s: &str, n: usize) -> &str {
    let skip = char_len(s).saturating_sub(n);
    if skip == 0 {
        return s;
    }
    match s.char_indices().nth(skip) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

/// `t.lstrip().startswith("[SUPERVISOR")` — the supervisor-turn skip shared
/// by _find_diff and build_views' observation scan.
fn is_supervisor(t: &str) -> bool {
    t.trim_start_matches(py_is_space).starts_with("[SUPERVISOR")
}

/// Python `str(x)` for the scalar tool_use input values the reference
/// stringified (file paths / patterns are strings in practice).
fn str_lite(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// adjudicator_v3 `_text`: string content, or the " "-join of the dict
/// parts' `text` values.
fn text_of(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_object)
            .map(|o| o.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

// ── transcript parsing (harness_runners.py:67-134) ──────────────────────────

/// harness_runners._action_from_tool_use: map a Claude Code `tool_use` block
/// to the adjudicator action shape — a readable `command` line for EVERY
/// tool so recent_actions reflects the real work in any domain. Deviation
/// (module docs): the block's `input` fields ride along so
/// `governor::struct_edited_src` can read file_path from the action.
fn action_from_tool_use(b: &Value) -> Value {
    let name_v = b
        .get("name")
        .filter(|v| py_truthy(v))
        .cloned()
        .unwrap_or_else(|| json!("tool"));
    let name = name_v.as_str().unwrap_or("");
    let inp = b
        .get("input")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let get = |k: &str| inp.get(k).filter(|v| py_truthy(v));
    let cmd: Value = match name {
        "Bash" | "bash" => get("command").cloned().unwrap_or_else(|| json!("")),
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let f = get("file_path")
                .or_else(|| get("notebook_path"))
                .map(str_lite)
                .unwrap_or_default();
            json!(py_strip(&format!("{name} {f}")))
        }
        "Grep" | "Glob" => {
            let p = get("pattern").map(str_lite).unwrap_or_default();
            json!(py_strip(&format!("{name} {p}")))
        }
        _ => name_v.clone(),
    };
    let tool = if matches!(name, "Bash" | "bash") {
        json!("bash")
    } else {
        name_v.clone()
    };
    let mut a = inp;
    a.insert("tool".into(), tool);
    a.insert("name".into(), name_v);
    a.insert("command".into(), cmd);
    Value::Object(a)
}

/// harness_runners._messages_from_transcript: convert the Claude Code
/// transcript JSONL (handed to every Stop hook as `transcript_path`) into
/// the flat message shape build_views consumes. RESILIENT: an unreadable
/// file → `[]`, a bad line → skipped, exactly like the reference.
pub fn messages_from_transcript(path: &str, max_msgs: usize, obs_cap: usize) -> Vec<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_transcript(&text, max_msgs, obs_cap),
        Err(_) => Vec::new(),
    }
}

/// The parse half of [`messages_from_transcript`] — a pure function of the
/// transcript bytes (the verdict's determinism seam).
pub fn parse_transcript(text: &str, max_msgs: usize, obs_cap: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for line in parsec_engine::pystr::py_splitlines(text) {
        let Ok(e) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !matches!(
            e.get("type").and_then(Value::as_str),
            Some("user") | Some("assistant")
        ) {
            continue;
        }
        let msg = e
            .get("message")
            .filter(|v| py_truthy(v))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let role = msg.get("role").and_then(Value::as_str);
        let cont = msg.get("content");
        if let Some(Value::String(s)) = cont {
            if role == Some("user") && py_has_content(s) {
                out.push(json!({"role": "user", "content": s}));
            }
            continue;
        }
        let Some(Value::Array(blocks)) = cont else {
            continue;
        };
        if role == Some("assistant") {
            let mut text_parts: Vec<String> = Vec::new();
            let mut actions: Vec<Value> = Vec::new();
            for b in blocks {
                let Some(o) = b.as_object() else { continue };
                match o.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        text_parts.push(o.get("text").and_then(Value::as_str).unwrap_or("").into())
                    }
                    Some("thinking") => text_parts.push(
                        o.get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                    ),
                    Some("tool_use") => actions.push(action_from_tool_use(b)),
                    _ => {}
                }
            }
            let joined = text_parts
                .iter()
                .filter(|t| !t.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            out.push(json!({
                "role": "assistant",
                "content": py_strip(&joined),
                "extra": {"actions": actions},
            }));
        } else {
            // tool results land under user entries.
            for b in blocks {
                let Some(o) = b.as_object() else { continue };
                if o.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let c = match o.get("content") {
                    Some(Value::Array(xs)) => xs
                        .iter()
                        .filter_map(Value::as_object)
                        .map(|x| x.get("text").and_then(Value::as_str).unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join(" "),
                    Some(Value::Null) | None => String::new(),
                    Some(v) => str_lite(v),
                };
                out.push(json!({"role": "tool", "content": char_prefix(&c, obs_cap)}));
            }
        }
    }
    let skip = out.len().saturating_sub(max_msgs);
    out.split_off(skip)
}

// ── views (adjudicator_v3.py:63-92) ─────────────────────────────────────────

/// adjudicator_v3._find_diff: most recent NON-EMPTY unified diff the agent
/// was shown (tool/user text, [SUPERVISOR turns skipped) — the mechanism-
/// agnostic "a submittable edit exists" signal. 3000-char slice from the
/// diff start; "" when no edit is in place yet.
pub fn find_diff(messages: &[Value]) -> String {
    for m in messages.iter().rev() {
        if !matches!(
            m.get("role").and_then(Value::as_str),
            Some("tool") | Some("user")
        ) {
            continue;
        }
        let t = text_of(m);
        if is_supervisor(&t) {
            continue;
        }
        let mut idx = t.find("diff --git");
        if idx.is_none() && t.contains("@@ ") && (t.contains("+++ ") || t.contains("--- ")) {
            idx = [t.find("@@ "), t.find("--- ")].into_iter().flatten().min();
        }
        if let Some(i) = idx {
            return char_prefix(&t[i..], DIFF_CAP).to_string();
        }
    }
    String::new()
}

/// The reference's pre-LLM views (adjudicator_v3.build_views defaults:
/// n_actions=14, n_obs=4, n_tail=22).
#[derive(Debug, Clone, Default)]
pub struct Views {
    /// Last 14 assistant actions, each capped at 160 chars.
    pub recent_actions: Vec<String>,
    /// Last 4 tool/user texts (supervisor turns skipped), LAST 400 chars.
    pub recent_observations: Vec<String>,
    /// Last 22 assistants: {"act": @140, "think": @240} objects.
    pub history_tail: Vec<Value>,
    /// The shown-diff signal: `bool(current_diff.strip())`.
    pub has_edit: bool,
    pub current_diff: String,
}

/// adjudicator_v3.build_views — exact port over the parsed message shape.
pub fn build_views(messages: &[Value]) -> Views {
    let assistants: Vec<&Value> = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .collect();
    let askip = assistants.len().saturating_sub(N_ACTIONS);
    let recent_actions = assistants[askip..]
        .iter()
        .map(|m| char_prefix(&governor::action_of(m), 160).to_string())
        .collect();
    let mut obs: Vec<String> = messages
        .iter()
        .filter(|m| {
            matches!(
                m.get("role").and_then(Value::as_str),
                Some("tool") | Some("user")
            )
        })
        .map(text_of)
        .filter(|t| !is_supervisor(t))
        .map(|t| char_suffix(&t, 400).to_string())
        .collect();
    let oskip = obs.len().saturating_sub(N_OBS);
    let recent_observations = obs.split_off(oskip);
    let tskip = assistants.len().saturating_sub(N_TAIL);
    let history_tail = assistants[tskip..]
        .iter()
        .map(|m| {
            let think = py_strip(&text_of(m)).replace('\n', " ");
            json!({
                "act": char_prefix(&governor::action_of(m), 140),
                "think": char_prefix(&think, 240),
            })
        })
        .collect();
    let current_diff = find_diff(messages);
    Views {
        recent_actions,
        recent_observations,
        history_tail,
        has_edit: py_has_content(&current_diff),
        current_diff,
    }
}

// ── has-edit ladder (adjudicator_submit.py:27-61, 89-105) ───────────────────

static GIT_DIFF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bgit\s+diff\b").expect("GIT_DIFF"));

/// adjudicator_submit._edit_on_disk: is a real edit ON DISK right now? Read
/// the agent's OWN most-recent `git diff` action and report whether its
/// result (the next <=3 tool/user texts) is a non-empty diff. `None` when
/// the agent never ran `git diff` (caller falls back to shown diff/struct).
pub fn edit_on_disk(messages: &[Value]) -> Option<bool> {
    for i in (0..messages.len()).rev() {
        let m = &messages[i];
        if m.get("role").and_then(Value::as_str) != Some("assistant")
            || !GIT_DIFF.is_match(&governor::action_of(m))
        {
            continue;
        }
        let end = (i + 4).min(messages.len());
        let out = messages[i + 1..end]
            .iter()
            .filter(|mj| {
                matches!(
                    mj.get("role").and_then(Value::as_str),
                    Some("tool") | Some("user")
                )
            })
            .map(text_of)
            .collect::<Vec<_>>()
            .join(" ");
        return Some(
            out.contains("diff --git")
                || (out.contains("@@ ") && (out.contains("+++ ") || out.contains("--- "))),
        );
    }
    None
}

/// adjudicator_submit._struct_src_edit: a real structured SOURCE edit exists
/// in the agent's own action stream — the governor's exact do-no-harm gate
/// (source-only, non-test, non-/tmp) via `governor::struct_edited_src`.
pub fn struct_src_edit(messages: &[Value]) -> bool {
    messages.iter().any(|m| {
        (m.get("role").and_then(Value::as_str) == Some("assistant")
            || m.get("type").and_then(Value::as_str) == Some("AssistantMessage"))
            && !governor::struct_edited_src(m).is_empty()
    })
}

// ── verdict (deterministic SUBMIT/CONTINUE twin) ────────────────────────────

/// One Stop-hook adjudication — a pure function of the parsed transcript.
#[derive(Debug, Clone)]
pub struct Adjudication {
    /// "SUBMIT" (bank the finished fix — ALLOW the stop) or "CONTINUE".
    pub verdict: &'static str,
    pub reason: String,
    pub has_edit: bool,
    /// Which rung of the ladder decided has_edit:
    /// disk_true | disk_false | shown_diff | struct_edit | none.
    pub has_edit_rung: &'static str,
    /// The reverted-edit short-circuit (adjudicator_submit.py:101-105): an
    /// edit WAS seen but the latest git diff reads a clean tree.
    pub short_circuit: bool,
    /// governor.py:516-517 mech gate over the assistant action stream.
    pub mech_stalled: bool,
    /// Tail (last <=6) of loop_fracs; empty below the 6-command gate.
    pub lf_tail: Vec<f64>,
    pub n_msgs: usize,
    pub n_cmds: usize,
    pub views: Views,
}

/// Deterministic port of AdjudicatorSubmit.adjudicate WITHOUT the model
/// call: the has-edit ladder verbatim, then SUBMIT iff has_edit AND the
/// mechanical stall gate ("banking a finished fix"); everything else
/// CONTINUE (fail-open bias — the reference's durability invariant).
pub fn adjudicate(messages: &[Value]) -> Adjudication {
    let views = build_views(messages);
    let disk = edit_on_disk(messages);
    let struct_edit = struct_src_edit(messages);
    let (has_edit, has_edit_rung) = match disk {
        // The agent's latest git diff is non-empty: ground truth of the tree.
        Some(true) => (true, "disk_true"),
        // Empty latest git diff = clean tree RIGHT NOW; overrides a stale
        // structured-edit signal so we never bank a withdrawn edit.
        Some(false) => (false, "disk_false"),
        // Never self-diffed: most-recent shown diff OR a structured edit.
        None if views.has_edit => (true, "shown_diff"),
        None if struct_edit => (true, "struct_edit"),
        None => (false, "none"),
    };
    let short_circuit = !has_edit && (views.has_edit || struct_edit);
    // Mech stall — governor.py:516-517: lf only at >= 6 commands, fire when
    // lf[-1] >= 0.5 and the last 3 are all >= 0.34 (governor::mech_flag).
    let cmds = governor::commands_of(messages);
    let lf = if cmds.len() >= 6 {
        governor::loop_fracs_default(&cmds)
    } else {
        Vec::new()
    };
    let mech_stalled = governor::mech_flag(&lf);
    let lf_tail = lf[lf.len().saturating_sub(6)..].to_vec();
    let (verdict, reason) = if short_circuit {
        // Reference reason verbatim (adjudicator_submit.py:103).
        (
            "CONTINUE",
            "latest git diff is empty (no edit on disk to submit)".to_string(),
        )
    } else if has_edit && mech_stalled {
        (
            "SUBMIT",
            format!("submittable edit on disk ({has_edit_rung}) + mechanical stall: bank the finished fix — allow the stop"),
        )
    } else if !has_edit {
        ("CONTINUE", "no submittable edit on disk".to_string())
    } else {
        (
            "CONTINUE",
            "edit on disk but no mechanical stall — the run was still progressing".to_string(),
        )
    };
    Adjudication {
        verdict,
        reason,
        has_edit,
        has_edit_rung,
        short_circuit,
        mech_stalled,
        lf_tail,
        n_msgs: messages.len(),
        n_cmds: cmds.len(),
        views,
    }
}

// ── live working-tree probe (telemetry only) ────────────────────────────────

/// Side-effect-free probe: `git diff HEAD --stat` in `cwd` with a 2s clamp.
/// TELEMETRY ONLY — recorded as the `disk_probe` row field, NEVER a verdict
/// input (the verdict stays a pure function of the transcript bytes).
/// `Some(true)` = non-empty stat (edits on disk); `Some(false)` = clean
/// tree; `None` = probe unavailable (no git / not a repo / timeout / error).
pub fn disk_probe(cwd: &str) -> Option<bool> {
    use std::io::Read as _;
    let mut cmd = std::process::Command::new("git");
    cmd.args(["diff", "HEAD", "--stat"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        // Same console-window suppression as the worker/detached spawns
        // (supervisor::spawn_worker, setup::spawn_detached): the Stop hook that
        // calls this can run under a no-console parent, and a bare `git` spawn
        // would then pop a visible console window. Output is piped/nulled, so
        // CREATE_NO_WINDOW hides nothing we want.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;
    // Drain stdout on a thread so a huge diff can never deadlock the pipe.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().unwrap_or_default();
                return if status.success() {
                    Some(py_has_content(&out))
                } else {
                    None
                };
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

// ── the advisory JSONL row ──────────────────────────────────────────────────

/// `~/.parsec/adjudicator.jsonl` — one row per stop attempt, local-only.
pub fn log_path() -> PathBuf {
    crate::setup::home_dir()
        .join(".parsec")
        .join("adjudicator.jsonl")
}

/// Build the advisory row. View snippets are already capped by construction
/// (the reference caps: 14@160 / 4@400 / 22@{140,240} / diff 3000) — no
/// other raw transcript text rides along.
pub fn row(
    adj: &Adjudication,
    session_id: &str,
    mode: &str,
    disk_probe: Option<bool>,
    blocked: bool,
) -> Value {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    json!({
        "schema": "adjudicator-stop/v0",
        "ts": ts,
        "session_id": session_id,
        "mode": mode,
        "verdict": adj.verdict,
        "reason": adj.reason,
        "has_edit": adj.has_edit,
        "has_edit_rung": adj.has_edit_rung,
        "short_circuit": adj.short_circuit,
        "mech_stalled": adj.mech_stalled,
        "lf_tail": adj.lf_tail,
        "n_msgs": adj.n_msgs,
        "n_cmds": adj.n_cmds,
        "n_actions": adj.views.recent_actions.len(),
        "n_obs": adj.views.recent_observations.len(),
        "n_tail": adj.views.history_tail.len(),
        "disk_probe": disk_probe,
        "blocked": blocked,
        "views": {
            "recent_actions": adj.views.recent_actions,
            "recent_observations": adj.views.recent_observations,
            "history_tail": adj.views.history_tail,
            "current_diff": adj.views.current_diff,
        },
    })
}

/// Append one row (atomic-enough: single write, O_APPEND).
pub fn append_row(row: &Value) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = log_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{row}")
}
