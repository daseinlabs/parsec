//! brain-api/v1 payload builder — CLIENT-SIDE featurization, where the text
//! lives (contracts/schemas/brain-api.schema.json; graph-construction.md §1-2).
//!
//! The wire carries vectors + structural features + salted opaque ids ONLY;
//! nothing leaving this module can represent raw text. Everything here must
//! reproduce the DEV brain's derivations bit-for-bit (the parity gate,
//! scripts/parity_v1.sh, asserts the two contracts forward byte-identical
//! turns):
//!
//!   dev brain (scorer.py)                      here
//!   ─────────────────────                      ────
//!   emb of c.text[:2000] / cmd / head[:240]    same clips (pystr char slices)
//!                                              BEFORE embedding, dedup cache
//!   node_struct_with_type(chunks) (n,21)       engine::features (f32 parity)
//!   markers [is_reasoning, has_head]           NOT sent — the server derives
//!                                              them from kind / head_id
//!   edges() rel 0/1/2 (id equality + lo)       server-side from ids
//!   edges() rel 4 (the text-dependent one)     engine supersession_edges
//!   _het_readout 49-col decided_struct         engine::readout (f32 parity)
//!   changeprone.pkl sidecar (col 42)           the committed engine fixture
//!                                              (verified equal to the pkl)
//!
//! Replay-purity note: the payload is a pure function of (BirthQuery,
//! embedder, checkpoint). For a REPLAYED step s (q.cur_step == s) we take
//! t/T/recent from the query — i.e. from the step being decided — while the
//! dev brain derives them from the FULL message list it re-parses. The two
//! agree whenever the replayed step IS the newest one (every call of the
//! sequential golden replay); they diverge only when several steps replay in
//! one serve (multi-step turns, post-fail-open retries), where the v1 values
//! are the prefix-pure ones the freeze design intends.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use parsec_engine::chunking::{
    accumulated_chunks, chunk_observation, Chunk, ChunkMode, DEFAULT_WIN,
};
use parsec_engine::features::{node_struct_with_type, supersession_edges};
use parsec_engine::freeze::BirthQuery;
use parsec_engine::messages::{actions, assistant_chunks_of, reasoning_chunks_of, steps_of};
use parsec_engine::pystr::{char_len, char_prefix, py_json_dumps_opts, py_strip};
use parsec_engine::readout::{decided_struct, Changeprone, ReadoutCtx};

/// AC_CHANGEPRONE sidecar, compiled in from the committed engine parity
/// fixture (scripts/changeprone_to_json.py over models/changeprone.pkl —
/// verified equal to the pickle the brain's dev path loads, so col 42 agrees
/// across contracts). Parse failure -> None -> zero-filled col (the trained
/// missing-value convention), never a hard error.
static CHANGEPRONE: LazyLock<Option<Changeprone>> = LazyLock::new(|| {
    Changeprone::from_json(include_str!(
        "../../engine/parity/fixtures/changeprone.json"
    ))
    .map_err(|e| tracing::warn!("changeprone sidecar unparseable ({e}) — col 42 zero-filled"))
    .ok()
});

pub fn changeprone() -> Option<&'static Changeprone> {
    CHANGEPRONE.as_ref()
}

