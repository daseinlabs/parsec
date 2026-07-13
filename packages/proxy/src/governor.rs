//! Step-governor signal layer (L2) — port of `meta/governor.py`'s
//! deterministic triggers + `optimizer/doom.py`'s loop math.
//!
//! Everything in this module is a pure function of the internal view / the
//! original body (no IO, no clock): the proxy computes signals per turn,
//! keeps latches in the per-conversation memo (a CACHE — loss only ever
//! makes the governor LESS likely to fire), and injects directives on the
//! moving tail via `splice::append_user_text` +
//! `place_cache_breakpoint(directive_appended=true)`.
//!
//! Default OFF (`DASEIN_GOVERNOR=off`): rule-τ is not yet bench-calibrated
//! on this stack; `advise` computes and records without touching the wire.
//!
//! Documented deviations from the reference (DESIGN_CONTRACT.md Track B):
//! - The step counter is the count of assistant messages in the internal
//!   view (prefix-derived), NOT the reference's `govern()` call counter —
//!   a client retry or replay must not advance latches/cooldowns.
//! - Directives are APPENDED as one trailing user turn (the sanctioned
//!   `append_user_text` mechanism), never tail-mutations of an existing
//!   message — the reference mutated the last message only because its wire
//!   layer dropped appended turns.
//! - Rule fires dedupe per `(eid, fire_step)`; the reference re-fired every
//!   step with no cooldown but its wire layer dropped the turns, so the
//!   dedupe matches the reference's EFFECTIVE on-wire behavior.
//! - No LLM adjudicator: the conservative no-adjudicator fallback semantics
//!   apply (runaway/head kill at the cost floor, bank window → DELIVER once,
//!   horizon once; budget/doomed flags are advisory records only).
//! - The kill floor is token-denominated (`DASEIN_KILL_FLOOR_TOK`, billed
//!   input-side tokens) — the port of AC_KILL_FLOOR_USD; the proxy does not
//!   price tokens. `runaway_factor` = billed input-side cumulative tokens /
//!   the hoods neighbour-cost median (both sides are billed-input-token
//!   denominated, exactly the units `build_hoods.py` baked).
//!
//! The regex/edit-detection helpers are `pub` and free of proxy-server
//! types: Track C's adjudicator consumes them via `use crate::governor::…`.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use dasein_engine::pystr::{char_len, py_json_dumps, py_split_ws, py_strip};

// ── config ──────────────────────────────────────────────────────────────────

/// `DASEIN_GOVERNOR`: off (default — zero behavior change) | advise (compute
/// + record, never touch the wire) | on (advise + inject directives).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GovMode {
    #[default]
    Off,
    Advise,
    On,
}

impl GovMode {
    pub fn from_env_value(v: Option<&str>) -> GovMode {
        match v.map(str::trim) {
            Some("advise") => GovMode::Advise,
            Some("on") => GovMode::On,
            _ => GovMode::Off,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            GovMode::Off => "off",
            GovMode::Advise => "advise",
            GovMode::On => "on",
        }
    }
}

/// Env dials (DESIGN_CONTRACT.md "Env dials"). Defaults mirror the reference
/// serve config where one exists; everything is inert while mode is Off.
#[derive(Debug, Clone)]
pub struct GovernorConfig {
    pub mode: GovMode,
    /// `DASEIN_RULE_TAU` — reference start_proxy.sh calibration for
    /// curator_v4_prod (0.25); our own bench calibration pending, which is
    /// why the governor defaults off.
    pub rule_tau: f64,
    /// `DASEIN_DOOM_THRESH` (reference proxy: 0.5).
    pub doom_thresh: f64,
    /// `DASEIN_DOOM_K` (reference: 3 consecutive).
    pub doom_k: usize,
    /// `DASEIN_RUNAWAY_RATIO` (AC_RUNAWAY_RATIO, the validated knee 3.25).
    pub runaway_ratio: f64,
    /// `DASEIN_DOOMED_RATIO` (AC_DOOMED_RATIO 2.0) — advisory arm only here.
    pub doomed_ratio: f64,
    /// `DASEIN_KILL_FLOOR_TOK` — billed input-side tokens; the token-
    /// denominated port of AC_KILL_FLOOR_USD=2.5 (default 750_000).
    pub kill_floor_tok: i64,
    /// `DASEIN_HORIZON_STEP` — 0 = off (reference serve used 40).
    pub horizon_step: i64,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        GovernorConfig {
            mode: GovMode::Off,
            rule_tau: 0.25,
            doom_thresh: 0.5,
            doom_k: 3,
            runaway_ratio: 3.25,
            doomed_ratio: 2.0,
            kill_floor_tok: 750_000,
            horizon_step: 0,
        }
    }
}

impl GovernorConfig {
    pub fn from_env() -> GovernorConfig {
        fn num<T: std::str::FromStr>(key: &str, default: T) -> T {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        }
        let d = GovernorConfig::default();
        GovernorConfig {
            mode: GovMode::from_env_value(std::env::var("DASEIN_GOVERNOR").ok().as_deref()),
            rule_tau: num("DASEIN_RULE_TAU", d.rule_tau),
            doom_thresh: num("DASEIN_DOOM_THRESH", d.doom_thresh),
            doom_k: num("DASEIN_DOOM_K", d.doom_k),
            runaway_ratio: num("DASEIN_RUNAWAY_RATIO", d.runaway_ratio),
            doomed_ratio: num("DASEIN_DOOMED_RATIO", d.doomed_ratio),
            kill_floor_tok: num("DASEIN_KILL_FLOOR_TOK", d.kill_floor_tok),
            horizon_step: num("DASEIN_HORIZON_STEP", d.horizon_step),
        }
    }

