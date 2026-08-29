//! Anthropic wire → internal projection — port of `to_internal`
//! (service/anthropic_shapes.py:130-163) plus the bash-twin projections it
//! feeds from (adaptive_context/eval/trace_contract.py:136-217).
//!
//! The internal list is a read-only VIEW for curate/govern/measure — fold-back
//! (`splice::apply_curation`) re-reads the ORIGINAL body, so nothing here is
//! ever serialized back to the wire. Assistant `tool_use` blocks are surfaced
//! twice: as OpenAI-shaped `tool_calls` and as `extra.actions` carrying a
//! synthesized shell `command` (the bash twin — Read→cat/sed, Grep→grep,
//! Bash→literal), so chunk typing and the no-reread detector see the exact
//! action vocabulary the GNN was trained on. Thinking/image/document blocks
//! are invisible to this view (they ride the wire verbatim through fold-back).
//! Parity: tests/parity_internal.rs replays fixtures generated from the
//! Python reference (parity/gen_internal_fixtures.py).

use serde_json::{json, Map, Value};

use parsec_engine::pystr::{
    char_len, char_prefix, py_float_repr, py_json_dumps, py_json_dumps_opts, py_strip,
};

use crate::splice::{content_text, py_truthy, system_to_text};

// Tool-name sets (trace_contract.py:130-132, 180-184); membership is over the
// stripped, lowercased name.
const PRJ_SHELL: &[&str] = &[
    "bash",
    "shell",
    "run",
    "execute",
    "bashoutput",
    "bash_output",
];
const PRJ_READ: &[&str] = &[
    "read",
    "notebookread",
    "notebook_read",
    "view",
    "cat",
    "openfile",
    "open_file",
];
const PRJ_EDIT: &[&str] = &[
    "edit",
    "multiedit",
    "write",
    "str_replace",
    "str_replace_editor",
    "str_replace_based_edit_tool",
    "notebookedit",
    "notebook_edit",
    "create",
    "createfile",
];
const PRJ_GREP: &[&str] = &[
    "grep",
    "glob",
    "search",
    "search_code",
    "searchcode",
    "find",
    "ripgrep",
    "rg",
];
const SHELL_TOOLS: &[&str] = &["bash", "shell", "run", "execute"];
const QUERY_KEYS: &[&str] = &["query", "pattern", "q", "search"];

/// Which entries of [`to_internal`]'s output are PROTECTED — human-authored
/// text or the agent's own prose — and must reach the model byte-for-byte
/// (`protect::restore_protected`). Aligned to that output index-for-index,
/// system entry included.
///
/// On this wire a `role: "user"` message is one of two very different things:
/// a `tool_result` carrier (the observation the curator exists to digest) or
/// a human turn. The discriminator is the presence of a `tool_result` block —
/// content that is a bare string, or a block list with no `tool_result` in
/// it, is the user talking. A MIXED message (tool_result plus an appended
/// `text` block, which is how Claude Code attaches system-reminders) stays
/// curatable: it is an observation with a note stapled on, and protecting the
/// whole thing would forfeit the main win on this wire.
///
/// Assistant prose is protected too (product decision 2026-08-29): the
/// agent's responses are served verbatim, leaving observations as the
/// cuttable mass. The restore only touches `content`, and the guard runs at
/// fold-back, so chunking/scoring/checksums are untouched — brain parity
/// holds and the freezer still scores assistant chunks; their cuts simply
/// count as refused. Reasoning stays curatable. Everything unrecognized is
/// unprotected because it is also unchunked: `parse` only ever cuts
/// user/tool/assistant.
pub fn protected_mask(body: &Value) -> Vec<bool> {
    let mut mask: Vec<bool> = Vec::new();
    if !system_to_text(body.get("system")).is_empty() {
        mask.push(false); // system is never chunked, so never restored
    }
    let src = body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for m in src {
        // to_internal defaults a MISSING role to "user"; mirror that exactly
        // so the mask and the view can never disagree about an entry.
        let is_user = match m.get("role") {
            None => true,
            Some(r) => r.as_str() == Some("user"),
        };
        let is_assistant = m.get("role").and_then(Value::as_str) == Some("assistant");
        let carries_tool_result =
            m.get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                });
        mask.push(is_assistant || (is_user && !carries_tool_result));
    }
    mask
}

