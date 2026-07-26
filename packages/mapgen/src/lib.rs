//! parsec-mapgen — the DETERMINISTIC core of codescout
//! (`adaptive-context-clean/bench/codescout.py` + `mapgen/map_one.py`),
//! exposed as MCP tools for the `parsec:explore` agent (free tier,
//! DIRECTION.md §3/§4.1).
//!
//! The reference pipeline was AST-grounding + paid LLM stages (scope →
//! analyze → synthesize) on our Vertex project. In the plugin, the LLM
//! stages ARE the explore subagent running on the user's own plan; this
//! crate ships only what codescout computed mechanically:
//! - repo map (source files of the detected package, depth-sorted, capped)
//! - file outlines (def/class signatures with line numbers)
//! - symbol lookup (def sites + the map_one.py "key lines" region)
//!
//! Multi-language via the SAME tree-sitter grammars as the engine
//! (engine::cst::language_for — single-sourced on purpose). Fail-open
//! everywhere: an unparseable file degrades to nothing, never an error that
//! breaks the agent.

pub mod mcp;
pub mod scan;
