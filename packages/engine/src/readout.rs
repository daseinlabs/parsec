//! The 49-col per-decided READOUT struct — the v1-contract tabular input to
//! the brain's read_head (graph-construction spec §1.3). Vectors + features
//! only: raw text never leaves this machine.
//!
//! Ports, in column order (references: adaptive-context-clean/adaptive_context/
//! optimizer/ + the rulehead training drop; the serving truth is the vendored
//! mirror `parsec_brain/scorer.py:_het_readout`, curator.py L563-636):
//!   0-15  `torch_curator.struct_features(alive, recent)` rows for decided
//!   16    age (0.0 admission-at-birth, 1.0 next-call re-decision)
//!   17-19 `torch_curator.decided_extra_feats` [rc0, task_overlap, dup_recency]
//!   20-23 AC_READMIT [t, t/max(T-1,1), resident_flag, n_resident]
//!         (trace_graph._readout L559-567)
//!   24-28 AC_ISSUEMATCH [contains_any, log1p(n_matches), defines_issue_symbol,
//!         coverage, file_in_issue_traceback] (trace_graph L590-610)
//!   29-36 AC_RERANK `torch_curator.decided_rerank_feats` (L119-158)
//!   37-41 AC_CENTRALITY `symbol_graph.decided_centrality_feats` — tree-sitter
//!         symbol graph + PageRank over the causal alive set
//!   42    AC_CHANGEPRONE cnt[file]/total (zeros when the sidecar is absent —
//!         the trained missing-value convention)
//!   43-46 AC_SCOUTFEAT — ZEROS in v1: there is no client-side scout record,
//!         and the serving pin AC_SCOUTFEAT="on" is no pickle path -> zeros
//!   47-48 AC_DUPCOS [max cosine vs EARLIER-alive content embs, 1/(1+Δstep)]
//!         (trace_train.py L87-100; zeros when embeddings are absent)
//!
//! Precision contract: the reference computes each block in Python float (f64)
//! and downcasts ONCE to float32 per np.hstack/np.asarray; we keep f64 per
//! element and downcast once at assembly — the same single rounding, asserted
//! bit-for-bit by tests/parity_readout.rs. Two documented exceptions:
//! - dupcos col 47 is an f32 pipeline in the reference (np.linalg.norm + BLAS
//!   matvec over float32), whose summation order is SIMD/machine-dependent —
//!   even two numpy hosts disagree in the last ulp. We use sequential f32
//!   accumulation; the parity test allows a tiny documented epsilon there.
//! - centrality PageRank sums follow Python set/dict iteration order, which is
//!   PYTHONHASHSEED-randomized — the reference itself is only f32-stable. We
//!   use first-seen insertion order (one valid instance of the same sums);
//!   the f32 downcast absorbs the f64 ulp-level ordering noise.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use crate::chunking::Chunk;
use crate::features::{spans, struct_features};
use crate::pystr::char_len;

/// read_head input width minus hidden — the ckpt-derived readout struct width.
pub const READ_STRUCT: usize = 49;

/// AC_CHANGEPRONE sidecar (models/changeprone.pkl -> committed JSON via
/// scripts/changeprone_to_json.py): per-basename corpus edit counts.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Changeprone {
    pub cnt: HashMap<String, i64>,
    pub total: i64,
}

impl Changeprone {
    /// Mirrors the reference load: total clamped >= 1 (curator.py L153).
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut cp: Changeprone = serde_json::from_str(json)?;
        cp.total = cp.total.max(1);
        Ok(cp)
    }
}

/// Inputs of one readout (curator._het_readout's arguments, plus the alive
/// projection the Freezer owns). `alive` are rows into `chunks` forming the
/// live set at the decision step (the serve path passes every row — the
/// reference's `alive = chunks`); `decided` are rows into `chunks` and must
/// each be a member of `alive` (the reference indexes sf by alive position).
pub struct ReadoutCtx<'a> {
    pub chunks: &'a [Chunk],
    pub alive: &'a [usize],
    pub decided: &'a [usize],
    /// First user message text [:2000] (curate L964).
    pub task_text: &'a str,
    /// Commands of the last two steps, " "-joined [:4000] (curate L965).
    pub recent: &'a str,
    /// (command, observation) per step index (curator._het_steps).
    pub steps: &'a [(String, String)],
    pub cur_step: i64,
    /// T; 0 falls back to maxstep+1 (curator._het_readout L578).
    pub t_total: i64,
    /// 0.0 admission-at-birth (the only serve value), 1.0 re-decision.
    pub age: f64,
    pub changeprone: Option<&'a Changeprone>,
    /// Content embeddings aligned to `chunks` rows (embedder over
    /// c.text[:2000]); None -> dupcos cols zero (fail-open).
    pub content_embs: Option<&'a [Vec<f32>]>,
}

