//! `dasein` — one binary, subcommands for every plugin role (DIRECTION.md §7b).
//!
//! Committed per-platform into packages/plugin/bin/; plugin distribution has
//! no build step, so this binary must be self-contained.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "dasein", version, about = "Dasein Learner client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Stdio MCP server: codescout exploration maps, search.
    Mcp,
    /// Hook handler (exec form): no-reread gate, session, telemetry spool.
    Hook {
        /// Hook event name, e.g. PreToolUse.
        event: String,
    },
    /// Local data-plane proxy (Pro tier); deploys unchanged as the Team gateway.
    Proxy,
    /// Status line: reads harness JSON on stdin, prints savings summary.
    Statusline,
    /// Human-readable savings report across recent sessions (/dasein-savings).
    Savings,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => dasein_mapgen::mcp::serve_stdio(),
        Command::Hook { event } => dasein_proxy::hook::run(&event),
        Command::Proxy => {
            // Long-running service: fail-open events are a first-class metric
            // (§8.3) and must be VISIBLE — stderr, RUST_LOG-filterable
            // (default info). Hook/statusline stay subscriber-free: their
            // stdout is protocol, and a stray log line would corrupt it.
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .with_writer(std::io::stderr)
                .init();
            dasein_proxy::server::run()
        }
        Command::Statusline => dasein_proxy::statusline::run(),
        Command::Savings => dasein_proxy::statusline::savings_report(),
    }
}
