# Per-platform `parsec` binaries

Plugin distribution is git-clone → local cache copy with **no build or
postinstall step**, so per-platform release binaries ship in this directory
of the published marketplace repo (placed by CI at release). In the source
repo the platform dirs are gitignored — `scripts/refresh_plugin_bin.sh`
populates a local copy for `--plugin-dir` testing.

```
bin/
  darwin-arm64/parsec
  darwin-x64/parsec
  linux-x64/parsec
  win-x64/parsec.exe
  parsec        ← two-line sh shim selecting by platform
  parsec.cmd    ← Windows shim
```

Built from `packages/proxy` (`cargo build --release --bin parsec`). The shims
are the only non-Rust client code in the product.

Free-tier v0 may ship with only the `mcp` + `hook` subcommands implemented;
`proxy` lands with the Pro tier (DIRECTION.md §7b sequencing guard).

The Pro-tier `proxy` subcommand is a **supervisor**: it owns the routed
loopback port (`ANTHROPIC_BASE_URL`) and spawns/restarts a hidden
`proxy-worker` that does the curation. If the worker crashes or wedges, the
supervisor forwards requests straight to Anthropic — so a session never
stalls on a dead proxy — then respawns the worker. `proxy-worker` is an
implementation detail of `proxy`, never launched by hand.
