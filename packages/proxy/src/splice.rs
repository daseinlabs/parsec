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
//! the map recomputable lives in dasein-engine's `freeze` module.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use dasein_engine::pystr::{py_json_dumps_opts, py_strip};

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

fn py_truthy(v: &Value) -> bool {
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

/// _set_cache_control_last: mark the LAST dict block ephemeral; promote a
/// bare non-empty string to a single text block first.
fn set_cache_control_last(content: &Value) -> Value {
    match content {
        Value::String(s) if !s.is_empty() => serde_json::json!(
            [{"type": "text", "text": s, "cache_control": {"type": "ephemeral"}}]
        ),
        Value::Array(blocks) if !blocks.is_empty() => {
            let mut out = blocks.clone();
            for b in out.iter_mut().rev() {
                if let Some(o) = b.as_object_mut() {
                    o.insert(
                        "cache_control".into(),
                        serde_json::json!({"type": "ephemeral"}),
                    );
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

/// place_cache_breakpoint, sessioned path. Two deliberate fixes vs the
/// reference: (1) this call's fingerprints are RETURNED, not auto-persisted
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
            let anchored = set_cache_control_last(sys);
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
    let anchored =
        set_cache_control_last(msgs[prefix_anchor].get("content").unwrap_or(&Value::Null));
    if let Some(o) = msgs[prefix_anchor].as_object_mut() {
        o.insert("content".into(), anchored);
    }

    // (4) tail anchor so this turn's new bytes persist a segment for the
    // NEXT call to read back to.
    if last_eligible > prefix_anchor {
        let anchored =
            set_cache_control_last(msgs[last_eligible].get("content").unwrap_or(&Value::Null));
        if let Some(o) = msgs[last_eligible].as_object_mut() {
            o.insert("content".into(), anchored);
        }
    }
    out.insert("messages".into(), Value::Array(msgs));
    (Value::Object(out), cur_fps)
}
