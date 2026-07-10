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

use dasein_engine::chunking::{
    accumulated_chunks, chunk_observation, Chunk, ChunkMode, DEFAULT_WIN,
};
use dasein_engine::embed::Embedder;
use dasein_engine::features::{node_struct_with_type, supersession_edges};
use dasein_engine::freeze::{BirthQuery, ScoreError};
use dasein_engine::messages::{actions, assistant_chunks_of, reasoning_chunks_of, steps_of};
use dasein_engine::pystr::{char_len, char_prefix, py_json_dumps_opts, py_strip};
use dasein_engine::readout::{decided_struct, Changeprone, ReadoutCtx};

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

/// Populate `cache` for every non-empty text not yet present (dedup,
/// first-occurrence order), through ONE embedder batch. The clip must have
/// happened BEFORE this call — cache keys are the exact embedded strings,
/// like the reference's exact-text cache (curator L281-286).
fn embed_into_cache(
    embedder: &dyn Embedder,
    cache: &mut HashMap<String, Vec<f32>>,
    texts: &[&str],
) -> Result<(), ScoreError> {
    let mut miss: Vec<&str> = Vec::new();
    for &t in texts {
        if !t.is_empty() && !cache.contains_key(t) && !miss.contains(&t) {
            miss.push(t);
        }
    }
    if miss.is_empty() {
        return Ok(());
    }
    let vecs = embedder
        .embed(&miss)
        .map_err(|e| ScoreError(format!("client embed: {e}")))?;
    if vecs.len() != miss.len() {
        return Err(ScoreError(format!(
            "client embed returned {} vectors for {} texts",
            vecs.len(),
            miss.len()
        )));
    }
    for (t, v) in miss.into_iter().zip(vecs) {
        cache.insert(t.to_string(), v);
    }
    Ok(())
}

/// cache[text], or the zero vector for "" (dev: `ace/hde = cache[t] if t else
/// zed`; the embedder is never called on the empty string).
fn vec_or_zeros(cache: &HashMap<String, Vec<f32>>, text: &str, dim: usize) -> Vec<f32> {
    if text.is_empty() {
        vec![0.0; dim]
    } else {
        cache.get(text).cloned().unwrap_or_else(|| vec![0.0; dim])
    }
}

/// One V1Node per chunk + the aligned content-embedding matrix (the dupcos
/// input). `struct` rides as the engine's f64 rows — the server's single
/// np.float32 cast is the same one rounding the dev path applies.
fn v1_nodes(
    chunks: &[Chunk],
    embedder: &dyn Embedder,
    cache: &mut HashMap<String, Vec<f32>>,
    salt: &str,
) -> Result<(Vec<Value>, Vec<Vec<f32>>), ScoreError> {
    let dim = embedder.dim();
    let texts: Vec<String> = chunks
        .iter()
        .map(|c| char_prefix(&c.text, 2000).to_string())
        .collect();
    let heads: Vec<&str> = chunks.iter().map(|c| char_prefix(&c.head, 240)).collect();
    let mut to_embed: Vec<&str> = texts.iter().map(String::as_str).collect();
    to_embed.extend(chunks.iter().map(|c| c.cmd.as_str())); // cmd UNTRUNCATED
    to_embed.extend(heads.iter().copied());
    embed_into_cache(embedder, cache, &to_embed)?;
    let ns = node_struct_with_type(chunks);
    let mut nodes = Vec::with_capacity(chunks.len());
    let mut content = Vec::with_capacity(chunks.len());
    for (i, c) in chunks.iter().enumerate() {
        let emb_text = vec_or_zeros(cache, &texts[i], dim);
        // Python truthiness: empty-string basename is falsy -> null file_id.
        let file = c.file.as_deref().filter(|f| !f.is_empty());
        nodes.push(json!({
            "emb_text": emb_text,
            "emb_cmd": vec_or_zeros(cache, &c.cmd, dim),
            "emb_head": vec_or_zeros(cache, heads[i], dim),
            "struct": ns[i].to_vec(),
            "step": c.step,
            "kind": c.kind,
            "tokens": c.tokens,
            "file_id": file.map(|f| hid(salt, f)),
            "lo": c.lo,
            "hi": c.hi,
            "cmd_id": (!c.cmd.is_empty()).then(|| hid(salt, &c.cmd)),
            "head_id": (!heads[i].is_empty()).then(|| hid(salt, heads[i])),
        }));
        content.push(emb_text);
    }
    Ok((nodes, content))
}

fn embed_one(
    embedder: &dyn Embedder,
    cache: &mut HashMap<String, Vec<f32>>,
    text: &str,
) -> Result<Vec<f32>, ScoreError> {
    embed_into_cache(embedder, cache, &[text])?;
    Ok(vec_or_zeros(cache, text, embedder.dim()))
}

