# Changelog

All notable changes to parsec are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/).

Releases are tagged `vX.Y.Z`. Prebuilt binaries and installers for each tag
are published on the releases page.

## [Unreleased]

### Fixed

- tray (macOS): `parsec tray install` produced a bundle Gatekeeper rejected
  as "damaged". Since release binaries became Developer ID signed, the copied
  binary's hardened-runtime signature sat inside an `.app` with no resource
  seal, `codesign --verify` failed with "code has no resources but signature
  indicates they must be present", and the login item was SIGKILLed on first
  launch — which also left mitmproxy's Network Extension approval with no
  live owner. The bundle is now assembled in a staging directory, sealed with
  an ad-hoc `codesign --deep`, verified, and swapped into place.
- desktop: `parsec setup desktop --autostart` (macOS / Linux) started a
  second `mitmdump` next to the one the boot service had just launched. The
  service's process never wrote the pidfile, so `running()` said "no" and a
  hand-spawn followed; the two fought for one local-mode hook and the loser
  died with "deadline has elapsed" while `status` still said running. The
  service's process is now adopted into the pidfile instead, and `status`
  warns when more than one interceptor is running the addon.
- desktop: `parsec setup desktop` printed "NOT trusted yet" and the
  `add-trusted-cert` command unconditionally, contradicting `parsec desktop
  status` on the same machine. It now consults the trust store and reports
  trusted / stale / untrusted like `status` does.
- desktop (macOS): the "installed but not approved" message now explains
  what to do when System Settings shows no "Mitmproxy Redirector" row (run
  mitmdump by hand and keep it alive while approving; if the row still does
  not appear, trash the redirector app from Finder — `systemextensionsctl
  uninstall` is blocked by SIP).
- desktop (macOS): installing the boot services no longer prints launchctl's
  "Boot-out failed: 3: No such process" / "Unload failed: 5: Input/output
  error" noise on a first install.
- desktop (macOS / Linux): `parsec desktop stop --keep-autostart` and
  `parsec desktop restart` now stop the boot service for the session before
  signalling the process. Under launchd's `KeepAlive` a plain SIGTERM was
  answered with a respawn seconds later, so "stopped" was printed over an
  interceptor that was already coming back.
- tray (macOS): the login item now carries a PATH with the Homebrew and
  `/usr/local/bin` prefixes, and `mitmdump` lookup falls back to those
  locations when PATH has nothing. launchd starts the tray with the bare
  system PATH, so its guided Desktop completion reported "mitmproxy is not
  installed" on machines where it was.
