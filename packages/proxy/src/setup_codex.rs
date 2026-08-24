//! `parsec setup codex` — route Codex CLI through the local proxy,
//! DEFAULT-ON for every session (docs/codex-integration.md).
//!
//! Managed artifacts, all marker/sentinel-owned and non-destructive:
//!
//! 1. **Head block** at the TOP of `~/.codex/config.toml`: the top-level
//!    routing key. TOML top-level keys must precede any `[table]`, and the
//!    user's file usually ends in one — an appended bare key would silently
//!    land inside their last table — so the head block is PREPENDED. The
//!    user's bytes are never reserialized, only shifted.
//!    - Subscription (default): `openai_base_url = "http://127.0.0.1:PORT/chatgpt"`.
//!      Applies to every session of the built-in provider under the user's
//!      existing ChatGPT sign-in; the OAuth token makes one localhost hop
//!      and terminates at chatgpt.com's codex backend — the Claude Code
//!      subscription posture (§3). The config docs explicitly sanction
//!      `openai_base_url` for "an LLM proxy or router"
//!      (learn.chatgpt.com/docs/config-file/config-advanced).
//!    - BYOK (`--byok`): `model_provider = "parsec"` selects the custom
//!      provider table below (`env_key = OPENAI_API_KEY`, `/openai/v1`).
//! 2. **Tail block** APPENDED to `config.toml`: the `[model_providers.parsec]`
//!    table (inert unless BYOK selects it) and a `[[hooks.SessionStart]]`
//!    hook running `parsec up` — with routing default-on, every session
//!    depends on the proxy, so every session revives it (the Claude Code
//!    self-heal). Tables/array-tables are append-safe at end of file.
//! 3. **Skills** in `~/.agents/skills/parsec-*/SKILL.md` — the parsec command
//!    surface (`/skills` menu, `$parsec-savings` mention, or implicit
//!    invocation). Custom prompts (`~/.codex/prompts/`) are deprecated
//!    upstream; setup migrates old installs by removing our prompt files
//!    and the old opt-in profile (`parsec.config.toml`), both sentinel-gated.
//!
//! Product decision 2026-08-12: subscription routing default-on for auth
//! parity with Claude Code; OpenAI ToS review remains an open item before
//! public release (survey §3.7.2) — softened by the explicit proxy/router
//! sanction above. Deliberately NOT touched: `~/.parsec/setup_state.json`
//! (Claude Code's auto-setup phase machine).

use std::path::{Path, PathBuf};

/// Wires a Codex install rides: `/openai/*` in BYOK mode, `/chatgpt/*` under
/// the ChatGPT subscription. Setup refuses to call a listener compatible
/// unless it advertises both.
const REQUIRED_WIRES: &[&str] = &["openai", "chatgpt"];

/// Tail-block markers (provider table + hook). BEGIN is matched as a line
/// PREFIX so wording tweaks in later releases still find older blocks; END
/// is a full line. The prefix is unchanged from the first release, so
/// yesterday's installs migrate in place.
const TAIL_BEGIN: &str = "# >>> parsec-managed codex provider";
const TAIL_END: &str = "# <<< parsec-managed codex provider <<<";

/// Head-block markers (top-level routing key).
const HEAD_BEGIN: &str = "# >>> parsec-managed codex routing";
const HEAD_END: &str = "# <<< parsec-managed codex routing <<<";

/// Sentinel in the legacy opt-in profile file (pre-default-on installs);
/// setup/disable remove it when present.
const PROFILE_SENTINEL: &str = "parsec-managed-profile";

/// Sentinel in the legacy prompt files (custom prompts are deprecated
/// upstream); setup/disable remove ours when present.
const PROMPT_SENTINEL: &str = "parsec-managed-prompt";

/// Sentinel in each managed skill.
const SKILL_SENTINEL: &str = "parsec-managed-skill";

