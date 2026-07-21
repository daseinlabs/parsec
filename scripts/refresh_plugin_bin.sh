#!/usr/bin/env bash
# Refresh the local (gitignored) plugin binary the SAFE way. On Apple Silicon,
# `cp` over the existing file reuses the inode and the kernel's code-sign
# cache SIGKILLs the new binary (exit 137, "hook error, no stderr") — so:
# build → copy to temp → ad-hoc sign → atomic rename (fresh inode).
set -euo pipefail
cd "$(dirname "$0")/.."
# Optional: bake a default brain URL into the binary (brain.rs BAKED_BRAIN_URL,
# via option_env! — cargo rebuilds when it changes). `make plugin BRAIN_URL=…`
# sets it; empty = dev build with no baked default (reads DASEIN_BRAIN_URL at
# runtime). Same knob release.yml bakes for shipped plugins.
if [ -n "${DASEIN_DEFAULT_BRAIN_URL:-}" ]; then
  echo "baking DASEIN_DEFAULT_BRAIN_URL=$DASEIN_DEFAULT_BRAIN_URL"
  export DASEIN_DEFAULT_BRAIN_URL
else
  unset DASEIN_DEFAULT_BRAIN_URL || true
fi
# No cargo features remain: the ONNX embedder moved server-side on
# 2026-07-20 (docs/server-side-embedding.md), taking ort/tokenizers with it.
cargo build --release --bin dasein
DEST=packages/plugin/bin/darwin-arm64/dasein
cp target/release/dasein "$DEST.tmp"
codesign --force --sign - "$DEST.tmp" 2>/dev/null || true   # no-op on linux
mv -f "$DEST.tmp" "$DEST"
echo '{"hook_event_name":"SessionStart"}' | packages/plugin/bin/dasein hook SessionStart >/dev/null \
  && echo "refreshed + hook smoke OK: $DEST"
