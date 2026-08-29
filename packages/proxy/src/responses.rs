//! OpenAI Responses wire → internal projection + fold-back (Codex Milestone
//! B, docs/codex-integration.md). The Responses sibling of `internal.rs` +
//! `splice.rs`: project the wire body into the flat internal message list the
//! Freezer curates, then fold curated TEXT back onto the ORIGINAL items, in
//! place, by position.
//!
//! Contract (mirrors the Anthropic fold-back invariants):
//! - **One internal entry per input item**, same order — positional fold-back
//!   is exact because the curator preserves message count and order.
//! - **`internal content == item_text(item)`** for every item: the fold-back
//!   equality test and the projection MUST agree, or an uncurated item would
//!   read as rewritten. Enforced by construction (the projection calls
//!   [`item_text`]) and by test.
//! - **Only text is ever rewritten.** `reasoning` items (encrypted_content),
//!   `function_call` items, images, and any item type this module does not
//!   recognize project as text-less (`""`), so the equality path serves them
//!   VERBATIM — `reasoning.encrypted_content` and the tool-call pairing
//!   survive by construction, not by care.
//! - **The fold map replays exact bytes**: a turn already served folds to its
//!   recorded Value even if the curator would now decide differently — served
//!   bytes stay a pure function of (prefix, checkpoint, config), which is
//!   what keeps OpenAI's automatic prefix cache warm across turns.
//! - Top-level fields (`prompt_cache_key`, `store`, `include`, `tools`, …)
//!   ride through untouched: fold-back rewrites `instructions` and `input`
//!   only, starting from a clone of the original object.

use serde_json::{json, Map, Value};

use parsec_engine::pystr::py_strip;

use crate::internal::{bash_twin_command, derive_command, derive_query};
use crate::splice::{orig_fingerprint, FoldMap};

/// Content-part types that carry curatable text in a `message` item.
const MSG_TEXT_TYPES: &[&str] = &["input_text", "output_text", "text"];
/// ... and in a tool-output item's `output` list. `input_text` is in here
/// because that is what Codex's `custom_tool_call_output` actually uses for
/// its parts — captured from a real rollout, not from the wire docs.
const OUT_TEXT_TYPES: &[&str] = &["output_text", "text", "input_text"];

/// Tool-output item types whose text the curator may digest. `function_call_output`
/// is the documented Responses shape; `custom_tool_call_output` is what Codex
/// 0.147 emits for its `exec` tool and `local_shell_call_output` for the
/// local-shell tool. Until these were listed they fell to the `opaque` arm of
/// [`internal_entry`], which meant the curator could not see tool output on
/// this wire AT ALL — it produced no chunks, so the only cuttable mass left in
/// a Codex conversation was the user's own prose. That is the other half of
/// bugs/parsec_codex_user_message_trimming_report.md: parsec was cutting only
/// what it must never cut, and never cutting what it exists to cut.
const TOOL_OUTPUT_TYPES: &[&str] = &[
    "function_call_output",
    "custom_tool_call_output",
    "local_shell_call_output",
];

/// Assistant tool-invocation item types.
const TOOL_CALL_TYPES: &[&str] = &["function_call", "custom_tool_call", "local_shell_call"];

/// Is this input item a message? Codex sends `type: "message"` explicitly;
/// a typeless item with a `role` is treated as one (the wire allows it).
fn is_message(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => true,
        None => item.get("role").is_some(),
        Some(_) => false,
    }
}

/// The TEXT view of an input item — the exact string the internal projection
/// exposes to the curator, and the fold-back equality baseline. Text-less
/// item types (reasoning, function_call, images, unknown) return `""`.
pub fn item_text(item: &Value) -> String {
    if is_message(item) {
        return parts_text(item.get("content"), MSG_TEXT_TYPES);
    }
    match item.get("type").and_then(Value::as_str) {
        Some(t) if TOOL_OUTPUT_TYPES.contains(&t) => parts_text(item.get("output"), OUT_TEXT_TYPES),
        _ => String::new(),
    }
}