/// The (n_decided, 49) readout struct, column map above.
pub fn decided_struct(ctx: &ReadoutCtx) -> Vec<[f32; READ_STRUCT]> {
    let identity =
        ctx.alive.len() == ctx.chunks.len() && ctx.alive.iter().enumerate().all(|(i, &r)| i == r);
    let owned: Vec<Chunk>;
    let alive: &[Chunk] = if identity {
        ctx.chunks
    } else {
        owned = ctx.alive.iter().map(|&r| ctx.chunks[r].clone()).collect();
        &owned
    };
    let apos: HashMap<usize, usize> = ctx.alive.iter().enumerate().map(|(k, &r)| (r, k)).collect();
    let dpos: Vec<usize> = ctx
        .decided
        .iter()
        .map(|&r| *apos.get(&r).expect("decided row not in alive set"))
        .collect();

    let sf = struct_features(alive, ctx.recent, Some(&dpos));
    let extra = extra_feats(alive, &dpos, ctx.task_text);
    let t = ctx.cur_step;
    // T fallback: `int(T or (max(step)+1))` over the live set.
    let t_cap = if ctx.t_total != 0 {
        ctx.t_total
    } else {
        alive.iter().map(|c| c.step).max().unwrap_or(0) + 1
    };
    let readmit = readmit_feats(alive, &dpos, t, t_cap);
    let issue = issuematch_feats(alive, &dpos, ctx.task_text);
    let rerank = rerank_feats(alive, &dpos, ctx.steps, t);
    let central = centrality_feats(alive, &dpos, ctx.task_text);
    let dup = dupcos_feats(ctx);

    let mut out = Vec::with_capacity(ctx.decided.len());
    for k in 0..ctx.decided.len() {
        let c = &alive[dpos[k]];
        let mut row = [0.0f32; READ_STRUCT];
        for j in 0..16 {
            row[j] = sf[k][j] as f32;
        }
        row[16] = ctx.age as f32;
        for j in 0..3 {
            row[17 + j] = extra[k][j] as f32;
        }
        for j in 0..4 {
            row[20 + j] = readmit[k][j] as f32;
        }
        for j in 0..5 {
            row[24 + j] = issue[k][j] as f32;
        }
        for j in 0..8 {
            row[29 + j] = rerank[k][j] as f32;
        }
        for j in 0..5 {
            row[37 + j] = central[k][j] as f32;
        }
        // Python truthiness: `if c.file` — empty basename is falsy.
        row[42] = match (ctx.changeprone, c.file.as_deref().filter(|f| !f.is_empty())) {
            (Some(cp), Some(f)) => (*cp.cnt.get(f).unwrap_or(&0) as f64 / cp.total as f64) as f32,
            _ => 0.0,
        };
        // 43-46 scout: zeros (v1 — no client-side scout record).
        row[47] = dup[k][0];
        row[48] = dup[k][1];
        out.push(row);
    }
    out
}

fn len_spans(text: &str, min: usize) -> HashSet<String> {
    spans(text)
        .into_iter()
        .filter(|s| char_len(s) >= min)
        .collect()
}