/// `$CODEX_HOME`, else `~/.codex` — where Codex reads user-level config.
fn codex_home() -> PathBuf {
    std::env::var("CODEX_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::setup::home_dir().join(".codex"))
}

pub fn config_path() -> PathBuf {
    codex_home().join("config.toml")
}

/// Legacy opt-in profile (removed on setup/disable).
pub fn profile_path() -> PathBuf {
    codex_home().join("parsec.config.toml")
}

/// Legacy prompts dir (files removed on setup/disable).
fn prompts_dir() -> PathBuf {
    codex_home().join("prompts")
}

/// Skill install locations. The docs describe the cross-tool agents dir
/// (`~/.agents/skills`), but codex 0.147.0 demonstrably scans
/// `$CODEX_HOME/skills` (it creates that dir itself and never touches the
/// agents one) — doc-vs-binary skew, so install into BOTH, sentinel-gated;
/// whichever the running version reads, the skills are there.
fn skills_dirs() -> Vec<PathBuf> {
    vec![
        codex_home().join("skills"),
        crate::setup::home_dir().join(".agents").join("skills"),
    ]
}

/// The auth mode the head block selects (see module docs).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// ChatGPT sign-in via the built-in provider, base URL re-pointed at the
    /// proxy's `/chatgpt` namespace. The default.
    Subscription,
    /// The custom provider table: `env_key = OPENAI_API_KEY`.
    Byok,
}

/// The top-level routing key each mode writes, for the duplicate-key
/// conflict check: a user-set copy of the SAME key outside our block would
/// make the merged TOML invalid, so setup refuses instead of stomping.
fn head_key(mode: Mode) -> &'static str {
    match mode {
        Mode::Subscription => "openai_base_url",
        Mode::Byok => "model_provider",
    }
}

fn head_block(mode: Mode, port: u16) -> String {
    let line = match mode {
        Mode::Subscription => format!(
            "# ChatGPT-subscription mode: model traffic makes one localhost hop through\n\
             # the parsec proxy; your OAuth token never leaves this machine.\n\
             openai_base_url = \"http://127.0.0.1:{port}/chatgpt\""
        ),
        Mode::Byok => "# BYOK mode: the parsec custom provider (needs OPENAI_API_KEY).\n\
                       model_provider = \"parsec\""
            .to_string(),
    };
    format!(
        "{HEAD_BEGIN} — applies to ALL codex sessions; `parsec disable codex` removes it >>>\n\
         {line}\n\
         {HEAD_END}\n"
    )
}

/// The SessionStart hook's `command = …` TOML line, per platform. Unix:
/// `$HOME` expands in the hook's own `sh`, so a space-containing home
/// survives, with a PATH fallback. Windows: no `/bin/sh` — the resolved
/// absolute exe path is baked at setup time as a TOML LITERAL string
/// (single quotes: backslashes survive; the install.ps1 places parsec.exe
/// at exactly this path).
fn hook_command_line() -> String {
    #[cfg(unix)]
    {
        "command = \"/bin/sh -c '\\\"$HOME\\\"/.parsec/bin/parsec up --session-start || parsec up --session-start'\"".to_string()
    }
    #[cfg(windows)]
    {
        let exe = crate::setup::parsec_home().join("bin").join("parsec.exe");
        format!(
            "command = 'cmd /c \"\"{}\" up --session-start\"'",
            exe.display()
        )
    }
}

/// The tail block: the BYOK provider table (inert unless the head selects
/// it) and the SessionStart proxy-revival hook. `wire_api = "responses"` is
/// the only wire Codex still speaks; the attribution header rides Codex's
/// own `http_headers` surface (subscription mode is attributed by its route
/// namespace instead). `parsec up` is idempotent and detached. Deliberately
/// SYNCHRONOUS: codex 0.147.0 rejects `async = true` by skipping the whole
/// hook ("async hooks are not supported yet") — and `parsec up` returns
/// immediately anyway (the proxy detaches).
fn tail_block(port: u16) -> String {
    format!(
        "{TAIL_BEGIN} — do not edit; `parsec disable codex` removes this block >>>\n\
         [model_providers.parsec]\n\
         name = \"parsec (context compression proxy)\"\n\
         base_url = \"http://127.0.0.1:{port}/openai/v1\"\n\
         env_key = \"OPENAI_API_KEY\"\n\
         wire_api = \"responses\"\n\
         http_headers = {{ \"x-parsec-tool\" = \"codex\" }}\n\
         \n\
         [[hooks.SessionStart]]\n\
         matcher = \"startup|resume\"\n\
         \n\
         [[hooks.SessionStart.hooks]]\n\
         type = \"command\"\n\
         {hook_command}\n\
         statusMessage = \"parsec: reviving proxy\"\n\
         timeout = 30\n\
         {TAIL_END}\n",
        hook_command = hook_command_line()
    )
}

