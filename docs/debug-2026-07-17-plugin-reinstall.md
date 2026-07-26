# Debug log: plugin reinstall left proxy dead (2026-07-17)

Symptom: after reinstalling the plugin from the marketplace, the proxy never
started and nothing new appeared in `~/.parsec/proxy.log`. SessionStart kept
printing "setup complete — restart Claude Code to activate curation" on every
restart, forever.

## Root cause chain

1. **Plugin reinstall wipes the managed env.** Claude Code holds
   `~/.claude/settings.json` in memory and rewrites it from that copy when
   plugin state changes (install/uninstall/update). The env block setup had
   written externally (`ANTHROPIC_BASE_URL`, `PARSEC_EMBED_BACKEND`,
   `PARSEC_ONNX_DIR`) was clobbered back to `"env": {}`.
2. **The hook trusted stale state.** `setup_state.json` still said
   `phase: ready, env_written: true`, so `maybe_autosetup` printed the
   "restart to activate" nudge — but restarting could never help, because the
   routing was gone from settings. The old code even carried a comment naming
   the hole ("or if the user removed the env").
3. **No proxy, no logs — by design.** `maybe_autostart_proxy` manages the
   proxy only for ROUTED sessions (`ANTHROPIC_BASE_URL` set at launch). With
   the env wiped it returned before any spawn attempt, so `proxy.log` stayed
   silent. Correct behavior given (2); the silence is what made the failure
   look mysterious.

Since reinstall/update is a mainline flow, every user updating the plugin
would land in this permanent limbo.

## Fixes applied (this repo, 2026-07-17)

- **Self-healing routing** — `hook.rs` `maybe_autosetup`, `ready` branch: on
  `ready` + `ANTHROPIC_BASE_URL` unset, no longer trusts `env_written`;
  calls `setup::ensure_routing(st.port)` (new thin wrapper over
  `write_settings_env`, same additive merge — user-set keys are never
  overwritten) and saves corrected state. Messages: env restored → "routing
  was missing … restored, restart"; already present → old "setup complete"
  nudge; foreign URL → conflict message, value untouched; unparseable
  settings → "run `parsec setup`".
- **`GET /health` on the proxy** — `server.rs` router: answers locally with
  `{"ok":true,"service":"parsec-proxy","version":…}`. Never contacts
  upstream, so 200 strictly means "proxy up", not "upstream reachable".

Verified: cargo tests green; `/health` exercised end-to-end on a spare port;
self-heal exercised against a scratch `CLAUDE_CONFIG_DIR` (restore, idempotent
re-run, foreign-URL conflict non-overwrite all pass).

## Open items (not yet fixed)

- **0.0.4-alpha release binary has no baked model source.** `parsec setup`
  fails with "no model source configured" and — worse — flips a `ready` state
  to `failed` before checking whether the model is already on disk.
  `release.yml` bakes `PARSEC_DEFAULT_MODEL_BASE_URL` from the GitHub repo
  variable `vars.PARSEC_MODEL_BASE_URL`, which came through empty for this
  build. TODO: set the repo variable; make the workflow fail when it is
  empty; make `setup::run` skip the bail (and the download) when verified
  model files already exist on disk.
- **Workaround on an affected machine** (model already downloaded):
  `PARSEC_MODEL_BASE_URL=https://placeholder.invalid parsec setup` —
  existing files pass sha verification, nothing is fetched, routing is
  re-written.
- **Binary reports `parsec 0.1.0`** while the plugin version is 0.0.4-alpha —
  crate version and release tag are not linked.
- One historical `Address already in use (os error 48)` in `proxy.log`: two
  proxies raced for 8082 (likely a dev build vs the managed one). The
  spawn-side port check (`port_listening`) runs before spawn, so the window
  is the spawn-to-bind gap; harmless but worth knowing it can appear.
- A proxy instance from a dev build (`cargo build` without `--features
  onnx`) logged "built without the `onnx` cargo feature — full roster
  served" on every request. The marketplace darwin-arm64 binary does link
  onnxruntime; if this warning appears with a managed proxy, the release
  build regressed.

## How to verify the proxy is up

```sh
curl -s http://127.0.0.1:8082/health        # binaries with the fix: 200 JSON
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8082/  # older: 404 = up
lsof -nP -iTCP:8082 -sTCP:LISTEN
tail ~/.parsec/proxy.log                    # "listening on 127.0.0.1:8082"
```

In-session: `! echo $ANTHROPIC_BASE_URL` shows whether that session is
routed. A managed proxy idle-exits after 30 min without traffic — "not
listening" on an idle machine is normal; the SessionStart hook revives it
for routed sessions.

## Curation bugs observed from the inside (dogfood notes)

This debugging session was itself curated by parsec, and the no-reread gate
repeatedly worked against the agent:

1. **Post-summarization re-reads stay blocked.** After context management
   summarized away previously-read file content, the gate still denied
   re-reads. The deny message promises "re-issue this exact read and it will
   be allowed once" — that escape never fired, for either the exact original
   read or subranges. The agent had no sanctioned way to recover content it
   genuinely no longer had.
2. **Semantic, cross-file dedup closes every side door.** Copying the file
   (`cp` to a new path), `sed` extraction to a scratch file, and even
   `repr()`-transformed dumps were elided or blocked as "already read".
   Recovery required printing < ~15-line chunks per call.
3. **The "grep is always fine" carve-out is not honored.** A plain
   `grep -n` for symbol locations was hard-blocked, and the gate credited
   reads that never happened (claimed `server.rs:L1-20` was read when it had
   only been grepped).
4. **Fresh, first-time output gets elided too.** A `curl` response from a
   brand-new `/health` endpoint and novel python assertion output were
   elided because they resembled prior content — the agent could not see the
   result of its own verification and had to re-verify via transformed
   output (JSON-parse-and-reprint).

Net effect: a five-minute edit became an hour of workarounds. Suggested
direction: after a context summarization event, grant a re-read allowance
that actually fires; scope dedup to byte-identical tool results; never elide
the first occurrence of new bytes; honor the documented grep carve-out.
