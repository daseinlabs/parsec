# Claude Desktop integration

How parsec reaches Claude Desktop, what it deliberately does not touch, and
the two costs the feature carries.

Status: implemented, at feature parity with CC-Router's interceptor. Opt-in
from the binary; the Windows installer enables it when Claude Desktop is
present (`-NoDesktop` opts out) — see §3.
`packages/proxy/src/setup_desktop.rs` + `packages/proxy/src/desktop_addon.py`,
exposed as `parsec setup desktop`, `parsec desktop start|stop|restart|status`,
`parsec disable desktop`, and the `/parsec:desktop` skill. The mechanism is
the one CC-Router uses (`docs/cc-router-comparison.md` §2.2); §5 is the
parity matrix, including the two items deliberately not ported and why.

## 1. Why this is not just another `ANTHROPIC_BASE_URL`

Every other surface parsec captures asks the client politely:

| Surface | Mechanism | Module |
|---|---|---|
| Claude Code | `ANTHROPIC_BASE_URL` in `settings.json` | `setup.rs` |
| opencode | plugin file drop | `setup_opencode.rs` |
| Codex CLI | config profile / custom provider | `setup_codex.rs` |
| **Claude Desktop** | **process-scoped TLS interception** | **`setup_desktop.rs`** |

Claude Desktop embeds an Anthropic SDK hardwired to `api.anthropic.com` and
honors no endpoint override — `claude_desktop_config.json` is MCP-only
(`routing-and-liveness.md` §1.2). Interception is not a shortcut here; it is
the only path that exists.

## 2. The mechanism

```
Claude Desktop (Cowork / Agent mode)
   │  TLS to api.anthropic.com
   ▼
mitmdump --mode local:Claude        ← intercepts ONLY the Claude process
   │  addon.py rewrites host/port for /v1/messages*, /v1/models
   │  and tags them x-parsec-tool: claude-desktop
   ▼
127.0.0.1:<routed port>             ← the same parsec supervisor Claude Code uses
   │  curation, splicing, count_tokens counterfactual  (unchanged)
   ▼
api.anthropic.com                   ← user's own auth headers, forwarded verbatim
```

Three things worth stating precisely:

**The serving side needed no change.** A redirected `/v1/messages` arrives at
the routed port indistinguishable from Claude Code's. It hits the same
`server.rs` router, the same freezer, the same ledger. Desktop and Claude Code
therefore share one proxy, one ledger, and one savings number — which is why
`target_url()` resolves the port from `setup_state.json` rather than inventing
its own.

**The addon touches `/v1/messages*` and `/v1/models`, and nothing else.**
Those are exactly the paths the proxy serves — `/v1/models` was added to
`server.rs` as a verbatim GET passthrough *because* of this feature: the
interceptor redirects whole hosts, not individual routes, so a 404 there would
surface in Desktop as a broken model picker. Claude Desktop also talks to
`/v1/oauth/*` (session auth), `/v1/environments/*` (the Cowork bridge), and
`/api/*` (the claude.ai app wire); those carry the user's session token for
surfaces parsec does not serve, and redirecting them would break login and
Cowork rather than curate them.

Pinned by tests on both sides: `addon_only_redirects_the_paths_the_proxy_serves`
against the embedded addon source, and `count_tokens_verbatim_and_stateless`
asserting `/v1/oauth/token` still 404s at the proxy.

**The addon adds no auth header.** CC-Router's equivalent injects an auth
token, because substituting credentials is its whole product (§1 of the
comparison doc). parsec's is the opposite: the addon rewrites a destination
and `forward_auth_headers` passes Desktop's own OAuth token through untouched.
`addon_never_touches_auth_headers` pins that.

The one header it does set is `x-parsec-tool: claude-desktop` — parsec's
existing calling-tool attribution seam, the same one the opencode shim uses.
It is what makes Desktop savings separable from the CLI's in the ledger, it is
charset-gated server-side to `[a-z0-9-]{1,32}` so the contract cannot carry
raw text, and it never leaves the machine: `forward_auth_headers` whitelists
only auth headers upstream, so the tag is dropped at the hop
(`desktop_attribution_header_never_reaches_upstream`).