/// Salted 16-hex identity id: only equality/truthiness ride the wire; the
/// salt stays on the client, so the server cannot dictionary-reverse the
/// basename/command the id names.
fn hid(salt: &str, value: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(value.as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

/// curator._text / scorer._text: the internal flat view's message text.
fn m_text(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| {
                p.as_object()
                    .map(|o| o.get("text").and_then(Value::as_str).unwrap_or(""))
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// parse_internal's sys_text: first system entry with non-blank text, [:2000].
fn sys_text_of(messages: &[Value]) -> String {
    for m in messages {
        if m.get("role").and_then(Value::as_str) == Some("system") {
            let t = m_text(m);
            if !py_strip(&t).is_empty() {
                return char_prefix(&t, 2000).to_string();
            }
        }
    }
    String::new()
}

/// parse_internal's het_steps: (cmd, obs_text) per STEP INDEX (index 0 is the
/// task turn — always ("", "")), length cur_step+1. This is NOT steps_of():
/// the step counter advances on EVERY user/tool message (the task included),
/// and last_cmd persists across consecutive observations — byte-mirror of
/// scorer.parse_internal so the rerank block (readout cols 29-36) sees the
/// exact steps the dev brain sees.
fn het_steps(messages: &[Value], cur_step: i64) -> Vec<(String, String)> {
    let n = usize::try_from(cur_step + 1).unwrap_or(0);
    let mut out = vec![(String::new(), String::new()); n];
    let mut last_cmd = String::new();
    let mut step: i64 = 0;
    let mut seen_first_obs = false;
    for m in messages {
        match m.get("role").and_then(Value::as_str) {
            Some("assistant") => last_cmd = actions(m).join(" ; "),
            Some("user") | Some("tool") => {
                if seen_first_obs {
                    if (0..=cur_step).contains(&step) {
                        out[step as usize] = (last_cmd.clone(), m_text(m));
                    }
                } else {
                    seen_first_obs = true;
                }
                step += 1;
            }
            _ => {}
        }
    }
    out
}

// ── tool-schema featurization (trace_contract.tool_schema_chunks port) ──────

struct ToolNode {
    name: String,
    text: String,
    tokens: i64,
}

fn first_truthy<'a>(candidates: &[Option<&'a Value>]) -> Option<&'a Value> {
    candidates
        .iter()
        .copied()
        .flatten()
        .find(|v| crate::splice::py_truthy(v))
}

/// trace_contract.normalize_tool_def + tool_schema_chunks (L224-264): per
/// tool `{name, description, parameters}` serialized with json.dumps
/// (ensure_ascii=False, sort_keys=True) — the EXACT text the reference
/// embeds; tokens = chars//4; de-duped by name LAST-WINS at the name's first
/// position (Python dict update order).
fn tool_schema_chunks(tools: &Value) -> Vec<ToolNode> {
    let mut out: Vec<ToolNode> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for t in tools.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if !t.is_object() {
            continue;
        }
        let f = t.get("function").filter(|v| v.is_object()).unwrap_or(t);
        let Some(name) = first_truthy(&[f.get("name"), t.get("name")])
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let desc = first_truthy(&[f.get("description"), t.get("description")])
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        let params = first_truthy(&[
            f.get("parameters"),
            f.get("input_schema"),
            t.get("input_schema"),
            t.get("parameters"),
        ])
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
        let text = py_json_dumps_opts(
            &json!({"name": name, "description": desc, "parameters": params}),
            true,  // sort_keys
            false, // ensure_ascii=False
        );
        let node = ToolNode {
            name: name.to_string(),
            tokens: char_len(&text) as i64 / 4,
            text,
        };
        match by_name.get(name) {
            Some(&k) => out[k] = node, // last wins, first position kept
            None => {
                by_name.insert(name.to_string(), out.len());
                out.push(node);
            }
        }
    }
    out
}

/// build_tool_spec's task_text: first non-blank USER message (role "user"
/// only — unlike parse_internal's first user/tool), [:2000].
fn tool_task_text(messages: &[Value]) -> String {
    for m in messages {
        if m.get("role").and_then(Value::as_str) == Some("user") {
            let t = m_text(m);
            if !py_strip(&t).is_empty() {
                return char_prefix(&t, 2000).to_string();
            }
        }
    }
    String::new()
}

/// POST /v1/score/tools body: the tool-spec chunk pipeline view
/// (build_tool_spec, trace_graph.py L117-152 — steps_of pairing, step-0
/// task-chunk fallback) + the deduped roster embedded client-side.
/// Ok(None) = ineligible (no schemas / no chunkable view) — the caller
/// serves the FULL roster, the dev fail-open.
/// The tool-spec chunk pipeline view (build_tool_spec, trace_graph.py
/// L117-152) + its task text. `None` = nothing chunkable, the caller fails
/// open to the full roster. Shared by the v1 and v2 tool payload builders so
/// the two contracts can never drift on the pipeline itself.
fn tool_spec_view(internal: &[Value]) -> Option<(Vec<Chunk>, String)> {
    // The SAME chunk pipeline build_trace_graph uses; sort key is step ONLY
    // (stable: obs, then assistant, then reasoning within a step — the
    // vendored build_tool_spec's exact order, NOT parse_internal's
    // reasoning-last key).
    let steps = steps_of(internal);
    let t_last = steps.len().max(1) - 1;
    let mut chunks = accumulated_chunks(&steps, t_last, Some(10), ChunkMode::Fixed);
    chunks.extend(
        assistant_chunks_of(internal)
            .into_iter()
            .filter(|c| c.step <= t_last as i64),
    );
    chunks.extend(
        reasoning_chunks_of(internal)
            .into_iter()
            .filter(|c| c.step <= t_last as i64),
    );
    chunks.sort_by_key(|c| c.step);
    let task_text = tool_task_text(internal);
    if chunks.is_empty() {
        // step-0 fallback: no observations yet — condition the head on the
        // task text chunked as an observation (the PARSEC-PATCH the brain's
        // vendored build_tool_spec carries).
        chunks = chunk_observation("", &task_text, 0, DEFAULT_WIN, Some(10), ChunkMode::Fixed);
    }
    if chunks.is_empty() {
        return None; // nothing chunkable (empty task): dev assemble would fail -> fail-open
    }
    Some((chunks, task_text))
}

