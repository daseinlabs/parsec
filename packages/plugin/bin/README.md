# Committed `dasein` binaries

Plugin distribution is git-clone → local cache copy with **no build or
postinstall step**, so per-platform release binaries are committed here:

```
bin/
  darwin-arm64/dasein
  darwin-x64/dasein
  linux-x64/dasein
  win-x64/dasein.exe
  dasein        ← two-line sh shim selecting by platform
  dasein.cmd    ← Windows shim
```

Built from `packages/proxy` (`cargo build --release --bin dasein`). The shims
are the only non-Rust client code in the product.

Free-tier v0 may ship with only the `mcp` + `hook` subcommands implemented;
`proxy` lands with the Pro tier (DIRECTION.md §7b sequencing guard).
