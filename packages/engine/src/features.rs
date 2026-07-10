//! Structural feature extraction for brain-API scoring requests.
//! Emits vectors + features only — raw text never leaves this machine.
//!
//! Ports (reference: adaptive-context-clean/adaptive_context/optimizer/):
//! - `chunking.dup_feats_fast`. The Python version bucketed by `hash()`
//!   (PYTHONHASHSEED-randomized) and re-verified with `==`, so its OUTPUT was
//!   deterministic even though the buckets weren't; we bucket by content
//!   directly, which is the same equivalence relation.
//! - `attribution.spans` (rare-identifier spans; the recent-overlap column's
//!   tokenizer). The Python memo cache is dropped — pure function either way.
//! - `torch_curator.{struct_features, struct_features_causal,
//!   struct_type_features, node_struct_with_type}` and the trace-graph role
//!   markers (trace_train.py L59-62): the v1-contract NODE featurization.
//!
//! Precision contract: the reference computes each element in Python float
//! (f64) and casts ONCE to float32 (`np.asarray(..., dtype=np.float32)`). We
//! keep f64 and let consumers downcast — the same single rounding, asserted
//! bit-for-bit as f32 by tests/parity_features.rs. `struct_type_features` is
//! the exception: numpy divides float32 count arrays (`counts / counts.sum()`),
//! so we divide in f32 too — an f64 divide + downcast double-rounds.
//!
//! Regex deltas vs Python `re` follow the freeze.rs conventions: `\s` becomes
//! `[\s\x1c-\x1f]` (Python's \s includes the C0 separators); `\w`/`\b` keep
//! the regex crate's word class (Python counts No/Nl numerics and excludes
//! combining marks — the crate is the inverse; only pathological unicode
//! adjacent to keywords diverges).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use crate::chunking::Chunk;
use crate::pystr::char_len;

/// O(n) supersession/duplication signals for serving:
/// - sup[i] in [0,1]: a NEWER chunk supersedes i (identical content -> 1.0,
///   or same-file line-range overlap ratio).
/// - dup_earlier[i]: count of EARLIER copies (exact dup, or same-file overlap
///   >= 0.6 of i's span).
pub fn dup_feats_fast(chunks: &[Chunk], rows: Option<&[usize]>) -> (Vec<f64>, Vec<i64>) {
    let n = chunks.len();
    let mut sup = vec![0.0f64; n];
    let mut dup_earlier = vec![0i64; n];
    let mut by_text: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in chunks.iter().enumerate() {
        by_text.entry(c.text.as_str()).or_default().push(i);
        // Python truthiness: `if c.file` — an empty-string basename
        // ("src/:5:match" grep lines) is falsy and never a file bucket.
        if let (Some(f), Some(_)) = (c.file.as_deref().filter(|f| !f.is_empty()), c.lo) {
            by_file.entry(f).or_default().push(i);
        }
    }
    let all: Vec<usize>;
    let targets: &[usize] = match rows {
        Some(r) => r,
        None => {
            all = (0..n).collect();
            &all
        }
    };
    for &i in targets {
        let c = &chunks[i];
        for &j in &by_text[c.text.as_str()] {
            if j == i {
                continue;
            }
            if chunks[j].step > c.step {
                sup[i] = 1.0;
            } else if chunks[j].step < c.step {
                dup_earlier[i] += 1;
            }
        }
        if let (Some(f), Some(lo), Some(hi)) =
            (c.file.as_deref().filter(|f| !f.is_empty()), c.lo, c.hi)
        {
            for &j in &by_file[f] {
                if j == i {
                    continue;
                }
                let d = &chunks[j];
                let (Some(dlo), Some(dhi)) = (d.lo, d.hi) else {
                    continue;
                };
                let ov = std::cmp::max(0, std::cmp::min(hi, dhi) - std::cmp::max(lo, dlo) + 1)
                    as f64
                    / std::cmp::max(1, hi - lo + 1) as f64;
                if d.step > c.step {
                    sup[i] = sup[i].max(ov);
                } else if d.step < c.step && ov >= 0.6 {
                    dup_earlier[i] += 1;
                }
            }
        }
    }
    (sup, dup_earlier)
}