/// Bring a proxy that serves the Codex wires up on `port`, replacing an
/// older parsec build that holds it. A foreign listener is never touched and
/// never reported as success — the user has to free the port themselves.
fn warm_proxy(port: u16) {
    use crate::setup::PortOccupant;
    let spawn = |what: &str| match crate::setup::spawn_proxy_detached(port, &[]) {
        Ok(()) => println!("proxy {what} on 127.0.0.1:{port}"),
        Err(e) => println!("proxy pre-warm failed ({e}) — run `parsec up` before codex"),
    };
    match crate::setup::classify_port(port, REQUIRED_WIRES) {
        PortOccupant::Compatible => println!("proxy already listening on 127.0.0.1:{port}"),
        PortOccupant::Free => spawn("starting"),
        PortOccupant::StaleParsec => {
            println!(
                "an older parsec proxy holds 127.0.0.1:{port} and does not serve the \
                 codex routes — replacing it"
            );
            if !crate::setup::shutdown_parsec_on(port) {
                println!(
                    "it refused to stop — run `parsec up --restart`, then re-run \
                     `parsec setup codex`"
                );
                return;
            }
            for _ in 0..40 {
                if !crate::hook::port_listening(port) {
                    spawn("restarted");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            println!(
                "it acked the stop but kept the port — run `parsec up --restart`, then \
                 re-run `parsec setup codex`"
            );
        }
        PortOccupant::Foreign => println!(
            "127.0.0.1:{port} is held by something that is not the parsec proxy — \
             left alone. Codex traffic will fail until that port is free; stop it \
             and re-run `parsec setup codex`."
        ),
    }
}

// ── skills (the parsec command surface) ─────────────────────────────────────

/// The Codex port of the parsec commands, as skills
/// (`~/.agents/skills/<name>/SKILL.md`). Skills are instructions — they
/// cannot execute anything themselves — so each one tells the agent to run
/// the parsec binary through its shell tool: the stable alias path first
/// (`refresh_bin_alias` keeps it current), PATH second. Invoke via the
/// `/skills` menu, a `$parsec-savings` mention, or implicitly by asking.
/// This is also the savings display: the Codex status line is a closed item
/// catalog, so on-demand is the ceiling (research §4).
fn skill_files(port: u16) -> Vec<(&'static str, String)> {
    let sentinel_line = format!(
        "<!-- {SKILL_SENTINEL}: written by `parsec setup codex`; \
         removed by `parsec disable codex`. -->"
    );
    // The binary path the skills tell the agent to run. Unix: the stable
    // alias (refresh_bin_alias keeps it current), PATH fallback in the
    // templates. Windows: no symlink alias — install.ps1 puts parsec.exe at
    // this path AND on the user PATH, so the env-var form works from any
    // shell the agent picks.
    #[cfg(unix)]
    let run = "~/.parsec/bin/parsec";
    #[cfg(windows)]
    let run = "%USERPROFILE%\\.parsec\\bin\\parsec.exe";
    let skill = |name: &'static str, description: &str, body: String| {
        (
            name,
            format!(
                "---\nname: {name}\ndescription: {description}\n---\n{sentinel_line}\n\n{body}"
            ),
        )
    };
    vec![
        skill(
            "parsec-savings",
            "Show parsec context-compression savings for recent sessions",
            format!(
                "Run this shell command and show the user its FULL output in a code block:\n\n\
                 ```\n{run} savings || parsec savings\n```\n\n\
                 Do not summarize the numbers away — the table is the answer. If both \
                 invocations fail, say the parsec binary is missing and suggest re-running \
                 `parsec setup codex`.\n"
            ),
        ),
        skill(
            "parsec-trim",
            "Compact this Codex session — a deterministic needed-set trim plus standing directives",
            format!(
                "Two steps, both through the parsec binary. NEVER summarize the session \
                 yourself — step 1 is deterministic and step 2 is the only part you write.\n\n\
                 1. Run `{run} trim --tool codex || parsec trim --tool codex`. It computes \
                 which parts of this session were actually re-read, edited, or used later \
                 and stages them. Exit code 2 means there is nothing worth compacting yet — \
                 say so and stop. Its token numbers are chars/4 ESTIMATES; never present \
                 them as measured.\n\n\
                 2. Write a STANDING DIRECTIVES block from your own context — the durable \
                 rulings, constraints, and scope decisions of this session, NOT a summary of \
                 the work — and pipe it in:\n\n\
                 ```\n\
                 printf '%s' \"<your directives>\" | {run} trim --finalize\n\
                 ```\n\n\
                 Then tell the user to start a fresh Codex session: the staged trim is \
                 injected once, at the next session start, and expires unused after 30 \
                 minutes. Do not start that session yourself.\n"
            ),
        ),
        skill(
            "parsec-proxy",
            "Check (and revive) the parsec proxy this Codex session routes through",
            format!(
                "Run: `curl -s http://127.0.0.1:{port}/health`\n\n\
                 - If it answers with service \"parsec-proxy\": report it healthy (include \
                   the version) and remind the user this Codex session routes through it.\n\
                 - If it does not answer: run `{run} up || parsec up`, wait 2 seconds, and \
                   re-check. Report what happened. Already-running sessions reconnect on \
                   their next request; nothing needs restarting.\n"
            ),
        ),
        skill(
            "parsec-key",
            "Show or set the parsec API key used for savings reporting",
            format!(
                "Manage the parsec dashboard key.\n\n\
                 - No value given: run `{run} key show || parsec key show`.\n\
                 - Asked to clear: run `{run} key clear || parsec key clear`.\n\
                 - Given a value starting with `psc_`: run `{run} key set <value> || parsec key set <value>`.\n\n\
                 Show the command output. Never echo a full key back to the user — `key show` \
                 masks it; keep it masked.\n"
            ),
        ),
        skill(
            "parsec-setup",
            "Re-run parsec setup for Codex (repairs routing config)",
            format!(
                "Run `{run} setup codex || parsec setup codex` and show the output. Mention \
                 that a `--byok` flag exists for API-key mode if the user asks about auth.\n"
            ),
        ),
        skill(
            "parsec-uninstall",
            "Fully remove parsec (proxy, configs, downloaded data)",
            format!(
                "This removes parsec's routing for every tool (Claude Code, opencode, Codex), \
                 stops the proxy, and deletes its local data. CONFIRM with the user first. On \
                 an explicit yes, run `{run} uninstall || parsec uninstall` and show the output.\n"
            ),
        ),
    ]
}

/// Drop one managed skill. Ownership: missing → write; ours (sentinel) →
/// refresh; foreign → warn and leave (one squatter must not abort setup).
fn write_skill(dir: &Path, name: &str, contents: &str) -> anyhow::Result<bool> {
    let path = dir.join(name).join("SKILL.md");
    match read_optional(&path)? {
        Some(cur) if cur == contents => Ok(true),
        Some(cur) if !cur.contains(SKILL_SENTINEL) => {
            println!(
                "{} exists and is not parsec-managed — left alone",
                path.display()
            );
            Ok(false)
        }
        _ => {
            if let Some(d) = path.parent() {
                std::fs::create_dir_all(d)?;
            }
            write_via_tmp(&path, contents)?;
            Ok(true)
        }
    }
}

// ── text-block plumbing (format-preserving config edits) ────────────────────

fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Atomic-ish write: temp file + rename.
fn write_via_tmp(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("parsec-tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Byte range (start, end_exclusive) of a marker block in `text`, including
/// END's trailing newline when present. None when no BEGIN line exists. A
/// BEGIN without END reports as corrupt via `Err`.
fn block_range(text: &str, begin: &str, end: &str) -> Result<Option<(usize, usize)>, String> {
    let Some(b) = text
        .lines()
        .scan(0usize, |off, l| {
            let start = *off;
            *off += l.len() + 1;
            Some((start, l))
        })
        .find(|(_, l)| l.trim_start().starts_with(begin))
        .map(|(start, _)| start)
    else {
        return Ok(None);
    };
    let after = &text[b..];
    let Some(end_off) = after.find(end) else {
        return Err(format!(
            "found `{begin}…` without its closing `{end}` — the block was hand-edited; \
             remove it manually, then re-run"
        ));
    };
    let e = b + end_off + end.len() + usize::from(after[end_off + end.len()..].starts_with('\n'));
    Ok(Some((b, e)))
}

/// Does `key` appear as a top-level assignment anywhere in `text`? (Line
/// scan; good enough to refuse creating a duplicate TOML key.)
fn has_toplevel_key(text: &str, key: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with(key) && t[key.len()..].trim_start().starts_with('=')
    })
}

/// Insert or refresh the HEAD block (prepended: top-level keys must precede
/// every table). A user-set copy of the same routing key outside our block
/// is never stomped — that is their router/proxy config.
fn upsert_head(existing: &str, block: &str, key: &str) -> Result<String, String> {
    match block_range(existing, HEAD_BEGIN, HEAD_END)? {
        Some((a, b)) => Ok(format!("{}{}{}", &existing[..a], block, &existing[b..])),
        None if has_toplevel_key(existing, key) => Err(format!(
            "config.toml already sets `{key}` outside a parsec-managed block — routing \
             through parsec would duplicate it; remove or move your setting, then re-run"
        )),
        None => {
            let sep = if existing.is_empty() { "" } else { "\n" };
            Ok(format!("{block}{sep}{existing}"))
        }
    }
}

/// Insert or refresh the TAIL block (appended: tables are safe at end).
/// A foreign `[model_providers.parsec]` (theirs, no markers) is never
/// touched.
fn upsert_tail(existing: &str, block: &str) -> Result<String, String> {
    match block_range(existing, TAIL_BEGIN, TAIL_END)? {
        Some((a, b)) => Ok(format!("{}{}{}", &existing[..a], block, &existing[b..])),
        None if existing.contains("[model_providers.parsec]") => Err(
            "config.toml already defines [model_providers.parsec] outside a \
             parsec-managed block — rename or remove it, then re-run"
                .to_string(),
        ),
        None => {
            let mut out = existing.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block);
            Ok(out)
        }
    }
}