/// The shell command behind a tool call, for the bash-twin projection.
///
/// Three shapes in the wild, in the order they are tried:
///   * `arguments` JSON with a `command` key — the documented `function_call`
///     (argv array or string; [`coerce_argv_command`] handles both);
///   * `action.command` argv — `local_shell_call`;
///   * a `cmd:"…"` field inside Codex's `custom_tool_call.input`, which is a
///     JavaScript SNIPPET (`await tools.exec_command({cmd:"sed -n '1,240p' f"})`),
///     not JSON. Without this the twin sees no command at all, chunk typing
///     falls back to "other", and the needed-set's read/grep regexes have
///     nothing to match.
fn call_command(item: &Value) -> Option<String> {
    if let Some(argv) = item.pointer("/action/command").and_then(Value::as_array) {
        let joined = argv
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    let input = item.get("input").and_then(Value::as_str)?;
    // A JSON object input (some custom tools) before the snippet scrape.
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(input) {
        for k in ["command", "cmd"] {
            if let Some(c) = o.get(k).and_then(Value::as_str) {
                if !c.is_empty() {
                    return Some(c.to_string());
                }
            }
        }
    }
    extract_cmd_field(input)
}

/// Pull the value of a `cmd:"…"` / `command:"…"` field out of a JS snippet,
/// honouring backslash escapes so an embedded quote does not end the scan.
fn extract_cmd_field(src: &str) -> Option<String> {
    for key in ["cmd", "command"] {
        let mut from = 0usize;
        while let Some(rel) = src[from..].find(key) {
            let at = from + rel;
            from = at + key.len();
            // key must be followed by optional space, a colon, optional space, a quote
            let rest = src[at + key.len()..].trim_start();
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix('"') else {
                continue;
            };
            let mut out = String::new();
            let mut chars = rest.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => match chars.next() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some(other) => out.push(other),
                        None => break,
                    },
                    '"' => {
                        return (!out.is_empty()).then_some(out);
                    }
                    other => out.push(other),
                }
            }
        }
    }
    None
}

/// Flatten a content value (string, or list of typed parts) to text:
/// text-bearing parts space-joined, empties dropped — the `content_text`
/// convention of the Anthropic view.
fn parts_text(content: Option<&Value>, text_types: &[&str]) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_object)
            .filter(|p| {
                p.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| text_types.contains(&t))
            })
            .map(|p| p.get("text").and_then(Value::as_str).unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Codex's shell tool sends `command` as an argv array
/// (`["bash","-lc","…"]`); the bash-twin projection expects a string. Coerce
/// a same-shape copy so chunk typing sees the real command; everything else
/// passes through.
fn coerce_argv_command(args: &Value) -> Value {
    let Some(obj) = args.as_object() else {
        return args.clone();
    };
    let Some(Value::Array(argv)) = obj.get("command") else {
        return args.clone();
    };
    if argv.is_empty() || !argv.iter().all(Value::is_string) {
        return args.clone();
    }
    let joined = argv
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = obj.clone();
    out.insert("command".into(), Value::String(joined));
    Value::Object(out)
}

/// Which entries of [`to_internal`]'s output are PROTECTED — human-authored
/// text or the agent's own prose — and must reach the model byte-for-byte
/// (`protect::restore_protected`). Aligned to that output index-for-index,
/// system entry included.
///
/// This wire draws the line the Anthropic one cannot: tool output is its own
/// `function_call_output` item, so a `role: "user"` message here is ALWAYS a
/// human turn. Every one of them is protected — the first (which
/// `freeze::parse` exempts as the task anyway) and, critically, every later
/// one, which the shared "user == observation" typing would otherwise chunk
/// and digest down to a head line, an omission marker, and a tail line.
///
/// Assistant `message` items are protected as well (product decision
/// 2026-08-29: the agent's responses are served verbatim on every wire).
/// This wire already spares them at parse time (`cut_assistant: false` on
/// the Codex path), so the mask entry is belt-and-braces — the guarantee no
/// longer hinges on freeze config.
///
/// `developer`/`system` items project to `role: "system"` and are never
/// chunked; reasoning stays curatable; unrecognized item types project as
/// inert `opaque` entries.
pub fn protected_mask(body: &Value) -> Vec<bool> {
    let mut mask: Vec<bool> = Vec::new();
    if body
        .get("instructions")
        .and_then(Value::as_str)
        .is_some_and(|i| !i.is_empty())
    {
        mask.push(false); // instructions are never chunked, so never restored
    }
    for item in body
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let protected = is_message(item)
            && !matches!(
                item.get("role").and_then(Value::as_str),
                Some("system") | Some("developer")
            );
        mask.push(protected);
    }
    mask
}

