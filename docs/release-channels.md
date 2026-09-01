# Releases

How parsec versions are cut, published, and rolled out. The implementation is
`release.yml`'s publish step; this is the policy it encodes.

⚠️ **Single channel since 2026-09-02.** Until then releases were split into a
stable channel (v0.X.0, forced to everyone) and an opt-in patch channel
(v0.X.Y, the de-facto test ring) — deliberately inverted from convention
because there was no internal test rig. In practice no stable was ever
promoted after v0.2.0, so marketplace users sat seven releases behind the
installers; the split was dropped at Surya's direction so **every install
route always gets the newest release**.

## The flow

Nothing ships from pushes to `main` — they run the cross-platform build matrix
as CI and stop. A release is cut with:

```
make release VERSION=0.2.8
git push origin main v0.2.8   # pushing the tag is what publishes
```

`make release` bumps the workspace version in the root `Cargo.toml` — the
single source of truth that `parsec --version`, `/health`, and hook.rs's
plugin-updated check all report — then commits and tags in one step so tag
and binary can never disagree. release.yml enforces this: a hand-cut tag
whose version differs from `Cargo.toml` (or from what the built binary
actually reports) fails the publish.

## What a publish produces (every tag)

1. **A GitHub Release on `daseinlabs/plugins`** (the parsec source repo is
   private, so its releases can't serve downloads) carrying the per-platform
   binaries (`parsec-darwin-arm64`, `parsec-linux-x64`, `parsec-win-x64.exe`
   + CRT DLLs), the plugin zip, and `manifest.json` with per-asset sha256s.
   This is the permanent versioned history — immutable and per-tag.
2. **One normal commit on `plugins` `main`**, tagged `vX.Y.Z`, refreshing the
   whole tree: marketplace files, plugin (with `plugin.json` version stamp),
   binaries, and install scripts. Never a force-push — history is
   append-only (as of 2026-08-27). Binaries now land in history on every
   release (~80 MB each); if clone size becomes a problem, squash ancient
   history once — do not resume force-per-publish.
3. An update to **`latest.json`** at the repo root — the pollable pointer the
   install scripts and the future self-updater read. Both keys advance
   together; the `{stable, patch}` two-key shape survives only because
   install scripts already in the wild parse the `patch` segment:

   ```json
   {
     "stable": {"version": "0.2.8", "tag": "v0.2.8", "assets": {"parsec-darwin-arm64": "<sha256>", "…": "…"}},
     "patch":  {"version": "0.2.8", "tag": "v0.2.8", "assets": {"…": "…"}}
   }
   ```

## Consumer map

- **Claude Code plugin** (`parsec@parsec-marketplace`): follows `plugin.json`
  at the tree tip — now the newest release.
- **`install.sh` / `install.ps1`**: resolve `latest.json` and install that
  version from the tag's GitHub release assets, sha256-verified against the
  same file. Any resolution failure falls back to the tree (which now lags
  only within a single publish window), and a custom `PARSEC_INSTALL_BASE`
  skips resolution so test installs keep pointing at a tree.
- **Anti-downgrade guards**: `refresh_bin_alias` (every SessionStart hook via
  `ensure_callable`) leaves a real file at the alias path alone when it
  reports a NEWER version than the running build, and `hook.rs`'s
  `maybe_upgrade_proxy` replaces the proxy only on a semver *upgrade* or an
  incompatible wire set. With one channel these guards matter less, but they
  keep mixed-version windows (mid-rollout, custom installs) safe.
- **Self-updater / OTA (planned)**: polls `latest.json` and applies the
  newest release.

## What we gave up, knowingly

There is still no pre-release test ring: every tag now reaches every user on
their next install or plugin update. The old inverted scheme bought staged
exposure at the cost of marketplace users never actually receiving fixes.
Pre-release tags (any `-suffix`, e.g. `v0.3.0-rc.1`) are the escape hatch:
they publish release assets only — no tree refresh, `latest.json` untouched —
so nothing reaches users who didn't type the tag name. When a real test rig
exists, staged rollout should build on those or a percentage rollout — not on
resurrecting the two-channel split.
