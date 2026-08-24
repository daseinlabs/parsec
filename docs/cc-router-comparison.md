# CC-Router comparison — routing, Desktop interception, effort

How [CC-Router](https://github.com/VictorMinemu/CC-Router) puts itself in the
model-request path (including for Claude Desktop), how that differs from what
parsec does today, and what it would cost parsec to adopt the Desktop piece.

Status: researched from the CC-Router source (commit as vendored under
`../../VictorMinemu/CC-Router`) and parsec's own code + `docs/routing-and-liveness.md`.
The effort estimate in §4 is a recommendation, not a committed plan.

## 1. What the two projects are actually doing in the middle

The single most important framing: **CC-Router and parsec sit in the request
path for opposite reasons**, and that difference drives everything below.

- **CC-Router** is in the path to **swap the auth token** — it rotates requests
  across N Claude Max / OpenAI subscription accounts, injecting a *different*
  OAuth token than the client sent. Being in the path is the whole product.
- **parsec** is in the path to **rewrite the body** — cache-safe splicing +
  curation — while forwarding the user's own auth headers **verbatim**
  (`server.rs:453` `forward_auth_headers`; DIRECTION §2 "data plane local").
  parsec never substitutes credentials.

| | CC-Router | parsec |
|---|---|---|
| Reason it's in the path | Substitute OAuth token (account rotation) | Rewrite request body (curation); passthrough auth |
| Claude Code capture | `ANTHROPIC_BASE_URL` in settings.json | `ANTHROPIC_BASE_URL` in settings.json — **identical** |
| Claude Desktop capture | mitmproxy local mode + system CA trust + `addon.py` rewrite | same mechanism, **opt-in** since `setup_desktop.rs` (`docs/claude-desktop-integration.md`) |
| Sits in TLS path for OAuth tokens | Yes (terminates, reads, re-signs) | Only on the opt-in Desktop path; Claude Code stays a plaintext loopback hop |
| Client runtime deps | Node + Python (mitmproxy) | Single signed Rust binary, zero runtime deps (DIRECTION §7b) — mitmproxy only if Desktop is enabled |
| Multi-account / rotation | Core feature (`TokenPool`, round-robin, cooldowns) | Out of scope — single user, own credentials |

## 2. How CC-Router captures each surface

### 2.1 Claude Code — same mechanism as parsec

CC-Router writes `ANTHROPIC_BASE_URL=http://localhost:3456` and a placeholder
`ANTHROPIC_AUTH_TOKEN=proxy-managed` into `~/.claude/settings.json`. Claude Code
then talks to the proxy voluntarily. This is exactly what `parsec setup` does
(`setup.rs`, merge into the `env` block, foreign base URL refused rather than
clobbered).

At the proxy, CC-Router's `proxyReq` hook swaps the placeholder for the selected
account's real OAuth token, strips `x-api-key`, and **appends** `oauth-2025-04-20`
to `anthropic-beta` (required for `sk-ant-oat01-*` tokens; appended so existing
tool/computer-use betas survive). parsec does the body rewrite instead and leaves
auth untouched — the only structural overlap is "listen on loopback, forward to
`api.anthropic.com`."

### 2.2 Claude Desktop — the part parsec does not have

Claude Desktop has an embedded Anthropic SDK hardwired to `api.anthropic.com`
and honors no endpoint override. CC-Router reaches it with **mitmproxy in "local
mode"** — process-scoped OS interception that captures only the `Claude` process,
not system-wide traffic (`src/interceptor/mitmproxy-manager.ts`):

- Spawns `mitmdump --mode local:Claude -s addon.py --set connection_strategy=lazy`.
  Process name is `Claude` (macOS) / `Claude.exe` (Windows) / `claude` (Linux).
- Per-platform OS mechanism (mitmproxy's own): **macOS = Network Extension (App
  Proxy Provider)**, needing one-time user approval in System Settings (parsed
  from `systemextensionsctl list`); **Windows = WinDivert (WFP driver)**;
  **Linux = eBPF (kernel ≥ 6.8)**.
- Because the traffic is TLS to `api.anthropic.com`, CC-Router **installs
  mitmproxy's CA into the OS trust store** (`sudo security add-trusted-cert` on
  macOS, `certutil -addstore root` on Windows, `update-ca-certificates` on
  Linux) so mitmproxy can terminate and re-issue TLS.
- `addon.py` (79 lines) is deliberately narrow: it rewrites **only** `/v1/messages*`
  and `/v1/models` (host/port/scheme → `localhost:3456`, plus the proxy secret as
  `x-api-key`). `/v1/oauth/*`, `/v1/environments/*` (Cowork bridge), and `/api/*`
  are passed through untouched — they carry the user's own session token for
  Desktop features CC-Router doesn't handle. Redirecting them would break
  login/Cowork.
- Lifecycle: detached spawn + PID file, optionally installed as a launchd agent /
  systemd user service / Windows Run-key for boot persistence (~620 lines of
  manager code).

parsec's `docs/routing-and-liveness.md` independently reached the same map:
§1.2 lists Desktop as "no model-endpoint configuration → out of reach," and §3.2
names MITM-with-custom-CA as "the only path that could plausibly reach Claude
Desktop … a deliberate product decision, not an implementation detail."

## 3. What porting Desktop support into parsec would touch

