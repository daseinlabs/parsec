//! The 6 RE-REQUEST columns of the HS curator (spec v6828t3): node-row and
//! decided-row block 16–21. Port of gnn-train `torch_curator.rereq_features_base`
//! and `rereq_apply_cut`, and of the r3 recognizer helpers they reuse
//! (`scripts/fight_targets_helpers._norm` / `_bash_read_targets`).
//!
//! Columns, per chunk, causal (only STRICTLY earlier steps contribute):
//!   0 n_prior_serves     earlier observation copies overlapping this chunk's range(s)
//!   1 prior_copy_cut     1 if the most recent such copy is cut
//!   2 file_rereq_count   earlier re-requests of this chunk's first file
//!   3 log1p(trace_rereq_count)  log1p of earlier re-requests anywhere
//!   4 is_rereq           n_prior_serves > 0
//!   5 is_rereq_of_cut    is_rereq AND prior_copy_cut — the serving rule's `fought`
//!
//! Cut state is a caller-supplied predicate over the earlier copy's index. The
//! reference reads it from fold markers (node path) or `alive_at(t)` (readout);
//! at serve the Freezer answers it from its dropped registry, by chunk identity.
//!
//! Parity: tests/parity_rereq.rs, fixtures from parity/gen_rereq_fixtures.py.
//! Two documented divergences, both on inputs no harness emits: a range bound
//! written in non-ASCII decimal digits (Python's `int()` accepts them; the
//! segment is skipped here), and a bound that overflows i64 (arbitrary
//! precision in Python; skipped here).

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::chunking::Chunk;
use crate::pystr::{char_len, py_strip};

pub const REREQ_WIDTH: usize = 6;

/// `10**9`: the reference's "whole file" limit and range cap.
const WHOLE: i64 = 1_000_000_000;

