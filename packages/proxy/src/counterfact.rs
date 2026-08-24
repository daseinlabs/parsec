//! Local-BPE counterfactual for wires with no `count_tokens` endpoint.
//!
//! §8.4 says savings come from the per-request count_tokens counterfactual and
//! never from a modeled baseline. OpenAI publishes no count_tokens endpoint,
//! so Codex traffic recorded `counterfactual_input_tokens: null` forever: real
//! cuts, permanently unmeasured.
//!
//! What this module adds is NOT a model. It counts the tokens that were
//! actually removed, with the same byte-pair encoding the provider bills
//! with, and anchors the result to the provider's own reported usage:
//!
//! ```text
//!   delta          = bpe(original text) - bpe(served text)
//!   counterfactual = billed_input_side + delta
//!   savings        = counterfactual - billed_input_side = delta
//! ```
//!
//! The billed term cancels out of the savings figure entirely, which is the
//! point: the only quantity this computes is the difference between two
//! bodies that differ ONLY in their text, measured directly. Per-message
//! framing overhead, tool schemas, and image/audio parts are identical in
//! both bodies (the fold-back contract preserves item count, order and every
//! non-text field), so they cancel too — and because they are excluded rather
//! than guessed at, the delta is a FLOOR on what was saved. Undervalued,
//! never overvalued.
//!
//! Two things it is honest about, both surfaced rather than hidden:
//!
//! - The result is stamped `counterfactual_source: "local_bpe"` in the ledger
//!   so it never silently pools with provider-probed rows.
//! - The encoding is o200k_base, which is an ASSUMPTION about what the served
//!   model tokenizes with. [`sanity_check`] tests it against the provider's
//!   own billed count and warns when they disagree beyond what framing
//!   overhead can explain.

use std::sync::OnceLock;

use serde_json::Value;
use tiktoken_rs::CoreBPE;

/// The encoding GPT-4o/o-series/gpt-5-family models bill with. Built once —
/// loading the ranks is milliseconds and megabytes, so never per request.
fn bpe() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| match tiktoken_rs::o200k_base() {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(
                "o200k_base encoding unavailable ({e}) — OpenAI savings stay unmeasured (null)"
            );
            None
        }
    })
    .as_ref()
}

/// Token count of one string. `encode_ordinary` treats special-token
/// sequences as the literal text they are, which is what counting CONTENT
/// requires.
fn count(text: &str) -> usize {
    bpe().map(|b| b.encode_ordinary(text).len()).unwrap_or(0)
}

/// Every piece of text on a Responses body that curation is able to rewrite:
/// `instructions` plus each input item's [`crate::responses::item_text`].
/// Deliberately the SAME surface `apply_curation` folds back onto — anything
/// outside it is byte-identical between the two bodies and would cancel.
pub fn responses_text_tokens(body: &Value) -> i64 {
    let mut total = 0usize;
    if let Some(i) = body.get("instructions").and_then(Value::as_str) {
        total += count(i);
    }
    for item in body
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let t = crate::responses::item_text(item);
        if !t.is_empty() {
            total += count(&t);
        }
    }
    total as i64
}

/// What one request's local count found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCount {
    /// Tokens removed: `bpe(original) - bpe(served)`, floored at 0.
    pub delta: i64,
    /// Tokens of the text actually served — the term [`sanity_check`] holds
    /// against the provider's billed total.
    pub served: i64,
}

/// Measure one request. `None` when the encoding is unavailable — the ledger
/// then records null, exactly as before, rather than a guess.
pub fn responses_delta(original: &Value, curated: &Value) -> Option<LocalCount> {
    bpe()?;
    let served = responses_text_tokens(curated);
    Some(LocalCount {
        delta: (responses_text_tokens(original) - served).max(0),
        served,
    })
}

/// Does the local encoding agree with the provider's own accounting?
///
/// The served text is a SUBSET of what was billed (billing also covers
/// per-message framing and tool schemas), so `local <= billed` must hold. A
/// local count that EXCEEDS the billed total means the encoding is producing
/// more tokens than the provider charges for — a different tokenizer — and
/// the delta computed from it is proportionally wrong. Warn once; the number
/// stays labeled `local_bpe` either way, so a reader can discount it.
pub fn sanity_check(local_served: i64, billed_input_side: i64) {
    if billed_input_side <= 0 || local_served <= billed_input_side {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            local = local_served,
            billed = billed_input_side,
            "o200k_base counts MORE tokens for the served text than the provider billed for \
             the whole request — the served model likely uses a different encoding, so \
             local_bpe savings are overstated. Treat them as directional."
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(instructions: &str, texts: &[&str]) -> Value {
        json!({
            "instructions": instructions,
            "input": texts
                .iter()
                .map(|t| json!({"type": "message", "role": "user",
                                "content": [{"type": "input_text", "text": t}]}))
                .collect::<Vec<_>>(),
        })
    }

    #[test]
    fn delta_is_the_tokens_actually_removed() {
        let long = "the quick brown fox jumps over the lazy dog. ".repeat(200);
        let orig = body("sys", &[&long, "keep me"]);
        let cut = body("sys", &["[... omitted ...]", "keep me"]);
        let m = responses_delta(&orig, &cut).expect("encoding available");
        let expected = count(&long) as i64 - count("[... omitted ...]") as i64;
        assert_eq!(m.delta, expected);
        assert_eq!(m.served, responses_text_tokens(&cut));
        assert!(
            m.delta > 1000,
            "a 200x repeat should be four figures: {m:?}"
        );
    }

    #[test]
    fn an_uncurated_body_measures_exactly_zero_saved() {
        let b = body("sys", &["a", "b"]);
        assert_eq!(responses_delta(&b, &b).map(|m| m.delta), Some(0));
    }

    /// A body that GREW (never happens through fold-back, but the arithmetic
    /// must not report negative savings if it ever did).
    #[test]
    fn growth_clamps_at_zero_rather_than_reporting_negative_savings() {
        let small = body("sys", &["x"]);
        let big = body("sys", &["x".repeat(4000).as_str()]);
        assert_eq!(responses_delta(&small, &big).map(|m| m.delta), Some(0));
    }

    /// Non-text fields are outside the counted surface, so they cancel: a
    /// reasoning blob or a tool call changes nothing about the delta.
    #[test]
    fn opaque_items_do_not_enter_the_count() {
        let with_opaque = json!({
            "instructions": "sys",
            "input": [
                {"type": "reasoning", "id": "r1", "encrypted_content": "gAAAA".repeat(500)},
                {"type": "function_call", "name": "shell", "call_id": "c1",
                 "arguments": "{\"command\":[\"bash\",\"-lc\",\"ls\"]}"},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hello"}]},
            ],
        });
        assert_eq!(
            responses_text_tokens(&with_opaque),
            responses_text_tokens(&body("sys", &["hello"]))
        );
    }

    /// The encoding is the real thing, not a chars/4 stand-in: code and CJK
    /// tokenize at rates a divide-by-four cannot reproduce.
    #[test]
    fn counts_are_bpe_not_a_character_ratio() {
        assert_eq!(count("hello world"), 2);
        assert_eq!(count("def parse(x):\n    return None\n"), 8);
        assert_eq!(count("修正してください 🚀"), 5);
    }
}
