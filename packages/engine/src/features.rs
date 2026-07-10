//! Structural feature extraction for brain-API scoring requests.
//! Emits vectors + features only — raw text never leaves this machine.
//!
//! Port of chunking.dup_feats_fast. The Python version bucketed by `hash()`
//! (PYTHONHASHSEED-randomized) and re-verified with `==`, so its OUTPUT was
//! deterministic even though the buckets weren't; we bucket by content
//! directly, which is the same equivalence relation.

use std::collections::HashMap;

use crate::chunking::Chunk;

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
