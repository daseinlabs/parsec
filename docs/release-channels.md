# Release channels

How parsec versions are cut, published, and rolled out. The implementation is
`release.yml`'s publish step; this is the policy it encodes and the reasoning
behind it.

## The flow

Nothing ships from pushes to `main` — they run the cross-platform build matrix
as CI and stop. A release is cut with:

```
make release VERSION=0.4.0    # stable — rolled out to everyone
make release VERSION=0.4.1    # patch  — opt-in only
git push origin main v0.4.1   # pushing the tag is what publishes
```

`make release` bumps the workspace version in the root `Cargo.toml` — the
single source of truth that `parsec --version`, `/health`, and hook.rs's
plugin-updated check all report — then commits and tags in one step so tag
and binary can never disagree. release.yml enforces this: a hand-cut tag
whose version differs from `Cargo.toml` (or from what the built binary
actually reports) fails the publish. Pre-release suffixes (`-alpha`, `-rc.1`)
are allowed and always classify as patch.

The tag pattern is the channel switch:

| Tag | Channel | Who gets it |
|---|---|---|
| `v0.X.0` | **stable** | Everyone: marketplace plugin users, `install.sh` / `install.ps1` users, and (once the self-updater lands) forced auto-update. |
| `v0.X.Y` (Y > 0) | **patch** | Opt-in. The plugins repo *tree* does not change, so marketplace plugin users never see it — but **running an install script is itself the opt-in gesture**: the scripts resolve `latest.json` and install the newest published version (patch included) from that tag's release assets. |

## Why patches are gated and minors are forced

⚠️ Deliberately inverted from the common convention (where patches auto-apply
because they are "safe fixes"). **We have no internal test setup yet, so the
patch channel *is* the test ring**: a change ships as `v0.4.1`, `v0.4.2`, …
to opt-in users, and when a patch has survived real use it is promoted by
tagging `v0.5.0`. A stable tag is a promotion decision, not just a build.

Consequences to keep in mind until this changes:

- A **critical/security fix must be tagged as a minor** (`v0.X+1.0`) to reach
  everyone — a patch tag reaches only opt-in users.
- Revisit this whole scheme when there is a real pre-release test rig or
  enough scale that opt-in users stop being representative. At that point the
  conventional direction (patches auto, minors reviewed) likely wins.

## What a publish produces

Every tag, both channels:

1. **A GitHub Release on `daseinlabs/plugins`** (the parsec source repo is
   private, so its releases can't serve downloads) carrying the per-platform
   binaries (`parsec-darwin-arm64`, `parsec-linux-x64`, `parsec-win-x64.exe`
   + CRT DLLs), the plugin zip, and `manifest.json` with per-asset sha256s.
   This is the **permanent versioned history** — immutable and per-tag.
2. **One normal commit on `plugins` `main`**, tagged `vX.Y.Z`. Never a
   force-push (history is append-only as of 2026-08-27; before that every
   publish force-pushed a single squashed commit).
3. An update to **`latest.json`** at the repo root — the pollable pointer the
   update check reads:

   ```json
   {
     "stable": {"version": "0.4.0", "tag": "v0.4.0", "assets": {"parsec-darwin-arm64": "<sha256>", "…": "…"}},
     "patch":  {"version": "0.4.2", "tag": "v0.4.2", "assets": {"…": "…"}}
   }
   ```

Stable releases additionally refresh the whole tree (plugin + binaries +
install scripts + `plugin.json` version stamp) and advance **both** channels
of `latest.json` — a new stable supersedes any patch on the previous minor.
Patch releases commit only `latest.json` and the install scripts (both
text-only — the scripts download binaries from the stable tree regardless,
and an installer fix should not wait for a minor to reach the public URL),
so the ~80 MB of per-publish binaries land in git history at minor cadence,
not patch cadence. That bound is what made dropping the force-push affordable; if clone
size ever becomes a problem again, the fix is squashing ancient history once,
not resuming force-per-publish.

## Consumer map

- **Claude Code plugin** (`parsec@parsec-marketplace`): follows `plugin.json`
  at the tree tip — stable channel by construction.
- **`install.sh` / `install.ps1`**: resolve `latest.json` and install the
  **newest published version** (patch channel included) from that tag's
  GitHub release assets, sha256-verified against the same file. Someone
  explicitly running the installer is asking for the newest build — that IS
  the patch opt-in. Any resolution failure falls back to the stable tree
  (and a custom `PARSEC_INSTALL_BASE` skips resolution, so test installs
  keep pointing at a tree).
- **Anti-downgrade guards** (what keeps a latest-install from being silently
  reverted by the stable-channel plugin): `refresh_bin_alias` (runs from
  every SessionStart hook via `ensure_callable`) leaves a real file at the
  alias path alone when it reports a NEWER version than the running build,
  and `hook.rs`'s `maybe_upgrade_proxy` replaces the proxy only on a semver
  *upgrade* or an incompatible wire set — never on a plain downgrade.
- **Self-updater / OTA (planned)**: polls `latest.json`; a newer *stable
  minor* auto-applies, a newer *patch* only notifies unless the user opted
  in. The downgrade half of the old channel-awareness concern is done (see
  the guards above); the updater still needs the apply/notify split.
