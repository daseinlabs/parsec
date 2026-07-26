#!/usr/bin/env bash
# Launch the LOCAL data-plane proxy for manual testing (docs/manual-testing.md).
# The proxy is a host process on purpose: your Claude auth headers pass
# through it to api.anthropic.com and must never enter a container.
#
#   scripts/proxy_dev.sh          # v2 contract (default) — the brain embeds
#   scripts/proxy_dev.sh dev-raw  # legacy dev contract (raw messages, server re-chunks)
#
# Env overrides: PARSEC_BRAIN_URL (default http://127.0.0.1:8090 = compose),
# PARSEC_PROXY_PORT (default 8082), PARSEC_UPSTREAM, PARSEC_RECORD_DIR.
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-demo}"
export PARSEC_BRAIN_URL="${PARSEC_BRAIN_URL:-http://127.0.0.1:8090}"
export PARSEC_PROXY_PORT="${PARSEC_PROXY_PORT:-8082}"

case "$MODE" in
  demo)
    export PARSEC_BRAIN_CONTRACT=v2 ;;
  dev-raw)
    export PARSEC_BRAIN_DEV_RAW=1 ;;
  *) echo "usage: $0 [demo|dev-raw]"; exit 1 ;;
esac

curl -sf "$PARSEC_BRAIN_URL/health" >/dev/null \
  || { echo "brain not reachable at $PARSEC_BRAIN_URL — docker compose up -d brain"; exit 1; }
echo "── brain: $(curl -s "$PARSEC_BRAIN_URL/v1/bundle" | python3 -c 'import json,sys; b=json.load(sys.stdin); print(f"ckpt {b[\"checkpoint_id\"][:12]}… tau_q {b[\"tau_q\"]} heads {b[\"heads\"]}")')"

[ -x target/release/parsec ] || cargo build --release --bin parsec
echo "── proxy: 127.0.0.1:$PARSEC_PROXY_PORT → ${PARSEC_UPSTREAM:-https://api.anthropic.com} (mode: $MODE, contract: ${PARSEC_BRAIN_CONTRACT:-dev})"
echo "   point Claude Code at it:  ANTHROPIC_BASE_URL=http://127.0.0.1:$PARSEC_PROXY_PORT claude"
exec ./target/release/parsec proxy
