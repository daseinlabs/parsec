//! `dasein statusline` — reads the Claude Code statusline JSON on stdin and
//! prints one line. Savings numbers come only from what the hook actually
//! measured (blocked re-reads x on-disk bytes of the denied range) — never a
//! modeled baseline (DIRECTION.md §8.4).

use serde_json::Value;

use crate::noreread::{load_session, sessions_dir, SessionState};

fn fmt_tokens(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

pub fn run() -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let model = payload
        .pointer("/model/display_name")
        .and_then(Value::as_str)
        .unwrap_or("claude");
    let dir = payload
        .pointer("/workspace/current_dir")
        .and_then(Value::as_str)
        .map(|d| d.rsplit('/').next().unwrap_or(d).to_string())
        .unwrap_or_default();
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let st = load_session(session_id);

    let dasein = if st.blocked_rereads == 0 && st.loops_broken == 0 {
        "⌁ dasein watching".to_string()
    } else {
        let mut parts = Vec::new();
        if st.blocked_rereads > 0 {
            parts.push(format!(
                "{} re-read{} blocked · ~{} tok saved",
                st.blocked_rereads,
                if st.blocked_rereads == 1 { "" } else { "s" },
                fmt_tokens(st.tokens_saved)
            ));
        }
        if st.loops_broken > 0 {
            parts.push(format!(
                "{} loop{} broken",
                st.loops_broken,
                if st.loops_broken == 1 { "" } else { "s" }
            ));
        }
        format!("⌁ dasein {}", parts.join(" · "))
    };
    println!("{model} · {dir} · {dasein}");
    Ok(())
}

/// `dasein savings` — aggregate report over recent sessions for the
/// /dasein-savings skill. Numbers come from the hook's measured counters
/// only; if there is nothing recorded we say so rather than estimate.
pub fn savings_report() -> anyhow::Result<()> {
    let mut sessions: Vec<(String, std::time::SystemTime, SessionState)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(sessions_dir()) {
        for e in entries.flatten() {
            let name = e
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".json")
                .to_string();
            let modified = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if let Ok(data) = std::fs::read_to_string(e.path()) {
                if let Ok(st) = serde_json::from_str::<SessionState>(&data) {
                    sessions.push((name, modified, st));
                }
            }
        }
    }
    if sessions.is_empty() {
        println!("No dasein activity recorded yet — the ledger is empty.");
        return Ok(());
    }
    sessions.sort_by(|a, b| b.1.cmp(&a.1));
    let (mut blocked, mut tokens, mut loops) = (0u64, 0u64, 0u64);
    for (_, _, st) in &sessions {
        blocked += st.blocked_rereads;
        tokens += st.tokens_saved;
        loops += st.loops_broken;
    }
    println!(
        "dasein savings — {} session(s): {} re-reads blocked, ~{} tokens saved, {} command loops broken",
        sessions.len(),
        blocked,
        fmt_tokens(tokens),
        loops
    );
    for (name, _, st) in sessions.iter().take(10) {
        if st.blocked_rereads > 0 || st.loops_broken > 0 {
            println!(
                "  {}: {} re-reads blocked (~{} tok), {} loops",
                &name[..name.len().min(12)],
                st.blocked_rereads,
                fmt_tokens(st.tokens_saved),
                st.loops_broken
            );
        }
    }
    Ok(())
}