It is set only on rewritten `api.anthropic.com` traffic. Claude Code running
*inside* Desktop (Cowork) already points at the proxy via
`ANTHROPIC_BASE_URL`, and the addon leaves it strictly alone — tagging it
`claude-desktop` would mis-attribute Claude Code's own savings. CC-Router
does act on that case, but only to inject its proxy secret, which parsec has
no equivalent of.

### What is actually captured

**Cowork / Agent-mode inference only.** Regular Claude Desktop chat runs over
the claude.ai `/api/*` wire, not `/v1/messages`, and is untouched. Do not
describe this feature as "parsec now works in Claude Desktop" without that
qualifier.

## 3. Commands

| Command | What it does |
|---|---|
| `parsec setup desktop` | Provision: explain the scope, check mitmproxy, generate the CA, print (or with `--install-ca` run) the trust command, clear the platform gates, then start. Idempotent. |
| `parsec setup desktop --status` | Alias for `parsec desktop status`. |
| `parsec desktop start [--autostart]` | Start intercepting with what setup provisioned. Refreshes the addon first, so a moved routed port is picked up. |
| `parsec desktop stop [--keep-autostart]` | Stop. Also removes the boot service unless `--keep-autostart`, so "stopped" does not silently un-stop itself at next login. |
| `parsec desktop restart` | Stop and start, preserving the auto-start choice. |
| `parsec desktop status` | Configured / running / auto-start / Desktop present / mitmproxy / CA / extension approval / routed scope. Changes nothing. |
| `parsec disable desktop` | Full teardown: stop, remove the boot service, the addon, and the state file; print the CA removal command. |

### From the Windows installer

`scripts/install.ps1` sets Desktop up as part of its normal run: auto-detect
adds `desktop` whenever `%LOCALAPPDATA%\AnthropicClaude\Claude.exe` exists —
the same location `claude_desktop_installed()` probes, so the script and the
binary cannot disagree about what is installed. It then installs mitmproxy via
winget if missing and runs `parsec setup desktop --install-ca`.

**Windows needs administrator for the whole provision, not just the CA.**
Interception there runs through WinDivert, whose driver requires elevation, and
`run_start` spawns `mitmdump` detached and then requires it alive 1.5 s later —
a window no human can approve a UAC dialog inside. So the installer asks for
elevation **once**, up front, and runs the entire `parsec setup desktop` under
it (`Start-Process -Verb RunAs -Wait`), then prints `parsec desktop status`
from the unelevated shell because the elevated console takes its own output
with it. A declined prompt leaves Desktop unprovisioned with the exact command
to re-run — every other tool in that install still succeeded.

Two consequences worth stating plainly:

- The interceptor ends up running **elevated**, so `parsec desktop stop` needs
  an admin shell too — `taskkill` against a higher-integrity process is
  otherwise denied.
- This is where the installer departs from the binary's posture (§5.2): parsec
  itself only ever *prints* the trust command. The installer completes it, but
  never silently — the user sees Windows ask and can say no.

Opt-outs: `-NoDesktop` skips Desktop entirely even when installed; `-NoCa`
provisions without trusting the CA (Desktop then stays unintercepted until the
printed command is run). Both have env equivalents (`PARSEC_NO_DESKTOP`,
`PARSEC_NO_CA`) for the `irm | iex` form. On ARM64, auto-detected desktop is
dropped with a warning rather than aborting the install — the parsec binary is
published for win-x64 only.

`setup` and `start` share one code path — `setup` is `start` with the CA and
explainer steps in front — so the two entry points cannot drift on which gates
they check.

State lives in `~/.parsec/interceptor/state.json` (`enabled`, `autostart`,
`target`), beside the addon it describes, so a teardown removes both together.
It is the analogue of CC-Router's `desktopEnabled` / `desktopAutoStart`
config pair.

## 4. Lifecycle

`parsec setup desktop` is idempotent and reports every precondition rather
than assuming it, because the characteristic failure here is silent: an
unapproved Network Extension lets `mitmdump` start happily and capture
nothing.

1. Resolve `mitmdump` absolutely (launchd/systemd start with a minimal PATH).
2. Generate mitmproxy's CA if absent — there is no dedicated command, so it
   runs `mitmdump` on a throwaway port until `~/.mitmproxy/` appears.
3. Print the trust-store command, or run it under `--install-ca`.
4. Write the managed addon to `~/.parsec/interceptor/addon.py`, under the same
   sentinel-ownership discipline as the opencode shim: a file we wrote is
   refreshed, a file without `parsec-managed-interceptor` is the user's and is
   never overwritten.