// ── brain-api/v2: text out, the server embeds ──────────────────────────────
// v2 is v1 with the three embedding slots carrying text (contracts/schemas/
// brain-api-v2.schema.json). NO embedder runs here — that is the entire point
// of the contract (docs/server-side-embedding.md). Structural featurization
// stays: the freezer already owns the chunk set, so node_struct_with_type,
// the readout, and the supersession pairs cost nothing extra.

/// One v2 node per chunk. Field-for-field `v1_nodes` with text where the
/// vectors were — same clips (text 2000, head 240, cmd UNTRUNCATED, exactly
/// what v1 fed its embedder) and the same salted ids, so the graph the brain
/// rebuilds is bit-identical to the v1 one.
fn v2_nodes(chunks: &[Chunk], salt: &str) -> Vec<Value> {
    let ns = node_struct_with_type(chunks);
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let head = char_prefix(&c.head, 240);
            // Python truthiness: empty-string basename is falsy -> null file_id.
            let file = c.file.as_deref().filter(|f| !f.is_empty());
            json!({
                "text": char_prefix(&c.text, 2000),
                "cmd": c.cmd,                       // UNTRUNCATED, as v1 embedded it
                "head": head,
                "struct": ns[i].to_vec(),
                "step": c.step,
                "kind": c.kind,
                "tokens": c.tokens,
                "file_id": file.map(|f| hid(salt, f)),
                "lo": c.lo,
                "hi": c.hi,
                "cmd_id": (!c.cmd.is_empty()).then(|| hid(salt, &c.cmd)),
                "head_id": (!head.is_empty()).then(|| hid(salt, head)),
            })
        })
        .collect()
}

/// POST /v1/score/trace body on v2. Infallible: nothing here can fail the way
/// an embedder call can, so the birth step can no longer fail open on a local
/// embed error — one whole class of fail-open disappears with the embedder.
///
/// `content_embs: None` is deliberate and load-bearing: it zeroes the readout's
/// trailing AC_DUPCOS pair (cols 47-48), which is the ONE block a client
/// without an embedder cannot compute. The brain refills it from its own
/// content embeddings (`_v2_fill_dupcos`), and the cross-contract parity gate
/// in packages/brain/tests/test_v2.py proves the handoff.
pub fn build_v2_trace_payload(
    q: &BirthQuery,
    changeprone: Option<&Changeprone>,
    conv_salt: &str,
    conv_id: &str,
    checkpoint_id: &str,
    target_cov: &str,
) -> Value {
    let nodes = v2_nodes(q.live, conv_salt);
    let sys_text = sys_text_of(q.messages);
    let steps = het_steps(q.messages, q.cur_step);
    let alive: Vec<usize> = (0..q.live.len()).collect();
    let ds = decided_struct(&ReadoutCtx {
        chunks: q.live,
        alive: &alive,
        decided: &q.mask,
        task_text: &q.task_text,
        recent: &q.recent_cmds,
        steps: &steps,
        cur_step: q.cur_step,
        t_total: q.cur_step + 1,
        age: 0.0, // admission-at-birth — the only serve value
        changeprone,
        content_embs: None, // -> dupcos zeros; the SERVER fills them (see doc above)
    });
    let sup: Vec<[usize; 2]> = supersession_edges(q.live)
        .into_iter()
        .map(|(a, b)| [a, b])
        .collect();
    let mut payload = json!({
        "contract": "brain-api/v2",
        "conv_id": conv_id,
        "checkpoint_id": checkpoint_id,
        "cur_step": q.cur_step,
        "nodes": nodes,
        "task_text": q.task_text,
        "mask": q.mask,
        "decided_struct": ds.iter().map(|row| row.to_vec()).collect::<Vec<_>>(),
        "edges_supersession": sup,
        "target_cov": target_cov,
    });
    if !sys_text.is_empty() {
        payload["sys_text"] = json!(sys_text);
    }
    payload
}

