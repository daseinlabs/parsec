//! Port of `adaptive_context/optimizer/cst_chunk.py` — the tree-sitter
//! semantic read-chunker (Arm-2 alternative to fixed line windows).
//!
//! Error-tolerant by design: returns None (caller falls back to line windows)
//! for unknown languages, parse failure, or output with no recoverable
//! structure — this is also the fail-open path when a grammar isn't compiled
//! in. Grammar coverage note: the Python reference links tree-sitter-language-
//! pack (100+ grammars); we compile in the top languages below and fall back
//! to windows elsewhere. Grammar crate versions are part of the checkpoint's
//! matched-pair contract — a grammar bump can shift node boundaries and must
//! be treated like an embedder change, not a routine dep update.

use tree_sitter::{Language, Parser};

pub const DEFAULT_MAX_LINES: usize = 80;

/// Grammar for a filename, by extension. Public so mapgen shares the SAME
/// grammar table/versions (checkpoint-matched — see module doc); two tables
/// would drift.
pub fn language_for(file: &str) -> Option<Language> {
    let ext = file.rsplit('.').next()?.to_lowercase();
    let lang: Language = match ext.as_str() {
        "py" | "pyi" => tree_sitter_python::LANGUAGE.into(),
        "js" | "jsx" | "mjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c" | "h" => tree_sitter_c::LANGUAGE.into(),
        "cc" | "cpp" | "cxx" | "hpp" => tree_sitter_cpp::LANGUAGE.into(),
        _ => return None,
    };
    Some(lang)
}

/// Coarse structural class per top-level node kind (mirrors _KIND_CLASS).
pub fn kind_class(kind: &str) -> &'static str {
    match kind {
        "function_definition"
        | "function_declaration"
        | "method_declaration"
        | "fn_item"
        | "decorated_definition"
        | "method"
        | "constructor_declaration" => "func",
        "class_definition"
        | "class_declaration"
        | "struct_item"
        | "impl_item"
        | "trait_item"
        | "interface_declaration"
        | "enum_item" => "class",
        "import_statement"
        | "import_from_statement"
        | "import_declaration"
        | "use_declaration"
        | "preproc_include" => "import",
        _ => "body",
    }
}

/// cst_chunk.cst_read_atoms: [(orig_lineno, text)] for one code read ->
/// Some([(lo, hi, text, struct_class)]) semantic atoms, or None meaning
/// "fall back to line windows".
pub fn cst_read_atoms(
    file: Option<&str>,
    coord_lines: &[(i64, &str)],
    max_lines: usize,
) -> Option<Vec<(i64, i64, String, String)>> {
    let file = file?;
    // The Python reference uses os.path.splitext: no extension -> no language.
    if !file.contains('.') {
        return None;
    }
    let lang = language_for(file)?;
    let n = coord_lines.len();
    if n < 4 {
        return None;
    }
    let src = coord_lines
        .iter()
        .map(|(_, t)| *t)
        .collect::<Vec<_>>()
        .join("\n");
    let mut parser = Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(&src, None)?;
    let root = tree.root_node();
    let ncc = root.named_child_count();
    if ncc < 1 {
        return None;
    }
    let clamp = |r: usize| -> usize { r.min(n - 1) };
    let mut children: Vec<(usize, usize, &'static str)> = Vec::with_capacity(ncc);
    for i in 0..ncc {
        let c = root.named_child(i)?;
        let sr = clamp(c.start_position().row);
        let er = std::cmp::max(sr, clamp(c.end_position().row));
        children.push((sr, er, kind_class(c.kind())));
    }
    children.sort();

    let mut blocks: Vec<(usize, usize, &'static str)> = Vec::new();
    let emit =
        |a: usize, b: usize, kls: &'static str, blocks: &mut Vec<(usize, usize, &'static str)>| {
            let mut r = a;
            while r <= b {
                let e = std::cmp::min(b, r + max_lines - 1);
                blocks.push((r, e, kls));
                r = e + 1;
            }
        };
    let mut cursor = 0usize;
    for (sr, er, kls) in &children {
        if *sr > cursor {
            emit(cursor, sr - 1, "body", &mut blocks);
        }
        emit(*sr, *er, kls, &mut blocks);
        cursor = std::cmp::max(cursor, er + 1);
    }
    if cursor < n {
        emit(cursor, n - 1, "body", &mut blocks);
    }
    if blocks.len() <= 1 {
        return None; // no granularity gained
    }
    if blocks.len() as f64 > 0.6 * n as f64 {
        return None; // atoms ~= lines => output mis-read as a file
    }
    let mut out = Vec::new();
    for (a, b, kls) in blocks {
        let seg = &coord_lines[a..=b];
        if !seg.is_empty() {
            out.push((
                seg[0].0,
                seg[seg.len() - 1].0,
                seg.iter().map(|(_, t)| *t).collect::<Vec<_>>().join("\n"),
                kls.to_string(),
            ));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}