5. macOS only: refuse to continue on `AwaitingApproval` / `NotInstalled`,
   parsed from `systemextensionsctl list`.
6. **Restart** the proxy on the target port. Not merely pre-warm: a proxy
   that predates the current binary keeps serving the OLD image, while the
   addon is rendered against the route table of the NEW one — so `/v1/models`
   gets redirected to a proxy that 404s it and Desktop's model picker breaks.
   Provisioning restarts to keep the two in step; `parsec desktop start` does
   not, because it is the routine path and must not drop a Claude Code
   session's in-flight requests. Only a process that identifies itself as a
   parsec proxy is ever shut down. Then it probes `/v1/models` and warns if
   the *binary* is stale — the case a restart cannot fix.

   This deliberately does not call `setup::up()`: that resolves the port from
   Claude Code's settings, which can differ from (or be absent after
   `parsec disable`) the port the addon was rendered against.
7. Spawn `mitmdump` detached (own process group / no console), PID at
   `~/.parsec/interceptor/mitmdump.pid`, log beside it. Under `--autostart`,
   install a launchd agent / systemd `--user` unit / Run-key entry instead.

`stop()` verifies the PID is actually `mitmdump` (via `ps -o comm=` /
`tasklist`) before signalling it — PIDs get reused, and killing an unrelated
process on the word of a stale pidfile is the bug that check exists to avoid.

`parsec uninstall` calls `setup_desktop::remove_if_managed()` alongside the
opencode and Codex teardowns, so a full cleanup never leaves an interceptor
aiming Desktop at a port nothing will answer on again.

## 5. The two costs, and how they are bounded

`docs/cc-router-comparison.md` §4 recommended against building this. That
recommendation was about *default* posture. It still holds for the binary —
`parsec setup` does not touch Desktop, and nothing in Claude Code's first-run
path mentions it — but **not** for `install.ps1`, which enables Desktop
whenever it finds it (§3). What follows is how the costs are contained rather
than a claim that they are absent.

### 5.1 A runtime dependency on mitmproxy (Python)

DIRECTION §7b's client thesis is "Node is not guaranteed; ship one auditable
static binary." That holds for everything parsec *ships* — the binary still
has zero runtime dependencies, and `desktop_addon.py` is embedded via
`include_str!`, not fetched. It cannot hold for the interception layer itself:
no Rust crate bundles macOS Network Extension + Windows WinDivert + Linux eBPF
the way mitmproxy does, and writing one is the weeks-to-months line in the
comparison doc's estimate table.

The bound: the dependency exists only when a user opts into Desktop. Absent
that, `mitmdump` is never invoked and nothing about the binary changes.

### 5.2 A CA in the system trust store

This is the machine-wide, hard-to-reverse change, so parsec does not make it
silently in either direction:

- `parsec setup desktop` **prints** the exact trust command by default. Only
  `--install-ca` runs it, and then the OS prompts for authentication. The
  Windows installer does complete the step (§3) — through a UAC prompt the
  user can decline, never silently.
- `parsec disable desktop` **prints** the removal command and does not run it
  — on macOS the certificate is matched by SHA-1, and a wrong guess would
  delete some other trust root.

Consequence worth being straight about: terminating TLS puts the proxy in the
path of **subscription OAuth tokens**, not just plaintext loopback. It stays on
the user's machine and the addon reads nothing, so the CLAUDE.md data-plane
rule holds — but it is a materially larger trust ask than routing Claude Code,
and the skill is written to say so before the first run.

## 6. CC-Router parity matrix

Item-by-item against `../../VictorMinemu/CC-Router`
(`src/interceptor/mitmproxy-manager.ts`, `src/interceptor/addon.py`,
`src/cli/cmd-client.ts`).

