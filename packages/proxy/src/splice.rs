//! Cache-safe splicing — port of the fold-back layer of
//! `service/anthropic_shapes.py`: curated TEXT folded back onto the ORIGINAL
//! Anthropic content blocks, in place, by position.
//!
//! The curator preserves message COUNT and ORDER and only rewrites text
//! content, so positional fold-back is exact. tool_use/tool_result/image
//! blocks are never dropped or reordered — the upstream tool pairing must
//! stay intact. Parity: tests/parity_fold.rs replays fixtures generated from
//! the Python reference (parity/gen_fold_fixtures.py).
//!
//! The `served_folds` session dict of the reference is reproduced here only
//! as the FoldMap argument to [`apply_curation`] so parity fixtures can
//! exercise the reference semantics; the deterministic replacement that makes
//! the map recomputable lives in parsec-engine's `freeze` module.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use parsec_engine::pystr::{py_json_dumps_opts, py_strip};

/// anthropic_shapes._system_to_text: Anthropic `system` is a str OR a list of
/// text blocks; flatten to one string.
pub fn system_to_text(system: Option<&Value>) -> String {
    match system {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.as_object())
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| b.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// anthropic_shapes._tool_result_text: text inside a `tool_result` block.
fn tool_result_text(block: &Map<String, Value>) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.as_object())
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .map(|p| p.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// anthropic_shapes._content_text: the TEXT view of an Anthropic content
/// value — `text` blocks plus tool_result inner text, space-joined, empties
/// dropped.
pub fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut out: Vec<String> = Vec::new();
            for b in blocks {
                let Some(o) = b.as_object() else { continue };
                match o.get("type").and_then(Value::as_str) {
                    Some("text") => out.push(
                        o.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                    Some("tool_result") => out.push(tool_result_text(o)),
                    _ => {}
                }
            }
            out.into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => String::new(),
    }
}

/// Pointer text a later sibling tool_result carries after consolidation.
const CONSOLIDATED: &str = "[result consolidated into the first result of this turn above]";
/// Non-empty marker when a trimmed error observation would otherwise be empty
/// (Anthropic 400s on empty is_error tool_results).
const ERROR_OMITTED: &str = "[error output omitted]";

/// anthropic_shapes._set_tool_result_text: rewrite the text INSIDE a
/// tool_result block, preserving tool_use_id and non-text parts.
fn set_tool_result_text(block: &Map<String, Value>, new_text: &str) -> Value {
    let mut nb = block.clone();
    let is_error = block.get("is_error").map(py_truthy).unwrap_or(false);
    let new_text = if is_error && py_strip(new_text).is_empty() {
        ERROR_OMITTED
    } else {
        new_text
    };
    match block.get("content") {
        Some(Value::Array(parts)) => {
            let mut out: Vec<Value> = Vec::new();
            let mut placed = false;
            for p in parts {
                match p.as_object() {
                    Some(o) if o.get("type").and_then(Value::as_str) == Some("text") => {
                        if !placed {
                            let mut np = o.clone();
                            np.insert("text".into(), Value::String(new_text.to_string()));
                            out.push(Value::Object(np));
                            placed = true;
                        }
                    }
                    _ => out.push(p.clone()),
                }
            }
            if !placed {
                out.insert(0, serde_json::json!({"type": "text", "text": new_text}));
            }
            nb.insert("content".into(), Value::Array(out));
        }
        _ => {
            nb.insert("content".into(), Value::String(new_text.to_string()));
        }
    }
    Value::Object(nb)
}