    /// Rule-fire threshold on the 1e-6 grid: `p_q >= tau_q` fires.
    pub fn tau_q(&self) -> i64 {
        (self.rule_tau * 1_000_000.0).round() as i64
    }
}

// ── doom.py loop math (exact ports) ─────────────────────────────────────────

/// doom.py `_jac`: token Jaccard over whitespace-split sets.
pub fn jac(a: &str, b: &str) -> f64 {
    let sa: HashSet<&str> = py_split_ws(a).into_iter().collect();
    let sb: HashSet<&str> = py_split_ws(b).into_iter().collect();
    let inter = sa.intersection(&sb).count();
    let uni = sa.union(&sb).count();
    inter as f64 / std::cmp::max(1, uni) as f64
}

/// doom.py `loop_fracs(cmds, window=6, sim=0.6)`: per step, fraction of the
/// trailing window whose commands near-duplicate an EARLIER command.
pub fn loop_fracs(cmds: &[String], window: usize, sim: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(cmds.len());
    for i in 0..cmds.len() {
        let lo = (i + 1).saturating_sub(window); // max(0, i - window + 1)
        let win = &cmds[lo..=i];
        let mut dup = 0usize;
        for (j, c) in win.iter().enumerate() {
            let gi = lo + j;
            if (0..gi).any(|k| jac(c, &cmds[k]) >= sim) {
                dup += 1;
            }
        }
        out.push(dup as f64 / std::cmp::max(1, win.len()) as f64);
    }
    out
}

/// The reference call-site defaults (window 6, sim 0.6).
pub fn loop_fracs_default(cmds: &[String]) -> Vec<f64> {
    loop_fracs(cmds, 6, 0.6)
}

/// doom.py `loop_feats(lf, t, lo=0.34)`: the doom head's 4 run-state
/// features — [current loop fraction, trailing-3 mean, slope over 3 steps,
/// trailing-6 fraction >= lo].
pub fn loop_feats(lf: &[f64], t: i64, lo: f64) -> [f64; 4] {
    if lf.is_empty() {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let t = t.clamp(0, lf.len() as i64 - 1) as usize;
    let w3 = &lf[t.saturating_sub(2)..=t];
    let w6 = &lf[t.saturating_sub(5)..=t];
    [
        lf[t],
        w3.iter().sum::<f64>() / w3.len() as f64,
        lf[t] - lf[t.saturating_sub(3)],
        w6.iter().filter(|f| **f >= lo).count() as f64 / w6.len() as f64,
    ]
}

/// The mech gate (governor.py:517): `lf[-1] >= 0.5 && all(last 3 >= 0.34)`.
/// Shared with Track C's Stop-hook adjudicator.
pub fn mech_flag(lf: &[f64]) -> bool {
    if lf.is_empty() {
        return false;
    }
    lf[lf.len() - 1] >= 0.5 && lf[lf.len().saturating_sub(3)..].iter().all(|f| *f >= 0.34)
}

// ── action extraction (governor._action_of over the internal view) ─────────

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

static BASH_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```(?:bash|sh)?\n(.*?)```").expect("BASH_BLOCK regex"));

/// governor.py `_action_of`: the ACTION a message proposes. On the internal
/// view the bash-twin commands are already rendered into `extra.actions` by
/// `internal.rs` — this only extracts them (with the reference's tool_calls /
/// code-block / text fallbacks for prose turns). Any malformed shape falls
/// back to the message text, like the reference's outer `except`.
pub fn action_of(m: &Value) -> String {
    if let Some(acts) = m
        .pointer("/extra/actions")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        let mut parts: Vec<String> = Vec::with_capacity(acts.len());
        for a in acts {
            if !a.is_object() {
                return text_of(m); // reference: a.get on a non-dict raised
            }
            // `a.get("command") or a.get("query") or ""` — a truthy non-str
            // raised in the reference join and fell back to _text.
            let v = ["command", "query"]
                .iter()
                .filter_map(|k| a.get(*k))
                .find(|v| crate::splice::py_truthy(v));
            match v {
                Some(Value::String(s)) => parts.push(s.clone()),
                Some(_) => return text_of(m),
                None => parts.push(String::new()),
            }
        }
        return parts.join(" ; ");
    }
    let mut out: Vec<String> = Vec::new();
    if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
        for tc in tcs {
            let f = tc.get("function").filter(|v| crate::splice::py_truthy(v));
            let args = f
                .and_then(|f| f.get("arguments"))
                .filter(|v| crate::splice::py_truthy(v));
            if let Some(Value::String(s)) = args {
                match serde_json::from_str::<Value>(s) {
                    Ok(d) if d.is_object() => {
                        let v = ["command", "query"]
                            .iter()
                            .filter_map(|k| d.get(*k))
                            .find(|v| crate::splice::py_truthy(v));
                        match v {
                            Some(Value::String(c)) => out.push(c.clone()),
                            Some(_) => return text_of(m), // truthy non-str: join raised
                            None => out.push(s.clone()),
                        }
                    }
                    _ => out.push(s.clone()), // parse error / non-dict: append raw args
                }
            } else if args.is_none() {
                // `fn.get("arguments") or ""` — falsy collapses to "".
                out.push(String::new());
            }
            // truthy non-str arguments: `isinstance(args, str)` false — skipped.
        }
    }
    if !out.is_empty() {
        return out.join(" ; ");
    }
    let txt = text_of(m);
    match BASH_BLOCK.captures(&txt) {
        Some(c) => py_strip(c.get(1).map(|g| g.as_str()).unwrap_or("")).to_string(),
        None => txt,
    }
}

/// The governor's command stream: `_action_of` of every assistant entry.
pub fn commands_of(internal: &[Value]) -> Vec<String> {
    internal
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .map(action_of)
        .collect()
}

