# Session-scoped proxy — folding the daemon into the per-session MCP process

**Status:** design note, 2026-07-29. Not yet implemented. Prior art surveyed from
`~/code/Autoloops/greplica` (npm `greplica` v0.1.12) and `~/code/LuD1161/agentjail`
(v1.1.0) — file references into those repos are as of this date.

## 1. The question

Can the proxy run in-process inside the session-scoped `parsec mcp` process
(one per Claude Code session, spawned and killed by CC) instead of as the
long-lived `parsec proxy` supervisor daemon — each session carrying its own
instance, all reaching the same brain?

Short answer: yes, and it is the recommended direction. The listener cannot go
away (interception fundamentally requires an HTTP server on the wire), but the
*daemon* can.

## 2. Hard constraints

- **The only interception point CC offers is `ANTHROPIC_BASE_URL`,** read once
  at session launch from settings. Hooks/MCP cannot intercept model traffic:
  hooks can only mutate tool results at creation time (`updatedToolOutput`),
  which permanently rewrites the transcript and destroys the per-request
  `count_tokens` counterfactual (measurement honesty). Request-time
  freezing/splicing needs a server on the path.
- **One fixed port, shared by all sessions.** A SessionStart hook cannot set
  env for its own session (`hook.rs`, `maybe_autostart_proxy` doc comment), so
  per-session ports are impossible for a plugin. Whatever the process shape,
  exactly one loopback port exists; N session processes must share or hand
  off one socket.
- **Determinism makes instances interchangeable.** Served bytes are a pure
  function of (conversation prefix, checkpoint, config) — no session dicts on
  the serving path (CLAUDE.md). Session A's request being served by session
  B's instance is correct by construction; the invariant that makes brain
  replicas round-robin-safe makes proxy instances round-robin-safe. Shared
  mutable state is append-only files in `~/.parsec`, which concurrent hook
  processes already write today.

## 3. Why bother — what the daemon costs

Every field incident so far has been daemon lifecycle, not curation logic:

- Reinstall left the proxy permanently dead with stale `setup_state.json`
  (`docs/debug-2026-07-17-plugin-reinstall.md`).
- "Not listening on an idle machine is normal" confusion; `Address already in
  use` race between two proxies; dev-build vs managed-binary mixups.
- **Version skew:** `claude plugin update` swaps the binary on disk but the
  old supervisor keeps serving until something kills it — potentially until
  reboot.

Tying the proxy's life to the session process makes these states
unrepresentable: no proxy exists to be dead, stale, or orphaned when no
session runs; there is always one when a session runs; every new session runs
the freshly installed binary. Deleted outright: `supervisor.rs`, the
heartbeat-file orphan guard, `maybe_autostart_proxy`, `parsec up`, idle/revive
logic, the proxy half of any future `parsec doctor`. Uninstall becomes file
removal. Port count *drops* from two (supervisor + worker ephemeral) to one.

Non-benefits, stated honestly: no curation, savings, or latency improvement
(one loopback hop removed — noise).

What is given up: the supervisor's separate-address-space crash isolation and
its seamless mid-crash passthrough. This mattered when the worker linked
ORT/ONNX native code; server-side embedding (2026-07-20,
`docs/server-side-embedding.md`) removed all of that from the client, so the
remaining worker is tokio + reqwest + serde — roughly the supervisor's own
risk profile. Residual cost: a hard crash of an in-process proxy takes its
session's MCP tools with it and drops in-flight requests that landed on it —
one visible failed request instead of zero. Mitigate with a `catch_unwind`
boundary around the proxy task; keep per-request fail-open (forward direct to
Anthropic on internal error) inside each instance.

## 4. Prior art

### 4a. agentjail capture gateway — the same problem, solved per-session

agentjail needed to capture Claude Code's `/v1/messages` traffic. Their MITM
tunnel could not (CC's bundled TLS stack ignores every CA-trust override —
`NODE_EXTRA_CA_CERTS`, keychain, even `NODE_TLS_REJECT_UNAUTHORIZED=0`), so
they fell back to the supported base-URL override — parsec's interception
point. Their design (ADR 0109, `internal/captureproxy/gateway.go`):

- **Per-session, in-process reverse proxy** running as a thread inside the
  process that launches `claude` (their sandbox "shield"). One gateway per
  session; dies with the session. No daemon on the LLM path.
