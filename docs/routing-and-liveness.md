# Routing and proxy liveness

How Claude gets pointed at the dasein proxy, what the alternatives are, and
what to do when the proxy isn't there. Two topics because they are one
problem: the routing mechanism we use is fixed at session launch, so it
cannot route around a dead proxy — liveness is the routing story's other
half.

Status: §1–§3 are researched from the Claude Code docs and the code in this
repo. §4 is a proposal, not implemented. §2.3 records an unresolved factual
conflict — settle it before building on §2.

## 1. The interception landscape

Every documented way to put something between Claude and the model API.

### 1.1 Exists and works

| Mechanism | Surface | Notes |
|---|---|---|
| `ANTHROPIC_BASE_URL` | Claude Code CLI | What we use. Set via the `env` block of the user's `settings.json` (`setup.rs:691`). |
| `ANTHROPIC_{AWS,BEDROCK,VERTEX,FOUNDRY}_BASE_URL` | Claude Code CLI | Per-platform twins; irrelevant unless we support those backends. |
| `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` | Claude Code CLI | Standard proxy vars, honored. SOCKS is **not** supported. See §3.2. |
| `NODE_EXTRA_CA_CERTS`, `CLAUDE_CODE_CERT_STORE` | Claude Code CLI | Custom CA — the supported path for MITM interception. Ignored in cloud sessions. |
| `base_url` / `baseURL` constructor arg | Anthropic API SDKs | Not dynamic; a new client must be constructed. Only relevant to `bench`. |
| `env` in `ClaudeAgentOptions` | Claude Agent SDK | Forwarded to the CLI subprocess, so it reduces to `ANTHROPIC_BASE_URL`. |

### 1.2 Does not exist — do not design around these

- **Hooks cannot intercept model requests.** Every hook event fires around
  *tool* execution or user input. There is no pre-LLM-request event, and no
  hook output field rewrites the outbound API call. `additionalContext`
  injects into the context window, not into the request.
- **MCP servers cannot intercept model requests.** They supply tools; they
  are not in the request path.
- **Claude Desktop has no model-endpoint configuration.**
  `claude_desktop_config.json` is MCP-only. `ANTHROPIC_BASE_URL` is not
  documented as honored. Absent §3.2, Desktop is out of reach — scope it out
  explicitly rather than carrying it as an open aspiration.
- **Claude Code has no failover.** If the configured base URL does not
  answer, the request fails. It does not fall back to `api.anthropic.com`.
  A hung (rather than refused) connection burns `API_TIMEOUT_MS` first —
  10 minutes by default.

## 2. What we do today, and what it costs

`dasein setup` merges `ANTHROPIC_BASE_URL` into the `env` block of the
user's `settings.json`, additively: a key the user already set is never
overwritten, and a foreign base URL is refused rather than clobbered
(`setup.rs:678`, `setup.rs:701`).

### 2.1 Known cost: Claude Code features disable themselves

Pointing `ANTHROPIC_BASE_URL` at a non-first-party host causes Claude Code
to disable:

- **Remote Control** — disabled outright as of Claude Code v2.1.196.
- **MCP tool search** — disabled by default; recoverable with
  `ENABLE_TOOL_SEARCH=true`.

This is a real product cost of the routing mechanism, independent of
anything our proxy does. If we keep `ANTHROPIC_BASE_URL`, we inherit it.
Worth surfacing to users rather than letting them discover it as a bug.

### 2.2 Known cost: no mid-session repointing

`ANTHROPIC_BASE_URL` is read at Claude Code launch. Nothing in the product
lets a running session be repointed at a different endpoint, and nothing
falls back. Recovery therefore means *putting a listener back on the same
port* — which is what `dasein up` does (`setup.rs:308`) and why §4 is framed
the way it is.

### 2.3 Open question — settle before relying on §2.2

There is a direct conflict here that we have not resolved empirically:

- `setup.rs:305` asserts the routing env "is read at Claude Code launch and
  cannot change." The whole `dasein up` design rests on this.
- A docs review claims the `settings.json` `env` block is watched and
  reloaded per session turn, which would make the base URL hot-swappable.

One of these is wrong. **Test:** start a session, flip the port in
`settings.json` mid-conversation, send a turn, observe which listener
receives it. If the env block really is live, dynamic repointing becomes
available and §4 gains a cheaper option (repoint to a passthrough shim
instead of reviving the engine). Until then, assume `setup.rs:305`.

