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
    /// One-time activation. `claude` (or no TOOL): write Claude Code routing
    /// env and start the proxy; runs automatically on first session. With
    /// another TOOL (`parsec setup opencode|codex`): install that tool's
    /// shim instead.
    Setup {
        /// Hook-spawned first-run mode: respects terminal states (disable,
        /// unsupported) and never races a live download. Manual runs retry.
        #[arg(long)]
        auto: bool,
        /// codex only: use an OPENAI_API_KEY custom provider instead of the
        /// default ChatGPT-subscription routing.
        #[arg(long)]
        byok: bool,
        /// Tool to set up: `claude` | `opencode` | `codex` (default: claude).
        tool: Option<String>,
    },
    /// Undo setup. `claude` (or no TOOL): remove the parsec-managed env keys
    /// from Claude Code settings and stop auto-setup from re-running. With
    /// another TOOL (`parsec disable opencode|codex`): remove that tool's
    /// managed artifacts.
    Disable {
        /// Tool to disable: `claude` | `opencode` | `codex` (default: claude).
        tool: Option<String>,
    },
    /// Full local cleanup ahead of `claude plugin uninstall`: disable, stop
    /// the proxy, and delete downloaded models/logs/ledger.
    Uninstall,
    /// Bring the proxy back on the routed port if it died mid-session
    /// (detached; no-op when it is already listening).
    Up {
        /// Stop a running parsec proxy first, then start fresh — used by the
        /// install scripts so a just-installed binary actually serves.
        /// Foreign processes on the port are never killed.
        #[arg(long)]
        restart: bool,
    },
    /// Set/show/clear the per-account API key the proxy reports savings with
    /// (from the dashboard). Stored in ~/.parsec/credentials.json.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Stage a det+dir compaction of the current session (/parsec:trim):
    /// compute the deterministic needed-set trim of the transcript and stage
    /// it under ~/.parsec/trim/; the SessionStart hook injects it after
    /// /clear. Exit 2: session too short / no transcript.
    Trim {
        /// Session transcript JSONL (default: newest for cwd under
        /// ~/.claude/projects).
        #[arg(long)]
        transcript: Option<std::path::PathBuf>,
        /// Session id recorded in the staged payload (informational).
        #[arg(long)]
        session_id: Option<String>,
        /// Stage to this path instead of ~/.parsec/trim/<project-key>.json.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Union a unified diff into the needed label as a gold stand-in
        /// (default: off — the label is purely the session's own future use).
        #[arg(long)]
        patch_file: Option<std::path::PathBuf>,
        /// Machine-readable stats on stdout.
        #[arg(long)]
        json: bool,
        /// Read the STANDING DIRECTIVES from stdin and mark the staged
        /// payload ready for injection.
        #[arg(long)]
        finalize: bool,
        /// Trim aggressiveness: 1 (low trimming, keep more) to 5 (very high).
        /// Default 3 — the measured, parity-locked configuration; other
        /// levels are unmeasured presets. Env fallback: PARSEC_TRIM_LEVEL.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
        level: Option<u8>,
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
        Command::Mcp => parsec_mapgen::mcp::serve_stdio_with(parsec_proxy::mcp_ext::tools()),
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
        Command::Setup { auto, byok, tool } => {
            if byok && tool.as_deref() != Some("codex") {
                anyhow::bail!("--byok only applies to `parsec setup codex`");
            }
            match tool.as_deref() {
                None | Some("claude") => parsec_proxy::setup::run(auto),
                Some("opencode") => parsec_proxy::setup_opencode::setup(),
                Some("codex") => parsec_proxy::setup_codex::setup(if byok {
                    parsec_proxy::setup_codex::Mode::Byok
                } else {
                    parsec_proxy::setup_codex::Mode::Subscription
                }),
                Some(t) => anyhow::bail!("unknown tool '{t}' — supported: claude, opencode, codex"),
            }
        }
        Command::Disable { tool } => match tool.as_deref() {
            None | Some("claude") => parsec_proxy::setup::disable(),
            Some("opencode") => parsec_proxy::setup_opencode::disable(),
            Some("codex") => parsec_proxy::setup_codex::disable(),
            Some(t) => anyhow::bail!("unknown tool '{t}' — supported: claude, opencode, codex"),
        },
        Command::Uninstall => parsec_proxy::setup::uninstall(),
        Command::Up { restart } => parsec_proxy::setup::up(restart),
        Command::Trim {
            transcript,
            session_id,
            out,
            patch_file,
            json,
            finalize,
            level,
        } => parsec_proxy::trim::run(parsec_proxy::trim::TrimArgs {
            transcript,
            session_id,
            out,
            patch_file,
            json,
            finalize,
            level,
        }),
        Command::Key { action } => match action {
            KeyAction::Set { key, platform_url } => parsec_proxy::setup::key_set(key, platform_url),
            KeyAction::Show => parsec_proxy::setup::key_show(),
            KeyAction::Clear => parsec_proxy::setup::key_clear(),
        },
    }
}
