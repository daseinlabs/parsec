#!/usr/bin/env bash
# Refresh the committed plugin binary the SAFE way. On Apple Silicon,
# `cp` over the existing file reuses the inode and the kernel's code-sign
# cache SIGKILLs the new binary (exit 137, "hook error, no stderr") — so:
# build → copy to temp → ad-hoc sign → atomic rename (fresh inode).
set -euo pipefail
cd "$(dirname "$0")/.."
# DASEIN_BUILD_FEATURES=onnx builds the local-embedder binary (adds ort +
# tokenizers; the default stays light — docs/brain-serving-v0.md appendix).
cargo build --release --bin dasein ${DASEIN_BUILD_FEATURES:+--features "$DASEIN_BUILD_FEATURES"}
DEST=packages/plugin/bin/darwin-arm64/dasein
cp target/release/dasein "$DEST.tmp"
codesign --force --sign - "$DEST.tmp" 2>/dev/null || true   # no-op on linux
mv -f "$DEST.tmp" "$DEST"
echo '{"hook_event_name":"SessionStart"}' | packages/plugin/bin/dasein hook SessionStart >/dev/null \
  && echo "refreshed + hook smoke OK: $DEST"