// attribution._SPAN: identifiers / dotted-paths / file paths / a-b-c chains /
// quoted strings / standalone numbers. Alternation order is load-bearing:
// both engines are leftmost-first, so the path/dotted branch wins over the
// bare-identifier branch at the same start position, exactly like Python.
static SPAN_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"[A-Za-z_][A-Za-z0-9_]*(?:[./-][A-Za-z0-9_]+)+",
        r"|[A-Za-z_][A-Za-z0-9_]{3,}",
        r#"|"[^"\n]{2,}""#,
        r"|'[^'\n]{2,}'",
        r"|\b\d{2,}\b",
    ))
    .unwrap()
});

/// attribution._STOP: never-distinctive tokens.
static STOP: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "self", "true", "false", "none", "null", "return", "import", "from", "class", "value",
        "string", "result", "object", "the", "and", "for", "with", "that", "this", "your", "def",
        "function", "print", "test", "tests", "file", "code", "line", "name",
    ])
});

/// Python `str.isdigit()`. `char::is_numeric` is Nd|Nl|No where Python wants
/// Nd + Numeric_Type=Digit (a No subset); the overshoot (Nl / non-digit No)
/// only misroutes quoted >=2-char numeric-only spans between the two accept
/// clauses below, and every len>=3 outcome agrees — len-2 disagreements are
/// filtered by all consumers (len>=5 span sets).
fn py_isdigit(s: &str) -> bool {
    !s.is_empty() && s.chars().all(char::is_numeric)
}

/// attribution.spans: the distinctive-span set of `text` (lowered, quote-
/// stripped, stop-worded; digit runs only at len>=3).
pub fn spans(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    if text.is_empty() {
        return out;
    }
    for m in SPAN_RE.find_iter(text) {
        let s = m
            .as_str()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_lowercase();
        let isdig = py_isdigit(&s);
        if (!s.is_empty() && !STOP.contains(s.as_str()) && !isdig) || (isdig && char_len(&s) >= 3) {
            out.insert(s);
        }
    }
    out
}

fn len5_spans(text: &str) -> HashSet<String> {
    spans(text)
        .into_iter()
        .filter(|s| char_len(s) >= 5)
        .collect()
}