/// anthropic_shapes.to_internal: adapt an inbound Anthropic Messages body to
/// the internal flat message list — optional `{"role":"system"}` entry first
/// (only when the flattened system text is non-empty), then one entry per
/// message, same order, `{"role", "content": text}` with `tool_calls` +
/// `extra.actions` on assistant messages that carry `tool_use` blocks.
pub fn to_internal(body: &Value) -> Vec<Value> {
    let mut msgs: Vec<Value> = Vec::new();
    let sys_text = system_to_text(body.get("system"));
    if !sys_text.is_empty() {
        msgs.push(json!({"role": "system", "content": sys_text}));
    }
    let src = body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for m in src {
        let mo = m.as_object();
        // m.get("role", "user") — a PRESENT key passes through whatever value
        // it holds (incl. null); only a MISSING key defaults.
        let role = mo
            .and_then(|o| o.get("role"))
            .cloned()
            .unwrap_or_else(|| Value::String("user".into()));
        let content = mo.and_then(|o| o.get("content"));
        let mut im = Map::new();
        im.insert("role".into(), role.clone());
        im.insert("content".into(), Value::String(content_text(content)));
        if role.as_str() == Some("assistant") {
            let tus = tool_use_blocks(content);
            if !tus.is_empty() {
                let tool_calls: Vec<Value> = tus
                    .iter()
                    .map(|b| {
                        // json.dumps(b.get("input") or {}) — falsy input
                        // (missing, null, "", 0, [], {}) collapses to "{}".
                        let arguments = match b.get("input") {
                            Some(v) if py_truthy(v) => py_json_dumps(v),
                            _ => "{}".to_string(),
                        };
                        json!({
                            "id": b.get("id").cloned().unwrap_or(Value::Null),
                            "type": "function",
                            "function": {
                                "name": b.get("name").cloned().unwrap_or(Value::Null),
                                "arguments": arguments,
                            },
                        })
                    })
                    .collect();
                im.insert("tool_calls".into(), Value::Array(tool_calls));
                im.insert(
                    "extra".into(),
                    json!({"actions": actions_from_tool_use(content)}),
                );
            }
        }
        msgs.push(Value::Object(im));
    }
    msgs
}

/// anthropic_shapes._tool_use_blocks: the `tool_use` dict blocks of a content
/// list (an assistant message's actions this turn).
fn tool_use_blocks(content: Option<&Value>) -> Vec<&Map<String, Value>> {
    let Some(Value::Array(blocks)) = content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(Value::as_object)
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .collect()
}

/// anthropic_shapes._actions_from_tool_use: each tool_use block becomes
/// `{"tool", "name", **input, "command", "query"?}`. Order is load-bearing:
/// the input splat can OVERRIDE tool/name (keeping their slot positions), and
/// an input "command" key is subsequently overwritten by the twin/derived
/// command — command is ALWAYS set; query only when non-empty.
pub(crate) fn actions_from_tool_use(content: Option<&Value>) -> Vec<Value> {
    let empty = Value::Object(Map::new());
    let mut out: Vec<Value> = Vec::new();
    for b in tool_use_blocks(content) {
        let name = py_strip(b.get("name").and_then(Value::as_str).unwrap_or("")).to_string();
        let args = b.get("input");
        // a = args if isinstance(args, dict) else {} — every projection below
        // receives the coerced dict, so a non-dict input never reaches
        // derive_command's str(args) branch from here.
        let a: &Value = match args {
            Some(v) if v.is_object() => v,
            _ => &empty,
        };
        let mut act = Map::new();
        act.insert("tool".into(), Value::String(name.clone()));
        act.insert("name".into(), Value::String(name.clone()));
        if let Some(obj) = a.as_object() {
            for (k, v) in obj {
                act.insert(k.clone(), v.clone());
            }
        }
        let twin = bash_twin_command(&name, a).filter(|t| !t.is_empty());
        let command = twin.unwrap_or_else(|| derive_command(&name, a));
        act.insert("command".into(), Value::String(command));
        let q = derive_query(&name, a);
        if !q.is_empty() {
            act.insert("query".into(), Value::String(q));
        }
        out.push(Value::Object(act));
    }
    out
}