The routing-in-the-middle — the token/body work at the proxy — is the part parsec
**already has and CC-Router's Desktop path barely uses**. CC-Router's addon just
rewrites the destination; all the real work happens in its normal `/v1/messages`
server, which is the analog of parsec's `server.rs` (2256 lines, already doing the
harder version: splicing, deterministic freezing, count_tokens, passthrough
forward). A request redirected into parsec's loopback `/v1/messages` would work as
soon as it arrives — the engine doesn't care whether the bytes came from Claude
Code or a mitmproxy hop.

So the **server-side change is ≈ zero.** Everything new is the interception layer
parsec deliberately omits:

1. **A Python runtime dependency (mitmproxy).** The biggest cost, and it's a
   values collision, not code. DIRECTION §7b's client thesis is "Node is not
   guaranteed; ship one auditable static binary." Requiring `brew/pip install
   mitmproxy` reintroduces the runtime bet the Rust port exists to kill. The
   alternative — TLS-terminating, process-scoped interception written natively in
   Rust — is a multi-week project on its own; no crate bundles macOS Network
   Extension + WinDivert + eBPF the way mitmproxy does.
2. **A CA cert in the system trust store.** `routing-and-liveness.md` §3.2 already
   flags this as "a large escalation of the installer's footprint and a matching
   uninstall obligation." It is worse for parsec than for CC-Router: parsec's pitch
   is *one signed credential-handling binary that audits cleanly*, and a
   system-store CA is a real regression against that.
3. **Cross-platform interceptor lifecycle** — launchd/systemd/registry service
   management, Network Extension approval polling, PID files (~600 lines rewritten
   in Rust) — plus the genuinely painful part: testing against macOS Network
   Extension approval UX, Windows WinDivert UAC-every-launch, and Linux's kernel
   requirement.
4. **A larger trust posture.** parsec's passthrough model means it does *not* need
   CC-Router's token-injection dance — the Desktop request arrives carrying its own
   OAuth token, and parsec forwards it. But terminating TLS now puts parsec in the
   path of **subscription OAuth tokens**, not just plaintext loopback. It stays on
   the user's machine, so it does not break the CLAUDE.md data-plane rule (§3.2
   confirms), but it is a materially bigger ask than what parsec does today.

## 4. Effort estimate and recommendation

| Piece | Estimate |
|---|---|
| Server/routing changes (accept redirected `/v1/messages`) | ~1 day; mostly already handled |
| Interception layer, **accepting mitmproxy as a dependency** | ~1–2 weeks eng + ~1 week cross-platform testing |
| Interception layer, **Rust-native (no mitmproxy)** | weeks-to-months (reimplementing mitmproxy's OS layer) |
| Installer/uninstaller for CA trust + boot services | ~3–5 days + the product/security decision |

> **Superseded 2026-08-24 for the default-posture part only.** Desktop support
> was subsequently built — `packages/proxy/src/setup_desktop.rs`,
> `docs/claude-desktop-integration.md`. The recommendation below still governs
> how: it is opt-in, absent from `parsec setup`, and neither the mitmproxy
> dependency nor the CA is ever taken on without the user asking. §4.1–4.2 of
> the integration doc record how each cost is bounded. The effort table above
> proved roughly right; the server-side change was indeed ~zero.

**Recommendation (as written, 2026-08): do not port this as-is.** CC-Router built Desktop support to
extend *account-pooling* to Desktop — a strong driver for its product. parsec's
equivalent driver would be extending *curation/savings* to Desktop, which is
weaker: Desktop/Cowork is not the Claude-Code power-user ICP, its cache and
tool-schema shape differ from what the curator was trained and measured on, and
the savings ROI there is unproven. The port would take on a Python runtime
dependency, a system CA, and a cross-platform lifecycle burden — against parsec's
"one auditable binary" identity — for a segment that isn't the wedge.

If Desktop ever becomes a real ask, the cheaper first step (`routing-and-liveness.md`
§3.2) is the `NODE_EXTRA_CA_CERTS` path for Claude **Code** on locked-down setups:
same CA cost, no mitmproxy, and it stays inside the CLI parsec already serves. Full
Desktop capture is a deliberate, later product bet — not a quick port.

## 5. Incidental finding — the Windows console-window bug (fixed)

While comparing the two spawn paths, the Windows "a terminal window opens when the
proxy starts" bug became explained by the contrast:

- `parsec.exe` is a **console-subsystem** binary (no `#![windows_subsystem]`).
- parsec spawns supervisor → worker. The **supervisor** spawn
  (`setup::spawn_detached`) already sets `DETACHED_PROCESS |
  CREATE_NEW_PROCESS_GROUP`, so it comes up windowless. The **worker** spawn
  (`supervisor::spawn_worker`) set no creation flags — so Windows, seeing a
  console app launched by a parent with no console, allocated a fresh visible
  console window for the worker.
- CC-Router never hits this: it's a single-process Node daemon whose one spawn
  passes `windowsHide: true` (`launcher.ts:58`), Node's wrapper for the Win32
  `CREATE_NO_WINDOW` flag.

Fixed by adding `CREATE_NO_WINDOW` to the worker spawn in `supervisor.rs` (matching
what `spawn_detached` already does for the supervisor). `CREATE_NO_WINDOW` rather
than `DETACHED_PROCESS` because the worker stays a managed child the supervisor
reaps and kills.