- **Ephemeral ports, no shared-port problem:** they are the *launcher*, so
  they inject `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/aj~<nonce>` into the
  child's environment at spawn — each session binds `127.0.0.1:0`. The
  per-session URL nonce is a capability token and labels traffic by session.
  **A plugin cannot do this** (settings-file env only), which is exactly why
  parsec needs socket sharing/handoff on one fixed port. (Unless a launcher/
  wrapper mode is ever shipped — then this whole design collapses to
  agentjail's.)
- Fail-closed at launch (refuse to start the agent if the gateway can't
  bind), never mid-session.

Their *other* shared services show the fixed-port failure catalog first-hand:

- **netproxy** (one shared proxy for all sessions on :9100, ADR 0042):
  suffered stale cross-session state reuse and a lifecycle wart where session
  A's exit killed the proxy session B was using. Fixes: per-session bearer
  tokens keying all state, **registration leases (24h TTL) with a reaper
  instead of deregistration**, never kill-by-port, protocol-version handshake
  with "never restart a proxy serving other live sessions" (binary-version
  drift tolerated; incompatible protocol → fail closed, not kill). Their ADR
  names "a persistent service or refcount" as the known fix for the lifetime
  wart — i.e. leaseholding.
- **policy daemon**: supervised by launchd/systemd `KeepAlive`, not
  self-managed. ADR 0070: the updater exits 0 expecting the supervisor to
  restart the new binary; `Restart=on-failure` silently stranded it after
  every auto-update. Post-update health check verifies the *version string*
  over the socket and rolls back on mismatch.
- **Singleton via flock + probe-before-unlink** (`daemonapp/singleton.go`):
  exclusive flock beside the socket; before unlinking an apparently-stale
  socket, *ping it for a valid protocol response*. A bare successful
  connect() is deliberately not treated as "live"; an unrecognizable
  squatter → refuse and exit, never steal.
- **Settings watchdog**: fsnotify on `~/.claude/settings.json` re-injects
  their hook entry if anything rewrites it — their answer to the same
  reinstall-wipes-settings bug parsec hit on 2026-07-17.
- **Fail-open, measured**: hook fails open when the daemon is unreachable,
  with per-occurrence telemetry + user-visible notice via `systemMessage` +
  a re-arming sentinel file — same posture as parsec's "fail open, but
  measured" non-negotiable.

### 4b. greplica — no server at all

Pure CLI + hooks; no daemon, no MCP server, no port. Ephemeral hook processes
open one shared SQLite DB directly (session rows keyed by session id, short
transactions). Anything needing a singleton is **elected through a lease row
in SQLite**: atomic insert-or-steal-if-expired, 5-minute TTL, 60s heartbeat
renewal; losers exit, a crashed winner's lease is stolen after TTL
(`libs/utils/worker-lease.ts`). Heavy compute is delegated to headless
`claude --print` rather than anything resident. Ops: a `doctor` command,
idempotent hook-merge on reinstall, atomic staged-copy + rename for updates,
no uninstall story.

Relevance: proves the free tier needs no resident process — parsec's
no-reread gate and scout MCP are already ephemeral/per-session; only curation
needs a listener. Never let the daemon become a free-tier dependency.

### 4c. Convergence

Both repos independently landed on **leases with TTL + stealing** as the
cross-process coordination primitive (greplica's SQLite lease table;
agentjail's netproxy registration leases), and agentjail's LLM-path component
is per-session in-process. That is strong external validation for the shape
below — and for choosing lease-file coordination over `SO_REUSEPORT`, which
would work on macOS/Linux (kernel balances N binders of one port) but has no
Windows equivalent, leaving Windows on a permanently divergent code path.

## 5. Proposed design

Proxy listener as a tokio task inside each session's `parsec mcp` process.
One fixed port (from settings, as today). Coordination via a lease file in
`~/.parsec`:

1. **Startup:** each MCP process registers a session lease (TTL ~2× heartbeat)
   and tries to become the port owner (exclusive lock). The owner binds the
   port and serves for *all* sessions — valid because serving is deterministic
   and per-conversation state-free. Non-owners idle, renew their lease, and
   watch the owner.
2. **Ownership check before takeover:** on finding the port occupied, probe
   `GET /health` for a valid parsec response. Healthy parsec → stand down.
   Unrecognizable squatter → surface loudly, never kill-by-port. Stale lock +
   dead port → take over. (agentjail's probe-before-unlink; kills the
   `Address already in use` race class.)
3. **Owner exit:** when the owning session ends, CC kills its MCP process; a
   surviving non-owner's watch fires and it rebinds. The connection-refused
   window during handoff is bounded by the watch interval; CC surfaces a
   failed request and retries — acceptable given fail-open posture, and the
   window only exists when ≥2 sessions overlap an owner's exit.
4. **Last lease drains → nothing listens.** Correct: no sessions, no callers.
5. **Version skew:** a non-owner running a newer binary tolerates an older
   healthy owner if the `/health`-advertised protocol version matches
   ("never restart a proxy serving other live sessions"); on mismatch it
   reports and waits for natural handoff rather than killing.
6. **Crash isolation:** `catch_unwind` around the proxy task; per-request
   fail-open forwards direct to Anthropic on any internal error, counted and
   surfaced per the fail-open non-negotiable.
7. **Windows:** identical code path — lock file + TTL + probe; no socket
   tricks needed. (This is the argument against `SO_REUSEPORT`.)

Also adopt regardless of shape: extend the SessionStart re-assert to *all*
managed settings keys (statusline, subagentStatusLine, spinnerVerbs), not
just the routing env — the reinstall wipe takes all of them.

## 6. Open items

- Ledger/adjudicator concurrent-append audit: multiple hook processes already
  append; confirm writes stay under atomic-append size or add per-line
  locking before N proxy instances join in.
- Handoff-window behavior: verify CC's retry semantics on connection-refused
  mid-session; decide whether the non-owner watch interval needs to be
  sub-second.
- MCP process lifetime: confirm CC spawns `parsec mcp` in headless
  (`claude -p`) runs too, and what happens for users who pointed non-CC tools
  at the proxy port (unsupported today; stays unsupported).
- Whether a launcher/wrapper mode (agentjail-style env-at-spawn, ephemeral
  ports, zero sharing) is worth offering for power users — it deletes this
  entire coordination layer where usable.
- Prototype order: macOS first, single-session and two-session overlap tests
  (owner exit mid-flight), then Windows lock semantics.
