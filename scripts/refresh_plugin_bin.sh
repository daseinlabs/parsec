#!/usr/bin/env bash
# Refresh the local (gitignored) plugin binary the SAFE way. On Apple Silicon,
# `cp` over the existing file reuses the inode and the kernel's code-sign
# cache SIGKILLs the new binary (exit 137, "hook error, no stderr") — so:
# build → copy to temp → ad-hoc sign → atomic rename (fresh inode).
set -euo pipefail
cd "$(dirname "$0")/.."
# Optional: bake default endpoints into the binary via option_env! (cargo
# rebuilds when they change) — brain.rs BAKED_BRAIN_URL and ledger_ship.rs
# BAKED_PLATFORM_URL. `make plugin BRAIN_URL=… PLATFORM_URL=…` sets them; empty
# = dev build with no baked default (reads PARSEC_BRAIN_URL / PARSEC_PLATFORM_URL
# at runtime). Same two knobs release.yml bakes for shipped plugins.
for v in PARSEC_DEFAULT_BRAIN_URL PARSEC_DEFAULT_PLATFORM_URL; do
  eval "val=\${$v:-}"
  if [ -n "$val" ]; then
    echo "baking $v=$val"
    export "$v"
  else
    unset "$v" || true
  fi
done
# No cargo features remain: the ONNX embedder moved server-side on
# 2026-07-20 (docs/server-side-embedding.md), taking ort/tokenizers with it.
cargo build --release --bin parsec
DEST=packages/plugin/bin/darwin-arm64/parsec
cp target/release/parsec "$DEST.tmp"
codesign --force --sign - "$DEST.tmp" 2>/dev/null || true   # no-op on linux
mv -f "$DEST.tmp" "$DEST"
echo '{"hook_event_name":"SessionStart"}' | packages/plugin/bin/parsec hook SessionStart >/dev/null \
  && echo "refreshed + hook smoke OK: $DEST"