static SED_RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"sed\s+(?:-[a-zA-Z]+\s+)*-n\s+['"]?(\d+),(\d+)p['"]?\s+(.+)"#).unwrap()
});
static READ_CMD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:cat|head|tail|grep|egrep|fgrep|awk|type|more|less|Get-Content)\b(.*)")
        .unwrap()
});
static SEG_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[|;&]+").unwrap());
static PATHTOK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"'([^']+)'|"([^"]+)"|(\S+)"#).unwrap());

/// `_norm`: slash-normalise, strip surrounding quote chars, lowercase.
pub fn ft_norm(p: &str) -> String {
    p.replace('\\', "/")
        .trim_matches(|c| c == '\'' || c == '"')
        .to_lowercase()
}

/// `_path_tokens`: quoted or bare tokens that look like a path to a file —
/// not a flag, at least 4 chars, containing a separator, with a dot in the
/// basename. Yields the token as written (not normalised).
fn path_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for caps in PATHTOK.captures_iter(s) {
        let raw = caps
            .get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .map_or("", |m| m.as_str());
        let tok = py_strip(raw);
        if tok.starts_with('-') || char_len(tok) < 4 {
            continue;
        }
        let slashed = tok.replace('\\', "/");
        let base = slashed.rsplit('/').next().unwrap_or("");
        if (tok.contains('/') || tok.contains('\\')) && base.contains('.') {
            out.push(tok.to_string());
        }
    }
    out
}

/// `_bash_read_targets`: (raw_path, offset, limit) for each file-reading shell
/// segment. `sed -n 'A,Bp' F` maps to the [A,B] window (limit >= 1); the other
/// readers key the whole file (offset 1, limit 10**9).
pub fn bash_read_targets(cmd: &str) -> Vec<(String, i64, i64)> {
    let mut out = Vec::new();
    for seg in SEG_SPLIT.split(cmd) {
        let seg = py_strip(seg);
        if let Some(m) = SED_RANGE.captures(seg) {
            let (Some(lo), Some(hi)) = (
                m.get(1).and_then(|x| x.as_str().parse::<i64>().ok()),
                m.get(2).and_then(|x| x.as_str().parse::<i64>().ok()),
            ) else {
                continue; // non-ASCII digits / overflow: see module doc
            };
            let lim = (hi.saturating_sub(lo).saturating_add(1)).max(1);
            for tok in path_tokens(m.get(3).map_or("", |x| x.as_str())) {
                out.push((tok, lo, lim));
            }
            continue;
        }
        if let Some(m) = READ_CMD.captures(seg) {
            for tok in path_tokens(m.get(1).map_or("", |x| x.as_str())) {
                out.push((tok, 1, WHOLE));
            }
        }
    }
    out
}

/// `_rereq_ranges`: the chunk's served (file, lo, hi) ranges. A chunk with a
/// file and both line coords keys on its (basename) file; otherwise the bash
/// reads parsed out of its command key on the path as written. Empty = not an
/// observation for re-request purposes.
fn rereq_ranges(c: &Chunk) -> Vec<(String, i64, i64)> {
    match (c.file.as_deref().filter(|f| !f.is_empty()), c.lo, c.hi) {
        (Some(f), Some(lo), Some(hi)) => vec![(ft_norm(f), lo, hi)],
        _ => bash_read_targets(&c.cmd)
            .into_iter()
            .map(|(p, off, lim)| {
                let hi = off.saturating_add(lim).saturating_sub(1).min(WHOLE);
                (ft_norm(&p), off, hi)
            })
            .collect(),
    }
}

/// `rereq_features_base`: columns 0, 2, 3, 4 (1 and 5 are left 0 for the cut
/// pass) and, per chunk, the index of its most recent overlapping earlier copy.
pub fn rereq_base(chunks: &[Chunk]) -> (Vec<[f64; REREQ_WIDTH]>, Vec<Option<usize>>) {
    let n = chunks.len();
    let mut base = vec![[0.0f64; REREQ_WIDTH]; n];
    let mut mr_idx: Vec<Option<usize>> = vec![None; n];
    let rng: Vec<Vec<(String, i64, i64)>> = chunks.iter().map(rereq_ranges).collect();
    // file key -> ascending chunk indices holding a range on it. Walking these
    // in order visits exactly the j's the reference's full `for j in range(n)`
    // scan can hit, in the same order (a j with no range on the file never
    // hits), so npr and the first-max tie-break are unchanged.
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (j, rs) in rng.iter().enumerate() {
        for (f, _, _) in rs {
            let v = by_file.entry(f.as_str()).or_default();
            if v.last() != Some(&j) {
                v.push(j);
            }
        }
    }
    for i in 0..n {
        if rng[i].is_empty() {
            continue;
        }
        let ti = chunks[i].step;
        let mut npr: i64 = 0;
        let mut mr: Option<usize> = None;
        let mut mr_step = i64::MIN;
        for (fi, lo, hi) in &rng[i] {
            for &j in by_file.get(fi.as_str()).map_or(&[][..], |v| v.as_slice()) {
                if j == i || chunks[j].step >= ti {
                    continue;
                }
                let hit = rng[j]
                    .iter()
                    .any(|(fj, lo2, hi2)| fj == fi && *lo <= *hi2 && *lo2 <= *hi);
                if hit {
                    npr += 1;
                    if chunks[j].step > mr_step {
                        mr_step = chunks[j].step;
                        mr = Some(j);
                    }
                }
            }
        }
        base[i][0] = npr as f64;
        base[i][4] = if npr > 0 { 1.0 } else { 0.0 };
        mr_idx[i] = mr;
    }
    // ev = the re-request events (step, first file); counts only, so no sort.
    let ev: Vec<(i64, &str)> = (0..n)
        .filter(|&i| base[i][4] > 0.5)
        .map(|i| (chunks[i].step, rng[i][0].0.as_str()))
        .collect();
    for i in 0..n {
        if rng[i].is_empty() {
            continue;
        }
        let ti = chunks[i].step;
        let fi = rng[i][0].0.as_str();
        let earlier = ev.iter().filter(|(st, _)| *st < ti);
        base[i][2] = earlier.clone().filter(|(_, ff)| *ff == fi).count() as f64;
        base[i][3] = (earlier.count() as f64).ln_1p();
    }
    (base, mr_idx)
}

/// `rereq_apply_cut`: fill columns 1 and 5 from the cut state of each chunk's
/// most recent earlier copy. `is_cut(j)` answers "is chunk j cut", as of the
/// step being decided.
pub fn rereq_apply_cut(
    base: &[[f64; REREQ_WIDTH]],
    mr_idx: &[Option<usize>],
    is_cut: impl Fn(usize) -> bool,
) -> Vec<[f64; REREQ_WIDTH]> {
    let mut out = base.to_vec();
    for (i, row) in out.iter_mut().enumerate() {
        if let Some(j) = mr_idx[i] {
            let cut = if is_cut(j) { 1.0 } else { 0.0 };
            row[1] = cut;
            row[5] = if row[4] > 0.5 { cut } else { 0.0 };
        }
    }
    out
}

/// All six columns: `rereq_base` then `rereq_apply_cut`.
pub fn rereq_columns(chunks: &[Chunk], is_cut: impl Fn(usize) -> bool) -> Vec<[f64; REREQ_WIDTH]> {
    let (base, mr) = rereq_base(chunks);
    rereq_apply_cut(&base, &mr, is_cut)
}
