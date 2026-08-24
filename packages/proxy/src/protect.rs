//! Human-authored content protection — the wire-provenance guard between the
//! Freezer's decisions and the served bytes.
//!
//! `parsec_engine::freeze::parse` types every `role: "user"` **and**
//! `role: "tool"` message as an *observation* (freeze.rs:295) and chunks all
//! of them but the first. That equivalence is calibrated on the Anthropic
//! wire, where a `role: "user"` message is overwhelmingly a `tool_result`
//! carrier — Anthropic has no separate tool role, so "user == observation" is
//! nearly true there. It is FALSE on the OpenAI Responses wire, where tool
//! output arrives as its own `function_call_output` item
//! (`responses::internal_entry` maps it to `role: "tool"`), leaving
//! `role: "user"` to carry human prose exclusively. Ported unchanged, the
//! curator pointed straight at user instructions: a long Codex prompt came
//! back as its first line, an omission marker, and its last line, with every
//! requirement in between gone (bugs/parsec_codex_user_message_trimming_report.md).
//!
//! The guard runs at fold-back rather than in the chunker on purpose:
//!
//! - The chunk array feeds `BirthQuery::chunk_checksum`, which the brain
//!   re-derives per request and answers 409 on mismatch. Skipping human
//!   messages at parse time would move every checksum and fail every request
//!   open until the Python side shipped the identical change. Parity is the
//!   port's definition of done — so chunking, scoring, and featurization stay
//!   byte-identical and only the SERVED text changes.
//! - Restoring protected entries makes `curated[i] == original[i]`, which is
//!   exactly the equality branch both `apply_curation` implementations
//!   already forward verbatim. No fold-back code has to learn about roles.
//!
//! Determinism is unaffected: the mask is a pure function of the inbound body,
//! so served bytes remain a pure function of (prefix, checkpoint, config).
//!
//! NB the fold map memoizes served bytes per conversation and is in-memory
//! only (`AppState::convs`), so conversations resident from before this guard
//! keep replaying their recorded cuts until the proxy restarts.

use std::collections::BTreeMap;

use serde_json::Value;

use parsec_engine::pystr::char_len;

/// chars/4 estimate of one internal entry's text — the same measure
/// `server::internal_mass` sums, applied per entry.
fn entry_mass(m: &Value) -> i64 {
    let t = match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        other => crate::splice::content_text(other),
    };
    char_len(&t) as i64 / 4
}

/// Put the ORIGINAL text back on every protected entry the curator rewrote,
/// and report the chars/4 mass that restored — the "cuts refused" signal.
///
/// `mask[i]` marks internal entry `i` as human-authored. A length mismatch
/// (the curator is contracted to preserve count and order) protects whatever
/// prefix does line up and leaves the rest alone: fail closed, never panic.
pub fn restore_protected(original: &[Value], curated: &mut [Value], mask: &[bool]) -> i64 {
    let n = original.len().min(curated.len()).min(mask.len());
    let mut restored = 0i64;
    for i in 0..n {
        if !mask[i] {
            continue;
        }
        let (Some(orig_c), Some(cur_c)) = (original[i].get("content"), curated[i].get("content"))
        else {
            continue;
        };
        if orig_c == cur_c {
            continue;
        }
        restored += (entry_mass(&original[i]) - entry_mass(&curated[i])).max(0);
        let orig_c = orig_c.clone();
        if let Some(o) = curated[i].as_object_mut() {
            o.insert("content".into(), orig_c);
        }
    }
    restored
}

/// Per-role chars/4 breakdown of what the curator actually cut, for the
/// ledger's role-aware accounting (report fix criterion 8). Roles are the
/// INTERNAL view's: `tool` = tool output, `user` = an observation-bearing
/// user turn (Anthropic `tool_result` carriers), `assistant` = the model's
/// own prior text. Only positive deltas are recorded; a role with no cut is
/// absent, so a row with no `user` key is a row that cut no user turn.
pub fn cut_by_role(original: &[Value], curated: &[Value]) -> BTreeMap<String, i64> {
    let mut out: BTreeMap<String, i64> = BTreeMap::new();
    for (o, c) in original.iter().zip(curated.iter()) {
        let delta = entry_mass(o) - entry_mass(c);
        if delta <= 0 {
            continue;
        }
        *out.entry(role_key(o.get("role")).to_string()).or_insert(0) += delta;
    }
    out
}