/// torch_curator.decided_extra_feats: [rc0, task_overlap, dup_recency].
fn extra_feats(alive: &[Chunk], dpos: &[usize], task_text: &str) -> Vec<[f64; 3]> {
    let tsp = len_spans(task_text, 8);
    let asets: Vec<HashSet<String>> = alive.iter().map(|d| len_spans(&d.text, 5)).collect();
    let mut out = Vec::with_capacity(dpos.len());
    for &p in dpos {
        let c = &alive[p];
        let rc0 = if c.rc == Some(0) { 1.0 } else { 0.0 };
        let cl = len_spans(&c.text, 8);
        let tov = if !cl.is_empty() && !tsp.is_empty() {
            (cl.intersection(&tsp).count() as f64 / 3.0).min(1.0)
        } else {
            0.0
        };
        let cset = &asets[p]; // spans(c.text) len>=5 — same pure function
        let mut dr = 0.0f64;
        for (q, d) in alive.iter().enumerate() {
            // `d is c` in the reference is object identity == alive position.
            if q == p || d.step >= c.step {
                continue;
            }
            let mut ov = 0.0f64;
            // `if c.file and d.file == c.file and c.lo is not None and
            //  d.lo is not None` — then min/max over hi (paired by chunker).
            if let (Some(cf), Some(clo), Some(chi)) =
                (c.file.as_deref().filter(|f| !f.is_empty()), c.lo, c.hi)
            {
                if d.file.as_deref() == Some(cf) {
                    if let (Some(dlo), Some(dhi)) = (d.lo, d.hi) {
                        ov = std::cmp::max(0, std::cmp::min(chi, dhi) - std::cmp::max(clo, dlo) + 1)
                            as f64
                            / std::cmp::max(1, chi - clo + 1) as f64;
                    }
                }
            }
            let ds = &asets[q];
            if ov < 0.6 && !cset.is_empty() && !ds.is_empty() {
                let u = cset.union(ds).count();
                if u > 0 {
                    ov = ov.max(cset.intersection(ds).count() as f64 / u as f64);
                }
            }
            if ov >= 0.6 {
                dr += 1.0 / (1.0 + (c.step - d.step) as f64); // nearer re-reads weigh more
            }
        }
        out.push([rc0, tov, dr.min(3.0)]);
    }
    out
}

/// AC_READMIT: [t_abs, t_norm, resident_flag, n_resident].
fn readmit_feats(alive: &[Chunk], dpos: &[usize], t: i64, t_cap: i64) -> Vec<[f64; 4]> {
    let mut out = Vec::with_capacity(dpos.len());
    for &p in dpos {
        let c = &alive[p];
        let mut row = [
            t as f64,
            t as f64 / std::cmp::max(t_cap - 1, 1) as f64,
            0.0,
            0.0,
        ];
        if let Some(cf) = c.file.as_deref().filter(|f| !f.is_empty()) {
            let nres = alive
                .iter()
                .filter(|a| a.step < c.step && a.file.as_deref() == Some(cf))
                .count();
            row[2] = if nres > 0 { 1.0 } else { 0.0 };
            row[3] = nres as f64;
        }
        out.push(row);
    }
    out
}

// trace_graph._IDENT_RE: dotted/camel/snake idents >= 6 chars (ASCII classes).
static IDENT_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]{5,}(?:\.[A-Za-z_][A-Za-z0-9_]*)*").unwrap()
});
// trace_graph._DEFCLASS_RE; `\s` -> `[\s\x1c-\x1f]` per the freeze.rs convention.
static DEFCLASS_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?:^|\n)[\s\x1c-\x1f]*(?:def|class)[\s\x1c-\x1f]+([A-Za-z_][A-Za-z0-9_]*)")
        .unwrap()
});
// trace_graph._TB_FRAME_RE == torch_curator._TB_RE.
static TB_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"File "([^"]+)""#).unwrap());
// trace_graph._OF_PATH_RE (crate \w kept — see features.rs regex conventions).
static OF_PATH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"([\w./\-]+\.(?:py|rst|txt|cfg|ini|toml|md|json|yaml|yml|c|h|cpp|js|ts))")
        .unwrap()
});
// trace_graph._OF_EDIT_RE; `\S` -> `[^\s\x1c-\x1f]` (complement of Python \s).
static OF_EDIT_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"The file ([^\s\x1c-\x1f]+) has been updated").unwrap());

fn basename(f: &str) -> &str {
    f.rsplit('/').next().unwrap_or(f)
}

/// trace_graph._chunk_file: basename identity for ANY chunk — .file when
/// truthy, else a path dug out of the text (edit-echo / cmd output).
fn chunk_file(c: &Chunk) -> Option<String> {
    if let Some(f) = c.file.as_deref().filter(|f| !f.is_empty()) {
        return Some(basename(f).to_string());
    }
    let m = OF_EDIT_RE
        .captures(&c.text)
        .or_else(|| OF_PATH_RE.captures(&c.text))?;
    Some(basename(m.get(1).unwrap().as_str()).to_string())
}

