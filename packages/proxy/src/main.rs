//! `dasein` — one binary, subcommands for every plugin role (DIRECTION.md §7b).
//!
//! Shipped per-platform in packages/plugin/bin/ (CI-built at release;
//! gitignored in this repo — scripts/refresh_plugin_bin.sh populates a local
//! copy); plugin distribution has no build step, so this binary must be
//! self-contained.

use clap::{Parser, Subcommand};

/// `dasein key …` actions — manage the per-account API key used for savings
/// reporting to the dashboard.
#[derive(Subcommand)]
enum KeyAction {
    /// Store the dsn_ API key minted in the dashboard (takes effect next request).
    Set {
        /// The `dsn_…` key from the dashboard.
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
    /// Local data-plane proxy (Pro tier); deploys unchanged as the Team
    /// gateway. This is the SUPERVISOR: it owns the routed port, spawns +
    /// restarts the curating worker, and forwards straight to Anthropic
    /// whenever the worker is down (so a worker crash never wedges a session).
    Proxy,
    /// The curating worker behind the supervisor (spawned by `dasein proxy`;
    /// binds an ephemeral loopback port passed via DASEIN_PROXY_PORT). Hidden:
    /// it is an implementation detail of `proxy`, never launched by hand.
    #[command(hide = true)]
    ProxyWorker,
    /// Status line: reads harness JSON on stdin, prints savings summary.
    Statusline,
    /// Human-readable savings report across recent sessions (/dasein-savings).
    Savings,
    /// One-time activation: download the local embedder, write Claude Code
    /// routing env, start the proxy. Runs automatically on first session.
    Setup {
        /// Hook-spawned first-run mode: respects terminal states (disable,
        /// unsupported) and never races a live download. Manual runs retry.
        #[arg(long)]
        auto: bool,
    },
    /// Undo setup: remove the dasein-managed env keys from Claude Code
    /// settings and stop auto-setup from re-running.
    Disable,
    /// Full local cleanup ahead of `claude plugin uninstall`: disable, stop
    /// the proxy, and delete downloaded models/logs/ledger.
    Uninstall,
    /// Bring the proxy back on the routed port if it died mid-session
    /// (detached; no-op when it is already listening).
    Up,
    /// Set/show/clear the per-account API key the proxy reports savings with
    /// (from the dashboard). Stored in ~/.dasein/credentials.json.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
}

/// Tracing for the long-running proxy processes (supervisor + worker):
/// fail-open events are a first-class metric (§8.3) and must be VISIBLE —
/// stderr, RUST_LOG-filterable (default info; DASEIN_VERBOSE=1 flips the
/// default to debug for a per-request pipeline trace — counts/hashes/timings
/// only, never message text or auth headers). Hook/statusline stay
/// subscriber-free: their stdout is protocol, and a stray log line would
/// corrupt it.
fn init_service_tracing() {
    let default_filter = if std::env::var("DASEIN_VERBOSE").ok().as_deref() == Some("1") {
        "info,dasein_proxy=debug"
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
        Command::Mcp => dasein_mapgen::mcp::serve_stdio(),
        Command::Hook { event } => dasein_proxy::hook::run(&event),
        Command::Proxy => {
            init_service_tracing();
            dasein_proxy::supervisor::run()
        }
        Command::ProxyWorker => {
            init_service_tracing();
            dasein_proxy::server::run()
        }
        Command::Statusline => dasein_proxy::statusline::run(),
        Command::Savings => dasein_proxy::statusline::savings_report(),
        Command::Setup { auto } => dasein_proxy::setup::run(auto),
        Command::Disable => dasein_proxy::setup::disable(),
        Command::Uninstall => dasein_proxy::setup::uninstall(),
        Command::Up => dasein_proxy::setup::up(),
        Command::Key { action } => match action {
            KeyAction::Set { key, platform_url } => dasein_proxy::setup::key_set(key, platform_url),
            KeyAction::Show => dasein_proxy::setup::key_show(),
            KeyAction::Clear => dasein_proxy::setup::key_clear(),
        },
    }
}
