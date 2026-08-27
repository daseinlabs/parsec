# Release channels

How parsec versions are cut, published, and rolled out. The implementation is
`release.yml`'s publish step; this is the policy it encodes and the reasoning
behind it.

## The flow

Nothing ships from pushes to `main` — they run the cross-platform build matrix
as CI and stop. A release is created by pushing a tag:

```
git tag v0.4.0 && git push origin v0.4.0     # stable — rolled out to everyone
git tag v0.4.1 && git push origin v0.4.1     # patch  — opt-in only
```

The tag pattern is the channel switch:

| Tag | Channel | Who gets it |
|---|---|---|
| `v0.X.0` | **stable** | Everyone: marketplace plugin users, `install.sh` / `install.ps1` users, and (once the self-updater lands) forced auto-update. |
| `v0.X.Y` (Y > 0) | **patch** | Opt-in only. Reachable through `latest.json`'s `patch` entry and the release assets — the plugins repo *tree* does not change, so marketplace and installer users never see it. |

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
Patch releases commit only the `latest.json` change: text-only, so the ~80 MB
of per-publish binaries land in git history at minor cadence, not patch
cadence. That bound is what made dropping the force-push affordable; if clone
size ever becomes a problem again, the fix is squashing ancient history once,
not resuming force-per-publish.

## Consumer map

- **Claude Code plugin** (`parsec@parsec-marketplace`): follows `plugin.json`
  at the tree tip — stable channel by construction.
- **`install.sh` / `install.ps1`**: download from raw `main` tree — stable.
- **Self-updater / OTA (planned)**: polls `latest.json`; a newer *stable
  minor* auto-applies, a newer *patch* only notifies unless the user opted in.
  Note for that work: the SessionStart hook restarts the proxy on *any*
  version mismatch, downgrade included (`hook.rs`), which would fight an
  opted-in patch user — the check must become channel-aware (restart on minor
  mismatch, leave a same-minor-newer-patch proxy alone).