/// Responses body → internal flat message list: optional
/// `{"role":"system"}` from `instructions` first, then one entry per input
/// item, same order. `function_call` items become assistant entries with
/// `tool_calls` + the bash-twin `extra.actions` (the action vocabulary the
/// GNN was trained on); `function_call_output` becomes a `tool` observation;
/// unrecognized types become inert `opaque` entries the freezer's parser
/// ignores (no chunks, no step advance) — present only to keep positions
/// aligned.
pub fn to_internal(body: &Value) -> Vec<Value> {
    let mut msgs: Vec<Value> = Vec::new();
    if let Some(instr) = body.get("instructions").and_then(Value::as_str) {
        if !instr.is_empty() {
            msgs.push(json!({"role": "system", "content": instr}));
        }
    }
    for item in body
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        msgs.push(internal_entry(item));
    }
    msgs
}

/// One input item → its internal entry. Public because the Codex transcript
/// reader (`codex.rs`) reshapes exactly this projection for /trim, so the
/// action vocabulary a trim labels against is the same one the live curator
/// sees — one definition, not two that can drift.
pub fn internal_entry(item: &Value) -> Value {
    if is_message(item) {
        let role = match item.get("role").and_then(Value::as_str) {
            Some("assistant") => "assistant",
            // developer is the Responses-era system role; parse() ignores
            // system either way (instructions-class text is never curated).
            Some("system") | Some("developer") => "system",
            _ => "user",
        };
        return json!({"role": role, "content": item_text(item)});
    }
    match item.get("type").and_then(Value::as_str) {
        Some(t) if TOOL_CALL_TYPES.contains(&t) => {
            let name = py_strip(item.get("name").and_then(Value::as_str).unwrap_or(""));
            let args_raw = item.get("arguments").and_then(Value::as_str).unwrap_or("");
            let args: Value = serde_json::from_str::<Value>(args_raw)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            let twin_args = coerce_argv_command(&args);
            let mut act = Map::new();
            act.insert("tool".into(), Value::String(name.to_string()));
            act.insert("name".into(), Value::String(name.to_string()));
            if let Some(obj) = args.as_object() {
                for (k, v) in obj {
                    act.insert(k.clone(), v.clone());
                }
            }
            // The item's own command field wins when it has one (Codex's
            // `exec`/`local_shell_call` keep the command outside `arguments`);
            // otherwise the documented function_call path applies.
            let command = call_command(item)
                .or_else(|| bash_twin_command(name, &twin_args).filter(|t| !t.is_empty()))
                .unwrap_or_else(|| derive_command(name, &twin_args));
            act.insert("command".into(), Value::String(command));
            let q = derive_query(name, &args);
            if !q.is_empty() {
                act.insert("query".into(), Value::String(q));
            }
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": if args_raw.is_empty() { "{}" } else { args_raw },
                    },
                }],
                "extra": {"actions": [Value::Object(act)]},
            })
        }
        Some(t) if TOOL_OUTPUT_TYPES.contains(&t) => {
            json!({"role": "tool", "content": item_text(item)})
        }
        // reasoning (encrypted_content), web_search_call, computer_call,
        // item_reference, …: invisible to curation, verbatim on the wire.
        _ => json!({"role": "opaque", "content": ""}),
    }
}

/// Fold curated text back onto the ORIGINAL Responses body. Mirrors
/// `splice::apply_curation`: `internal_curated` follows [`to_internal`]'s
/// shape (system first when the body had instructions, then one entry per
/// item); a turn already in `folds` replays its exact bytes; a new turn is
/// folded once and recorded. Only `instructions` and `input` are replaced on
/// the output object — every other top-level field keeps its original Value.
pub fn apply_curation(
    body: &Value,
    internal_curated: &[Value],
    mut folds: Option<&mut FoldMap>,
) -> Value {
    let mut out = body.as_object().cloned().unwrap_or_default();
    let mut idx = 0usize;
    let orig_instr = body
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !orig_instr.is_empty() {
        let frozen = folds.as_ref().and_then(|f| f.get("__system__").cloned());
        if let Some(sys) = frozen {
            out.insert("instructions".into(), sys);
        } else {
            let cur = internal_curated
                .first()
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if cur != orig_instr {
                out.insert("instructions".into(), Value::String(cur.to_string()));
            }
            if let Some(f) = folds.as_deref_mut() {
                f.insert(
                    "__system__".into(),
                    out.get("instructions").cloned().unwrap_or(Value::Null),
                );
            }
        }
        idx = 1;
    }

    let src = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut new_items: Vec<Value> = Vec::with_capacity(src.len());
    for (j, orig) in src.iter().enumerate() {
        let key = format!("{}:{}", j, orig_fingerprint(orig));
        if let Some(f) = folds.as_ref() {
            if let Some(frozen) = f.get(&key) {
                new_items.push(frozen.clone());
                continue;
            }
        }
        let cur_text = internal_curated
            .get(idx + j)
            .map(|c| c.get("content").and_then(Value::as_str).unwrap_or(""));
        let item = match cur_text {
            Some(ct) if ct != item_text(orig) => rewrite_item_text(orig, ct),
            _ => orig.clone(),
        };
        new_items.push(item.clone());
        if let Some(f) = folds.as_deref_mut() {
            f.insert(key, item);
        }
    }
    out.insert("input".into(), Value::Array(new_items));
    Value::Object(out)
}