/// torch_curator.struct_features: the 16-col per-chunk struct row. Column
/// order (two unnamed leading cols before the STRUCT name list — do not
/// rebuild from that list):
///   0 log1p(obs_tok)/8 (whole (step,cmd) dump size)  1 tokens/max(1,obs_tok)
///   2 age (maxstep-step)/max(1,maxstep)              3 log1p(tokens)/8
///   4 is_file                                        5 file_focus
///   6 overlap with recent-action spans (0 when recent_text is empty)
///   7..10 kind one-hots read/grep/other/reasoning
///   11 sup_newer  12 min(1,dup_earlier/5)  13 token_frac  14 step_pos
///   15 is_current (step>=maxstep)
/// `rows`: emit ONLY those chunk indices (in order); the set-relative terms
/// are still computed over the FULL chunk set, so each emitted row is
/// identical to the full pass.
pub fn struct_features(
    chunks: &[Chunk],
    recent_text: &str,
    rows: Option<&[usize]>,
) -> Vec<[f64; 16]> {
    let rec = len5_spans(recent_text);
    let maxstep = chunks.iter().map(|c| c.step).max().unwrap_or(1);
    // Keyed by the raw file value (None / "" / name distinct, like the Python
    // dict); the truthiness gate below decides whether the count is used.
    let mut ff: HashMap<Option<&str>, i64> = HashMap::new();
    for c in chunks {
        *ff.entry(c.file.as_deref()).or_insert(0) += 1;
    }
    let (sup, dup) = dup_feats_fast(chunks, rows);
    let tot_tok = match chunks.iter().map(|c| c.tokens).sum::<i64>() {
        0 => 1, // Python `or 1`
        t => t,
    };
    let mut obs_tok: HashMap<(i64, &str), i64> = HashMap::new();
    for c in chunks {
        *obs_tok.entry((c.step, c.cmd.as_str())).or_insert(0) += c.tokens;
    }
    let all: Vec<usize>;
    let targets: &[usize] = match rows {
        Some(r) => r,
        None => {
            all = (0..chunks.len()).collect();
            &all
        }
    };
    let mut out = Vec::with_capacity(targets.len());
    for &i in targets {
        let c = &chunks[i];
        // Python truthiness: empty-string basename is falsy on both file gates.
        let has_file = c.file.as_deref().is_some_and(|f| !f.is_empty());
        // Skip the per-chunk spans() pass entirely when rec is empty (the
        // node_struct/serve path): the only consumer is col 6, which is 0
        // against an empty rec.
        let cs = (!rec.is_empty()).then(|| len5_spans(&c.text));
        let ot = obs_tok[&(c.step, c.cmd.as_str())];
        let overlap = match &cs {
            Some(cs) if !cs.is_empty() => {
                cs.iter().filter(|s| rec.contains(*s)).count() as f64
                    / std::cmp::max(1, cs.len()) as f64
            }
            _ => 0.0,
        };
        out.push([
            (ot as f64).ln_1p() / 8.0,
            c.tokens as f64 / std::cmp::max(1, ot) as f64,
            (maxstep - c.step) as f64 / std::cmp::max(1, maxstep) as f64,
            (c.tokens as f64).ln_1p() / 8.0,
            if has_file { 1.0 } else { 0.0 },
            if has_file {
                ff[&c.file.as_deref()] as f64 / std::cmp::max(1, chunks.len() as i64) as f64
            } else {
                0.0
            },
            overlap,
            if c.kind == "read" { 1.0 } else { 0.0 },
            if c.kind == "grep" { 1.0 } else { 0.0 },
            if c.kind == "other" { 1.0 } else { 0.0 },
            if c.kind == "reasoning" { 1.0 } else { 0.0 },
            sup[i],
            (dup[i] as f64 / 5.0).min(1.0),
            c.tokens as f64 / tot_tok as f64,
            c.step as f64 / std::cmp::max(1, maxstep) as f64,
            if c.step >= maxstep { 1.0 } else { 0.0 },
        ]);
    }
    out
}

/// torch_curator.struct_features_causal: PREFIX-INVARIANT node struct — each
/// node's row is struct_features over its causal prefix (chunks with step <=
/// its own step) with recent="", so the row is identical whether computed
/// over the full run (training) or the live prefix (serving). One
/// struct_features pass per DISTINCT step, rows mapped back by position (the
/// prefix filter preserves input order — same mapping as Python's id() dict).
pub fn struct_features_causal(chunks: &[Chunk]) -> Vec<[f64; 16]> {
    let n = chunks.len();
    if n == 0 {
        return Vec::new();
    }
    let mut out = vec![[0.0f64; 16]; n];
    let mut seen: HashSet<i64> = HashSet::new();
    for j in 0..n {
        let t = chunks[j].step;
        if !seen.insert(t) {
            continue;
        }
        let keep: Vec<usize> = (0..n).filter(|&i| chunks[i].step <= t).collect();
        let prefix: Vec<Chunk> = keep.iter().map(|&i| chunks[i].clone()).collect();
        let sf = struct_features(&prefix, "", None);
        for (r, &i) in keep.iter().enumerate() {
            if chunks[i].step == t {
                out[i] = sf[r];
            }
        }
    }
    out
}