/// trace_contract.bash_twin_command: one structured tool call → a
/// bash-equivalent command string (or None), so the chunk/label pipeline
/// types CC reads/greps exactly as it does for shell commands. Non-dict args
/// are treated as {}.
pub fn bash_twin_command(tool: &str, args: &Value) -> Option<String> {
    let name = py_strip(tool).to_lowercase();
    let empty = Map::new();
    let args = args.as_object().unwrap_or(&empty);
    if PRJ_SHELL.contains(&name.as_str()) {
        return match args.get("command") {
            Some(Value::String(c)) if !py_strip(c).is_empty() => Some(c.clone()),
            _ => None,
        };
    }
    // First TRUTHY value wins regardless of type; the str check comes after
    // (a truthy non-str file_path masks a valid path key).
    let f = ["file_path", "path", "filename", "file"]
        .iter()
        .filter_map(|k| args.get(*k))
        .find(|v| py_truthy(v));
    let fstr = f.and_then(Value::as_str).filter(|s| !s.is_empty());
    if PRJ_READ.contains(&name.as_str()) {
        if let Some(f) = fstr {
            // int() failure on EITHER offset or limit nulls BOTH.
            let coerce = |k: &str| -> Result<Option<i64>, ()> {
                match args.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(v) => py_int(v).map(Some),
                }
            };
            let (off, lim) = match (coerce("offset"), coerce("limit")) {
                (Ok(o), Ok(l)) => (o, l),
                _ => (None, None),
            };
            // Python truthiness: offset=0 or limit=0 falls through.
            let off = off.filter(|o| *o != 0);
            let lim = lim.filter(|l| *l != 0);
            return Some(match (off, lim) {
                (Some(o), Some(l)) => {
                    format!("sed -n '{},{}p' {}", o, o + std::cmp::max(1, l) - 1, f)
                }
                (Some(o), None) => format!("sed -n '{},{}p' {}", o, o + 1999, f),
                _ => format!("cat {}", f),
            });
        }
    }
    if PRJ_EDIT.contains(&name.as_str()) {
        if let Some(f) = fstr {
            // basename only; deliberately no read/grep keyword so chunking
            // types it "other".
            return Some(format!("edit {}", f.rsplit('/').next().unwrap_or(f)));
        }
    }
    if PRJ_GREP.contains(&name.as_str()) {
        let pat = ["pattern", "query", "q"]
            .iter()
            .filter_map(|k| args.get(*k))
            .find(|v| py_truthy(v))
            .map(py_str_value)
            .unwrap_or_default();
        // str(...)[:200].replace("'", " ") — clip BEFORE the quote swap.
        let pat = char_prefix(&pat, 200).replace('\'', " ");
        let path = ["path", "glob", "include"]
            .iter()
            .filter_map(|k| args.get(*k))
            .find(|v| py_truthy(v))
            .map(py_str_value)
            .unwrap_or_default();
        return Some(py_strip(&format!("grep '{}' {}", pat, path)).to_string());
    }
    None
}

/// trace_contract.derive_command: shell tools yield the literal command;
/// everything else a deterministic `"<Tool> k=v …"` render — sorted keys,
/// values char-clipped at 160 with U+2026, whole string stripped then
/// char-clipped at 400. The tool name keeps its ORIGINAL case here (only set
/// membership lowercases).
pub fn derive_command(tool: &str, args: &Value) -> String {
    let Some(map) = args.as_object() else {
        return char_prefix(&py_str_value(args), 300).to_string();
    };
    if SHELL_TOOLS.contains(&tool.to_lowercase().as_str()) {
        if let Some(Value::String(c)) = map.get("command") {
            return c.clone();
        }
    }
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_unstable();
    let mut parts: Vec<String> = Vec::with_capacity(keys.len());
    for k in keys {
        let v = &map[k.as_str()];
        let sv = match v {
            Value::String(s) => s.clone(),
            other => py_json_dumps_opts(other, false, false),
        };
        let sv = if char_len(&sv) > 160 {
            format!("{}\u{2026}", char_prefix(&sv, 160))
        } else {
            sv
        };
        parts.push(format!("{}={}", k, sv));
    }
    let full = format!("{} {}", tool, parts.join(" "));
    char_prefix(py_strip(&full), 400).to_string()
}