/// Put curated text into an item, preserving everything else: first
/// text-bearing part rewritten, later text parts dropped (consolidated),
/// non-text parts untouched. Item types with no text surface — or an item
/// whose content has no text part to receive the rewrite — return unchanged:
/// this function never invents structure.
fn rewrite_item_text(item: &Value, new_text: &str) -> Value {
    let Some(o) = item.as_object() else {
        return item.clone();
    };
    if is_message(item) {
        let mut no = o.clone();
        if let Some(c) = o.get("content") {
            no.insert("content".into(), rewrite_parts(c, new_text, MSG_TEXT_TYPES));
        }
        return Value::Object(no);
    }
    if item
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| TOOL_OUTPUT_TYPES.contains(&t))
    {
        let mut no = o.clone();
        if let Some(out_v) = o.get("output") {
            no.insert(
                "output".into(),
                rewrite_parts(out_v, new_text, OUT_TEXT_TYPES),
            );
        }
        return Value::Object(no);
    }
    item.clone()
}

fn rewrite_parts(content: &Value, new_text: &str, text_types: &[&str]) -> Value {
    match content {
        Value::String(_) => Value::String(new_text.to_string()),
        Value::Array(parts) => {
            let mut out: Vec<Value> = Vec::new();
            let mut placed = false;
            for p in parts {
                match p.as_object() {
                    Some(po)
                        if po
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|t| text_types.contains(&t)) =>
                    {
                        if !placed {
                            let mut np = po.clone();
                            np.insert("text".into(), Value::String(new_text.to_string()));
                            out.push(Value::Object(np));
                            placed = true;
                        }
                        // later text parts consolidated into the first
                    }
                    _ => out.push(p.clone()),
                }
            }
            if !placed {
                // No text part to receive the rewrite: leave the item alone
                // rather than inventing one.
                return content.clone();
            }
            Value::Array(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parsec_engine::freeze::{FreezeConfig, Freezer, PassthroughScorer};

    fn codex_body() -> Value {
        json!({
            "model": "gpt-5.6-codex",
            "instructions": "You are Codex.",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "fix the bug"}]},
                {"type": "reasoning", "id": "rs_1",
                 "encrypted_content": "gAAAAABopaque", "summary": []},
                {"type": "function_call", "name": "shell", "call_id": "call_1",
                 "arguments": "{\"command\":[\"bash\",\"-lc\",\"cat src/parser.py\"]}"},
                {"type": "function_call_output", "call_id": "call_1",
                 "output": "1: def parse():\n2:     return None\n"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Found it."}]}
            ],
            "prompt_cache_key": "sess-1",
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "stream": true
        })
    }

    #[test]
    fn projection_shapes_and_bash_twin() {
        let internal = to_internal(&codex_body());
        assert_eq!(internal.len(), 6, "system + one entry per item");
        assert_eq!(internal[0]["role"], "system");
        assert_eq!(internal[0]["content"], "You are Codex.");
        assert_eq!(internal[1]["role"], "user");
        assert_eq!(internal[1]["content"], "fix the bug");
        // Reasoning is opaque: no content, ignored by the freezer's parser.
        assert_eq!(internal[2]["role"], "opaque");
        assert_eq!(internal[2]["content"], "");
        // function_call: assistant with tool_calls + the argv-joined twin.
        assert_eq!(internal[3]["role"], "assistant");
        assert_eq!(internal[3]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            internal[3]["extra"]["actions"][0]["command"],
            "bash -lc cat src/parser.py"
        );
        assert_eq!(internal[4]["role"], "tool");
        assert!(internal[4]["content"]
            .as_str()
            .unwrap()
            .starts_with("1: def parse()"));
        assert_eq!(internal[5]["role"], "assistant");
        assert_eq!(internal[5]["content"], "Found it.");
    }

    #[test]
    fn projection_content_equals_item_text_for_every_item() {
        // The fold-back equality baseline and the projection MUST agree —
        // a drift here would make uncurated items read as rewritten.
        let body = codex_body();
        let internal = to_internal(&body);
        let items = body["input"].as_array().unwrap();
        for (j, item) in items.iter().enumerate() {
            assert_eq!(
                internal[j + 1]["content"].as_str().unwrap(),
                item_text(item),
                "item {j} projection/equality drift"
            );
        }
    }

    #[test]
    fn equality_path_serves_every_item_verbatim() {
        let body = codex_body();
        let internal = to_internal(&body);
        let mut folds = FoldMap::new();
        let out = apply_curation(&body, &internal, Some(&mut folds));
        assert_eq!(out, body, "uncurated fold-back must be a fixpoint");
        // system + 5 items recorded.
        assert_eq!(folds.len(), 6);
    }

    #[test]
    fn curated_tool_output_rewrites_text_only() {
        let body = codex_body();
        let mut internal = to_internal(&body);
        internal[4]["content"] = json!("[digest] re-read src/parser.py");
        let out = apply_curation(&body, &internal, None);
        // The output text was replaced…
        assert_eq!(out["input"][3]["output"], "[digest] re-read src/parser.py");
        assert_eq!(out["input"][3]["call_id"], "call_1");
        // …and NOTHING else moved: reasoning (encrypted_content), the
        // function_call, and every top-level field are byte-identical.
        assert_eq!(out["input"][1], body["input"][1]);
        assert_eq!(out["input"][2], body["input"][2]);
        assert_eq!(out["prompt_cache_key"], body["prompt_cache_key"]);
        assert_eq!(out["include"], body["include"]);
        assert_eq!(out["instructions"], body["instructions"]);
    }

    #[test]
    fn fold_replay_wins_over_new_curation() {
        let body = codex_body();
        let internal = to_internal(&body);
        let mut folds = FoldMap::new();
        let first = apply_curation(&body, &internal, Some(&mut folds));
        // Second call: curator now wants to rewrite item 3 — the fold wins,
        // served bytes stay a pure function of what was already served.
        let mut cur2 = internal.clone();
        cur2[4]["content"] = json!("[digest]");
        let second = apply_curation(&body, &cur2, Some(&mut folds));
        assert_eq!(first, second, "folded turn must replay exact bytes");
    }

    #[test]
    fn structured_output_parts_rewrite_first_and_consolidate() {
        let item = json!({"type": "function_call_output", "call_id": "c",
        "output": [
            {"type": "output_text", "text": "long line one"},
            {"type": "output_image", "image_url": "data:..."},
            {"type": "output_text", "text": "long line two"}
        ]});
        assert_eq!(item_text(&item), "long line one long line two");
        let out = rewrite_item_text(&item, "[digest]");
        let parts = out["output"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "second text part consolidated");
        assert_eq!(parts[0]["text"], "[digest]");
        assert_eq!(parts[1]["type"], "output_image");
        // Rewritten item's text view equals the curated text — next-turn
        // equality (after a memo loss) re-derives the same decision.
        assert_eq!(item_text(&out), "[digest]");
    }

    #[test]
    fn textless_items_never_rewrite() {
        let reasoning = json!({"type": "reasoning", "encrypted_content": "gAAA"});
        assert_eq!(rewrite_item_text(&reasoning, "junk"), reasoning);
        let call = json!({"type": "function_call", "name": "shell",
                          "arguments": "{}", "call_id": "c1"});
        assert_eq!(rewrite_item_text(&call, "junk"), call);
    }

    #[test]
    fn freezer_serves_responses_internal_view_shape_preserving() {
        // The projected view must be servable by the real Freezer: count and
        // order preserved, opaque/system entries untouched.
        let internal = to_internal(&codex_body());
        let mut fz = Freezer::new(FreezeConfig::default(), PassthroughScorer);
        let served = fz.serve(&internal).expect("freezer accepts the view");
        assert_eq!(served.len(), internal.len());
        for (i, (a, b)) in internal.iter().zip(served.iter()).enumerate() {
            assert_eq!(a.get("role"), b.get("role"), "role changed at {i}");
        }
        assert_eq!(served[0], internal[0], "system entry untouched");
        assert_eq!(served[2], internal[2], "opaque entry untouched");
    }
}