## 3. Alternatives considered

### 3.1 Keep `ANTHROPIC_BASE_URL` (status quo)

Cheapest, already built, CLI-only, carries §2.1.

### 3.2 `HTTPS_PROXY` + custom CA

A genuinely different interception layer: Claude Code explicitly supports
MITM-with-custom-CA via `NODE_EXTRA_CA_CERTS`. It is the only path that
could plausibly reach Claude Desktop, via the macOS system network proxy,
and it survives any client that ignores `ANTHROPIC_BASE_URL`.

Against it:

- It puts us in the TLS path for **subscription OAuth tokens**, not just
  BYOK API keys. This stays on the user's machine, so it does not violate
  the letter of the data-plane rule in `CLAUDE.md` — but it is a materially
  larger trust ask than terminating a plaintext loopback connection.
- Installing a CA into the system trust store is a large escalation of the
  installer's footprint and a matching uninstall obligation.

Treat as a deliberate product decision, not an implementation detail.

### 3.3 Hooks / MCP

Not possible. See §1.2.

## 4. Liveness: keeping the port answered

Because §2.2 holds, the requirement is: **a connection to
`127.0.0.1:<routed_port>` always gets a valid Anthropic response.** Two
separable failure classes.

### 4.1 The process is gone — nothing accepts

Causes, most to least common:

1. **Our own idle timer.** `spawn_proxy_detached` sets
   `DASEIN_PROXY_IDLE_EXIT_S=1800` on autostart (`setup.rs:864`), and the
   sweep calls `process::exit(0)` (`server.rs:374`). The usual "proxy died
   mid-conversation" is not a crash — it is a user returning after 30 idle
   minutes to a dead port. In-flight requests hold the timer off
   (`server.rs:195`), so agentic loops are safe; walking away is not.
2. Crash, OOM kill, machine sleep/reboot, binary replaced by an upgrade.

`SessionStart` revives the proxy, but only at session start. `dasein up` is
the manual twin. Nothing covers mid-session.

### 4.2 The process is alive but the engine is broken

Partly handled: the freezer runs in `spawn_blocking` with the panic mapped
to an error (`server.rs:800`), poisoned locks are recovered
(`server.rs:433`), and the engine fails open per step
(`freeze.rs:848`). What is not pinned: neither `Cargo.toml` sets
`panic`, so unwind is inherited rather than guaranteed, and there is no test
asserting a panicking scorer still yields a served response. This is what
makes the "fail open, but measured" guarantee in `CLAUDE.md` true at the
process level and not only per step.

### 4.3 Options, ranked by value over effort

**(1) Revive on `UserPromptSubmit`.** Highest leverage. That hook fires
*before* the turn's API request goes out, so a liveness check there closes
the exact mid-session gap. Both halves already exist — `port_listening`
(`hook.rs:369`) and `up()` (`setup.rs:308`) — and the event is not currently
registered in `packages/plugin/hooks/hooks.json`. Zero install footprint;
turns "dead port" into "one slow turn."

Caveats: cold start includes the ONNX model load, so the revived turn may
stall noticeably — the hook must wait for the listener rather than let the
turn race it. And `UserPromptSubmit` does not fire during tool loops, so
this is a recovery mechanism, not a liveness guarantee.

**(2) Socket activation.** The literal answer, and elegant here: the init
system owns the listening socket, so the port stays bound while our process
is dead, and the first connection launches us. This makes idle-exit not just
safe but *correct* — we keep the memory savings and lose the failure mode.

- macOS: LaunchAgent with the `Sockets` key.
- Linux: a `.socket` unit, `ListenStream=127.0.0.1:PORT`, `Accept=no`.
- Windows: **no equivalent.** Scheduled Task (at logon, restart-on-failure)
  or a Windows Service, which needs admin and is likely disqualifying for
  our install story. Windows realistically falls back to (1).

Cost: an install-time system registration plus a matching uninstall
obligation. This is the only option that is a true guarantee, and the only
one that meaningfully grows the installer.

**(3) Harden §4.2.** Pin `panic = "unwind"` explicitly; add a test that a
panicking scorer still serves a response.

### 4.4 Recommendation

(1) first — cheap, local, fixes the common case. Then (3), to make fail-open
honest end to end. Treat (2) as a separate product decision about installer
footprint, not a bug fix. Revisit all of this if §2.3 resolves in favor of a
live `env` block.