/// AC_ISSUEMATCH: span-level issue-symbol match, all-zero when the task has
/// no idents (the reference's `if iss:` gates every column incl. the tb flag).
fn issuematch_feats(alive: &[Chunk], dpos: &[usize], task_text: &str) -> Vec<[f64; 5]> {
    let iss: HashSet<&str> = IDENT_RE.find_iter(task_text).map(|m| m.as_str()).collect();
    let tb_files: HashSet<&str> = TB_RE
        .captures_iter(task_text)
        .map(|m| basename(m.get(1).unwrap().as_str()))
        .collect();
    let niss = std::cmp::max(iss.len(), 1);
    let mut out = vec![[0.0f64; 5]; dpos.len()];
    if iss.is_empty() {
        return out;
    }
    for (k, &p) in dpos.iter().enumerate() {
        let c = &alive[p];
        let nmatch = iss.iter().filter(|w| c.text.contains(*w)).count();
        out[k][0] = if nmatch > 0 { 1.0 } else { 0.0 };
        out[k][1] = (nmatch as f64).ln_1p();
        out[k][2] = if DEFCLASS_RE
            .captures_iter(&c.text)
            .any(|m| iss.contains(m.get(1).unwrap().as_str()))
        {
            1.0
        } else {
            0.0
        };
        out[k][3] = nmatch as f64 / niss as f64;
        out[k][4] = match chunk_file(c) {
            Some(cf) if tb_files.contains(cf.as_str()) => 1.0,
            _ => 0.0,
        };
    }
    out
}

/// torch_curator.decided_rerank_feats: file-reranker raw signals; `steps` is
/// the per-step (cmd, obs) list and T = len(steps) INSIDE this block.
fn rerank_feats(
    alive: &[Chunk],
    dpos: &[usize],
    steps: &[(String, String)],
    t: i64,
) -> Vec<[f64; 8]> {
    let tn = steps.len() as i64;
    let (last_cmd, last_obs): (&str, &str) = if t > 0 && t <= tn {
        let s = &steps[(t - 1) as usize];
        (&s.0, &s.1)
    } else {
        ("", "")
    };
    let cur_cmd: &str = if (0..tn).contains(&t) {
        &steps[t as usize].0
    } else {
        ""
    };
    let tb: Vec<&str> = TB_RE
        .captures_iter(last_obs)
        .map(|m| m.get(1).unwrap().as_str())
        .collect();
    let mut depth_re: HashMap<&str, regex::Regex> = HashMap::new();
    let mut out = Vec::with_capacity(dpos.len());
    for &p in dpos {
        let c = &alive[p];
        let f = c.file.as_deref().unwrap_or("");
        let (rec, first) = if !f.is_empty() {
            match alive
                .iter()
                .filter(|a| a.file.as_deref() == Some(f) && a.step < c.step)
                .map(|a| a.step)
                .max()
            {
                Some(mx) => ((c.step - mx) as f64, 0.0),
                None => (tn as f64, 1.0),
            }
        } else {
            (tn as f64, 1.0)
        };
        let in_tb = if !f.is_empty() && tb.iter().any(|x| x.contains(f)) {
            1.0
        } else {
            0.0
        };
        let in_obs = if !f.is_empty() && last_obs.contains(f) {
            1.0
        } else {
            0.0
        };
        let in_cmd = if !f.is_empty() && (last_cmd.contains(f) || cur_cmd.contains(f)) {
            1.0
        } else {
            0.0
        };
        let is_test = if !f.is_empty() && basename(f).contains("test") {
            1.0
        } else {
            0.0
        };
        let mut depth = 0.0f64;
        if !f.is_empty() && !c.cmd.is_empty() {
            let re = depth_re.entry(f).or_insert_with(|| {
                regex::Regex::new(&format!(r"/[\w./-]+/{}", regex::escape(f))).unwrap()
            });
            if let Some(m) = re.find(&c.cmd) {
                depth = m.as_str().matches('/').count() as f64;
            }
        }
        out.push([
            rec,
            1.0 / (1.0 + rec),
            in_tb,
            in_obs,
            in_cmd,
            is_test,
            first,
            depth,
        ]);
    }
    out
}