/// POST /v1/score/trace body for one BirthQuery — the v1 twin of the dev
/// contract's (messages, live_gi, mask) triple. `conv_salt` scopes the opaque
/// ids; `checkpoint_id` is the /v1/bundle handshake result (the §8.2
/// matched-pair guard rides in the payload).
#[allow(clippy::too_many_arguments)]
pub fn build_v1_trace_payload(
    q: &BirthQuery,
    embedder: &dyn Embedder,
    embed_cache: &mut HashMap<String, Vec<f32>>,
    changeprone: Option<&Changeprone>,
    conv_salt: &str,
    conv_id: &str,
    checkpoint_id: &str,
    target_cov: &str,
) -> Result<Value, ScoreError> {
    let (nodes, content_embs) = v1_nodes(q.live, embedder, embed_cache, conv_salt)?;
    // task_text is already [:2000] (freeze parse); sys from the internal view.
    let task_emb = embed_one(embedder, embed_cache, &q.task_text)?;
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
        content_embs: Some(&content_embs),
    });
    let sup: Vec<[usize; 2]> = supersession_edges(q.live)
        .into_iter()
        .map(|(a, b)| [a, b])
        .collect();
    let mut payload = json!({
        "contract": "brain-api/v1",
        "conv_id": conv_id,
        "checkpoint_id": checkpoint_id,
        "cur_step": q.cur_step,
        "nodes": nodes,
        "task_emb": task_emb,
        "mask": q.mask,
        "decided_struct": ds.iter().map(|row| row.to_vec()).collect::<Vec<_>>(),
        "edges_supersession": sup,
        "target_cov": target_cov,
    });
    if !sys_text.is_empty() {
        payload["sys_emb"] = json!(embed_one(embedder, embed_cache, &sys_text)?);
    }
    Ok(payload)
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
pub fn build_v1_tools_payload(
    internal: &[Value],
    tools: &Value,
    embedder: &dyn Embedder,
    embed_cache: &mut HashMap<String, Vec<f32>>,
    conv_salt: &str,
    conv_id: &str,
    checkpoint_id: &str,
) -> Result<Option<Value>, ScoreError> {
    let tool_nodes = tool_schema_chunks(tools);
    if tool_nodes.is_empty() {
        return Ok(None); // build_tool_spec returns None -> full roster
    }
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
        // task text chunked as an observation (the DASEIN-PATCH the brain's
        // vendored build_tool_spec carries).
        chunks = chunk_observation("", &task_text, 0, DEFAULT_WIN, Some(10), ChunkMode::Fixed);
    }
    if chunks.is_empty() {
        return Ok(None); // nothing chunkable (empty task): dev assemble would fail -> fail-open
    }
    let (nodes, _content) = v1_nodes(&chunks, embedder, embed_cache, conv_salt)?;
    let task_emb = embed_one(embedder, embed_cache, &task_text)?;
    let sys_text = sys_text_of(internal);
    let tool_texts: Vec<&str> = tool_nodes.iter().map(|tn| tn.text.as_str()).collect();
    embed_into_cache(embedder, embed_cache, &tool_texts)?; // UNTRUNCATED schema text
    let tools_json: Vec<Value> = tool_nodes
        .iter()
        .map(|tn| {
            json!({
                "name": tn.name,               // harness identifier — rides deliberately
                "emb": vec_or_zeros(embed_cache, &tn.text, embedder.dim()),
                "tokens": tn.tokens,
            })
        })
        .collect();
    let mut payload = json!({
        "contract": "brain-api/v1",
        "conv_id": conv_id,
        "checkpoint_id": checkpoint_id,
        "nodes": nodes,
        "task_emb": task_emb,
        "tools": tools_json,
    });
    if !sys_text.is_empty() {
        payload["sys_emb"] = json!(embed_one(embedder, embed_cache, &sys_text)?);
    }
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dasein_engine::embed::HashEmbedder;

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

    #[test]
    fn v1_nodes_ids_and_zero_vectors() {
        let mut chunks = vec![
            Chunk::new("text a", Some("a.py".into()), Some(1), Some(3), 1, "read"),
            Chunk::new("no file", Some("".into()), None, None, 1, "grep"),
        ];
        chunks[0].cmd = "cat a.py".into();
        chunks[0].head = "returncode: 0".into();
        let e = HashEmbedder::new(16);
        let mut cache = HashMap::new();
        let (nodes, content) = v1_nodes(&chunks, &e, &mut cache, "salt").unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(content.len(), 2);
        assert_eq!(nodes[0]["file_id"].as_str().unwrap().len(), 16);
        assert!(nodes[0]["cmd_id"].is_string() && nodes[0]["head_id"].is_string());
        // empty-string basename is Python-falsy -> null id; no cmd/head -> null
        assert!(nodes[1]["file_id"].is_null());
        assert!(nodes[1]["cmd_id"].is_null() && nodes[1]["head_id"].is_null());
        // chunk 1 has no cmd: its emb_cmd is the zero vector, NOT hash("")
        let z: Vec<f64> = nodes[1]["emb_cmd"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        assert!(z.iter().all(|&x| x == 0.0));
        // struct rows are 21 wide, markers are NOT sent
        assert_eq!(nodes[0]["struct"].as_array().unwrap().len(), 21);
        assert!(nodes[0].get("markers").is_none());
    }
}