/// The internal view's closed role set. `to_internal` copies the inbound
/// `role` VERBATIM (internal.rs: "a PRESENT key passes through whatever value
/// it holds"), so without this gate an arbitrary client string would become a
/// KEY in a ledger row that ships to our cloud — the ledger contract must
/// stay unable to carry raw text, the same rule `tool_from_headers` and
/// `session_id_from_metadata` enforce on their fields. Anything off the list
/// buckets to `(other)`; a missing role reads `(unrecorded)`.
fn role_key(role: Option<&Value>) -> &'static str {
    match role.and_then(Value::as_str) {
        Some("system") => "system",
        Some("user") => "user",
        Some("assistant") => "assistant",
        Some("tool") => "tool",
        Some("opaque") => "opaque",
        Some(_) => "(other)",
        None => "(unrecorded)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn role_breakdown_keys_cannot_carry_wire_text() {
        // to_internal copies the inbound role verbatim, so this is reachable
        // from any client: without the gate the secret becomes a ledger KEY
        // and ships to our cloud on the next row.
        let orig = vec![
            json!({"role": "user", "content": "x".repeat(400)}),
            json!({"role": "sk-live-abc123 and a whole sentence", "content": "y".repeat(400)}),
            json!({"content": "z".repeat(400)}),
        ];
        let cur = vec![
            json!({"role": "user", "content": "x"}),
            json!({"role": "sk-live-abc123 and a whole sentence", "content": "y"}),
            json!({"content": "z"}),
        ];
        let by_role = cut_by_role(&orig, &cur);
        let keys: Vec<&str> = by_role.keys().map(String::as_str).collect();
        assert_eq!(keys, ["(other)", "(unrecorded)", "user"]);
        // The mass is still accounted for — bucketed, not dropped.
        assert!(by_role["(other)"] > 0);
    }

    #[test]
    fn restores_only_masked_entries_and_reports_the_refused_mass() {
        let orig = vec![
            json!({"role": "user", "content": "x".repeat(400)}),
            json!({"role": "tool", "content": "y".repeat(400)}),
        ];
        let mut cur = vec![
            json!({"role": "user", "content": "x"}),
            json!({"role": "tool", "content": "y"}),
        ];
        let restored = restore_protected(&orig, &mut cur, &[true, false]);
        assert_eq!(cur[0]["content"], orig[0]["content"], "user not restored");
        assert_eq!(cur[1]["content"], "y", "tool output wrongly restored");
        // 400 chars -> 100 est. tokens in, 1 char -> 0 out.
        assert_eq!(restored, 100);
    }

    #[test]
    fn mismatched_lengths_protect_the_common_prefix_without_panicking() {
        let orig = vec![json!({"role": "user", "content": "abcd"})];
        let mut cur = vec![
            json!({"role": "user", "content": ""}),
            json!({"role": "tool", "content": ""}),
        ];
        restore_protected(&orig, &mut cur, &[true, true, true]);
        assert_eq!(cur[0]["content"], "abcd");
    }

    #[test]
    fn cut_by_role_buckets_positive_deltas_only() {
        let orig = vec![
            json!({"role": "tool", "content": "y".repeat(800)}),
            json!({"role": "user", "content": "x".repeat(400)}),
            json!({"role": "assistant", "content": "z"}),
        ];
        let cur = vec![
            json!({"role": "tool", "content": ""}),
            json!({"role": "user", "content": "x".repeat(400)}),
            json!({"role": "assistant", "content": "z"}),
        ];
        let by = cut_by_role(&orig, &cur);
        assert_eq!(by.get("tool"), Some(&200));
        assert_eq!(by.get("user"), None, "uncut role must be absent");
        assert_eq!(by.get("assistant"), None);
    }
}