// ---- symbol_graph.py port (AC_CENTRALITY, cols 37-41) ----------------------

/// posixpath.splitext extension (leading dots of the basename never start one).
fn splitext_ext(p: &str) -> &str {
    let sep = p.rfind('/').map(|i| i as i64).unwrap_or(-1);
    let dot = match p.rfind('.') {
        Some(i) => i as i64,
        None => return "",
    };
    if dot > sep {
        let mut fi = sep + 1;
        while fi < dot {
            if p.as_bytes()[fi as usize] != b'.' {
                return &p[dot as usize..];
            }
            fi += 1;
        }
    }
    ""
}

/// symbol_graph._file_key: basename up to a pytest '::' node-id suffix for
/// python READ chunks; None otherwise (legitimately zero centrality).
fn file_key(c: &Chunk) -> Option<&str> {
    let f = c.file.as_deref().filter(|f| !f.is_empty())?;
    if c.kind != "read" {
        return None;
    }
    let name = f.split("::").next().unwrap();
    let ext = splitext_ext(name).to_lowercase();
    (ext == ".py" || ext == ".pyi").then_some(name)
}

/// Python s[a:b] on chars, with tree-sitter's BYTE columns used as char
/// indices — the reference's exact (unicode-quirky) slicing, preserved.
fn py_slice_chars(s: &str, a: usize, b: usize) -> String {
    s.chars().skip(a).take(b.saturating_sub(a)).collect()
}

/// symbol_graph._text: node source sliced by start/end Points over
/// src.split("\n") lines.
fn node_text(node: tree_sitter::Node, lines: &[&str]) -> String {
    let (r0, c0) = (node.start_position().row, node.start_position().column);
    let (r1, c1) = (node.end_position().row, node.end_position().column);
    if r0 >= lines.len() {
        return String::new();
    }
    if r0 == r1 {
        return py_slice_chars(lines[r0], c0, c1);
    }
    let mut seg = vec![lines[r0].chars().skip(c0).collect::<String>()];
    let last = std::cmp::min(r1, lines.len() - 1);
    for (r, line) in lines.iter().enumerate().take(last + 1).skip(r0 + 1) {
        seg.push(if r == r1 {
            py_slice_chars(line, 0, c1)
        } else {
            line.to_string()
        });
    }
    seg.join("\n")
}

/// symbol_graph._name_of: the def-name identifier (field 'name', else the
/// first named child of kind 'identifier').
fn name_of(node: tree_sitter::Node, lines: &[&str]) -> String {
    let nm = node.child_by_field_name("name").or_else(|| {
        (0..node.named_child_count())
            .filter_map(|i| node.named_child(i))
            .find(|ch| ch.kind() == "identifier")
    });
    nm.map(|n| node_text(n, lines)).unwrap_or_default()
}