/// Remove one marker block (plus the separator blank line its insert
/// added, on whichever side it was). `None` = no block found.
fn remove_marked(existing: &str, begin: &str, end: &str) -> Result<Option<String>, String> {
    match block_range(existing, begin, end)? {
        None => Ok(None),
        Some((a, b)) => {
            let mut head = &existing[..a];
            let mut tail = &existing[b..];
            if head.ends_with("\n\n") {
                head = &head[..head.len() - 1];
            } else if a == 0 && tail.starts_with('\n') {
                tail = &tail[1..];
            }
            Ok(Some(format!("{head}{tail}")))
        }
    }
}

/// Both managed blocks removed; None when neither was present.
fn remove_blocks(existing: &str) -> Result<Option<String>, String> {
    let after_head = remove_marked(existing, HEAD_BEGIN, HEAD_END)?;
    let base = after_head.as_deref().unwrap_or(existing);
    match remove_marked(base, TAIL_BEGIN, TAIL_END)? {
        Some(done) => Ok(Some(done)),
        None => Ok(after_head),
    }
}

// ── commands ────────────────────────────────────────────────────────────────

/// `parsec setup codex [--byok]`: head + tail config blocks (routing is
/// DEFAULT-ON for every codex session), skills, legacy-artifact migration,
/// bin alias, proxy warm. Repeat runs converge (and switch modes); nothing
/// of the user's is rewritten.
pub fn setup(mode: Mode) -> anyhow::Result<()> {
    let home = codex_home();
    if !home.exists() {
        println!(
            "note: {} does not exist yet — is Codex CLI installed? Creating it anyway.",
            home.display()
        );
    }

    // Port: state-file port when Claude Code setup ran, else the default —
    // the same resolution the opencode installer uses, no state written.
    //
    // Then run it through choose_free_port, which is the part that was
    // missing. codex bakes this number into config.toml as literal text and
    // nothing re-validates it afterwards, so a port held by a FOREIGN process
    // is not a dead route — it is a live one, pointed at a stranger, carrying
    // the user's OAuth Bearer and chatgpt-account-id on every request. The
    // opencode shim refuses to route without a /health identity check
    // (`proxyHealthy`); this is the installer-side equivalent, and it has to
    // happen HERE, before the block is written — warm_proxy already warns
    // about a foreign listener, but by then the config names it.
    let preferred = crate::setup::load_state()
        .map(|st| st.port)
        .filter(|p| *p > 0)
        .unwrap_or_else(crate::setup::default_port);
    let port = crate::setup::choose_free_port(preferred);
    if port != preferred {
        println!(
            "127.0.0.1:{preferred} is held by a non-parsec process — routing codex at \
             {port} instead. If Claude Code is routed at {preferred}, re-run `parsec setup` \
             so both land on one proxy."
        );
    }

    let cfg_path = config_path();
    let existing = read_optional(&cfg_path)?.unwrap_or_default();
    let updated = upsert_head(&existing, &head_block(mode, port), head_key(mode))
        .and_then(|v| upsert_tail(&v, &tail_block(port)))
        .map_err(|why| anyhow::anyhow!("{}: {why}", cfg_path.display()))?;
    if updated == existing {
        println!("routing already current in {}", cfg_path.display());
    } else {
        write_via_tmp(&cfg_path, &updated)?;
        println!(
            "routing written to {} ({} mode, port {port}) — applies to ALL codex sessions",
            cfg_path.display(),
            match mode {
                Mode::Subscription => "ChatGPT-subscription",
                Mode::Byok => "BYOK",
            }
        );
    }

    // Legacy migration: the opt-in profile and deprecated prompt files from
    // pre-default-on installs (sentinel-gated, quiet when absent).
    let prof = profile_path();
    if let Some(cur) = read_optional(&prof)? {
        if cur.contains(PROFILE_SENTINEL) {
            std::fs::remove_file(&prof)?;
            println!(
                "removed {} — routing is default-on now, no --profile flag needed",
                prof.display()
            );
        }
    }
    remove_prompts(true);

    // The command surface, as skills (both scan locations — see skills_dirs).
    for sdir in skills_dirs() {
        let mut dropped = 0usize;
        for (name, contents) in skill_files(port) {
            if write_skill(&sdir, name, &contents)? {
                dropped += 1;
            }
        }
        println!(
            "{dropped} skills in {} (try /skills or $parsec-savings in codex)",
            sdir.display()
        );
    }

    // Stable binary path for the skills and the SessionStart hook: symlink
    // (unix) or a refreshed copy (Windows) at the running binary.
    //
    // This used to be #[cfg(unix)] while the skills at hook_command_line()
    // point at %USERPROFILE%\.parsec\bin\parsec.exe unconditionally — so for
    // anyone who installed via the Claude Code plugin rather than
    // install.ps1, that path never existed and BOTH the /parsec-* skill
    // surface and the proxy-reviving SessionStart hook were dead on Windows.
    if let Err(e) = crate::setup_opencode::refresh_bin_alias() {
        println!(
            "could not refresh the {} alias ({e}) — skills and the session hook fall \
             back to `parsec` on PATH",
            crate::setup_opencode::bin_alias_path().display()
        );
    }

    // Warm the proxy now; the SessionStart hook keeps it alive from here on.
    // Port occupancy is NOT compatibility: a supervisor from an older plugin
    // release answers /health 200 while 404-ing every Codex route, and setup
    // used to call that success (report, "Additional setup defect"). Require
    // the wires this integration actually rides.
    warm_proxy(port);

    match mode {
        Mode::Subscription => println!(
            "done. Every codex session now routes through the proxy with your \
             existing ChatGPT sign-in (token stays on this machine). BYOK instead: \
             parsec setup codex --byok. Undo anytime: parsec disable codex"
        ),
        Mode::Byok => println!(
            "done. Every codex session now routes through the proxy using \
             OPENAI_API_KEY. Subscription mode instead: parsec setup codex. \
             Undo anytime: parsec disable codex"
        ),
    }
    Ok(())
}

