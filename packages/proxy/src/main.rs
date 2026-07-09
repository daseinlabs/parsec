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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => anyhow::bail!("dasein mcp: not yet implemented"),
        Command::Hook { event } => anyhow::bail!("dasein hook {event}: not yet implemented"),
        Command::Proxy => anyhow::bail!("dasein proxy: not yet implemented"),
        Command::Statusline => anyhow::bail!("dasein statusline: not yet implemented"),
    }
}