fn walk_defs_refs(
    node: tree_sitter::Node,
    lines: &[&str],
    defs: &mut Vec<String>,
    seen: &mut HashSet<String>,
    refs: &mut Vec<String>,
) {
    match node.kind() {
        "function_definition" | "class_definition" => {
            let t = name_of(node, lines);
            if !t.is_empty() && seen.insert(t.clone()) {
                defs.push(t);
            }
        }
        "call" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let t = node_text(f, lines);
                        if !t.is_empty() {
                            refs.push(t);
                        }
                    }
                    "attribute" => {
                        if let Some(at) = f.child_by_field_name("attribute") {
                            let t = node_text(at, lines);
                            if !t.is_empty() {
                                refs.push(t);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    for i in 0..node.named_child_count() {
        if let Some(ch) = node.named_child(i) {
            walk_defs_refs(ch, lines, defs, seen, refs);
        }
    }
}

/// symbol_graph._py_defs_refs via the compiled python grammar (cst.rs table —
/// symbol_graph's language detection is python-only). defs keep first-seen
/// order (a deterministic stand-in for Python's hash-ordered set — see the
/// module-doc ordering note); refs keep occurrence order like the reference.
fn py_defs_refs(src: &str) -> (Vec<String>, Vec<String>) {
    let empty = (Vec::new(), Vec::new());
    let Some(lang) = crate::cst::language_for("x.py") else {
        return empty;
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        return empty;
    }
    let Some(tree) = parser.parse(src, None) else {
        return empty;
    };
    let lines: Vec<&str> = src.split('\n').collect();
    let (mut defs, mut seen, mut refs) = (Vec::new(), HashSet::new(), Vec::new());
    walk_defs_refs(tree.root_node(), &lines, &mut defs, &mut seen, &mut refs);
    (defs, refs)
}

/// symbol_graph._ident_mul: Aider-style per-identifier edge multiplier.
fn ident_mul(ident: &str, n_definers: usize, task_lower: &str) -> f64 {
    let mut mul = 1.0f64;
    if n_definers > 5 {
        mul *= 0.1;
    }
    if ident.starts_with('_') {
        mul *= 0.1;
    }
    let long_named = char_len(ident) >= 8
        && (ident.contains('_')
            || (ident.chars().any(char::is_uppercase) && ident.chars().any(char::is_lowercase)));
    if long_named {
        mul *= 3.0;
    }
    if !task_lower.is_empty() && task_lower.contains(&ident.to_lowercase()) {
        mul *= 3.0;
    }
    mul
}

/// symbol_graph._pagerank: weighted directed power iteration, d=0.85,
/// iters=80, tol=1e-7, uniform teleport (pers=None path only).
fn pagerank(n: usize, edges: &[(usize, usize, f64)]) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    const D: f64 = 0.85;
    let pv = vec![1.0 / n as f64; n];
    let mut outw = vec![0.0f64; n];
    let mut inl: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for &(s, t, w) in edges {
        outw[s] += w;
        inl[t].push((s, w));
    }
    let mut r = pv.clone();
    for _ in 0..80 {
        let mut dsum = 0.0f64;
        for i in 0..n {
            if outw[i] == 0.0 {
                dsum += r[i]; // dangling mass -> teleport
            }
        }
        let dang = D * dsum;
        let mut nr: Vec<f64> = (0..n).map(|i| (1.0 - D) * pv[i] + dang * pv[i]).collect();
        for ti in 0..n {
            let mut acc = 0.0f64;
            for &(si, w) in &inl[ti] {
                acc += r[si] * w / outw[si];
            }
            nr[ti] += D * acc;
        }
        let s: f64 = nr.iter().sum();
        if s > 0.0 {
            for x in nr.iter_mut() {
                *x /= s;
            }
        }
        let mx = (0..n)
            .map(|i| (nr[i] - r[i]).abs())
            .fold(f64::NEG_INFINITY, f64::max);
        r = nr;
        if mx < 1e-7 {
            break;
        }
    }
    r
}

/// symbol_graph.decided_centrality_feats: [file_pr, def_rank, defines_any,
/// log1p(n_defs), log1p(n_refs)] over the causal alive set.
fn centrality_feats(alive: &[Chunk], dpos: &[usize], task_text: &str) -> Vec<[f64; 5]> {
    let task_lower = task_text.to_lowercase();
    // chunk_symbols, memoized per alive row (the reference caches per object).
    let syms: Vec<(Vec<String>, Vec<String>)> = alive
        .iter()
        .map(|c| {
            if file_key(c).is_some() {
                py_defs_refs(&c.text)
            } else {
                (Vec::new(), Vec::new())
            }
        })
        .collect();
    // defines: ident -> files defining it; refs_by_file: file -> ident counts.
    // Insertion-ordered (Vec + index map) — the deterministic ordering stand-in.
    let mut defines: Vec<(String, Vec<String>)> = Vec::new();
    let mut def_idx: HashMap<String, usize> = HashMap::new();
    let mut refs_by_file: Vec<(String, Vec<(String, i64)>)> = Vec::new();
    let mut rbf_idx: HashMap<String, usize> = HashMap::new();
    for (q, c) in alive.iter().enumerate() {
        let Some(f) = file_key(c) else { continue };
        let (d, r) = &syms[q];
        for ident in d {
            let di = *def_idx.entry(ident.clone()).or_insert_with(|| {
                defines.push((ident.clone(), Vec::new()));
                defines.len() - 1
            });
            let files = &mut defines[di].1;
            if !files.iter().any(|x| x == f) {
                files.push(f.to_string());
            }
        }
        if !r.is_empty() {
            let fi = *rbf_idx.entry(f.to_string()).or_insert_with(|| {
                refs_by_file.push((f.to_string(), Vec::new()));
                refs_by_file.len() - 1
            });
            let cnt = &mut refs_by_file[fi].1;
            for ident in r {
                match cnt.iter_mut().find(|(i, _)| i == ident) {
                    Some((_, n)) => *n += 1,
                    None => cnt.push((ident.clone(), 1)),
                }
            }
        }
    }
    // node_set = definer files ∪ referencer files. The reference's set order
    // is hash-randomized; first-seen order here (see module-doc ordering note).
    let mut node_names: Vec<&str> = Vec::new();
    let mut node_idx: HashMap<&str, usize> = HashMap::new();
    for f in defines
        .iter()
        .flat_map(|(_, files)| files.iter())
        .chain(refs_by_file.iter().map(|(f, _)| f))
    {
        node_idx.entry(f.as_str()).or_insert_with(|| {
            node_names.push(f.as_str());
            node_names.len() - 1
        });
    }
    let n = node_names.len();
    // edges: referencer -> definer, weight = ident_mul * sqrt(num), in
    // refs_by_file insertion order (the reference's dict order).
    let mut edges: Vec<(usize, usize, f64, &str)> = Vec::new();
    for (rf, cnt) in &refs_by_file {
        let rfi = node_idx[rf.as_str()];
        for (ident, num) in cnt {
            let Some(&di) = def_idx.get(ident.as_str()) else {
                continue;
            };
            let definers = &defines[di].1;
            if definers.is_empty() {
                continue;
            }
            let w = ident_mul(ident, definers.len(), &task_lower) * (*num as f64).sqrt();
            for df in definers {
                edges.push((rfi, node_idx[df.as_str()], w, ident.as_str()));
            }
        }
    }
    let pr = pagerank(
        n,
        &edges
            .iter()
            .map(|&(s, t, w, _)| (s, t, w))
            .collect::<Vec<_>>(),
    );
    // distribute each file's rank onto its out-edges -> per-(file, ident) rank.
    let mut outw = vec![0.0f64; n];
    let mut src_order: Vec<usize> = Vec::new();
    let mut src_seen = vec![false; n];
    let mut out_edges: Vec<Vec<(usize, f64, &str)>> = vec![Vec::new(); n];
    for &(s, t, w, ident) in &edges {
        outw[s] += w;
        if !src_seen[s] {
            src_seen[s] = true;
            src_order.push(s);
        }
        out_edges[s].push((t, w, ident));
    }
    let mut defrank: HashMap<(usize, &str), f64> = HashMap::new();
    for &s in &src_order {
        let tw = outw[s];
        if tw <= 0.0 {
            continue;
        }
        let sr = pr[s];
        for &(t, w, ident) in &out_edges[s] {
            *defrank.entry((t, ident)).or_insert(0.0) += sr * w / tw;
        }
    }
    let maxpr = pr.iter().copied().fold(0.0f64, f64::max);
    let maxdr = defrank.values().copied().fold(0.0f64, f64::max);
    let mut feats = vec![[0.0f64; 5]; dpos.len()];
    for (k, &p) in dpos.iter().enumerate() {
        let c = &alive[p];
        let Some(f) = file_key(c) else { continue };
        let (d, r) = &syms[p];
        let fpr = if maxpr > 0.0 {
            node_idx.get(f).map_or(0.0, |&i| pr[i]) / maxpr
        } else {
            0.0
        };
        let mut dr = 0.0f64;
        if let Some(&fi) = node_idx.get(f) {
            for ident in d {
                if let Some(&v) = defrank.get(&(fi, ident.as_str())) {
                    if v > dr {
                        dr = v;
                    }
                }
            }
        }
        let dr = if maxdr > 0.0 { dr / maxdr } else { 0.0 };
        feats[k] = [
            fpr,
            dr,
            if d.is_empty() { 0.0 } else { 1.0 },
            (d.len() as f64).ln_1p(),
            (r.len() as f64).ln_1p(),
        ];
    }
    feats
}

// ---- AC_DUPCOS (cols 47-48) -------------------------------------------------

/// Sequential f32 accumulation — see the module-doc precision note (numpy's
/// SIMD/BLAS summation order is machine-dependent; parity is epsilon there).
fn norm_f32(v: &[f32]) -> Vec<f32> {
    let mut acc = 0.0f32;
    for &x in v {
        acc += x * x;
    }
    let denom = acc.sqrt() + 1e-8f32;
    v.iter().map(|&x| x / denom).collect()
}

fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        acc += x * y;
    }
    acc
}