// torch_curator._line_type regexes. Python `\s` -> `[\s\x1c-\x1f]` per the
// freeze.rs convention; the two negative lookaheads (unsupported by the regex
// crate) are decomposed below with their exact backtracking semantics.
static SIG_HEAD_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^[\s\x1c-\x1f]*(?:async[\s\x1c-\x1f]+def|def|class|@)\b|^[\s\x1c-\x1f]*(?:function|func|fn|public|private|protected)\b",
    )
    .unwrap()
});
// The C/Java-signature branch minus its (?!_CTRL\b) lookahead, minus the
// leading ^\s* (sig_c_alt3 enumerates the \s* splits by hand).
static SIG_BODY_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^[\w<>\[\], *&]+[\s\x1c-\x1f]+\w+[\s\x1c-\x1f]*\([^;{]*\)[\s\x1c-\x1f]*\{[\s\x1c-\x1f]*$",
    )
    .unwrap()
});
static CTRL_AT_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^(?:for|while|if|elif|else|with|try|except|finally|return|yield|raise|assert|del|print|switch|case)\b",
    )
    .unwrap()
});
static IMPORT_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^[\s\x1c-\x1f]*(?:import|from|#include|require|use)\b").unwrap()
});
// `=(?!=)` (an '=' not followed by another) becomes `=(?:[^=]|$)` — same
// acceptance, one consumed char more (irrelevant for is_match).
static DECL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^[\s\x1c-\x1f]*[A-Za-z_][\w.]*[\s\x1c-\x1f]*(?::[^=]+)?=(?:[^=]|$)|^[\s\x1c-\x1f]*(?:const|let|var|val|static|final|public|private|export)\b.*=",
    )
    .unwrap()
});
static COMMENT_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"^[\s\x1c-\x1f]*(?:#|//|/\*|\*|--|;)",
        r#"|^[\s\x1c-\x1f]*(?:'''|""")"#,
    ))
    .unwrap()
});

/// The `^\s*(?!_CTRL\b)[\w<>\[\], *&]+...\{\s*$` branch of _SIG_RE, with
/// Python's backtracking reproduced: the char class contains a literal space,
/// so `\s*` can backtrack past leading SPACES and dodge the lookahead — a
/// guarded control keyword with leading blanks still matches (" for x in
/// range(10) {" IS a signature, "for x in range(10) {" and "\tfor ..." are
/// not; verified against the reference). We enumerate every `\s*` stop
/// position k: the lookahead can only fail at the end of the whitespace run.
fn sig_c_alt3(ln: &str) -> bool {
    let w = ln
        .char_indices()
        .find(|&(_, c)| !crate::pystr::py_is_space(c))
        .map(|(i, _)| i)
        .unwrap_or(ln.len());
    for k in ln[..w].char_indices().map(|(i, _)| i).chain([w]) {
        if k == w && CTRL_AT_RE.is_match(&ln[w..]) {
            continue;
        }
        if SIG_BODY_RE.is_match(&ln[k..]) {
            return true;
        }
    }
    false
}

/// torch_curator._line_type: 0 signature, 1 import, 2 decl, 3 body,
/// 4 comment. Callers pass non-blank lines only.
fn line_type(ln: &str) -> usize {
    if COMMENT_RE.is_match(ln) {
        4
    } else if IMPORT_RE.is_match(ln) {
        1
    } else if SIG_HEAD_RE.is_match(ln) || sig_c_alt3(ln) {
        0
    } else if DECL_RE.is_match(ln) {
        2
    } else {
        3
    }
}

/// torch_curator.struct_type_features: per-chunk fractions of non-blank lines
/// in [signature, import, decl, body, comment] (sum to 1; all-zero for a
/// chunk with no non-blank lines). Division in f32 — the reference divides
/// float32 numpy arrays (see module doc).
pub fn struct_type_features(chunks: &[Chunk], rows: Option<&[usize]>) -> Vec<[f64; 5]> {
    let all: Vec<usize>;
    let targets: &[usize] = match rows {
        Some(r) => r,
        None => {
            all = (0..chunks.len()).collect();
            &all
        }
    };
    let mut out = Vec::with_capacity(targets.len());
    for &i in targets {
        let mut counts = [0.0f32; 5];
        let mut any = false;
        for ln in crate::pystr::py_splitlines(&chunks[i].text) {
            if crate::pystr::py_has_content(ln) {
                counts[line_type(ln)] += 1.0;
                any = true;
            }
        }
        if !any {
            out.push([0.0; 5]);
            continue;
        }
        let total: f32 = counts.iter().sum();
        out.push(std::array::from_fn(|k| (counts[k] / total) as f64));
    }
    out
}