/// `parsec disable codex` — remove exactly (and only) the managed artifacts:
/// both config blocks, the skills, and any legacy profile/prompt files.
pub fn disable() -> anyhow::Result<()> {
    let cfg_path = config_path();
    match read_optional(&cfg_path)? {
        Some(cur) => match remove_blocks(&cur) {
            Ok(Some(updated)) => {
                write_via_tmp(&cfg_path, &updated)?;
                println!("removed parsec routing from {}", cfg_path.display());
            }
            Ok(None) => println!(
                "no parsec-managed blocks in {} — nothing to undo",
                cfg_path.display()
            ),
            Err(why) => println!("{}: {why}", cfg_path.display()),
        },
        None => println!("no {} — nothing to undo", cfg_path.display()),
    }

    let prof = profile_path();
    if let Ok(Some(cur)) = read_optional(&prof) {
        if cur.contains(PROFILE_SENTINEL) {
            std::fs::remove_file(&prof)?;
            println!("removed {}", prof.display());
        } else {
            println!("{} is not parsec-managed — left alone", prof.display());
        }
    }
    remove_prompts(false);
    remove_skills(false);
    println!("codex routing disabled — takes effect on the next codex launch");
    Ok(())
}

/// Remove exactly the managed skill dirs (sentinel-gated), from every
/// install location.
fn remove_skills(quiet: bool) {
    for dir in skills_dirs() {
        for (name, _) in skill_files(0) {
            let path = dir.join(name).join("SKILL.md");
            if let Ok(Some(cur)) = read_optional(&path) {
                if cur.contains(SKILL_SENTINEL) {
                    let removed = std::fs::remove_file(&path).and_then(|()| {
                        std::fs::remove_dir(path.parent().unwrap_or(Path::new("/")))
                    });
                    match removed {
                        Ok(()) => {
                            if !quiet {
                                println!("removed skill {}", path.display());
                            }
                        }
                        Err(e) => {
                            eprintln!("could not remove {}: {e} — delete by hand", path.display())
                        }
                    }
                }
            }
        }
    }
}

