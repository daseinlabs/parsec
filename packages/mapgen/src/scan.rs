//! Repo scanning, outlines, and symbol lookup — ports of codescout's
//! `_index`/`detect_pkg`/`_repo_map`/`_file_outline` and map_one.py's
//! `def_region`, generalized from Python-`ast` to tree-sitter so the free
//! tier works on arbitrary repos.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use dasein_engine::cst::language_for;
use tree_sitter::{Node, Parser};

/// Def-node classes for OUTLINES. Deliberately NOT engine::cst::kind_class:
/// that table is checkpoint-matched (its stale kinds, e.g. `fn_item` for
/// modern tree-sitter-rust's `function_item`, are baked into trained
/// features and must not change); this one just needs to find definitions.
fn def_class(kind: &str) -> &'static str {
    match kind {
        "function_definition"
        | "function_declaration"
        | "function_item"
        | "fn_item"
        | "method_declaration"
        | "method_definition"
        | "decorated_definition"
        | "method"
        | "constructor_declaration" => "func",
        "class_definition"
        | "class_declaration"
        | "class_specifier"
        | "struct_item"
        | "struct_specifier"
        | "impl_item"
        | "trait_item"
        | "interface_declaration"
        | "enum_item"
        | "enum_specifier"
        | "type_declaration" => "class",
        _ => "",
    }
}

/// Directories codescout never treats as the package (_PKG_EXCLUDE).
const PKG_EXCLUDE: &[&str] = &[
    "tests",
    "test",
    "testing",
    "docs",
    "doc",
    "examples",
    "example",
    "benchmarks",
    "asv_benchmarks",
    "maint_tools",
    "build_tools",
    "utils",
    "scripts",
    "ext",
    "tools",
];

static TEST_PATH: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"/tests?/|/test_|_test\.[A-Za-z0-9]+$").unwrap());

/// map_one.py KEY_LINE, extended with the equivalents of its Python-only
/// keywords for the other grammars we ship.
static KEY_LINE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^\s*(raise |assert |if |elif |else if |return |for |while |def |class |fn |func |throw |match |switch |case )",
    )
    .unwrap()
});

/// All source files (grammar-supported extensions) under root, .gitignore-
/// aware, test paths excluded, sorted by (depth, path length) like _repo_map.
pub fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .hidden(true)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| {
            let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy();
            language_for(&rel).is_some() && !TEST_PATH.is_match(&format!("/{rel}"))
        })
        .collect();
    out.sort_by_key(|p| {
        let rel = p
            .strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .to_string();
        (rel.matches('/').count(), rel.len(), rel)
    });
    out
}

/// codescout.detect_pkg: the top-level dir with the most source files,
/// excluding the non-package names; falls back to "src", then ".".
pub fn detect_pkg(root: &Path) -> String {
    let files = source_files(root);
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for f in &files {
        let rel = f.strip_prefix(root).unwrap_or(f);
        if let Some(top) = rel.components().next() {
            let name = top.as_os_str().to_string_lossy().to_string();
            if rel.components().count() > 1 && !PKG_EXCLUDE.contains(&name.as_str()) {
                *counts.entry(name).or_default() += 1;
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(name, _)| name)
        .unwrap_or_else(|| ".".into())
}

/// The repo map text: detected package + up to `cap` source files.
pub fn repo_map(root: &Path, cap: usize) -> String {
    let pkg = detect_pkg(root);
    let files = source_files(root);
    let listed: Vec<String> = files
        .iter()
        .take(cap)
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .to_string()
        })
        .collect();
    let more = files.len().saturating_sub(cap);
    let mut out = format!(
        "package: {pkg}\nsource files ({} total{}):\n{}",
        files.len(),
        if more > 0 {
            format!(", first {cap} shown")
        } else {
            String::new()
        },
        listed.join("\n")
    );
    if more > 0 {
        out.push_str(&format!("\n... and {more} more"));
    }
    out
}

/// One definition found in a file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Def {
    pub file: String,
    pub line: usize, // 1-based
    pub end_line: usize,
    pub class: &'static str, // func | class
    pub signature: String,
}