/// The doom head's `gf` vector for a message view: `loop_feats(loop_fracs(
/// cmds), len-1)`; None when there are no commands (reference: gf=None →
/// doom not scored — today's behavior).
pub fn gf_of(internal: &[Value]) -> Option<[f64; 4]> {
    let cmds = commands_of(internal);
    if cmds.is_empty() {
        return None;
    }
    Some(loop_feats(
        &loop_fracs_default(&cmds),
        cmds.len() as i64 - 1,
        0.34,
    ))
}

// ── budget features (governor._budget_features over the internal view) ─────

/// CHAR-derived cum_tok + step count. Port of `_budget_features` operating
/// on the internal view (roles + rendered text): per assistant step,
/// chars(content + tool_call args + following observations); double cumsum
/// / 4; entries containing "[SUPERVISOR" / "[CONTEXT RESET" excluded so our
/// own injections never inflate the signal. <6 steps → (0.0, 0).
pub fn budget_features(messages: &[Value]) -> (f64, usize) {
    let mut steps: Vec<i64> = Vec::new();
    for m in messages {
        let is_asst = m.get("role").and_then(Value::as_str) == Some("assistant")
            || m.get("type").and_then(Value::as_str) == Some("AssistantMessage");
        if is_asst {
            let mut n: i64 = match m.get("content") {
                Some(Value::String(s)) => char_len(s) as i64,
                _ => 0,
            };
            if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                for tc in tcs {
                    // a = (tc.get("function") or {}).get("arguments") or ""
                    let a = tc
                        .get("function")
                        .filter(|v| crate::splice::py_truthy(v))
                        .and_then(|f| f.get("arguments"))
                        .filter(|v| crate::splice::py_truthy(v));
                    n += match a {
                        Some(Value::String(s)) => char_len(s) as i64,
                        Some(v) => char_len(&py_json_dumps(v)) as i64,
                        None => 0,
                    };
                }
            }
            steps.push(n);
        } else {
            let t = match m.get("content") {
                Some(Value::String(s)) => s.as_str(),
                _ => "",
            };
            if t.contains("[SUPERVISOR") || t.contains("[CONTEXT RESET") {
                continue;
            }
            let add = char_len(t) as i64;
            if let Some(last) = steps.last_mut() {
                *last += add;
            }
        }
    }
    if steps.len() < 6 {
        return (0.0, 0);
    }
    let n = steps.len();
    let mut run: i64 = 0;
    let mut cum: f64 = 0.0;
    for s in steps {
        run += s;
        cum += run as f64 / 4.0;
    }
    (cum, n)
}

// ── edit-action recognition (exact ports of the governor regexes) ──────────

/// governor.py `_SRC_EXT`.
pub const SRC_EXT: &[&str] = &[
    ".py", ".pyx", ".pyi", ".js", ".jsx", ".ts", ".tsx", ".java", ".go", ".rb", ".rs", ".c", ".cc",
    ".cpp", ".cxx", ".h", ".hpp", ".php", ".cs", ".scala", ".kt", ".swift", ".m", ".mm", ".sh",
    ".pl", ".lua", ".r", ".jl", ".ex", ".exs", ".clj", ".erl", ".hs", ".ml", ".vue", ".svelte",
    ".sql",
];

const PATHTOK: &str = r"/?[\w.\-/]+\.[A-Za-z][A-Za-z0-9]*";

static PATHTOK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(PATHTOK).expect("PATHTOK"));

/// governor.py `_STRONG_EDIT` (re.I).
static STRONG_EDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\bsed\s+-i\b|\btee\b|\bpatch\b|git\s+apply|apply_patch|str_replace|replace_in_file|edit_file|write_file|create_file|\.write\(|open\([^)]*['"]w['"]|open\([^)]*,\s*['"]w"#,
    )
    .expect("STRONG_EDIT")
});

/// governor.py `_REDIR`: redirects count only their target.
static REDIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r">>?\s*({PATHTOK})")).expect("REDIR"));

/// The START of governor.py `_TMP_HEREDOC` (the body + backreferenced
/// delimiter are matched procedurally in [`excise_tmp_heredoc`] — the regex
/// crate has no backreferences, and the two-stage scan reproduces the
/// reference's lazy-body + `^\s*\1\b|\Z` close semantics exactly).
static TMP_HEREDOC_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)>>?\s*['"]?/tmp/[\w.\-/]*['"]?[^\n]*<<-?\s*['"]?(\w+)['"]?\n"#)
        .expect("TMP_HEREDOC_START")
});

