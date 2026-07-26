//! One-time backfill of the pre-rename savings ledger (`parsec migrate`).
//!
//! Users who ran the tool under its old name accumulated `~/.dasein/ledger.jsonl`
//! on builds that had no platform reporting at all. Those savings were really
//! measured — they just never reached an account. This command replays those
//! rows into the same `/ledger` ingest the live proxy posts to, so a migrating
//! user's dashboard shows their whole history instead of starting from zero on
//! the day they upgrade.
//!
//! Why replay ROWS instead of POSTing one total: §8.4 measurement honesty says
//! a savings figure is only ever the sum of per-request count_tokens
//! counterfactuals. Shipping rows lets the platform derive the total exactly
//! the way it does for live traffic, and preserves the per-model and per-day
//! breakdowns that a lump sum would flatten. Ingest is idempotent on
//! `request_id`, so re-running after a partial failure never double-counts.
//!
//! Read-only with respect to the legacy directory: nothing is moved, rewritten,
//! or deleted, so a user can re-run this (or roll back to the old binary)
//! without having lost anything.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use crate::ledger_ship::{LedgerSink, ShipOutcome};

/// The pre-rename home directory. Deliberately a literal here rather than a
/// `setup` helper: this is the only code path that should ever look at it.
const LEGACY_DIR: &str = ".dasein";

/// In-flight POSTs. The row count is in the hundreds, so this is about not
/// opening a burst of connections against the platform, not about throughput.
const CONCURRENCY: usize = 8;

/// Per-row ceiling. Generous — a backfill is not on anyone's critical path,
/// and a slow-but-succeeding row beats a spurious "failed" the user must retry.
const ROW_TIMEOUT: Duration = Duration::from_secs(30);

/// `~/.dasein/ledger.jsonl` — the legacy counterpart of
/// `statusline::ledger_file`.
pub fn legacy_ledger() -> PathBuf {
    crate::setup::home_dir()
        .join(LEGACY_DIR)
        .join("ledger.jsonl")
}

/// Split the legacy file into shippable rows, returning the malformed-line
/// count rather than aborting on one. The proxy appends to this file, so a
/// crash mid-write can leave a truncated final line; that must not block an
/// otherwise complete backfill.
pub(crate) fn parse_rows(data: &str) -> (Vec<Value>, usize) {
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) if v.is_object() => rows.push(v),
            _ => skipped += 1,
        }
    }
    (rows, skipped)
}

/// Tally of one backfill run.
#[derive(Default, Debug, PartialEq)]
struct Report {
    accepted: usize,
    rejected: usize,
    failed: usize,
    /// First of each kind, kept verbatim so the user gets a real diagnostic
    /// instead of just a count.
    first_reject: Option<String>,
    first_fail: Option<String>,
}

/// `parsec migrate` entrypoint. `from` overrides the legacy ledger path
/// (mainly for users whose home moved); `dry_run` reports and stops.
pub fn run(from: Option<PathBuf>, dry_run: bool) -> anyhow::Result<()> {
    let path = from.unwrap_or_else(legacy_ledger);
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Nothing to migrate is the common case for a fresh install, and
            // it is a success, not an error.
            println!(
                "no legacy ledger at {} — nothing to migrate.",
                path.display()
            );
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("reading {}", path.display())));
        }
    };

    let (rows, skipped) = parse_rows(&data);
    println!("{} legacy ledger: {}", crate::brand::MARK, path.display());
    if skipped > 0 {
        println!("  {skipped} unparseable line(s) skipped");
    }
    if rows.is_empty() {
        println!("  no rows — nothing to migrate.");
        return Ok(());
    }

    // Same aggregation the statusline and `parsec savings` use, so the number
    // printed here is the number the user already recognises.
    let agg = crate::statusline::aggregate_ledger(&data);
    println!(
        "  {} row(s) / {} conversation(s) · {} tok saved to migrate",
        rows.len(),
        agg.convs.len(),
        agg.saved
    );

    if dry_run {
        println!("\n--dry-run: nothing sent.");
        return Ok(());
    }

    let Some(sink) = crate::ledger_ship::resolve() else {
        anyhow::bail!(
            "no platform destination configured, so there is nowhere to migrate to.\n\
             Set your account key (from the dashboard), then re-run `parsec migrate`:\n  \
             parsec key set psc_…"
        );
    };

    let rt = tokio::runtime::Runtime::new()?;
    let report = rt.block_on(ship_all(&sink, &rows))?;

    println!(
        "\nmigrated: {} accepted, {} rejected, {} failed",
        report.accepted, report.rejected, report.failed
    );
    if let Some(d) = &report.first_reject {
        println!("  first rejection: {d}");
    }
    if let Some(d) = &report.first_fail {
        println!("  first failure:   {d}");
    }

    if report.failed > 0 {
        // Non-zero exit: the migration is incomplete and the user should act.
        anyhow::bail!(
            "{} row(s) did not reach the platform. Re-run `parsec migrate` — \
             ingest is idempotent, so already-migrated rows will not double-count.",
            report.failed
        );
    }
    if report.accepted > 0 {
        println!("your dashboard now includes the pre-migration history.");
    }
    Ok(())
}

