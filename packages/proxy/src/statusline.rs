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

/// §8.4 aggregation over savings-ledger rows: tokens saved = counterfactual
/// − all billed input-side tokens (uncached + cache read + cache write),
/// summed SIGNED over probed rows only. Null-probe rows are excluded and
/// counted — never estimated. Mirrors packages/bench ledger math.
#[derive(Default, Debug, PartialEq)]
struct LedgerAgg {
    rows: u64,
    probed: u64,
    null_probes: u64,
    fail_open: u64,
    scorer_fail_opens: u64,
    counterfactual: i64,
    billed_input_side: i64,
    cache_read: i64,
    cache_write: i64,
    saved: i64,
    freeze_cut: i64,
    convs: std::collections::HashSet<String>,
    /// model id -> (probed requests, counterfactual, saved) — token-
    /// denominated per model; dollarize with packages/bench pricing.
    by_model: std::collections::BTreeMap<String, (u64, i64, i64)>,
}

fn aggregate_ledger(lines: &str) -> LedgerAgg {
    let mut a = LedgerAgg::default();
    for line in lines.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if row.get("contract_version").and_then(Value::as_str) != Some("savings-ledger/v0") {
            continue;
        }
        a.rows += 1;
        let g = |k: &str| row.get(k).and_then(Value::as_i64).unwrap_or(0);
        if let Some(c) = row.get("conv_id").and_then(Value::as_str) {
            a.convs.insert(c.to_string());
        }
        if row
            .get("fail_open")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            a.fail_open += 1;
        }
        a.scorer_fail_opens += g("scorer_fail_opens") as u64;
        a.freeze_cut += g("freeze_cut_tokens");
        a.cache_read += g("billed_cache_read_tokens");
        a.cache_write += g("billed_cache_write_tokens");
        match row
            .get("counterfactual_input_tokens")
            .and_then(Value::as_i64)
        {
            Some(cf) => {
                let billed = g("billed_input_tokens")
                    + g("billed_cache_read_tokens")
                    + g("billed_cache_write_tokens");
                a.probed += 1;
                a.counterfactual += cf;
                a.billed_input_side += billed;
                a.saved += cf - billed;
                let model = row
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("(unrecorded)")
                    .to_string();
                let m = a.by_model.entry(model).or_default();
                m.0 += 1;
                m.1 += cf;
                m.2 += cf - billed;
            }
            None => a.null_probes += 1,
        }
    }
    a
}

fn ledger_file() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home)
        .join(".dasein")
        .join("ledger.jsonl")
}

/// `dasein savings` — the measured roll-up for the /dasein-savings skill:
/// the proxy's savings ledger (§8.4 counterfactual vs billed) plus the
/// free-tier hook counters. Nothing here is ever estimated.
pub fn savings_report() -> anyhow::Result<()> {
    let mut printed = false;

    if let Ok(data) = std::fs::read_to_string(ledger_file()) {
        let a = aggregate_ledger(&data);
        if a.rows > 0 {
            printed = true;
            let pct = if a.counterfactual > 0 {
                100.0 * a.saved as f64 / a.counterfactual as f64
            } else {
                0.0
            };
            println!(
                "proxy ledger — {} request(s) / {} conversation(s):",
                a.rows,
                a.convs.len()
            );
            println!(
                "  input: {} counterfactual vs {} billed(+cache) → {}{} tok saved ({:.1}%)",
                a.counterfactual,
                a.billed_input_side,
                if a.saved < 0 { "" } else { "~" },
                a.saved,
                pct
            );
            let ratio = if a.cache_write > 0 {
                format!("{:.1}:1", a.cache_read as f64 / a.cache_write as f64)
            } else {
                "n/a".into()
            };
            println!(
                "  cache: {} read / {} write ({}) · freezer trimmed ~{} tok (diagnostic)",
                a.cache_read, a.cache_write, ratio, a.freeze_cut
            );
            for (model, (n, cf, saved)) in &a.by_model {
                let pct = if *cf > 0 {
                    100.0 * *saved as f64 / *cf as f64
                } else {
                    0.0
                };
                println!(
                    "    {model}: {n} req · {saved} tok saved ({pct:.1}%) — dollarize \
                     with packages/bench pricing"
                );
            }
            if a.fail_open > 0 || a.scorer_fail_opens > 0 || a.null_probes > 0 {
                println!(
                    "  fail-open: {} request(s), {} scorer step(s) · {} null probe(s) excluded (§8.4)",
                    a.fail_open, a.scorer_fail_opens, a.null_probes
                );
            }
        }
    }

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
    if !sessions.is_empty() {
        printed = true;
        sessions.sort_by(|a, b| b.1.cmp(&a.1));
        let (mut blocked, mut tokens, mut loops) = (0u64, 0u64, 0u64);
        for (_, _, st) in &sessions {
            blocked += st.blocked_rereads;
            tokens += st.tokens_saved;
            loops += st.loops_broken;
        }
        println!(
            "hook — {} session(s): {} re-reads blocked, ~{} tokens saved, {} command loops broken",
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
    }

    if !printed {
        println!("No dasein activity recorded yet — no ledger rows and no hook sessions.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_aggregation_is_honest() {
        let lines = concat!(
            r#"{"contract_version":"savings-ledger/v0","conv_id":"a","model":"claude-sonnet-5","counterfactual_input_tokens":467,"billed_input_tokens":163,"billed_cache_read_tokens":50,"billed_cache_write_tokens":7,"fail_open":false,"freeze_cut_tokens":318}"#,
            "\n",
            // null probe: EXCLUDED from savings, counted (§8.4)
            r#"{"contract_version":"savings-ledger/v0","conv_id":"a","counterfactual_input_tokens":null,"billed_input_tokens":10,"billed_cache_read_tokens":0,"billed_cache_write_tokens":0,"fail_open":false}"#,
            "\n",
            // fail-open row with a probe: counted in savings AND flagged
            r#"{"contract_version":"savings-ledger/v0","conv_id":"b","counterfactual_input_tokens":100,"billed_input_tokens":120,"billed_cache_read_tokens":0,"billed_cache_write_tokens":0,"fail_open":true,"scorer_fail_opens":2}"#,
            "\n",
            "not json\n",
        );
        let a = aggregate_ledger(lines);
        assert_eq!(a.rows, 3);
        assert_eq!(a.probed, 2);
        assert_eq!(a.null_probes, 1);
        assert_eq!(a.fail_open, 1);
        assert_eq!(a.scorer_fail_opens, 2);
        assert_eq!(a.saved, (467 - 220) + (100 - 120)); // signed, never clamped
        assert_eq!(a.convs.len(), 2);
        assert_eq!(a.freeze_cut, 318);
        assert_eq!(a.by_model["claude-sonnet-5"], (1, 467, 247));
        assert_eq!(a.by_model["(unrecorded)"], (1, 100, -20));
    }
}