/// governor.py `_TMP_TARGET` (re.I, anchored match()).
static TMP_TARGET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)^['"]?(?:/tmp/|tmp/|/dev/null)"#).expect("TMP_TARGET"));

/// governor.py `_TMP_HEREDOC.sub(_drop_body, a)`: excise the BODY of every
/// `> /tmp/... << EOF ... EOF` heredoc (a /tmp scratch script whose
/// source-path literals must not be harvested as live edits — the
/// dj-10973/11292/11555 false-bank root), keeping the redirect/delimiter
/// prefix and the closing-delimiter suffix.
pub fn excise_tmp_heredoc(action: &str) -> String {
    let mut out = String::with_capacity(action.len());
    let mut pos = 0usize;
    while pos <= action.len() {
        let Some(caps) = TMP_HEREDOC_START.captures(&action[pos..]) else {
            break;
        };
        let whole = caps.get(0).expect("group 0");
        let delim = caps.get(1).expect("delimiter").as_str();
        let body_start = pos + whole.end();
        // Everything before the body (untouched text + the start construct).
        out.push_str(&action[pos..body_start]);
        // Reference close: lazy body up to the FIRST `^\s*\1\b` (MULTILINE ^,
        // \s spans newlines) or `\Z`. `\b` consumes nothing.
        let close_re = Regex::new(&format!(r"(?m)^\s*{}\b", regex::escape(delim)))
            .expect("heredoc close regex");
        match close_re.find(&action[body_start..]) {
            Some(c) => {
                // Body (dropped when non-empty; group(0) rides unchanged when
                // empty — identical bytes either way) then the kept close.
                let close_start = body_start + c.start();
                let close_end = body_start + c.end();
                out.push_str(&action[close_start..close_end]);
                pos = close_end;
            }
            None => {
                // \Z branch: the body runs to end-of-string and is dropped.
                pos = action.len();
                break;
            }
        }
    }
    out.push_str(&action[pos.min(action.len())..]);
    out
}

/// governor.py `_is_test_path`: judged on path COMPONENTS, never substring,
/// so a real source file under /testbed/ is NOT mistaken for a test.
pub fn is_test_path(p: &str) -> bool {
    let norm = p.replace('\\', "/").to_lowercase();
    let norm = norm.trim_matches(|c| c == '\'' || c == '"');
    let parts: Vec<&str> = norm.split('/').collect();
    let base = *parts.last().unwrap_or(&"");
    if parts[..parts.len().saturating_sub(1)]
        .iter()
        .any(|seg| matches!(*seg, "test" | "tests" | "testing"))
    {
        return true;
    }
    if base.starts_with("test_")
        || base.starts_with("tests_")
        || base.starts_with("conftest")
        || base.contains("_test.")
        || base.contains("_tests.")
    {
        return true;
    }
    base.contains("reproduce")
        || matches!(
            base.split('.').next().unwrap_or(""),
            "repro" | "repro_bug" | "bug"
        )
}

/// governor.py `_EDIT_TOOLS`.
pub const EDIT_TOOLS: &[&str] = &[
    "edit",
    "multiedit",
    "multi_edit",
    "write",
    "str_replace",
    "str_replace_editor",
    "str_replace_based_edit_tool",
    "notebookedit",
    "notebook_edit",
    "create",
    "createfile",
    "create_file",
    "write_file",
    "edit_file",
    "apply_patch",
    "replace_in_file",
    "fsedit",
    "fswrite",
];

fn src_path_ok(low: &str) -> bool {
    SRC_EXT.iter().any(|e| low.ends_with(e))
        && !is_test_path(low)
        && !low.contains("/tmp/")
        && !low.starts_with("tmp/")
}

/// governor.py `_edited_src_files`: non-test source files an edit ACTION
/// targets (shell parser: strong-edit markers harvest every named source
/// path; redirects count only their target; /tmp scratch excluded).
pub fn edited_src_files(action: &str) -> BTreeSet<String> {
    let scan = excise_tmp_heredoc(action);
    let mut paths: BTreeSet<&str> = BTreeSet::new();
    if STRONG_EDIT.is_match(&scan) {
        for m in PATHTOK_RE.find_iter(&scan) {
            paths.insert(m.as_str());
        }
    }
    for c in REDIR.captures_iter(&scan) {
        let t = c.get(1).expect("redir target").as_str();
        if !TMP_TARGET.is_match(py_strip(t)) {
            paths.insert(t);
        }
    }
    let mut out = BTreeSet::new();
    for p in paths {
        let low = p.to_lowercase();
        let low = low.trim_matches(|c| c == '\'' || c == '"');
        if src_path_ok(low) && low != "/dev/null" {
            out.insert(low.to_string());
        }
    }
    out
}

/// Python `str(x)` for the truthy tool/name values the reference stringified.
fn py_str_lite(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// governor.py `_struct_edited_src`: non-test source files an assistant
/// message edited via a STRUCTURED edit-tool call — `extra.actions` entries
/// plus native Anthropic `tool_use` blocks (the CC wire), so it works on
/// BOTH the internal view (splatted actions) and the original body messages.
pub fn struct_edited_src(m: &Value) -> BTreeSet<String> {
    let mut acts: Vec<Value> = m
        .pointer("/extra/actions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(Value::Array(blocks)) = m.get("content") {
        for b in blocks {
            let Some(o) = b.as_object() else { continue };
            let is_tu = o.get("type").and_then(Value::as_str) == Some("tool_use")
                || o.get("_type").and_then(Value::as_str) == Some("ToolUseBlock");
            if is_tu {
                // dict(b.get("input") or {}, tool=b.get("name"), name=...)
                let mut a = o
                    .get("input")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let name = o.get("name").cloned().unwrap_or(Value::Null);
                a.insert("tool".into(), name.clone());
                a.insert("name".into(), name);
                acts.push(Value::Object(a));
            }
        }
    }
    let mut out = BTreeSet::new();
    for a in &acts {
        if !a.is_object() {
            continue;
        }
        let tool_v = ["tool", "name"]
            .iter()
            .filter_map(|k| a.get(*k))
            .find(|v| crate::splice::py_truthy(v));
        let tool = tool_v
            .map(py_str_lite)
            .map(|s| py_strip(&s).to_lowercase())
            .unwrap_or_default();
        if !EDIT_TOOLS.contains(&tool.as_str()) {
            continue;
        }
        let f = ["file_path", "path", "filename", "file"]
            .iter()
            .filter_map(|k| a.get(*k))
            .find(|v| crate::splice::py_truthy(v));
        let Some(Value::String(f)) = f else { continue };
        let low = f.to_lowercase();
        let low = low.trim_matches(|c| c == '\'' || c == '"');
        if src_path_ok(low) {
            out.insert(low.to_string());
        }
    }
    out
}

/// governor.py `_work_product_src` split across our two views: shell edits
/// from the INTERNAL view's action stream (+ its splatted structured
/// actions), structured edits from the ORIGINAL body's native tool_use
/// blocks. Count of distinct non-test source files the agent itself edited.
pub fn n_src(body: &Value, internal: &[Value]) -> usize {
    let mut files: BTreeSet<String> = BTreeSet::new();
    for m in internal {
        if m.get("role").and_then(Value::as_str) == Some("assistant") {
            files.extend(edited_src_files(&action_of(m)));
            files.extend(struct_edited_src(m));
        }
    }
    if let Some(msgs) = body.get("messages").and_then(Value::as_array) {
        for m in msgs {
            if m.get("role").and_then(Value::as_str) == Some("assistant") {
                files.extend(struct_edited_src(m));
            }
        }
    }
    files.len()
}

// ── per-conversation memo (ConvState cache) ─────────────────────────────────

/// Doom-history cap: latches/accumulators only — losing the memo makes the
/// governor LESS likely to fire, never corrupts served bytes.
pub const DOOM_MEMO_CAP: usize = 16;

/// Governor state cached per conversation (DESIGN_CONTRACT Track B item 3).
#[derive(Debug, Clone, Default)]
pub struct GovMemo {
    /// Rule fires already delivered: (eid, fire_step).
    pub fired: HashSet<(String, i64)>,
    pub coach_fired: bool,
    pub bank_fired: bool,
    pub kill_latched: bool,
    pub horizon_fired: bool,
    /// Latest doom scores (sigmoid grid /1e6), newest last, cap 16.
    pub dooms: VecDeque<f64>,
    /// Billed input-side tokens accumulated from usage rows (post-response).
    pub billed_in_cum: i64,
    /// None = not fetched yet (retry); Some(None) = fetched, hoods inert
    /// (null median); Some(Some(m)) = the neighbour-cost median.
    pub nbr_cost_median: Option<Option<f64>>,
}

impl GovMemo {
    pub fn note_doom(&mut self, d: f64) {
        self.dooms.push_back(d);
        while self.dooms.len() > DOOM_MEMO_CAP {
            self.dooms.pop_front();
        }
    }
}

// ── trigger evaluation ──────────────────────────────────────────────────────

/// Fitted trigger constants (governor.py:539-540, scripts/refit_trigger.py /
/// banktree2.py — verbatim).
pub const COACH_CUM_TOK: f64 = 230_168.0;
pub const COACH_NSTEPS: usize = 48;
pub const BANK_CUM_TOK_FLOOR: f64 = 140_701.0;
pub const BANK_CUM_TOK_CEIL: f64 = 1_835_088.0;

/// The per-turn signal set — a pure function of (internal view, original
/// body, memo, config).
#[derive(Debug, Clone, Default)]
pub struct GovSignals {
    /// Deterministic step counter: assistant messages in the internal view.
    pub cur_step: i64,
    /// loop_fracs tail (None when < 6 commands — the reference gate).
    pub lf_last: Option<f64>,
    /// loop_feats over the full command stream (zeros when no commands).
    pub gf: [f64; 4],
    pub cum_tok: f64,
    pub nsteps: usize,
    pub n_src: usize,
    pub runaway_factor: f64,
    pub mech_flagged: bool,
    pub head_flagged: bool,
    pub budget_flagged: bool,
    pub bank_flagged: bool,
    pub runaway_flagged: bool,
    pub doomed_flagged: bool,
    pub floor_ok: bool,
}

/// Compute every signal for one request. `internal` is the (curated)
/// internal view the model will see — the reference governed the curated
/// list; `body` is the original wire body (native tool_use blocks for the
/// struct-edit scan).
pub fn compute_signals(
    cfg: &GovernorConfig,
    internal: &[Value],
    body: &Value,
    memo: &GovMemo,
) -> GovSignals {
    let cmds = commands_of(internal);
    let cur_step = cmds.len() as i64;
    let lf_all = loop_fracs_default(&cmds);
    let gf = if cmds.is_empty() {
        [0.0, 0.0, 0.0, 0.0]
    } else {
        loop_feats(&lf_all, cur_step - 1, 0.34)
    };
    // The reference computed lf only at >= 6 commands (governor.py:516).
    let lf = if cmds.len() >= 6 { lf_all } else { Vec::new() };
    let mech_flagged = mech_flag(&lf);
    let head_flagged = memo.dooms.len() >= cfg.doom_k
        && memo
            .dooms
            .iter()
            .rev()
            .take(cfg.doom_k)
            .all(|d| *d >= cfg.doom_thresh);
    let (cum_tok, _wsteps) = budget_features(internal);
    // Deviation (documented): nsteps is the prefix-derived step counter, not
    // the reference's govern() call count.
    let nsteps = cur_step.max(0) as usize;
    let n_src = n_src(body, internal);
    let runaway_factor = match memo.nbr_cost_median {
        Some(Some(med)) if med > 0.0 => memo.billed_in_cum as f64 / med,
        _ => 0.0,
    };
    GovSignals {
        cur_step,
        lf_last: lf.last().copied(),
        gf,
        cum_tok,
        nsteps,
        n_src,
        runaway_factor,
        mech_flagged,
        head_flagged,
        budget_flagged: cum_tok >= COACH_CUM_TOK && nsteps > COACH_NSTEPS,
        bank_flagged: n_src >= 1 && cum_tok > BANK_CUM_TOK_FLOOR && cum_tok <= BANK_CUM_TOK_CEIL,
        runaway_flagged: runaway_factor > cfg.runaway_ratio,
        doomed_flagged: runaway_factor > cfg.doomed_ratio && mech_flagged,
        floor_ok: memo.billed_in_cum >= cfg.kill_floor_tok,
    }
}

// ── directives (reference texts VERBATIM) ───────────────────────────────────

/// governor.py:690-693 — the single-step KILL directive (kt4: never give a
/// killed run a two-step instruction).
pub const KILL_DIRECTIVE: &str = "MANDATORY: this run is over. Your next action MUST be the \
completion/submit command from your instructions — nothing else. Do NOT run git diff, tests, \
or any other command first. Any work you have is already in place; submit it NOW.";

/// governor.py:703-711 — DELIVER-NOW.
pub const DELIVER_DIRECTIVE: &str = "MANDATORY — DELIVER YOUR WORK NOW. You have a concrete \
working change and your recent actions are re-checking things you have already confirmed; they \
produce no new information. An undelivered fix has ZERO value to anyone, no matter how \
polished. You MUST NOT run further verification, exploration, or cleanup. Your IMMEDIATE next \
action MUST be to finalize and submit your current change using the required completion \
command. Do it now.";

/// governor.py:723-730 — the budget-horizon backstop.
pub const HORIZON_DIRECTIVE: &str = "[HARNESS DIRECTIVE — MANDATORY] The budget for this run is \
nearly exhausted. Delivering your best current change is now the ONLY acceptable use of the \
remaining budget. If you have a working change, finalize and submit it IMMEDIATELY. If not, \
make the single most promising minimal fix and submit it. You MUST NOT start new exploration, \
broad test sweeps, or cleanup. An undelivered fix is worth nothing.";

/// What the governor decided this turn. `directives` are joined with "\n\n"
/// into ONE appended user turn by the caller (mode=on only).
#[derive(Debug, Clone, Default)]
pub struct GovPlan {
    /// Kill latch active (directive appended every turn while latched).
    pub kill: bool,
    /// The latch flipped THIS turn.
    pub kill_new: bool,
    pub deliver: bool,
    pub horizon: bool,
    pub directives: Vec<String>,
}

/// Conservative no-adjudicator fallback (mode=on semantics; advise runs the
/// same evaluation as a dry-run so the record matches what `on` would do):
/// - runaway_flagged && floor_ok → KILL latch (the 3.25× terminal);
/// - head_flagged && floor_ok → KILL latch;
/// - KILL latched → the kill directive EVERY turn, nothing else (the
///   reference returned immediately on a killed run);
/// - bank window && mech_flagged && !bank_fired → DELIVER once;
/// - horizon_step>0 && nsteps>=horizon → HORIZON once;
/// - budget / doomed flags → advisory record only (they exist to arm an
///   adjudicator we don't ship).
pub fn evaluate_triggers(cfg: &GovernorConfig, sig: &GovSignals, memo: &mut GovMemo) -> GovPlan {
    let mut plan = GovPlan::default();
    if !memo.kill_latched && sig.floor_ok && (sig.runaway_flagged || sig.head_flagged) {
        memo.kill_latched = true;
        plan.kill_new = true;
    }
    if memo.kill_latched {
        plan.kill = true;
        plan.directives.push(KILL_DIRECTIVE.to_string());
        return plan;
    }
    if sig.budget_flagged && !memo.coach_fired {
        memo.coach_fired = true; // advisory-only COACH record (no adjudicator)
    }
    if sig.bank_flagged && sig.mech_flagged && !memo.bank_fired {
        memo.bank_fired = true;
        plan.deliver = true;
        plan.directives.push(DELIVER_DIRECTIVE.to_string());
    }
    if cfg.horizon_step > 0 && sig.nsteps as i64 >= cfg.horizon_step && !memo.horizon_fired {
        memo.horizon_fired = true;
        plan.horizon = true;
        plan.directives.push(HORIZON_DIRECTIVE.to_string());
    }
    plan
}

// ── rule fires ──────────────────────────────────────────────────────────────

/// One scored roster entry from `/v1/score/rules` (wire contract: `rules`
/// array of {eid, text, p_q, fire_step}).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RuleScore {
    pub eid: String,
    #[serde(default)]
    pub text: String,
    pub p_q: i64,
    pub fire_step: i64,
}

/// Fire = `p_q >= tau_q` AND `(eid, fire_step)` not already fired (recorded
/// into the memo). The reference re-fired every step with no cooldown but
/// its wire layer dropped the turns; the dedupe matches its EFFECTIVE
/// behavior.
pub fn rule_fires(rules: &[RuleScore], tau_q: i64, memo: &mut GovMemo) -> Vec<RuleScore> {
    let mut fired = Vec::new();
    for r in rules {
        if r.p_q >= tau_q && !memo.fired.contains(&(r.eid.clone(), r.fire_step)) {
            memo.fired.insert((r.eid.clone(), r.fire_step));
            fired.push(r.clone());
        }
    }
    fired
}

/// governor.py:798-805 — the SUPERVISOR rule turn, reference format
/// verbatim: "[SUPERVISOR] " + "\n\n"-joined MANDATORY RULE blocks.
pub fn rules_directive(fired: &[RuleScore]) -> String {
    let block = fired
        .iter()
        .map(|r| {
            format!(
                "MANDATORY RULE (learned from previous failed runs of this kind of task — \
                 you MUST comply, starting with your NEXT action): {}",
                r.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("[SUPERVISOR] {block}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmds(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn jac_and_loop_fracs_basics() {
        assert_eq!(jac("a b c", "a b c"), 1.0);
        assert_eq!(jac("", ""), 0.0); // empty sets: 0 / max(1,0)
        let lf = loop_fracs_default(&cmds(&["ls", "ls", "ls", "ls", "ls", "ls", "ls"]));
        assert_eq!(lf[0], 0.0); // first command has no earlier twin
        assert_eq!(*lf.last().unwrap(), 1.0);
        assert!(mech_flag(&lf));
        assert!(!mech_flag(&[]));
    }

    #[test]
    fn loop_feats_shapes() {
        assert_eq!(loop_feats(&[], 0, 0.34), [0.0, 0.0, 0.0, 0.0]);
        let lf = [0.0, 0.2, 0.5, 0.6];
        let f = loop_feats(&lf, 3, 0.34);
        assert_eq!(f[0], 0.6);
        assert!((f[1] - (0.2 + 0.5 + 0.6) / 3.0).abs() < 1e-12);
        assert_eq!(f[2], 0.6 - 0.0);
        // t clamped into range
        assert_eq!(loop_feats(&lf, 99, 0.34)[0], 0.6);
        assert_eq!(loop_feats(&lf, -5, 0.34)[0], 0.0);
    }

    #[test]
    fn heredoc_body_excised_but_prefix_and_close_kept() {
        let a = "cat > /tmp/fix.py << 'EOF'\nopen('/testbed/pkg/mod.py','w').write('x')\nEOF\necho done";
        let scanned = excise_tmp_heredoc(a);
        assert!(!scanned.contains("mod.py"), "body not excised: {scanned}");
        assert!(scanned.contains("cat > /tmp/fix.py << 'EOF'\n"));
        assert!(scanned.contains("EOF\necho done"));
        // ... and the harvester banks nothing from the scratch script.
        assert!(edited_src_files(a).is_empty());
    }

    #[test]
    fn edited_src_files_docstring_cases() {
        // A RUN with a redirect to a log is not an edit of x.py.
        assert!(edited_src_files("python x.py > log").is_empty());
        // sed -i on a /testbed source file IS an edit (path components, not
        // substring — the SWE-bench blindness bug).
        let s = edited_src_files("sed -i 's/a/b/' /testbed/django/db/models/query.py");
        assert!(s.contains("/testbed/django/db/models/query.py"));
        // tests are excluded even with a strong marker.
        assert!(edited_src_files("sed -i 's/a/b/' tests/test_app.py").is_empty());
        // redirect target counts without a strong marker.
        let s = edited_src_files("echo 'x' >> src/lib.rs");
        assert!(s.contains("src/lib.rs"));
        // /dev/null and /tmp targets never count.
        assert!(edited_src_files("echo hi > /dev/null").is_empty());
        assert!(edited_src_files("cat a.py > /tmp/scratch.py").is_empty());
    }

    #[test]
    fn is_test_path_component_rules() {
        assert!(!is_test_path("/testbed/django/db/models/query.py"));
        assert!(is_test_path("tests/test_x.py"));
        assert!(is_test_path("a/test/b.c"));
        assert!(is_test_path("conftest.py"));
        assert!(is_test_path("repro.py"));
        assert!(is_test_path("reproduce_issue.py"));
        assert!(!is_test_path("contest.py"));
        assert!(is_test_path("src\\tests\\a.py"));
    }

    #[test]
    fn struct_edit_reads_both_action_shapes() {
        let internal_shape = json!({"role": "assistant", "content": "",
            "extra": {"actions": [{"tool": "Edit", "name": "Edit",
                                   "file_path": "/src/x.py", "command": "edit x.py"}]}});
        assert!(struct_edited_src(&internal_shape).contains("/src/x.py"));
        let wire_shape = json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "Write",
             "input": {"file_path": "src/y.go", "content": "..."}}]});
        assert!(struct_edited_src(&wire_shape).contains("src/y.go"));
        // Non-edit tools and non-source files never count.
        let bash = json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t2", "name": "Bash",
             "input": {"file_path": "/src/z.py"}}]});
        assert!(struct_edited_src(&bash).is_empty());
        let readme = json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t3", "name": "Edit",
             "input": {"file_path": "README.md"}}]});
        assert!(struct_edited_src(&readme).is_empty());
    }

    #[test]
    fn budget_features_excludes_supervisor_turns() {
        let mut msgs: Vec<Value> = Vec::new();
        for _ in 0..6 {
            msgs.push(json!({"role": "assistant", "content": "x".repeat(100)}));
            msgs.push(json!({"role": "user", "content": "y".repeat(100)}));
        }
        let (cum, n) = budget_features(&msgs);
        assert_eq!(n, 6);
        // double cumsum of six 200-char steps: sum_{i=1..6} (200*i)/4 = 1050
        assert!((cum - 1050.0).abs() < 1e-9);
        // A supervisor observation adds nothing.
        msgs.push(json!({"role": "user", "content": "[SUPERVISOR] do the thing"}));
        let (cum2, _) = budget_features(&msgs);
        assert_eq!(cum, cum2);
        // Below six steps: (0.0, 0).
        assert_eq!(budget_features(&msgs[..4]), (0.0, 0));
    }

    fn sig_default() -> GovSignals {
        GovSignals::default()
    }

    #[test]
    fn kill_latch_needs_floor_and_persists() {
        let cfg = GovernorConfig::default();
        let mut memo = GovMemo::default();
        let mut sig = sig_default();
        sig.head_flagged = true;
        sig.floor_ok = false;
        let p = evaluate_triggers(&cfg, &sig, &mut memo);
        assert!(!p.kill && !memo.kill_latched, "kill below the floor");
        sig.floor_ok = true;
        let p = evaluate_triggers(&cfg, &sig, &mut memo);
        assert!(p.kill && p.kill_new && memo.kill_latched);
        assert_eq!(p.directives, vec![KILL_DIRECTIVE.to_string()]);
        // Latched: fires every turn, no re-latch, still ONLY the kill text.
        let mut calm = sig_default();
        calm.floor_ok = true;
        let p = evaluate_triggers(&cfg, &calm, &mut memo);
        assert!(p.kill && !p.kill_new);
        assert_eq!(p.directives.len(), 1);
    }

    #[test]
    fn runaway_kill_and_advisory_arms() {
        let cfg = GovernorConfig::default();
        let mut memo = GovMemo::default();
        let mut sig = sig_default();
        sig.runaway_flagged = true;
        sig.floor_ok = true;
        assert!(evaluate_triggers(&cfg, &sig, &mut memo).kill);
        // budget flag alone: advisory only (coach latch, no directive).
        let mut memo = GovMemo::default();
        let mut sig = sig_default();
        sig.budget_flagged = true;
        let p = evaluate_triggers(&cfg, &sig, &mut memo);
        assert!(p.directives.is_empty() && memo.coach_fired);
        // doomed flag alone: advisory only.
        let mut sig = sig_default();
        sig.doomed_flagged = true;
        sig.floor_ok = true;
        assert!(evaluate_triggers(&cfg, &sig, &mut memo)
            .directives
            .is_empty());
    }

    #[test]
    fn deliver_once_horizon_once() {
        let cfg = GovernorConfig {
            horizon_step: 40,
            ..GovernorConfig::default()
        };
        let mut memo = GovMemo::default();
        let mut sig = sig_default();
        sig.bank_flagged = true;
        sig.mech_flagged = true;
        sig.nsteps = 41;
        let p = evaluate_triggers(&cfg, &sig, &mut memo);
        assert!(p.deliver && p.horizon);
        assert_eq!(
            p.directives,
            vec![DELIVER_DIRECTIVE.to_string(), HORIZON_DIRECTIVE.to_string()]
        );
        // Both latched: nothing on the next identical turn.
        let p = evaluate_triggers(&cfg, &sig, &mut memo);
        assert!(p.directives.is_empty());
        // bank window WITHOUT the mech stall never delivers.
        let mut memo = GovMemo::default();
        let mut sig = sig_default();
        sig.bank_flagged = true;
        let p = evaluate_triggers(&cfg, &sig, &mut memo);
        assert!(!p.deliver);
    }

    #[test]
    fn rule_fire_threshold_and_dedupe() {
        let cfg = GovernorConfig::default();
        assert_eq!(cfg.tau_q(), 250_000);
        let mut memo = GovMemo::default();
        let rules = vec![
            RuleScore {
                eid: "rule1".into(),
                text: "run the tests".into(),
                p_q: 900_000,
                fire_step: 3,
            },
            RuleScore {
                eid: "rule4".into(),
                text: "low".into(),
                p_q: 100_000,
                fire_step: 3,
            },
        ];
        let fired = rule_fires(&rules, cfg.tau_q(), &mut memo);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].eid, "rule1");
        // Same (eid, fire_step): deduped. New fire_step: fires again.
        assert!(rule_fires(&rules, cfg.tau_q(), &mut memo).is_empty());
        let again = vec![RuleScore {
            eid: "rule1".into(),
            text: "run the tests".into(),
            p_q: 900_000,
            fire_step: 4,
        }];
        assert_eq!(rule_fires(&again, cfg.tau_q(), &mut memo).len(), 1);
        let d = rules_directive(&fired);
        assert!(d.starts_with("[SUPERVISOR] MANDATORY RULE (learned from previous failed runs"));
        assert!(d.ends_with("run the tests"));
    }

    #[test]
    fn signals_runaway_and_doom_memo() {
        let cfg = GovernorConfig::default();
        let mut memo = GovMemo {
            billed_in_cum: 1000,
            nbr_cost_median: Some(Some(100.0)),
            ..GovMemo::default()
        };
        let sig = compute_signals(&cfg, &[], &json!({}), &memo);
        assert!((sig.runaway_factor - 10.0).abs() < 1e-12);
        assert!(sig.runaway_flagged);
        assert!(!sig.floor_ok);
        // Inert median: no opinion.
        memo.nbr_cost_median = Some(None);
        let sig = compute_signals(&cfg, &[], &json!({}), &memo);
        assert_eq!(sig.runaway_factor, 0.0);
        // Doom cap.
        for i in 0..20 {
            memo.note_doom(i as f64);
        }
        assert_eq!(memo.dooms.len(), DOOM_MEMO_CAP);
        assert_eq!(*memo.dooms.back().unwrap(), 19.0);
    }

    #[test]
    fn config_env_defaults() {
        let cfg = GovernorConfig::default();
        assert_eq!(cfg.mode, GovMode::Off);
        assert_eq!(cfg.rule_tau, 0.25);
        assert_eq!(cfg.doom_thresh, 0.5);
        assert_eq!(cfg.doom_k, 3);
        assert_eq!(cfg.runaway_ratio, 3.25);
        assert_eq!(cfg.doomed_ratio, 2.0);
        assert_eq!(cfg.kill_floor_tok, 750_000);
        assert_eq!(cfg.horizon_step, 0);
        assert_eq!(GovMode::from_env_value(None), GovMode::Off);
        assert_eq!(GovMode::from_env_value(Some("advise")), GovMode::Advise);
        assert_eq!(GovMode::from_env_value(Some("on")), GovMode::On);
        assert_eq!(GovMode::from_env_value(Some("ON")), GovMode::Off); // exact strings only
    }

    #[test]
    fn gf_shape_and_omission() {
        assert!(gf_of(&[]).is_none());
        let internal = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "",
                   "extra": {"actions": [{"command": "ls -la"}]}}),
        ];
        let gf = gf_of(&internal).unwrap();
        assert_eq!(gf, [0.0, 0.0, 0.0, 0.0]); // single novel command: no loop
    }
}