| CC-Router | parsec | Notes |
|---|---|---|
| `--mode local:<Claude\|Claude.exe\|claude>` | ✅ `desktop_process_name()` | Same per-platform names |
| `checkMitmproxyInstalled()` | ✅ `mitmdump_path()` | Resolved absolutely, for launchd/systemd's minimal PATH |
| `getNetworkExtensionStatus()` | ✅ `extension_status()` | Same `systemextensionsctl` parse, both output shapes, unit-tested |
| `openNetworkExtensionSettings()` | ✅ `open_extension_settings()` | Same `x-apple.systempreferences:` deep link + fallback |
| `printNetworkExtensionInstructions()` | ✅ `print_extension_instructions()` | Same 5-step walkthrough, printed on the gate failure |
| `printDesktopSupportExplainer()` | ✅ `print_scope_explainer()` | Routed vs untouched, printed before anything happens |
| `generateCaCert()` / `installCaCert()` | ✅ | Install is opt-in (`--install-ca`) rather than a prompt — see §5.2 |
| `writeAddonScript()` with target baked in | ✅ `render_addon()` | Same substitution, so a hand-run `mitmdump -s addon.py` behaves identically |
| Addon redirects `/v1/messages*` + `/v1/models` | ✅ | `/v1/models` passthrough added to `server.rs` to make this true |
| Addon passes `/v1/oauth`, `/v1/environments`, `/api/*` | ✅ | Plus a proxy-side test that `/v1/oauth/token` 404s |
| `start/stopInterceptor()`, PID file | ✅ | Ours also verifies the PID *is* `mitmdump` before signalling |
| `isInterceptorRunning()` | ✅ `running()` | |
| launchd / systemd `--user` / Run key | ✅ | Ours quotes paths; CC-Router's Run key breaks on `C:\Program Files\…` |
| `client start-desktop` / `stop-desktop` | ✅ `parsec desktop start` / `stop` | Including `--keep-autostart` |
| `desktopEnabled` / `desktopAutoStart` config | ✅ `interceptor/state.json` | |
| `isClaudeDesktopInstalled()` | ✅ `claude_desktop_installed()` | Same macOS/Windows probes; warns, never gates |
| Desktop status block in `client status` | ✅ `parsec desktop status` | Same fields, same loud call-out for unapproved extension |
| `source: cli \| desktop \| api` request tagging | ✅ `x-parsec-tool: claude-desktop` | Reuses parsec's existing attribution seam; lands in the savings ledger |
| Proxy secret injected as `x-api-key` | ❌ **not ported** | See below |
| Multi-account OAuth rotation, cooldowns, dashboard | ❌ out of scope | See below |

### Not ported: the proxy secret

CC-Router's addon injects `x-api-key: <secret>` so an internet-exposed router
can authenticate callers. Porting it would be actively harmful here: parsec
forwards the caller's credentials verbatim, so writing `x-api-key` would
**overwrite the user's own key** and the request would fail upstream. The
feature only makes sense for a proxy that substitutes credentials anyway.
parsec's proxy binds `127.0.0.1` only, which is what CC-Router's secret exists
to compensate for when a router is exposed.

### Not ported: account rotation

CC-Router's reason for existing is rotating requests across N Claude Max /
OpenAI subscription accounts. That is a different product from curation, and
it collides head-on with the CLAUDE.md non-negotiable that subscription OAuth
tokens are never substituted or routed through anything but the user's own
machine with their own credentials. Nothing in the Desktop path depends on it:
the interceptor is complete without it. If it is genuinely wanted, it is its
own decision, not a parity gap.

## 7. Known limits

- **Scope**: Cowork / Agent mode only (§2).
- **TLS termination is process-wide, redirection is not.** mitmproxy cannot
  know a request's host or path until after the handshake, so every byte the
  Claude process sends to *any* host is decrypted in mitmproxy's memory, even
  though the addon only acts on two Anthropic paths. mitmproxy's
  allow/ignore-hosts options could narrow the decryption itself; that is not
  wired up, and CC-Router does not do it either. It is the honest cost of the
  mechanism, not something the addon's narrowness fixes.
- **Relaunch required**: mitmproxy hooks the process at launch, so a Desktop
  that was already running stays unintercepted until ⌘Q and relaunch.
- **`count_tokens` under OAuth**: the savings counterfactual re-calls
  `/v1/messages/count_tokens` with the caller's own credentials. A
  subscription token whose scope rejects that makes the probe fail; the proxy
  already handles this (`server.rs` logs it and the row reads as no measured
  saving) — savings from Desktop traffic may therefore be under-reported
  rather than wrong, which is the correct side to err on under the
  measurement-honesty rule.
- **Linux** needs kernel ≥ 6.8 for mitmproxy's eBPF local mode; **Windows**
  WinDivert prompts for elevation on each launch. Both paths are implemented
  and neither has been exercised on real hardware yet — macOS is the tested
  platform.
