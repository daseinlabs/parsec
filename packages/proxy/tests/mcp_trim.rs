//! End-to-end trim tools through the real `parsec mcp` stdio server
//! (CARGO_BIN_EXE_parsec), isolated HOME tempdir per test like trim_cli.rs.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct TempHome(PathBuf);

impl TempHome {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("parsec-mcp-trim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        // canonicalize: macOS temp_dir lives behind the /var -> /private/var
        // symlink, and the binary keys the pending file on the RESOLVED cwd.
        TempHome(dir.canonicalize().unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn proj(&self) -> PathBuf {
        self.0.join("proj")
    }

    fn write_transcript(&self, name: &str, lines: &[Value]) -> PathBuf {
        let p = self.0.join(name);
        let text = lines
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&p, text).unwrap();
        p
    }

    fn pending(&self) -> PathBuf {
        let key: String = self
            .proj()
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        self.0
            .join(".parsec")
            .join("trim")
            .join(format!("{key}.json"))
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn asst(cmd: &str) -> Value {
    json!({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": "t", "name": "Bash", "input": {"command": cmd}}]}})
}

fn tool(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t", "content": text}]}})
}

fn lines(prefix: &str, lo: usize, hi: usize) -> String {
    (lo..=hi)
        .map(|i| format!("{prefix}_{i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn keepable_session() -> Vec<Value> {
    let mut v = vec![
        asst("cat plan.md"),
        tool(&lines("plan_item_number", 1, 60)),
        asst("sed -n '1,10p' ctrl.py"),
        tool(&lines("ctrl_source_line", 1, 10)),
    ];
    for i in 0..4 {
        v.push(asst(&format!("echo filler_{i}")));
        v.push(tool("ok"));
    }
    v.push(asst("sed -n '3,4p' ctrl.py"));
    v.push(tool("ctrl_source_line_3\nctrl_source_line_4"));
    v.push(asst("echo done"));
    v.push(tool("done"));
    v
}

struct McpServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpServer {
    fn spawn(home: &TempHome) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_parsec"))
            .arg("mcp")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("PARSEC_TRIM_TTL_SECS")
            .env_remove("PARSEC_TRIM_MAX_EST_TOKENS")
            .env_remove("PARSEC_TRIM_LEVEL")
            .current_dir(home.proj())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn parsec mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut s = McpServer {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let init = s.request("initialize", json!({"protocolVersion": "2025-06-18"}));
        assert_eq!(init["serverInfo"]["name"], "parsec-scout");
        s
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(line.trim()).expect("one JSON-RPC response line");
        assert_eq!(resp["id"], self.next_id, "response id mismatch: {resp}");
        resp.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("error response: {resp}"))
    }

    /// tools/call, returning (text, isError).
    fn call(&mut self, name: &str, args: Value) -> (String, bool) {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        (
            r["content"][0]["text"].as_str().unwrap_or("").to_string(),
            r["isError"].as_bool().unwrap_or(false),
        )
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn tools_list_includes_trim_tools() {
    let home = TempHome::new("list");
    let mut s = McpServer::spawn(&home);
    let r = s.request("tools/list", json!({}));
    let names: Vec<&str> = r["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "repo_map",
        "file_outline",
        "find_symbol",
        "trim_stage",
        "trim_finalize",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
}

#[test]
fn stage_then_finalize_roundtrip_with_level() {
    let home = TempHome::new("roundtrip");
    let tp = home.write_transcript("sess.jsonl", &keepable_session());
    let mut s = McpServer::spawn(&home);

    let (text, is_err) = s.call(
        "trim_stage",
        json!({"transcript": tp.to_string_lossy(), "level": 5}),
    );
    assert!(!is_err, "{text}");
    assert!(text.contains("staged (status: det, level 5)"), "{text}");
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(home.pending()).unwrap()).unwrap();
    assert_eq!(payload["status"], "det");
    assert_eq!(payload["level"], 5);
    assert!(payload["body"]
        .as_str()
        .unwrap()
        .contains("ctrl_source_line_3"));

    let (text, is_err) = s.call(
        "trim_finalize",
        json!({"directives": "- keep fixing ctrl.py\n- never guess line numbers"}),
    );
    assert!(!is_err, "{text}");
    assert!(text.contains("ready:"), "{text}");
    assert!(text.contains("/clear"), "{text}");
    let payload: Value =
        serde_json::from_str(&std::fs::read_to_string(home.pending()).unwrap()).unwrap();
    assert_eq!(payload["status"], "ready");
    assert_eq!(payload["level"], 5, "finalize must preserve the level");
    assert!(payload["directives"]
        .as_str()
        .unwrap()
        .contains("never guess line numbers"));
}

#[test]
fn short_session_is_plain_outcome_and_bad_args_are_errors() {
    let home = TempHome::new("edges");
    let mut msgs = Vec::new();
    for i in 0..5 {
        msgs.push(asst(&format!("cat f{i}.py")));
        msgs.push(tool("body\nlines"));
    }
    let tp = home.write_transcript("short.jsonl", &msgs);
    let mut s = McpServer::spawn(&home);

    // nothing-to-stage is an expected outcome, not an error
    let (text, is_err) = s.call("trim_stage", json!({"transcript": tp.to_string_lossy()}));
    assert!(!is_err, "{text}");
    assert!(text.contains("too short"), "{text}");
    assert!(!home.pending().exists());

    // out-of-range level is an error
    let (text, is_err) = s.call(
        "trim_stage",
        json!({"transcript": tp.to_string_lossy(), "level": 6}),
    );
    assert!(is_err, "{text}");
    assert!(text.contains("1-5"), "{text}");

    // finalize without a staged trim is an error
    let (text, is_err) = s.call("trim_finalize", json!({"directives": "- x"}));
    assert!(is_err, "{text}");
    assert!(text.contains("no staged trim"), "{text}");

    // and the server is still alive for scan tools after errors
    let r = s.request("tools/list", json!({}));
    assert!(r["tools"].as_array().unwrap().len() >= 5);
}