/// torch_curator.node_struct_with_type: prefix-invariant struct (16) + the
/// per-atom structural-type fractions (5) -> the (N, 21) trace-path node
/// struct (`spec["node_struct"]`, xs cols 0-20).
pub fn node_struct_with_type(chunks: &[Chunk]) -> Vec<[f64; 21]> {
    let base = struct_features_causal(chunks);
    let typ = struct_type_features(chunks, None);
    base.into_iter()
        .zip(typ)
        .map(|(b, t)| {
            let mut row = [0.0f64; 21];
            row[..16].copy_from_slice(&b);
            row[16..].copy_from_slice(&t);
            row
        })
        .collect()
}

/// pyg_model.edges rel-4 SUPERSESSION pairs (pyg_model.py L426-444, causal
/// path): OLD i -> NEW j for every ordered pair with j.step strictly later
/// and overlap >= 0.6, where overlap is
///   (a) same-file line-range overlap `inter / max(1, hi_i - lo_i + 1)`
///       (normalized by chunk i's span; needs truthy c.file on i, equal file
///       on j, and lo on BOTH — Python's exact gate), else
///   (b) span-set Jaccard of the >=5-char `attribution.spans` sets, taken
///       only when the line overlap fell below 0.6 and both sets are
///       non-empty.
/// This is the ONE text-dependent edge relation of the trace graph — the v1
/// contract ships these pairs client-side (`edges_supersession`); the brain
/// rebuilds every other relation from ids. EMISSION ORDER is the vendored
/// loop's (i asc, then j asc) = lexicographic (src, dst) — the server re-sorts
/// to the same order, keeping the scatter summation order bit-identical.
/// Parity: tests/parity_features.rs replays fixtures generated from the
/// vendored edges() by parity/gen_supersession_fixtures.py.
pub fn supersession_edges(chunks: &[Chunk]) -> Vec<(usize, usize)> {
    let n = chunks.len();
    let sets: Vec<HashSet<String>> = chunks.iter().map(|c| len5_spans(&c.text)).collect();
    let mut out = Vec::new();
    for i in 0..n {
        for j in 0..n {
            // Python: `if chunks[j].step <= chunks[i].step or i == j: continue`
            if chunks[j].step <= chunks[i].step || i == j {
                continue;
            }
            let (ci, cj) = (&chunks[i], &chunks[j]);
            let mut ov = 0.0f64;
            // `if ci.file and cj.file == ci.file and ci.lo is not None and
            //  cj.lo is not None` — empty-string basename is falsy on ci; the
            //  reference then reads hi unconditionally (the chunker pairs
            //  lo/hi, so a lone lo is unreachable — we skip it defensively,
            //  matching dup_feats_fast's convention).
            if let (Some(fi), Some(ilo), Some(ihi)) =
                (ci.file.as_deref().filter(|f| !f.is_empty()), ci.lo, ci.hi)
            {
                if cj.file.as_deref() == Some(fi) {
                    if let (Some(jlo), Some(jhi)) = (cj.lo, cj.hi) {
                        let inter =
                            std::cmp::max(0, std::cmp::min(ihi, jhi) - std::cmp::max(ilo, jlo) + 1);
                        ov = inter as f64 / std::cmp::max(1, ihi - ilo + 1) as f64;
                    }
                }
            }
            if ov < 0.6 && !sets[i].is_empty() && !sets[j].is_empty() {
                let u = sets[i].union(&sets[j]).count();
                if u > 0 {
                    ov = ov.max(sets[i].intersection(&sets[j]).count() as f64 / u as f64);
                }
            }
            if ov >= 0.6 {
                out.push((i, j)); // OLD i -> NEW j (causal forward)
            }
        }
    }
    out
}