- desktop docs: the addon header and the desktop skill now say plainly that
  the Code tab inside Claude Desktop is not curated (Desktop overrides
  `ANTHROPIC_BASE_URL`, and its Node runtime rejects mitmproxy's CA).

- brain: large score/neighbors requests were rejected with `413 Request Entity
  Too Large` at the Cloud Run edge (32 MiB HTTP/1 cap enforced by the Google
  Frontend, before the container, so the service logs never showed them). The
  service now deploys with `--use-http2` and the image runs hypercorn (h2c) in
  place of uvicorn, which lifts the cap.

### Added

- desktop: a redirector health line, read from mitmdump's own log, on
  `parsec desktop start` / `setup desktop` and in `parsec desktop status`.
  A live pid was the only signal before, and it stayed "running" while the
  local-mode hook had never attached. `start` now fails with one clear
  error when the redirector reports a startup failure; `status` also warns
  when more than one interceptor is running the addon.
- installer (macOS): the pkg ships a pre-signed `parsec.app` for the menu-bar
  app at `/usr/local/parsec/parsec.app`, assembled by the new hidden
  `parsec tray bundle` subcommand and Developer ID signed with the rest of
  the payload. `parsec tray install` copies it verbatim when its version
  matches the current binary and falls back to assembling and ad-hoc
  sealing one otherwise (plugin and curl installs).
- `parsec setup pi` / `parsec disable pi`: route the pi coding agent through
  the local proxy. Setup re-points pi's built-in `anthropic` provider in
  `<pi dir>/models.json` (`baseUrl` plus an `x-parsec-tool: pi` attribution
  header; existing OAuth or API-key auth keeps working) and drops a managed,
  dependency-free extension at `<pi dir>/extensions/parsec.ts` that revives
  the proxy at session start and bridges pi's `tool_call` / `tool_result`
  events to the same `parsec hook` gates the Claude Code plugin runs. Both
  artifacts are ownership-gated: a foreign `baseUrl` or a non-parsec
  extension file is refused, never overwritten, and `disable` removes only
  what setup added. `parsec uninstall` cleans them up too.
- pi is auto-detected at install time like the other clients: the
  one-line installers (`install.sh`, `install.ps1`) probe for the `pi`
  binary or its agent dir (`PI_CODING_AGENT_DIR`, default `~/.pi/agent`)
  and run `parsec setup pi`, and accept `pi` as an explicit tool; the
  macOS `.pkg` and Windows `setup.exe` get a pi choice, pre-ticked when pi
  is found, and undo it on uninstall. The install ping lists `pi` among
  the configured harnesses when the managed extension is present.

### Fixed

- The curator no longer cuts the result of the call the model is about to
  act on. Birth decisions for the current turn's observations are deferred
  until the next assistant turn lands (`FreezeConfig::protect_current`, on
  by default; the parity harness keeps the reference policy). A fresh read
  cut on first serving sent the model into a re-read spiral: it re-asked
  with different, narrower calls, each a new birth, each cut again.
- Fold markers say who elided the chunk and how to get it back
  (`omitted by parsec · repeat the identical call to restore`) and only
  name a line range when the coordinates are real file lines. The old
  `· re-read FILE:Lx-y` suffix was rendered for content the model had never
  seen and, for `awk`/`tail`-style reads, with output-relative line numbers.
- The curation contract (what a marker means, the identical-call override)
  is injected as SessionStart context in every entitled session, not only
  under the `parsec:code` agent.
- The no-reread loop-breaker no longer counts or denies the single
  identical repeat the override protocol asks for: the proxy exports the
  freezer's override registry alongside the range visibility, and the hook
  exempts those commands.
- `PARSEC_PROTECT_CURRENT=off` restores the reference cut-at-birth policy.
  Under the default, each tool result crosses the wire in full once before
  its curated form replaces it, so the golden replay's simulated cache
  ratio moves from 10:1 to 8:1 and measured savings on the current turn
  are forgone by design; the wire-level fold map leaves the current turn
  unfrozen so the curated form can land on the next request.

### Changed

- Releases are published as GitHub Release assets on this repository:
  per-platform binaries, the win-x64 CRT DLLs, `parsec-plugin.zip`, the
  native installers, and a `manifest.json` of sha256s. The separate
  `daseinlabs/plugins` distribution repo, its `latest.json` pointer, and the
  `PLUGIN_PUBLISH_TOKEN` secret are retired; forks release with the default
  token. Pre-release tags publish as GitHub pre-releases, which keeps them
  out of `releases/latest`.
- The Claude Code marketplace is this repository
  (`.claude-plugin/marketplace.json` → `packages/plugin`):
  `claude plugin marketplace add https://github.com/daseinlabs/parsec`.
  Existing installs from `daseinlabs/plugins` keep working but no longer
  update; remove that marketplace and add this one.
- The plugin no longer ships binaries in its tree. `bin/parsec` (and
  `parsec.cmd` + `bootstrap.ps1` on Windows) runs a bundled build if one is
  beside it, else `~/.parsec/bin/parsec`, else downloads the plugin's own
  version from GitHub Releases into `~/.parsec/bin` (sha256-verified). At
  SessionStart an installed binary older than the plugin is upgraded the
  same way, so a marketplace plugin update brings its binary along.
- Install scripts move to `scripts/` on `main`
  (`raw.githubusercontent.com/daseinlabs/parsec/main/scripts/install.sh`)
  and resolve the newest release through `releases/latest/download/
  manifest.json`. `PARSEC_INSTALL_BASE` (a tree to download binaries from)
  is replaced by `PARSEC_RELEASE_BASE` (a mirror of the GitHub Releases URL
  layout); the shims honour the same variable.
- `make release` also stamps `packages/plugin/.claude-plugin/plugin.json`,
  and the release job refuses a tag that disagrees with it.

## [0.2.18] - 2026-09-15

First release cut from the public repository.

### Added
- The proxy, engine, mapgen, contracts, plugins, installers, and the scoring
  service (`packages/brain`) now develop in the open under MIT.
- `PARSEC_INSTALL_REPORT=0` (or `DO_NOT_TRACK=1`) disables the anonymous
  install ping. The README's "What parsec sends" section lists every byte
  that leaves the machine and its off switch.
- The scoring service loads its checkpoint from a local path, a Hugging Face
  id (`hf://`), or a GCS mirror, and defaults to the in-process embedder.

### Changed
- Plugin and marketplace descriptions no longer reference internal product
  names.
- `packages/brain`, the scoring service, is now in the open repository and
  self-hostable: `docker compose up -d` plus `PARSEC_BRAIN_URL`.
  `PARSEC_CKPT=hf://<org>/<repo>/<file>` pulls a checkpoint from the Hugging
  Face Hub. The remote-embedder backend is now named `remote` (was an
  internal service name) and the in-process `local` encoder is the default.

## [0.2.17] - 2026-09-13

Last release cut from the private repository. See the release notes on the
releases page for the history before this point.
