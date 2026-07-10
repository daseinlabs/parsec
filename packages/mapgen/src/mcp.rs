//! `dasein mcp` — a minimal MCP stdio server (JSON-RPC 2.0, newline-
//! delimited) exposing the scan tools to the `dasein:explore` agent.
//!
//! Hand-rolled on purpose: three tools need initialize / tools/list /
//! tools/call / ping and nothing else; a full SDK + tokio buys nothing for a
//! synchronous stdio loop and this is a committed, audited plugin binary
//! (DIRECTION.md §7b). Fail-open: a bad request gets a JSON-RPC error, the
//! loop never exits on malformed input.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    json!([
        {
            "name": "repo_map",
            "description": "Map of the repository's source files: detected package plus a depth-sorted file list. Start here to orient before outlining or reading files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": {"type": "string", "description": "Repo root (defaults to the server's working directory)"}
                }
            }
        },
        {
            "name": "file_outline",
            "description": "Signatures of every function/class in one source file with line numbers. Much cheaper than reading the file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "Path relative to the repo root"},
                    "root": {"type": "string"}
                },
                "required": ["file"]
            }
        },
        {
            "name": "find_symbol",
            "description": "Definition sites of a function/class by exact name, each with file:line and the region's key lines (control flow, raises, returns). Use to anchor findings to precise locations.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Exact symbol name"},
                    "root": {"type": "string"}
                },
                "required": ["name"]
            }
        }
    ])
}

fn root_of(args: &Value) -> PathBuf {
    args.get("root")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn call_tool(name: &str, args: &Value) -> Result<String, String> {
    let root = root_of(args);
    match name {
        "repo_map" => Ok(crate::scan::repo_map(&root, 200)),
        "file_outline" => {
            let file = args
                .get("file")
                .and_then(Value::as_str)
                .ok_or("missing 'file'")?;
            Ok(crate::scan::file_outline(&root, file, 200))
        }
        "find_symbol" => {
            let sym = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or("missing 'name'")?;
            Ok(crate::scan::find_symbol(&root, sym, 3))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn respond(out: &mut impl Write, id: &Value, result: Value) {
    let _ = writeln!(
        out,
        "{}",
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    );
    let _ = out.flush();
}

fn respond_err(out: &mut impl Write, id: &Value, code: i64, msg: &str) {
    let _ = writeln!(
        out,
        "{}",
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
    );
    let _ = out.flush();
}

pub fn serve_stdio() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        if id.is_null() {
            continue; // notification (e.g. notifications/initialized) — no response
        }
        match method {
            "initialize" => {
                let requested = msg
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL);
                respond(
                    &mut stdout,
                    &id,
                    json!({
                        "protocolVersion": requested,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "dasein-scout", "version": env!("CARGO_PKG_VERSION")}
                    }),
                );
            }
            "ping" => respond(&mut stdout, &id, json!({})),
            "tools/list" => respond(&mut stdout, &id, json!({"tools": tools()})),
            "tools/call" => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = msg
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                match call_tool(name, &args) {
                    Ok(text) => respond(
                        &mut stdout,
                        &id,
                        json!({"content": [{"type": "text", "text": text}], "isError": false}),
                    ),
                    Err(e) => respond(
                        &mut stdout,
                        &id,
                        json!({"content": [{"type": "text", "text": e}], "isError": true}),
                    ),
                }
            }
            _ => respond_err(&mut stdout, &id, -32601, "method not found"),
        }
    }
    Ok(())
}