/// The trace-graph role markers (trace_train.py L59-62, mirrored by
/// curator._trace_scores): [is_reasoning, has_head] per chunk — xs cols 21-22.
pub fn markers(chunks: &[Chunk]) -> Vec<[f64; 2]> {
    chunks
        .iter()
        .map(|c| {
            [
                if c.kind == "reasoning" { 1.0 } else { 0.0 },
                if c.head.is_empty() { 0.0 } else { 1.0 },
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_type_matches_reference_probes() {
        // Verbatim outputs of the Python reference on the probe corpus,
        // including the \s*-backtrack lookahead bypass (spaces dodge the
        // control-keyword guard, tabs don't).
        for (ln, want) in [
            ("for x in range(10) {", 3),
            (" for x in range(10) {", 0),
            ("\tfor x in range(10) {", 3),
            (" \tfor x in range(10) {", 3),
            ("\t for x in range(10) {", 0),
            ("while (x) foo(y) {", 3),
            (" while (x) foo(y) {", 3),
            ("int main(int argc) {", 0),
            ("print('hello') {", 3),
            ("form x(y) {", 0),
            ("@decorator", 0),
            ("@ nope", 3),
            ("async  def foo():", 0),
            ("x = 1", 2),
            ("x == 1", 3),
            ("x =:= y", 2),
            ("let y == 2", 2),
            ("x:= 5", 3),
            ("*bold*", 4),
            ("import os", 1),
            ("importantly not", 3),
        ] {
            assert_eq!(line_type(ln), want, "line {ln:?}");
        }
    }

    #[test]
    fn supersession_direction_and_branches() {
        let code = "def parse_config(path):\n    return validate_schema(load_yaml(path))";
        let chunks = vec![
            // 0: file read lines 1-10
            Chunk::new(code, Some("config.py".into()), Some(1), Some(10), 0, "read"),
            // 1: same file, re-read of lines 3-12 at a later step: overlap of
            // chunk 0's span = 8/10 >= 0.6 -> edge 0 -> 1 (OLD -> NEW)
            Chunk::new(
                "different words entirely",
                Some("config.py".into()),
                Some(3),
                Some(12),
                1,
                "read",
            ),
            // 2: fileless near-duplicate text at a later step: Jaccard branch
            Chunk::new(code, None, None, None, 2, "other"),
            // 3: same step as 2 -> never an edge with 2 in either direction
            Chunk::new(code, None, None, None, 2, "other"),
        ];
        let got = supersession_edges(&chunks);
        // 0->1 line overlap; 0->2/0->3 exact-text Jaccard 1.0; 1 shares no
        // spans with 2/3 (its text differs) and 2<->3 are same-step.
        assert_eq!(got, vec![(0, 1), (0, 2), (0, 3)]);
        // empty-basename file is Python-falsy: no line-overlap branch
        let e = vec![
            Chunk::new(
                "alpha_span beta_span gamma_span",
                Some("".into()),
                Some(1),
                Some(4),
                0,
                "grep",
            ),
            Chunk::new(
                "alpha_span beta_span gamma_span",
                Some("".into()),
                Some(2),
                Some(5),
                1,
                "grep",
            ),
        ];
        // identical spans -> Jaccard 1.0 still fires, but via branch (b)
        assert_eq!(supersession_edges(&e), vec![(0, 1)]);
    }

    #[test]
    fn spans_matches_reference_probe() {
        let mut got: Vec<String> = spans(
            "Fix parse_config in src/config.py: ValueError at line 12345 'quoted str' \
             \"another one\" the This form2 ab_cd_ef x²y",
        )
        .into_iter()
        .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "12345",
                "ab_cd_ef",
                "another one",
                "form2",
                "parse_config",
                "quoted str",
                "src/config.py",
                "valueerror"
            ]
        );
        assert!(spans("").is_empty());
    }
}