/// [max cosine of content emb vs EARLIER-alive chunks, 1/(1+Δstep to the
/// nearest match)] — trace_train.py L87-100 / curator L621-630. First-max on
/// ties (np.argmax). Zeros when embeddings are absent (fail-open).
fn dupcos_feats(ctx: &ReadoutCtx) -> Vec<[f32; 2]> {
    let mut out = vec![[0.0f32; 2]; ctx.decided.len()];
    let Some(embs) = ctx.content_embs else {
        return out;
    };
    assert_eq!(
        embs.len(),
        ctx.chunks.len(),
        "content_embs must align to chunks rows"
    );
    // normalize once per alive row (decided ⊆ alive)
    let mut en: HashMap<usize, Vec<f32>> = HashMap::new();
    for &r in ctx.alive {
        en.entry(r).or_insert_with(|| norm_f32(&embs[r]));
    }
    for (k, &j) in ctx.decided.iter().enumerate() {
        let sj = ctx.chunks[j].step;
        let mut best: Option<(usize, f32)> = None;
        for &r in ctx.alive {
            if ctx.chunks[r].step >= sj {
                continue;
            }
            let s = dot_f32(&en[&r], &en[&j]);
            if best.is_none_or(|(_, b)| s > b) {
                best = Some((r, s));
            }
        }
        if let Some((r, s)) = best {
            out[k][0] = s;
            // float(1.0 / (1.0 + Δ)) — f64 division, downcast once.
            out[k][1] = (1.0 / (1.0 + (sj - ctx.chunks[r].step) as f64)) as f32;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitext_matches_python_probes() {
        // Verbatim os.path.splitext(p)[1] outputs.
        for (p, want) in [
            ("a.py", ".py"),
            ("a.pyi", ".pyi"),
            ("archive.tar.gz", ".gz"),
            ("noext", ""),
            (".py", ""),
            ("..py", ""),
            (".hidden.py", ".py"),
            ("dir.d/file", ""),
            ("dir.d/file.PY", ".PY"),
            ("a.", "."),
        ] {
            assert_eq!(splitext_ext(p), want, "splitext({p:?})");
        }
    }

    #[test]
    fn py_defs_refs_probe() {
        let src = "import os\n\ndef alpha(x):\n    return beta(x) + obj.method(x)\n\nclass Gamma:\n    def inner(self):\n        return alpha(1)\n";
        let (defs, refs) = py_defs_refs(src);
        let mut ds = defs.clone();
        ds.sort();
        assert_eq!(ds, ["Gamma", "alpha", "inner"]);
        assert_eq!(refs, ["beta", "method", "alpha"]);
    }

    #[test]
    fn byte_column_slicing_quirk_matches_reference() {
        // tree-sitter Points carry BYTE columns; the reference slices str
        // lines by them as CHAR indices (symbol_graph._text) — garbling
        // identifiers after non-ASCII text on the same line. Verbatim
        // vendored output, locked (the parity fixtures exercise it too).
        let (defs, refs) = py_defs_refs("unicode_probe = \"é×π\"; probe = load_batch(0)");
        assert!(defs.is_empty());
        assert_eq!(refs, ["d_batch(0)"]);
    }

    #[test]
    fn dupcos_zeros_without_embs() {
        let chunks = vec![
            Chunk::new("a", None, None, None, 0, "other"),
            Chunk::new("b", None, None, None, 1, "other"),
        ];
        let ctx = ReadoutCtx {
            chunks: &chunks,
            alive: &[0, 1],
            decided: &[1],
            task_text: "",
            recent: "",
            steps: &[],
            cur_step: 1,
            t_total: 2,
            age: 0.0,
            changeprone: None,
            content_embs: None,
        };
        let rows = decided_struct(&ctx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][47], 0.0);
        assert_eq!(rows[0][48], 0.0);
        // scout block always zero in v1
        for &v in &rows[0][43..47] {
            assert_eq!(v, 0.0);
        }
    }
}