fn node_name(n: Node, src: &str) -> Option<String> {
    // decorated_definition wraps the real def; look one level down.
    let target = if n.kind() == "decorated_definition" {
        (0..n.named_child_count()).find_map(|i| {
            let c = n.named_child(i)?;
            matches!(def_class(c.kind()), "func" | "class").then_some(c)
        })?
    } else {
        n
    };
    let name = target.child_by_field_name("name")?;
    Some(src[name.byte_range()].to_string())
}

fn walk_defs(node: Node, src: &str, rel: &str, out: &mut Vec<(String, Def)>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let class = def_class(child.kind());
        if matches!(class, "func" | "class") {
            if let Some(name) = node_name(child, src) {
                let line = child.start_position().row + 1;
                let sig = src.lines().nth(line - 1).unwrap_or("").trim().to_string();
                out.push((
                    name,
                    Def {
                        file: rel.to_string(),
                        line,
                        end_line: child.end_position().row + 1,
                        class,
                        signature: sig,
                    },
                ));
            }
        }
        walk_defs(child, src, rel, out); // methods, nested defs
    }
}

/// All (name, def) pairs in one file; empty on parse failure (fail-open).
pub fn file_defs(root: &Path, rel: &str) -> Vec<(String, Def)> {
    let Some(lang) = language_for(rel) else {
        return Vec::new();
    };
    let Ok(src) = std::fs::read_to_string(root.join(rel)) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(&src, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_defs(tree.root_node(), &src, rel, &mut out);
    out
}

/// codescout._file_outline: signatures with line numbers, capped.
pub fn file_outline(root: &Path, rel: &str, cap: usize) -> String {
    let defs = file_defs(root, rel);
    if defs.is_empty() {
        return format!("# outline of {rel}\n(no definitions found)");
    }
    let body: Vec<String> = defs
        .iter()
        .take(cap)
        .map(|(_, d)| format!("L{}: {}", d.line, d.signature))
        .collect();
    format!("# outline of {rel}\n{}", body.join("\n"))
}

/// map_one.def_region: the def's start plus up to 14 "key lines" (control
/// flow / assertions / nested defs) within the region, each with its true
/// line number — enough to anchor a map entry without dumping the body.
pub fn key_lines(root: &Path, def: &Def, max_keys: usize) -> Vec<(usize, String)> {
    let Ok(src) = std::fs::read_to_string(root.join(&def.file)) else {
        return Vec::new();
    };
    let lines: Vec<&str> = src.lines().collect();
    let mut keys = Vec::new();
    if let Some(first) = lines.get(def.line - 1) {
        keys.push((def.line, first.trim().to_string()));
    }
    let end = def.end_line.min(def.line + 220).min(lines.len());
    for (idx, ln) in lines.iter().enumerate().take(end).skip(def.line) {
        if KEY_LINE.is_match(ln) && keys.len() < max_keys {
            keys.push((idx + 1, ln.trim().to_string()));
        }
    }
    keys
}

/// Find definition sites for `name` across the repo (codescout._index
/// lookup + map_one enrichment), capped.
pub fn find_symbol(root: &Path, name: &str, max_sites: usize) -> String {
    let mut sites: Vec<Def> = Vec::new();
    for f in source_files(root) {
        let rel = f
            .strip_prefix(root)
            .unwrap_or(&f)
            .to_string_lossy()
            .to_string();
        for (n, d) in file_defs(root, &rel) {
            if n == name {
                sites.push(d);
            }
        }
        if sites.len() >= max_sites {
            break;
        }
    }
    if sites.is_empty() {
        return format!("no definition of `{name}` found (searched non-test source files)");
    }
    let mut out = Vec::new();
    for d in sites.iter().take(max_sites) {
        let keys = key_lines(root, d, 14);
        let key_txt: Vec<String> = keys.iter().map(|(n, t)| format!("  L{n}: {t}")).collect();
        out.push(format!(
            "{} {} — {}:{}\n{}",
            d.class,
            name,
            d.file,
            d.line,
            key_txt.join("\n")
        ));
    }
    out.join("\n\n")
}
