//! `parsec` — one binary, subcommands for every plugin role (DIRECTION.md §7b).
//!
//! Shipped per-platform in packages/plugin/bin/ (CI-built at release;
//! gitignored in this repo — scripts/refresh_plugin_bin.sh populates a local
//! copy); plugin distribution has no build step, so this binary must be
//! self-contained.

use clap::{Parser, Subcommand};

/// `parsec key …` actions — inspect or hand-manage the stored account key.
/// `parsec login` is how a key normally gets here; `set` is for machines
/// with no browser (SSH, CI), fed a key minted on the dashboard account page.
#[derive(Subcommand)]
enum KeyAction {
    /// Store a psc_ key by hand (no browser on this machine; otherwise use `parsec login`).
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

/// `parsec tray …` — the menu-bar app. Split install/run the way the Desktop
/// interceptor is: `install` provisions and registers the login item, `run`
/// is what launchd actually invokes.
#[derive(Subcommand)]
enum TrayAction {
    /// Generate the .app bundle, register it as a login item, start it.
    Install,
    /// Unregister the login item and delete the bundle.
    Uninstall,
    /// Report what is installed. Changes nothing.
    Status,
    /// Run the menu-bar app in the foreground. Normally invoked by the login
    /// item rather than by hand.
    #[command(hide = true)]
    Run,
}

/// `parsec desktop …` — run the Claude Desktop interceptor that `parsec setup
/// desktop` provisioned. Split from `setup` for the same reason CC-Router
/// splits `client start-desktop` from `client connect`: starting and stopping
/// must not re-run the CA prompt or the approval walkthrough.
#[derive(Subcommand)]
enum DesktopAction {
    /// Start intercepting (refreshes the addon first, so a moved proxy port
    /// or an upgraded addon is picked up without re-running setup).
    Start {
        /// Install the boot service instead of a login-scoped process, so
        /// interception survives a reboot.
        #[arg(long)]
        autostart: bool,
    },
    /// Stop intercepting.
    Stop {
        /// Stop the running interceptor but leave the boot service in place,
        /// so it returns at the next login.
        #[arg(long)]
        keep_autostart: bool,
    },
    /// Stop and start — the way to pick up a changed routed port.
    Restart,
    /// Report configured / running / auto-start / approval state. Changes
    /// nothing.
    Status,
}

#[derive(Subcommand)]
enum Command {
    /// Stdio MCP server: repo exploration maps, search.
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
    /// another TOOL (`parsec setup opencode|codex|pi|desktop`): install that
    /// tool's shim instead.
    Setup {
        /// Hook-spawned first-run mode: respects terminal states (disable,
        /// unsupported) and never races a live download. Manual runs retry.
        #[arg(long)]
        auto: bool,
        /// codex only: use an OPENAI_API_KEY custom provider instead of the
        /// default ChatGPT-subscription routing.
        #[arg(long)]
        byok: bool,
        /// desktop only: trust mitmproxy's CA without a further prompt
        /// (runs the platform trust-store command; you will be asked to
        /// authenticate). Off by default — a machine-wide root CA is the
        /// user's call, so we print the command instead.
        #[arg(long)]
        install_ca: bool,
        /// desktop only: install the interceptor as a boot service
        /// (launchd / systemd --user / Run key) so Desktop stays routed
        /// across reboots, instead of running it for this login only.
        #[arg(long)]
        autostart: bool,
        /// desktop only: report readiness (mitmproxy, CA, extension
        /// approval, interceptor, target) and change nothing.
        #[arg(long)]
        status: bool,
        /// desktop only, for installers: provision everything up to the
        /// platform approval gate (no sudo, no interceptor start), leave a
        /// marker for the menu-bar app to finish the approval, print a
        /// machine-readable PARSEC_DESKTOP_PREPARE line, exit 0.
        #[arg(long)]
        prepare: bool,
        /// Tool to set up: `claude` | `opencode` | `codex` | `pi` | `desktop`
        /// (default: claude).
        tool: Option<String>,
    },
    /// Undo setup. `claude` (or no TOOL): remove the parsec-managed env keys
    /// from Claude Code settings and stop auto-setup from re-running. With
    /// another TOOL (`parsec disable opencode|codex|pi|desktop`): remove that
    /// tool's managed artifacts.
    Disable {
        /// Tool to disable: `claude` | `opencode` | `codex` | `pi` | `desktop`
        /// (default: claude).
        tool: Option<String>,
    },
    /// Run the Claude Desktop interceptor provisioned by `parsec setup
    /// desktop` (start/stop/restart/status). Opt-in; see
    /// docs/claude-desktop-integration.md.
    Desktop {
        #[command(subcommand)]
        action: DesktopAction,
    },
    /// Menu-bar app (macOS): live proxy/Desktop/savings status, and the
    /// guided Claude Desktop setup that waits for the System Settings
    /// approval instead of making you re-run the command.
    Tray {
        #[command(subcommand)]
        action: TrayAction,
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
        /// Called from a harness SessionStart hook. Prints a staged Codex
        /// trim (/parsec:trim) to stdout, which Codex surfaces to the model
        /// as a developer message — the injection channel Claude Code gets
        /// through the hook's additionalContext field.
        #[arg(long)]
        session_start: bool,
    },
    /// Sign in: opens the dashboard in your browser, which hands an API key
    /// back to this machine. Same result as `parsec key set`, no pasting.
    Login {
        /// Print the sign-in URL instead of opening a browser (SSH, WSL,
        /// containers — open it from any browser on this machine).
        #[arg(long)]
        no_browser: bool,
        /// Seconds to wait for the browser before giving up.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Show or clear the stored account key; `set` for browserless machines.
    /// Stored in ~/.parsec/credentials.json. Sign in with `parsec login`.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Stage a det+dir compaction of the current session (/parsec:trim):
    /// compute the deterministic needed-set trim of the transcript and stage
    /// it under ~/.parsec/trim/; the SessionStart hook (Claude Code) or
    /// `parsec up` (Codex) injects it into the next session.
    /// Exit 2: session too short / no transcript.
    Trim {
        /// Session transcript JSONL (default: the freshest transcript for
        /// cwd, under ~/.claude/projects or ~/.codex/sessions).
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
        /// Which harness's transcript to trim: auto (default — the freshest
        /// of the two for this directory), claude, or codex.
        #[arg(long)]
        tool: Option<String>,
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
        Command::Setup {
            auto,
            byok,
            install_ca,
            autostart,
            status,
            prepare,
            tool,
        } => {
            if byok && tool.as_deref() != Some("codex") {
                anyhow::bail!("--byok only applies to `parsec setup codex`");
            }
            if (install_ca || autostart || status || prepare) && tool.as_deref() != Some("desktop")
            {
                anyhow::bail!(
                    "--install-ca / --autostart / --status / --prepare only apply to \
                     `parsec setup desktop`"
                );
            }
            match tool.as_deref() {
                None | Some("claude") => parsec_proxy::setup::run(auto),
                Some("opencode") => parsec_proxy::setup_opencode::setup(),
                Some("pi") => parsec_proxy::setup_pi::setup(),
                Some("codex") => parsec_proxy::setup_codex::setup(if byok {
                    parsec_proxy::setup_codex::Mode::Byok
                } else {
                    parsec_proxy::setup_codex::Mode::Subscription
                }),
                Some("desktop") => {
                    parsec_proxy::setup_desktop::setup(parsec_proxy::setup_desktop::Options {
                        install_ca,
                        autostart,
                        status_only: status,
                        prepare,
                    })
                }
                Some(t) => anyhow::bail!(
                    "unknown tool '{t}' — supported: claude, opencode, codex, pi, desktop"
                ),
            }
        }
        Command::Disable { tool } => match tool.as_deref() {
            None | Some("claude") => parsec_proxy::setup::disable(),
            Some("opencode") => parsec_proxy::setup_opencode::disable(),
            Some("pi") => parsec_proxy::setup_pi::disable(),
            Some("codex") => parsec_proxy::setup_codex::disable(),
            Some("desktop") => parsec_proxy::setup_desktop::disable(),
            Some(t) => {
                anyhow::bail!("unknown tool '{t}' — supported: claude, opencode, codex, pi, desktop")
            }
        },
        Command::Desktop { action } => match action {
            DesktopAction::Start { autostart } => parsec_proxy::setup_desktop::start(autostart),
            DesktopAction::Stop { keep_autostart } => {
                parsec_proxy::setup_desktop::stop_cmd(keep_autostart)
            }
            DesktopAction::Restart => parsec_proxy::setup_desktop::restart(),
            DesktopAction::Status => parsec_proxy::setup_desktop::status(),
        },
        Command::Tray { action } => parsec_proxy::tray::run(match action {
            TrayAction::Install => parsec_proxy::tray::Action::Install,
            TrayAction::Uninstall => parsec_proxy::tray::Action::Uninstall,
            TrayAction::Status => parsec_proxy::tray::Action::Status,
            TrayAction::Run => parsec_proxy::tray::Action::Run,
        }),
        Command::Uninstall => parsec_proxy::setup::uninstall(),
        Command::Up {
            restart,
            session_start,
        } => parsec_proxy::setup::up(restart, session_start),
        Command::Trim {
            transcript,
            session_id,
            out,
            patch_file,
            json,
            finalize,
            level,
            tool,
        } => parsec_proxy::trim::run(parsec_proxy::trim::TrimArgs {
            transcript,
            session_id,
            out,
            patch_file,
            json,
            finalize,
            level,
            tool,
        }),
        Command::Login {
            no_browser,
            timeout,
        } => parsec_proxy::login::run(parsec_proxy::login::Options {
            no_browser,
            timeout: std::time::Duration::from_secs(timeout),
        }),
        Command::Key { action } => match action {
            KeyAction::Set { key, platform_url } => parsec_proxy::setup::key_set(key, platform_url),
            KeyAction::Show => parsec_proxy::setup::key_show(),
            KeyAction::Clear => parsec_proxy::setup::key_clear(),
        },
    }
}