/// Remove exactly the legacy managed prompt files (sentinel-gated). Quiet on
/// absence.
fn remove_prompts(quiet: bool) {
    let dir = prompts_dir();
    for name in [
        "parsec-savings.md",
        "parsec-proxy.md",
        "parsec-key.md",
        "parsec-setup.md",
        "parsec-uninstall.md",
    ] {
        let path = dir.join(name);
        if let Ok(Some(cur)) = read_optional(&path) {
            if cur.contains(PROMPT_SENTINEL) {
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        if !quiet {
                            println!("removed legacy prompt {}", path.display());
                        }
                    }
                    Err(e) => {
                        eprintln!("could not remove {}: {e} — delete by hand", path.display())
                    }
                }
            }
        }
    }
}

/// Uninstall hook: same removal as [`disable`], quiet and best-effort —
/// called from `parsec uninstall` so a full cleanup never leaves Codex
/// pointed at a proxy that no longer exists.
pub fn remove_if_managed() {
    let cfg_path = config_path();
    if let Ok(Some(cur)) = read_optional(&cfg_path) {
        if let Ok(Some(updated)) = remove_blocks(&cur) {
            match write_via_tmp(&cfg_path, &updated) {
                Ok(()) => println!("removed codex routing from {}", cfg_path.display()),
                Err(e) => eprintln!(
                    "could not update {}: {e} — remove the parsec blocks by hand",
                    cfg_path.display()
                ),
            }
        }
    }
    let prof = profile_path();
    if let Ok(Some(cur)) = read_optional(&prof) {
        if cur.contains(PROFILE_SENTINEL) {
            let _ = std::fs::remove_file(&prof);
        }
    }
    remove_prompts(true);
    remove_skills(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_CFG: &str =
        "# my codex config\nmodel = \"gpt-5.6\"\n\n[mcp_servers.db]\ncommand = \"db-mcp\"\n";

    fn installed(mode: Mode) -> String {
        let v = upsert_head(USER_CFG, &head_block(mode, 8082), head_key(mode)).unwrap();
        upsert_tail(&v, &tail_block(8082)).unwrap()
    }

    #[test]
    fn blocks_carry_the_contract_surface() {
        // A rename in any of these silently breaks routing/attribution/
        // revival for every user on the next release.
        let sub = head_block(Mode::Subscription, 8082);
        assert!(sub.contains("openai_base_url = \"http://127.0.0.1:8082/chatgpt\""));
        assert!(!sub.contains("model_provider"));
        let byok = head_block(Mode::Byok, 8082);
        assert!(byok.contains("model_provider = \"parsec\""));
        assert!(!byok.contains("openai_base_url"));
        let tail = tail_block(8082);
        for needle in [
            "[model_providers.parsec]",
            "base_url = \"http://127.0.0.1:8082/openai/v1\"",
            "env_key = \"OPENAI_API_KEY\"",
            "wire_api = \"responses\"",
            "\"x-parsec-tool\" = \"codex\"",
            "[[hooks.SessionStart]]",
        ] {
            assert!(tail.contains(needle), "{needle} missing from tail block");
        }
        // The revival hook command is platform-specific.
        #[cfg(unix)]
        assert!(tail.contains("/bin/sh -c") && tail.contains("parsec up"));
        #[cfg(windows)]
        assert!(tail.contains("cmd /c") && tail.contains("parsec.exe\" up"));
    }

    #[test]
    fn head_prepends_and_tail_appends_around_user_bytes() {
        let full = installed(Mode::Subscription);
        // TOML shape: our top-level key BEFORE the user's first table, the
        // provider/hook tables after their last byte.
        let head_pos = full.find("openai_base_url").unwrap();
        let user_pos = full.find("# my codex config").unwrap();
        let table_pos = full.find("[model_providers.parsec]").unwrap();
        assert!(head_pos < user_pos && user_pos < table_pos);
        assert!(
            full.contains(USER_CFG),
            "user bytes must survive contiguously"
        );
        // Idempotent re-run.
        let again = upsert_head(
            &full,
            &head_block(Mode::Subscription, 8082),
            "openai_base_url",
        )
        .and_then(|v| upsert_tail(&v, &tail_block(8082)))
        .unwrap();
        assert_eq!(again, full);
        // Mode switch rewrites the head block only.
        let switched = upsert_head(&full, &head_block(Mode::Byok, 8082), "model_provider").unwrap();
        assert!(switched.contains("model_provider = \"parsec\""));
        assert!(!switched.contains("openai_base_url ="));
    }

    #[test]
    fn refuses_foreign_keys_and_tables() {
        // A user-set routing key is their router config — never stomped.
        let has_url = format!("openai_base_url = \"http://my-router\"\n{USER_CFG}");
        assert!(upsert_head(
            &has_url,
            &head_block(Mode::Subscription, 8082),
            "openai_base_url"
        )
        .is_err());
        // …but only the SAME key conflicts: their model_provider doesn't
        // block subscription mode.
        let has_mp = format!("model_provider = \"other\"\n{USER_CFG}");
        assert!(upsert_head(
            &has_mp,
            &head_block(Mode::Subscription, 8082),
            "openai_base_url"
        )
        .is_ok());
        assert!(upsert_head(&has_mp, &head_block(Mode::Byok, 8082), "model_provider").is_err());
        // Foreign provider table refused, as before.
        let foreign = "[model_providers.parsec]\nbase_url = \"http://my-own-thing\"\n";
        assert!(upsert_tail(foreign, &tail_block(8082)).is_err());
        // A hand-broken block (BEGIN without END) is reported, not eaten.
        let broken = format!("{USER_CFG}{TAIL_BEGIN} >>>\n[model_providers.parsec]\n");
        assert!(upsert_tail(&broken, &tail_block(8082)).is_err());
    }

    #[test]
    fn remove_roundtrips_user_bytes_exactly() {
        for mode in [Mode::Subscription, Mode::Byok] {
            let full = installed(mode);
            let removed = remove_blocks(&full).unwrap().expect("blocks present");
            assert_eq!(
                removed, USER_CFG,
                "{mode:?}: user config must survive byte-identically"
            );
        }
        // Nothing to remove ⇒ None.
        assert_eq!(remove_blocks(USER_CFG).unwrap(), None);
        // Empty-file install/remove roundtrip.
        let only_ours = upsert_head("", &head_block(Mode::Subscription, 8082), "openai_base_url")
            .and_then(|v| upsert_tail(&v, &tail_block(8082)))
            .unwrap();
        assert_eq!(remove_blocks(&only_ours).unwrap().unwrap(), "");
    }

    #[test]
    fn skills_carry_the_full_command_surface() {
        // A name vanishing here silently drops that command for every user.
        let files = skill_files(8082);
        let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
        for want in [
            "parsec-savings",
            "parsec-proxy",
            "parsec-key",
            "parsec-setup",
            "parsec-uninstall",
        ] {
            assert!(names.contains(&want), "{want} missing from skill set");
        }
        for (name, contents) in &files {
            assert!(contents.contains(SKILL_SENTINEL), "{name}: no sentinel");
            assert!(
                contents.starts_with(&format!("---\nname: {name}\ndescription:")),
                "{name}: bad frontmatter"
            );
        }
        let proxy = &files.iter().find(|(n, _)| *n == "parsec-proxy").unwrap().1;
        assert!(proxy.contains("http://127.0.0.1:8082/health"));
        let uninst = &files
            .iter()
            .find(|(n, _)| *n == "parsec-uninstall")
            .unwrap()
            .1;
        assert!(uninst.contains("CONFIRM"));
    }
}
