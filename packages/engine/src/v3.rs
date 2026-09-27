//! brain-api/v3 row layouts (HS curator, spec v6828t3): the 104-col decided
//! row, and the extension class the brain's read columns need.
//!
//! Decided row, 0-indexed (gnn-train hs/README.md §3.4):
//!   struct_features 0–15 | rereq 16–21 | read columns 22–71 | age 72 |
//!   extra 73–75 | readmit 76–79 | issuematch 80–84 | rerank 85–92 |
//!   centrality 93–97 | scout 98–101 | dupcos 102–103
//! = the 49-col v2 row (`readout::decided_struct`) minus its changeprone
//! column 42 — this lineage never had it — with the re-request block after
//! column 15 and the 50 read columns after that. The client fills everything
//! except the read columns and dupcos, which it leaves zero for the brain.

use crate::readout::READ_STRUCT;
use crate::rereq::REREQ_WIDTH;

/// Decided-row width on the v3 wire.
pub const READ_STRUCT_V3: usize = 104;
/// The read-column block width (brain-filled).
pub const READCOLS_WIDTH: usize = 50;

/// v2-row column of the changeprone value, absent from the v3 row.
const V2_CHANGEPRONE: usize = 42;
/// First v3 column after the read-column block (age).
const V3_AGE: usize = 16 + REREQ_WIDTH + READCOLS_WIDTH;

/// The 30 extensions the checkpoint one-hot encodes
/// (torch_curator.READCOLS_EXTS = PATH_V9_EXTS + 14).
pub const READCOLS_EXTS: [&str; 30] = [
    "gml", "ps1", "md", "txt", "json", "py", "png", "log", "yy", "jsonl", "sh", "diff", "html",
    "yyp", "patch", "js", "ts", "tsx", "rs", "go", "java", "c", "h", "cpp", "yaml", "yml", "toml",
    "rb", "cs", "css",
];

/// The extension class `read_columns` one-hot encodes for a chunk's file:
/// one of `READCOLS_EXTS`, "other", or "none" (no file, or a basename with no
/// extension — `_path_v9_ext`: lowercase text after the last dot, none for
/// a leading-dot dotfile).
pub fn ext_class(file: Option<&str>) -> &'static str {
    let Some(bn) = file.filter(|f| !f.is_empty()) else {
        return "none";
    };
    let b = bn.to_lowercase();
    if !b.contains('.') || b.starts_with('.') {
        return "none";
    }
    let ext = b.rsplit('.').next().unwrap_or("");
    if ext.is_empty() {
        return "none";
    }
    READCOLS_EXTS
        .iter()
        .find(|&&e| e == ext)
        .copied()
        .unwrap_or("other")
}

/// One v3 decided row from the v2 row and the chunk's re-request columns.
pub fn decided_row(v2: &[f32; READ_STRUCT], rereq: &[f64; REREQ_WIDTH]) -> [f32; READ_STRUCT_V3] {
    let mut row = [0.0f32; READ_STRUCT_V3];
    row[..16].copy_from_slice(&v2[..16]);
    for (k, v) in rereq.iter().enumerate() {
        row[16 + k] = *v as f32;
    }
    // v2 16..42 (age .. centrality) and 43..49 (scout, dupcos) follow the
    // read columns contiguously once changeprone is gone.
    let tail: Vec<f32> = v2[16..V2_CHANGEPRONE]
        .iter()
        .chain(&v2[V2_CHANGEPRONE + 1..])
        .copied()
        .collect();
    row[V3_AGE..].copy_from_slice(&tail);
    // dupcos stays the brain's to fill, exactly as on v2 (the v2 row carries
    // zeros there already when built without content embeddings).
    row[READ_STRUCT_V3 - 2] = 0.0;
    row[READ_STRUCT_V3 - 1] = 0.0;
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decided_row_layout() {
        let mut v2 = [0.0f32; READ_STRUCT];
        for (i, x) in v2.iter_mut().enumerate() {
            *x = i as f32 + 1.0; // column c holds c+1
        }
        let rr = [10.0, 11.0, 12.0, 13.0, 14.0, 15.0];
        let row = decided_row(&v2, &rr);
        assert_eq!(&row[..16], &v2[..16], "struct features");
        assert_eq!(&row[16..22], &[10.0, 11.0, 12.0, 13.0, 14.0, 15.0], "rereq");
        assert!(row[22..72].iter().all(|&x| x == 0.0), "read columns: brain");
        assert_eq!(row[72], 17.0, "age = v2 col 16");
        assert_eq!(&row[73..76], &[18.0, 19.0, 20.0], "extra = v2 17-19");
        assert_eq!(
            &row[76..80],
            &[21.0, 22.0, 23.0, 24.0],
            "readmit = v2 20-23"
        );
        assert_eq!(row[80], 25.0, "issuematch starts at v2 24");
        assert_eq!(row[85], 30.0, "rerank starts at v2 29");
        assert_eq!(
            &row[93..98],
            &[38.0, 39.0, 40.0, 41.0, 42.0],
            "centrality = v2 37-41"
        );
        assert_eq!(
            &row[98..102],
            &[44.0, 45.0, 46.0, 47.0],
            "scout = v2 43-46; changeprone gone"
        );
        assert_eq!(&row[102..], &[0.0, 0.0], "dupcos: brain");
    }
}