/// trace_contract.derive_query: the first of query/pattern/q/search whose
/// value is a str with non-empty strip — returned UNCLIPPED and unstripped.
pub fn derive_query(_tool: &str, args: &Value) -> String {
    let Some(map) = args.as_object() else {
        return String::new();
    };
    for k in QUERY_KEYS {
        if let Some(Value::String(v)) = map.get(*k) {
            if !py_strip(v).is_empty() {
                return v.clone();
            }
        }
    }
    String::new()
}

/// Python `int(x)` over a JSON-decoded value; Err is the TypeError/ValueError
/// branch trace_contract catches. bool→0/1, float truncates toward zero, str
/// parses as a Python int literal (whitespace stripped, one sign, '_' only
/// between ASCII digits). Deviations unreachable from fixtures: unicode
/// digits rejected; int(±inf) — an UNCAUGHT OverflowError in the reference —
/// maps to Err instead of a crash.
fn py_int(v: &Value) -> Result<i64, ()> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f > i64::MIN as f64 && f < i64::MAX as f64 {
                    Ok(f.trunc() as i64)
                } else {
                    Err(())
                }
            } else {
                Err(())
            }
        }
        Value::String(s) => py_int_str(s),
        _ => Err(()),
    }
}

fn py_int_str(s: &str) -> Result<i64, ()> {
    let t = py_strip(s);
    let (neg, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return Err(());
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    let v: i64 = clean.parse().map_err(|_| ())?;
    Ok(if neg { -v } else { v })
}

/// Python `str(x)` of a JSON-decoded value — feeds the grep pattern/path
/// projection and derive_command's non-dict branch, so the bytes must match
/// CPython: str passes through, everything else renders as repr.
fn py_str_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr_value(other),
    }
}

/// Python `repr(x)` for the JSON-representable types: None/True/False,
/// numbers via CPython repr, single-quoted strings, list/dict with ", "
/// separators and insertion-ordered keys.
fn py_repr_value(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                n.to_string()
            } else {
                py_float_repr(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => py_str_repr(s),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(py_repr_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, val)| format!("{}: {}", py_str_repr(k), py_repr_value(val)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// CPython `repr(str)`: single quotes unless the string contains ' and not ",
/// \\ \n \r \t escapes, non-printable chars as \xXX/\uXXXX/\UXXXXXXXX.
fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if py_repr_printable(c) => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xff {
                    out.push_str(&format!("\\x{:02x}", cp));
                } else if cp <= 0xffff {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    out.push_str(&format!("\\U{:08x}", cp));
                }
            }
        }
    }
    out.push(quote);
    out
}

/// str.isprintable() over the code points that occur in tool args: false for
/// Cc plus the assigned Cf/Zs/Zl/Zp ranges (space itself is printable).
/// Unassigned/private-use planes are not enumerated — parity fixtures pin the
/// covered set.
fn py_repr_printable(c: char) -> bool {
    match c {
        ' ' => true,
        c if (c as u32) < 0x20 || c == '\x7f' => false,
        '\u{80}'..='\u{a0}' => false,
        '\u{ad}' => false,
        '\u{1680}' => false,
        '\u{2000}'..='\u{200f}' => false,
        '\u{2028}'..='\u{202f}' => false,
        '\u{205f}'..='\u{2064}' => false,
        '\u{206a}'..='\u{206f}' => false,
        '\u{3000}' => false,
        '\u{feff}' => false,
        '\u{fff9}'..='\u{fffb}' => false,
        _ => true,
    }
}
