//! `dasein statusline` — reads the Claude Code statusline JSON on stdin and
//! prints one line. Savings numbers come only from what the hook actually
//! measured (blocked re-reads x on-disk bytes of the denied range) — never a
//! modeled baseline (DIRECTION.md §8.4).

use serde_json::Value;

use crate::noreread::{load_session, sessions_dir, SessionState};

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
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
    // This session's proxy savings (§8.4 counterfactual math, signed — a
    // negative session shows as overhead, never clamped). Whole-ledger read:
    // a few hundred bytes per request keeps this inside the statusline
    // render budget; switch to a pre-aggregated per-session state file if
    // the ledger ever outgrows that.
    if let Some(saved) = std::fs::read_to_string(ledger_file())
        .ok()
        .and_then(|d| session_saved_from_lines(&d, session_id))
    {
        parts.push(if saved >= 0 {
            format!("proxy ~{} tok saved", fmt_tokens(saved as u64))
        } else {
            format!("proxy {saved} tok (overhead)")
        });
    }
    let mut dasein = if parts.is_empty() {
        "⌁ dasein watching".to_string()
    } else {
        format!("⌁ dasein {}", parts.join(" · "))
    };
    if let Some(note) = setup_note() {
        dasein.push_str(" · ");
        dasein.push_str(&note);
    }
    println!("{model} · {dir} · {dasein}");
    Ok(())
}

/// First-run setup / proxy-health note for the status line. States, in the
/// order a fresh install moves through them: downloading (with measured
/// progress), ready-pending-restart, then — once routed — a proxy-down
/// warning if the autostarted proxy ever dies mid-session. A routed session
/// with a live proxy shows nothing extra: healthy is the quiet state.
fn setup_note() -> Option<String> {
    // Routed = this session's env points at a local dasein proxy. The
    // statusline inherits the session env, so this is authoritative.
    let routed_port = std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .and_then(|u| crate::hook::local_proxy_port(&u));
    if let Some(port) = routed_port {
        // Loopback connect resolves in microseconds either way; the
        // statusline render budget is safe.
        let up = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            std::time::Duration::from_millis(100),
        )
        .is_ok();
        // A live port means the supervisor is up; a dead worker behind it is
        // invisible here (the supervisor answers and falls back to Anthropic),
        // which is correct — the session keeps working. Only a dead SUPERVISOR
        // shows through, and that is the case `dasein up` fixes.
        return (!up).then(|| format!("proxy DOWN (127.0.0.1:{port} — run `dasein up`)"));
    }
    let st = crate::setup::load_state()?;
    match st.phase.as_str() {
        // No byte progress since the model download went away — setup is
        // now just settings routing, which is effectively instant.
        "spawned" | "routing" => Some("setting up".to_string()),
        "ready" if st.env_written => Some("restart to activate curation".to_string()),
        "failed" => Some("setup failed — run `dasein setup`".to_string()),
        _ => None,
    }
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
    /// Distinct client sessions (rows carrying the optional session_id) —
    /// one session spans the several conv_ids compaction/subagents mint.
    sessions: std::collections::HashSet<String>,
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
        if let Some(s) = row.get("session_id").and_then(Value::as_str) {
            a.sessions.insert(s.to_string());
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

/// Signed proxy savings for ONE client session: Σ (counterfactual − billed
/// input-side) over that session's probed rows — the same §8.4 math as
/// aggregate_ledger, filtered by the row's optional session_id. Null probes
/// are excluded, never estimated. None when the session has no probed rows
/// (pre-session_id ledgers land here and the statusline simply omits it).
fn session_saved_from_lines(lines: &str, session_id: &str) -> Option<i64> {
    let mut probed = 0u64;
    let mut saved = 0i64;
    for line in lines.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if row.get("contract_version").and_then(Value::as_str) != Some("savings-ledger/v0")
            || row.get("session_id").and_then(Value::as_str) != Some(session_id)
        {
            continue;
        }
        if let Some(cf) = row
            .get("counterfactual_input_tokens")
            .and_then(Value::as_i64)
        {
            let g = |k: &str| row.get(k).and_then(Value::as_i64).unwrap_or(0);
            probed += 1;
            saved += cf
                - (g("billed_input_tokens")
                    + g("billed_cache_read_tokens")
                    + g("billed_cache_write_tokens"));
        }
    }
    (probed > 0).then_some(saved)
}

fn ledger_file() -> std::path::PathBuf {
    crate::setup::home_dir()
        .join(".dasein")
        .join("ledger.jsonl")
}

/// One-line lifetime roll-up for the SessionStart notice (the doc's one-time
/// awareness message: docs/plugin-user-messaging.md Part 1 §3). None when
/// nothing has been measured yet — a fresh install stays quiet. The numbers
/// are the same aggregates `dasein savings` prints: the proxy figure is the
/// §8.4 counterfactual sum (signed, never clamped), the hook figure keeps
/// its "~" because it is the on-disk-bytes approximation.
pub fn lifetime_note() -> Option<String> {
    let ledger = std::fs::read_to_string(ledger_file()).ok();
    let (mut blocked, mut tokens, mut loops) = (0u64, 0u64, 0u64);
    if let Ok(entries) = std::fs::read_dir(sessions_dir()) {
        for e in entries.flatten() {
            if let Ok(data) = std::fs::read_to_string(e.path()) {
                if let Ok(st) = serde_json::from_str::<SessionState>(&data) {
                    blocked += st.blocked_rereads;
                    tokens += st.tokens_saved;
                    loops += st.loops_broken;
                }
            }
        }
    }
    lifetime_note_from(ledger.as_deref(), blocked, tokens, loops)
}

