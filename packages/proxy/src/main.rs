//! `parsec` — one binary, subcommands for every plugin role (DIRECTION.md §7b).
//!
//! Shipped per-platform in packages/plugin/bin/ (CI-built at release;
//! gitignored in this repo — scripts/refresh_plugin_bin.sh populates a local
//! copy); plugin distribution has no build step, so this binary must be
//! self-contained.

use clap::{Parser, Subcommand};

/// `parsec key …` actions — manage the per-account API key used for savings
/// reporting to the dashboard.
#[derive(Subcommand)]
enum KeyAction {
    /// Store the psc_ API key minted in the dashboard (takes effect next request).
    Set {
        /// The `psc_…` key from the dashboard.
        key: String,
        /// Platform base URL (dev builds only; release builds bake it).
        #[arg(long)]
        platform_url: Option<String>,
    },
    /// Show the configured key (masked) and where it resolves from.
    Show,
    /// Remove the stored key (stops dashboard reporting).
    Clear,
}

#[derive(Parser)]
#[command(name = "parsec", version, about = "Parsec client")]
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
    /// Local data-plane proxy (Pro tier); deploys unchanged as the Team
    /// gateway. This is the SUPERVISOR: it owns the routed port, spawns +
    /// restarts the curating worker, and forwards straight to Anthropic
    /// whenever the worker is down (so a worker crash never wedges a session).
    Proxy,
    /// The curating worker behind the supervisor (spawned by `parsec proxy`;
    /// binds an ephemeral loopback port passed via PARSEC_PROXY_PORT). Hidden:
    /// it is an implementation detail of `proxy`, never launched by hand.
    #[command(hide = true)]
    ProxyWorker,
    /// Status line: reads harness JSON on stdin, prints savings summary.
    Statusline,
    /// Per-subagent status line for the agent panel: reads the harness task
    /// JSON on stdin, prints one {"id","content"} object per line.
    SubagentStatusline,
    /// Human-readable savings report across recent sessions (/parsec:savings).
    Savings,
    /// One-time backfill: replay the pre-rename savings ledger from ~/.dasein
    /// into the platform, so a user migrating from the old local-only build
    /// keeps their history instead of restarting from zero. Safe to re-run —
    /// platform ingest is idempotent on request_id.
    Migrate {
        /// Legacy ledger to read (default: ~/.dasein/ledger.jsonl).
        #[arg(long, value_name = "FILE")]
        from: Option<std::path::PathBuf>,
        /// Report what would be sent, then stop.
        #[arg(long)]
        dry_run: bool,
    },
    /// One-time activation: download the local embedder, write Claude Code
    /// routing env, start the proxy. Runs automatically on first session.
    Setup {
        /// Hook-spawned first-run mode: respects terminal states (disable,
        /// unsupported) and never races a live download. Manual runs retry.
        #[arg(long)]
        auto: bool,
    },
    /// Undo setup: remove the parsec-managed env keys from Claude Code
    /// settings and stop auto-setup from re-running.
    Disable,
    /// Full local cleanup ahead of `claude plugin uninstall`: disable, stop
    /// the proxy, and delete downloaded models/logs/ledger.
    Uninstall,
    /// Bring the proxy back on the routed port if it died mid-session
    /// (detached; no-op when it is already listening).
    Up,
    /// Set/show/clear the per-account API key the proxy reports savings with
    /// (from the dashboard). Stored in ~/.parsec/credentials.json.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
}

/// Tracing for the long-running proxy processes (supervisor + worker):
/// fail-open events are a first-class metric (§8.3) and must be VISIBLE —
/// stderr, RUST_LOG-filterable (default info; PARSEC_VERBOSE=1 flips the
/// default to debug for a per-request pipeline trace — counts/hashes/timings
/// only, never message text or auth headers). Hook/statusline stay
/// subscriber-free: their stdout is protocol, and a stray log line would
/// corrupt it.
fn init_service_tracing() {
    let default_filter = if std::env::var("PARSEC_VERBOSE").ok().as_deref() == Some("1") {
        "info,parsec_proxy=debug"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => parsec_mapgen::mcp::serve_stdio(),
        Command::Hook { event } => parsec_proxy::hook::run(&event),
        Command::Proxy => {
            init_service_tracing();
            parsec_proxy::supervisor::run()
        }
        Command::ProxyWorker => {
            init_service_tracing();
            parsec_proxy::server::run()
        }
        Command::Statusline => parsec_proxy::statusline::run(),
        Command::SubagentStatusline => parsec_proxy::statusline::subagent_statusline(),
        Command::Savings => parsec_proxy::statusline::savings_report(),
        Command::Migrate { from, dry_run } => parsec_proxy::migrate::run(from, dry_run),
        Command::Setup { auto } => parsec_proxy::setup::run(auto),
        Command::Disable => parsec_proxy::setup::disable(),
        Command::Uninstall => parsec_proxy::setup::uninstall(),
        Command::Up => parsec_proxy::setup::up(),
        Command::Key { action } => match action {
            KeyAction::Set { key, platform_url } => parsec_proxy::setup::key_set(key, platform_url),
            KeyAction::Show => parsec_proxy::setup::key_show(),
            KeyAction::Clear => parsec_proxy::setup::key_clear(),
        },
    }
}
