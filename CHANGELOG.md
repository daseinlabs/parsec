# Changelog

All notable changes to parsec are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/).

Releases are tagged `vX.Y.Z`. Prebuilt binaries and installers for each tag
are published on the releases page.

## [Unreleased]

### Added

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