/// POST /v1/score/tools body on v2. `None` = ineligible -> serve the FULL
/// roster (the dev fail-open), same as v1.
pub fn build_v2_tools_payload(
    internal: &[Value],
    tools: &Value,
    conv_salt: &str,
    conv_id: &str,
    checkpoint_id: &str,
) -> Option<Value> {
    let tool_nodes = tool_schema_chunks(tools);
    if tool_nodes.is_empty() {
        return None; // build_tool_spec returns None -> full roster
    }
    let (chunks, task_text) = tool_spec_view(internal)?;
    let sys_text = sys_text_of(internal);
    let tools_json: Vec<Value> = tool_nodes
        .iter()
        .map(|tn| {
            json!({
                "name": tn.name,          // harness identifier — rides deliberately
                "schema_text": tn.text,   // UNTRUNCATED serialized schema, as v1 embedded it
                "tokens": tn.tokens,
            })
        })
        .collect();
    let mut payload = json!({
        "contract": "brain-api/v2",
        "conv_id": conv_id,
        "checkpoint_id": checkpoint_id,
        "nodes": v2_nodes(&chunks, conv_salt),
        "task_text": task_text,
        "tools": tools_json,
    });
    if !sys_text.is_empty() {
        payload["sys_text"] = json!(sys_text);
    }
    Some(payload)
}

/// The v2 rules body: the trace payload MINUS mask/decided_struct/target_cov
/// (the rule head never reads them and the served model is extra="forbid", so
/// sending them is a 422) and over the TOOL-SPEC chunk view, not the live set.
pub fn build_v2_rules_payload(
    internal: &[Value],
    conv_salt: &str,
    conv_id: &str,
    checkpoint_id: &str,
    cur_step: i64,
) -> Option<Value> {
    let (chunks, task_text) = tool_spec_view(internal)?;
    let sys_text = sys_text_of(internal);
    let sup: Vec<[usize; 2]> = supersession_edges(&chunks)
        .into_iter()
        .map(|(a, b)| [a, b])
        .collect();
    let mut payload = json!({
        "contract": "brain-api/v2",
        "conv_id": conv_id,
        "checkpoint_id": checkpoint_id,
        "cur_step": cur_step,
        "nodes": v2_nodes(&chunks, conv_salt),
        "task_text": task_text,
        "edges_supersession": sup,
    });
    if !sys_text.is_empty() {
        payload["sys_text"] = json!(sys_text);
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_schema_chunks_matches_reference_semantics() {
        let tools = json!([
            {"name": "Read", "description": "read a file",
             "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}},
            // OpenAI shape
            {"type": "function", "function": {"name": "Bash", "description": "run",
             "parameters": {"type": "object"}}},
            // dup name: LAST WINS at the FIRST position
            {"name": "Read", "description": "read v2", "input_schema": {"type": "object"}},
            // no name: skipped
            {"description": "anonymous"},
            // non-dict params -> {}
            {"name": "Odd", "description": "d", "input_schema": "not-a-dict"},
        ]);
        let got = tool_schema_chunks(&tools);
        let names: Vec<&str> = got.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Read", "Bash", "Odd"]);
        // Python: json.dumps({...}, ensure_ascii=False, sort_keys=True)
        assert_eq!(
            got[0].text,
            r#"{"description": "read v2", "name": "Read", "parameters": {"type": "object"}}"#
        );
        assert_eq!(got[0].tokens, got[0].text.chars().count() as i64 / 4);
        assert_eq!(
            got[2].text,
            r#"{"description": "d", "name": "Odd", "parameters": {}}"#
        );
    }

    #[test]
    fn het_steps_mirrors_parse_internal() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "go",
                   "extra": {"actions": [{"command": "cat a.py"}]}}),
            json!({"role": "tool", "content": "obs one"}),
            // consecutive user message: same last_cmd persists (parse_internal
            // never resets it) — steps_of would SKIP this observation.
            json!({"role": "user", "content": "obs two"}),
        ];
        let steps = het_steps(&msgs, 2);
        assert_eq!(
            steps,
            vec![
                ("".into(), "".into()), // step 0 = the task turn
                ("cat a.py".into(), "obs one".into()),
                ("cat a.py".into(), "obs two".into()),
            ]
        );
    }
}