fn lifetime_note_from(
    ledger_lines: Option<&str>,
    blocked: u64,
    tokens: u64,
    loops: u64,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(lines) = ledger_lines {
        let a = aggregate_ledger(lines);
        if a.probed > 0 {
            parts.push(if a.saved >= 0 {
                format!("proxy ~{} tok saved", fmt_tokens(a.saved as u64))
            } else {
                format!("proxy {} tok (overhead)", a.saved)
            });
        }
    }
    if blocked > 0 || loops > 0 {
        let mut hook = format!("{blocked} re-read(s) blocked (~{} tok)", fmt_tokens(tokens));
        if loops > 0 {
            hook.push_str(&format!(" · {loops} loop(s) broken"));
        }
        parts.push(hook);
    }
    (!parts.is_empty()).then(|| {
        format!(
            "⌁ dasein active — lifetime: {}. /dasein:savings for details.",
            parts.join(" · ")
        )
    })
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
            if a.sessions.is_empty() {
                println!(
                    "proxy ledger — {} request(s) / {} conversation(s):",
                    a.rows,
                    a.convs.len()
                );
            } else {
                println!(
                    "proxy ledger — {} request(s) / {} conversation(s) / {} session(s):",
                    a.rows,
                    a.convs.len(),
                    a.sessions.len()
                );
            }
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
        sessions.sort_by_key(|s| std::cmp::Reverse(s.1));
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
            r#"{"contract_version":"savings-ledger/v0","conv_id":"a","session_id":"s-1","model":"claude-sonnet-5","counterfactual_input_tokens":467,"billed_input_tokens":163,"billed_cache_read_tokens":50,"billed_cache_write_tokens":7,"fail_open":false,"freeze_cut_tokens":318}"#,
            "\n",
            // null probe: EXCLUDED from savings, counted (§8.4); also a
            // pre-upgrade row — no session_id
            r#"{"contract_version":"savings-ledger/v0","conv_id":"a","counterfactual_input_tokens":null,"billed_input_tokens":10,"billed_cache_read_tokens":0,"billed_cache_write_tokens":0,"fail_open":false}"#,
            "\n",
            // fail-open row with a probe: counted in savings AND flagged.
            // Same session as conv "a" — compaction/subagent conv_id churn
            // must still roll up to ONE session.
            r#"{"contract_version":"savings-ledger/v0","conv_id":"b","session_id":"s-1","counterfactual_input_tokens":100,"billed_input_tokens":120,"billed_cache_read_tokens":0,"billed_cache_write_tokens":0,"fail_open":true,"scorer_fail_opens":2}"#,
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
        assert_eq!(a.sessions.len(), 1); // two conv_ids, one session
        assert_eq!(a.freeze_cut, 318);
        assert_eq!(a.by_model["claude-sonnet-5"], (1, 467, 247));
        assert_eq!(a.by_model["(unrecorded)"], (1, 100, -20));

        // Per-session slice: same math, filtered. Rows without a session_id
        // (pre-upgrade ledgers) and other sessions stay out of the sum.
        assert_eq!(session_saved_from_lines(lines, "s-1"), Some(247 - 20));
        assert_eq!(session_saved_from_lines(lines, "s-2"), None);
    }

    #[test]
    fn lifetime_note_quiet_until_measured_then_honest() {
        // Fresh install: no ledger, no hook counters — stay silent.
        assert_eq!(lifetime_note_from(None, 0, 0, 0), None);
        // Ledger present but nothing probed yet — still silent.
        assert_eq!(
            lifetime_note_from(
                Some(
                    r#"{"contract_version":"savings-ledger/v0","conv_id":"a","counterfactual_input_tokens":null,"billed_input_tokens":10,"billed_cache_read_tokens":0,"billed_cache_write_tokens":0,"fail_open":false}"#
                ),
                0,
                0,
                0
            ),
            None
        );
        // Both sources measured: proxy sum + hook counters, one line.
        let note = lifetime_note_from(
            Some(r#"{"contract_version":"savings-ledger/v0","conv_id":"a","counterfactual_input_tokens":1500,"billed_input_tokens":200,"billed_cache_read_tokens":0,"billed_cache_write_tokens":0,"fail_open":false}"#),
            3, 12000, 1,
        )
        .unwrap();
        assert!(note.contains("proxy ~1300 tok saved"), "{note}");
        assert!(note.contains("3 re-read(s) blocked (~12.0k tok)"), "{note}");
        assert!(note.contains("1 loop(s) broken"), "{note}");
        assert!(note.contains("/dasein:savings"), "{note}");
        // A net-negative proxy shows signed overhead, never clamped (§8.4).
        let neg = lifetime_note_from(
            Some(r#"{"contract_version":"savings-ledger/v0","conv_id":"a","counterfactual_input_tokens":100,"billed_input_tokens":150,"billed_cache_read_tokens":0,"billed_cache_write_tokens":0,"fail_open":false}"#),
            0, 0, 0,
        )
        .unwrap();
        assert!(neg.contains("proxy -50 tok (overhead)"), "{neg}");
    }
}