/// anthropic_shapes._rewrite_first_text: put curated text into the FIRST
/// text-bearing block, blank later text blocks, pointer later tool_results.
pub fn rewrite_first_text(content: Option<&Value>, new_text: &str) -> Value {
    match content {
        Some(Value::String(_)) => Value::String(new_text.to_string()),
        Some(Value::Array(blocks)) => {
            let mut out: Vec<Value> = Vec::new();
            let mut placed = false;
            for b in blocks {
                let Some(o) = b.as_object() else {
                    out.push(b.clone());
                    continue;
                };
                match o.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if !placed {
                            let mut nb = o.clone();
                            nb.insert("text".into(), Value::String(new_text.to_string()));
                            out.push(Value::Object(nb));
                            placed = true;
                        }
                        // drop subsequent text blocks
                    }
                    Some("tool_result") => {
                        if !placed {
                            out.push(set_tool_result_text(o, new_text));
                            placed = true;
                        } else {
                            out.push(set_tool_result_text(o, CONSOLIDATED));
                        }
                    }
                    _ => out.push(b.clone()),
                }
            }
            if !placed && !new_text.is_empty() {
                out.insert(0, serde_json::json!({"type": "text", "text": new_text}));
            }
            Value::Array(out)
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

/// anthropic_shapes._strip_cache_control: deep copy with every
/// `cache_control` key removed — a provider annotation, not semantic content.
pub fn strip_cache_control(content: &Value) -> Value {
    match content {
        Value::Array(a) => Value::Array(a.iter().map(strip_cache_control).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .map(|(k, v)| (k.clone(), strip_cache_control(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// anthropic_shapes._orig_fingerprint: sha256 hex of the RAW inbound
/// (pre-curation) blocks — the cross-call identity of a turn as the client
/// sent it. Bytes hashed are Python's json.dumps(sort_keys=True,
/// ensure_ascii=False), reproduced exactly.
pub fn orig_fingerprint(content: &Value) -> String {
    let blob = py_json_dumps_opts(&strip_cache_control(content), true, false);
    let mut h = Sha256::new();
    h.update(blob.as_bytes());
    format!("{:x}", h.finalize())
}

pub(crate) fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The served-fold map: "{index}:{fingerprint}" (or "__system__") -> the
/// exact served bytes for that turn. In the reference this lives in
/// `sess.extra["served_folds"]`; here it is an explicit argument so the
/// caller decides where it comes from — recomputed (deterministic freeze) or
/// carried (legacy session).
pub type FoldMap = Map<String, Value>;

/// anthropic_shapes.apply_curation: build the NATIVE Anthropic body whose
/// text is the curator's curated text folded onto the original blocks, with
/// the wire-level freeze: a turn already in `folds` replays its exact bytes;
/// a new turn is folded once and recorded. `internal_curated` follows
/// to_internal's shape: system first (when the body had one), then one entry
/// per message.
pub fn apply_curation(
    body: &Value,
    internal_curated: &[Value],
    mut folds: Option<&mut FoldMap>,
) -> Value {
    let mut out = body.as_object().cloned().unwrap_or_default();
    let mut idx = 0usize;
    let had_system = !system_to_text(body.get("system")).is_empty();
    if had_system {
        // Frozen at first serve, replayed forever after.
        let frozen = folds.as_ref().and_then(|f| f.get("__system__").cloned());
        if let Some(sys) = frozen {
            out.insert("system".into(), sys);
        } else {
            let sys_text = internal_curated
                .first()
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if sys_text != system_to_text(body.get("system")) {
                out.insert(
                    "system".into(),
                    rewrite_first_text(body.get("system"), sys_text),
                );
            }
            if let Some(f) = folds.as_deref_mut() {
                f.insert(
                    "__system__".into(),
                    out.get("system").cloned().unwrap_or(Value::Null),
                );
            }
        }
        idx = 1;
    }

    let src_msgs = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut new_msgs: Vec<Value> = Vec::with_capacity(src_msgs.len());
    for (j, orig) in src_msgs.iter().enumerate() {
        let key = format!(
            "{}:{}",
            j,
            orig_fingerprint(orig.get("content").unwrap_or(&Value::Null))
        );
        if let Some(f) = folds.as_ref() {
            if let Some(frozen) = f.get(&key) {
                new_msgs.push(frozen.clone());
                continue;
            }
        }
        let cur = internal_curated.get(idx + j);
        let Some(cur) = cur else {
            new_msgs.push(orig.clone());
            if let Some(f) = folds.as_deref_mut() {
                f.insert(key, orig.clone());
            }
            continue;
        };
        // Python: cur.get("content") or "" — None and non-string collapse to "".
        let cur_text = cur.get("content").and_then(Value::as_str).unwrap_or("");
        if cur_text == content_text(orig.get("content")) {
            new_msgs.push(orig.clone());
            if let Some(f) = folds.as_deref_mut() {
                f.insert(key, orig.clone());
            }
            continue;
        }
        let mut nm = orig.as_object().cloned().unwrap_or_default();
        nm.insert(
            "content".into(),
            rewrite_first_text(orig.get("content"), cur_text),
        );
        let nm = Value::Object(nm);
        new_msgs.push(nm.clone());
        if let Some(f) = folds.as_deref_mut() {
            f.insert(key, nm);
        }
    }
    out.insert("messages".into(), Value::Array(new_msgs));
    Value::Object(out)
}

/// Un-freeze the CURRENT turn's fold entries — every item after the last
/// assistant-authored item — so the next request folds them afresh.
///
/// The fold map memoizes a message's served bytes at its first serve and
/// replays them forever (cache stability). Under
/// `FreezeConfig::protect_current` a tool result's first serve is always
/// its FULL form; its curated form only exists once the next assistant turn
/// has landed. Left frozen, the full bytes would replay forever and no cut
/// of that message would ever reach the wire. Dropping the current turn's
/// entries costs one re-fold of the trailing messages at the next request
/// — the prefix before them keeps its bytes and its cache hits.
///
/// `is_assistant` decides authorship per wire shape (Anthropic: `role ==
/// "assistant"`; Responses: assistant messages, `function_call`,
/// `reasoning`). No assistant item at all ⇒ nothing is history yet ⇒ every
/// entry is dropped.
pub fn unfreeze_current_turn(
    items: Option<&Value>,
    folds: &mut FoldMap,
    is_assistant: impl Fn(&Value) -> bool,
) {
    let Some(items) = items.and_then(Value::as_array) else {
        return;
    };
    let last_asst = items.iter().rposition(is_assistant);
    let first_current = last_asst.map(|i| i + 1).unwrap_or(0);
    folds.retain(|key, _| {
        key.split_once(':')
            .and_then(|(j, _)| j.parse::<usize>().ok())
            .is_none_or(|j| j < first_current)
    });
}

// ── breakpoint placement (anthropic_shapes.place_cache_breakpoint) ──────────

/// anthropic_shapes.append_user_text: a governor/no-reread directive as a
/// native trailing user turn.
pub fn append_user_text(body: &Value, text: &str) -> Value {
    if text.is_empty() {
        return body.clone();
    }
    let mut out = body.as_object().cloned().unwrap_or_default();
    let mut msgs = out
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    msgs.push(serde_json::json!({
        "role": "user",
        "content": [{"type": "text", "text": text}]
    }));
    out.insert("messages".into(), Value::Array(msgs));
    Value::Object(out)
}

/// _clear_cache_control: strip cache_control off every block of a content
/// value (list of blocks); str/None untouched.
fn clear_cache_control(content: &Value) -> Value {
    match content {
        Value::Array(blocks) => Value::Array(
            blocks
                .iter()
                .map(|b| match b.as_object() {
                    Some(o) => Value::Object(
                        o.iter()
                            .filter(|(k, _)| k.as_str() != "cache_control")
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    ),
                    None => b.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The longest cache TTL the CLIENT asked for, read off the inbound body's
/// own `cache_control` markers before we clear them. Claude subscription
/// clients anchor with `{"type":"ephemeral","ttl":"1h"}`; recreating our
/// markers as bare `ephemeral` silently downgraded that to the five-minute
/// default and multiplied cache-creation billing after ordinary pauses
/// (docs/Parsec_Desktop_Cache_Incident_2026-08-30 §4.5). "1h" wins over any
/// other value; otherwise the first explicit ttl seen is preserved verbatim.
pub fn client_cache_ttl(body: &Value) -> Option<String> {
    fn scan(content: &Value, found: &mut Option<String>) {
        if let Value::Array(blocks) = content {
            for b in blocks {
                if let Some(t) = b
                    .pointer("/cache_control/ttl")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    if t == "1h" {
                        *found = Some("1h".into());
                    } else if found.is_none() {
                        *found = Some(t.to_string());
                    }
                }
            }
        }
    }
    let mut found: Option<String> = None;
    if let Some(sys) = body.get("system") {
        scan(sys, &mut found);
    }
    for m in body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if found.as_deref() == Some("1h") {
            break;
        }
        if let Some(c) = m.get("content") {
            scan(c, &mut found);
        }
    }
    found
}

/// The ephemeral marker we emit — carrying the client's ttl forward when it
/// asked for one (never silently downgrading 1h to the 5m default).
fn ephemeral_marker(ttl: Option<&str>) -> Value {
    match ttl {
        Some(t) => serde_json::json!({"type": "ephemeral", "ttl": t}),
        None => serde_json::json!({"type": "ephemeral"}),
    }
}

/// _set_cache_control_last: mark the LAST dict block ephemeral; promote a
/// bare non-empty string to a single text block first.
fn set_cache_control_last(content: &Value, ttl: Option<&str>) -> Value {
    match content {
        Value::String(s) if !s.is_empty() => serde_json::json!(
            [{"type": "text", "text": s, "cache_control": ephemeral_marker(ttl)}]
        ),
        Value::Array(blocks) if !blocks.is_empty() => {
            let mut out = blocks.clone();
            for b in out.iter_mut().rev() {
                if let Some(o) = b.as_object_mut() {
                    o.insert("cache_control".into(), ephemeral_marker(ttl));
                    break;
                }
            }
            Value::Array(out)
        }
        other => other.clone(),
    }
}

/// _msg_fingerprint: sha256 of the cache_control-free bytes as they will go
/// upstream (call AFTER clearing).
pub fn msg_fingerprint(content: &Value) -> String {
    let blob = py_json_dumps_opts(content, true, false);
    let mut h = Sha256::new();
    h.update(blob.as_bytes());
    format!("{:x}", h.finalize())
}

/// _frozen_prefix_len: longest leading run matching the prior call.
fn frozen_prefix_len(cur: &[String], prior: Option<&[String]>) -> usize {
    let Some(prior) = prior else { return 0 };
    cur.iter()
        .zip(prior.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

/// place_cache_breakpoint, sessioned path. Three deliberate fixes vs the
/// reference: (0) the client's cache TTL is preserved on every marker we
/// recreate — the reference emitted bare `ephemeral`, downgrading a
/// subscription client's one-hour cache to five minutes (incident
/// 2026-08-30 §4.5); (1) this call's fingerprints are RETURNED, not auto-persisted
/// — the caller commits them only after the upstream call succeeds
/// (anthropic_shapes.py:484 wrote them before send: a failed call anchored
/// the retry on bytes Anthropic never cached); (2) owned values — the
/// reference's in-place string→block promotion mutated dicts aliased inside
/// served_folds, silently changing frozen bytes once per turn. The offline
/// no-state degrade path (orig_body verbatim scan) is not ported — the
/// proxy always carries prior fingerprints.
pub fn place_cache_breakpoint(
    body: &Value,
    directive_appended: bool,
    prior_fps: Option<&[String]>,
) -> (Value, Vec<String>) {
    // (0) capture the client's ttl BEFORE the markers are cleared.
    let ttl = client_cache_ttl(body);
    let ttl = ttl.as_deref();
    let mut out = body.as_object().cloned().unwrap_or_default();
    let mut msgs: Vec<Value> = out
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let n = msgs.len();

    // (1) clear every cache_control: CC's anchors were computed for the
    // un-curated body and sit on trimmed/shifted bytes after fold-back.
    if let Some(sys) = out.get("system").cloned() {
        out.insert("system".into(), clear_cache_control(&sys));
    }
    for m in msgs.iter_mut() {
        if let Some(o) = m.as_object_mut() {
            let c = o.get("content").map(clear_cache_control);
            if let Some(c) = c {
                o.insert("content".into(), c);
            }
        }
    }

    // (2) fingerprint the cacheable bytes THIS call.
    let cur_fps: Vec<String> = msgs
        .iter()
        .map(|m| msg_fingerprint(m.get("content").unwrap_or(&Value::Null)))
        .collect();

    // (5) one anchor on the run-stable system block.
    if let Some(sys) = out.get("system") {
        if py_truthy(sys) {
            let anchored = set_cache_control_last(sys, ttl);
            out.insert("system".into(), anchored);
        }
    }
    if msgs.is_empty() {
        out.insert("messages".into(), Value::Array(msgs));
        return (Value::Object(out), cur_fps);
    }

    // Never anchor the appended directive turn (a brand-new moving tail).
    let last_eligible = if directive_appended && n >= 2 {
        n - 2
    } else {
        n.saturating_sub(1)
    };

    // (3) frozen-prefix anchor: deepest message proven byte-identical to the
    // prior call; first call (nothing proven) seeds the cache at the tail.
    let frozen = frozen_prefix_len(&cur_fps, prior_fps);
    let prefix_anchor = if frozen >= 1 {
        std::cmp::min(frozen - 1, last_eligible)
    } else {
        last_eligible
    };
    let anchored = set_cache_control_last(
        msgs[prefix_anchor].get("content").unwrap_or(&Value::Null),
        ttl,
    );
    if let Some(o) = msgs[prefix_anchor].as_object_mut() {
        o.insert("content".into(), anchored);
    }

    // (4) tail anchor so this turn's new bytes persist a segment for the
    // NEXT call to read back to.
    if last_eligible > prefix_anchor {
        let anchored = set_cache_control_last(
            msgs[last_eligible].get("content").unwrap_or(&Value::Null),
            ttl,
        );
        if let Some(o) = msgs[last_eligible].as_object_mut() {
            o.insert("content".into(), anchored);
        }
    }
    out.insert("messages".into(), Value::Array(msgs));
    (Value::Object(out), cur_fps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn every_marker(v: &Value, out: &mut Vec<Value>) {
        match v {
            Value::Array(a) => a.iter().for_each(|b| every_marker(b, out)),
            Value::Object(o) => {
                if let Some(cc) = o.get("cache_control") {
                    out.push(cc.clone());
                }
                o.values().for_each(|b| every_marker(b, out));
            }
            _ => {}
        }
    }

    #[test]
    fn breakpoints_preserve_client_one_hour_ttl() {
        // Incident 2026-08-30 §4.5: the client anchored with ttl "1h"; the
        // recreated markers silently downgraded to the 5-minute default.
        // Every marker we emit must carry the client's ttl forward.
        let body = json!({
            "system": [{"type": "text", "text": "be terse",
                        "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "yo",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}}]},
                {"role": "user", "content": [{"type": "text", "text": "go"}]}
            ]
        });
        let (out, _fps) = place_cache_breakpoint(&body, false, None);
        let mut markers = Vec::new();
        every_marker(&out, &mut markers);
        assert!(!markers.is_empty(), "no anchors emitted: {out}");
        for m in &markers {
            assert_eq!(
                m,
                &json!({"type": "ephemeral", "ttl": "1h"}),
                "marker downgraded the client ttl: {m}"
            );
        }
    }

    #[test]
    fn breakpoints_without_client_ttl_stay_bare_ephemeral() {
        // Reference behavior when the client never asked for a ttl.
        let body = json!({
            "system": "be terse",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi",
                    "cache_control": {"type": "ephemeral"}}]}
            ]
        });
        let (out, _fps) = place_cache_breakpoint(&body, false, None);
        let mut markers = Vec::new();
        every_marker(&out, &mut markers);
        assert!(!markers.is_empty());
        for m in &markers {
            assert_eq!(m, &json!({"type": "ephemeral"}), "{m}");
        }
    }

    #[test]
    fn client_ttl_prefers_one_hour_over_shorter() {
        let body = json!({
            "system": [{"type": "text", "text": "s",
                        "cache_control": {"type": "ephemeral", "ttl": "5m"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "u",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}
            ]
        });
        assert_eq!(client_cache_ttl(&body).as_deref(), Some("1h"));
        assert_eq!(client_cache_ttl(&json!({"messages": []})), None);
    }

    #[test]
    fn unfreeze_current_turn_drops_only_trailing_entries() {
        let body = serde_json::json!({ "messages": [
            {"role": "user", "content": "task"},
            {"role": "assistant", "content": "a1"},
            {"role": "user", "content": "r1"},
            {"role": "assistant", "content": "a2"},
            {"role": "user", "content": "r2a"},
            {"role": "user", "content": "r2b"},
        ]});
        let mut folds: FoldMap = FoldMap::new();
        for j in 0..6 {
            folds.insert(format!("{j}:fp"), Value::Null);
        }
        folds.insert("__system__".into(), Value::Null);
        unfreeze_current_turn(body.get("messages"), &mut folds, |m| {
            m.get("role").and_then(Value::as_str) == Some("assistant")
        });
        let mut keys: Vec<&String> = folds.keys().collect();
        keys.sort();
        assert_eq!(keys, ["0:fp", "1:fp", "2:fp", "3:fp", "__system__"]);

        // No assistant turn yet: nothing is history.
        let body = serde_json::json!({ "messages": [{"role": "user", "content": "task"}] });
        let mut folds: FoldMap = FoldMap::new();
        folds.insert("0:fp".into(), Value::Null);
        unfreeze_current_turn(body.get("messages"), &mut folds, |m| {
            m.get("role").and_then(Value::as_str) == Some("assistant")
        });
        assert!(folds.is_empty());
    }
}