/// POST every row, bounded-concurrent, tallying outcomes as they land.
async fn ship_all(sink: &LedgerSink, rows: &[Value]) -> anyhow::Result<Report> {
    let client = reqwest::Client::builder().timeout(ROW_TIMEOUT).build()?;
    let total = rows.len();
    let mut report = Report::default();
    let mut done = 0usize;

    let mut in_flight = futures_util::stream::iter(rows.iter().map(|row| {
        let client = client.clone();
        async move { crate::ledger_ship::ship_awaited(&client, sink, row).await }
    }))
    .buffer_unordered(CONCURRENCY);

    while let Some(outcome) = in_flight.next().await {
        done += 1;
        match outcome {
            ShipOutcome::Accepted => report.accepted += 1,
            ShipOutcome::Rejected { status, detail } => {
                report.rejected += 1;
                report
                    .first_reject
                    .get_or_insert_with(|| format!("HTTP {status}: {detail}"));
            }
            ShipOutcome::Failed(e) => {
                report.failed += 1;
                report.first_fail.get_or_insert(e);
            }
        }
        if done.is_multiple_of(50) && done != total {
            println!("  … {done}/{total}");
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rows_keeps_objects_and_counts_junk() {
        let data = concat!(
            "{\"request_id\":\"req_a\"}\n",
            "\n",
            "   \n",
            "{\"request_id\":\"req_b\"}\n",
            "{not json\n",
            "[1,2,3]\n",
            // A crash mid-append leaves a truncated last line; it must be
            // skipped, not fatal.
            "{\"request_id\":\"req_c\",\"billed_"
        );
        let (rows, skipped) = parse_rows(data);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["request_id"], "req_a");
        assert_eq!(rows[1]["request_id"], "req_b");
        // bad json, a non-object array, and the truncated tail
        assert_eq!(skipped, 3);
    }

    #[test]
    fn parse_rows_empty_input() {
        assert_eq!(parse_rows(""), (vec![], 0));
        assert_eq!(parse_rows("\n\n  \n"), (vec![], 0));
    }

    #[test]
    fn legacy_ledger_points_at_the_old_home() {
        let p = legacy_ledger();
        assert!(p.ends_with(".dasein/ledger.jsonl"), "got {}", p.display());
    }

    #[test]
    fn missing_legacy_ledger_is_success_not_error() {
        let missing = std::env::temp_dir().join("parsec-migrate-does-not-exist-9f3a/ledger.jsonl");
        assert!(run(Some(missing), false).is_ok());
    }

    /// Dry-run must never need a configured sink — it is the "what would this
    /// do?" affordance a user runs before setting up an account key.
    #[test]
    fn dry_run_reports_without_shipping() {
        let dir = std::env::temp_dir().join("parsec-migrate-dry-run-4c1e");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.jsonl");
        std::fs::write(
            &path,
            "{\"contract_version\":\"savings-ledger/v0\",\"conv_id\":\"c1\",\
             \"counterfactual_input_tokens\":100,\"billed_input_tokens\":10,\
             \"billed_output_tokens\":5,\"billed_cache_read_tokens\":20,\
             \"billed_cache_write_tokens\":0}\n",
        )
        .unwrap();
        assert!(run(Some(path), true).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
