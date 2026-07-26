#!/usr/bin/env bash
# Bring up the full local loop for manual testing: brain in a container,
# proxy as a HOST process. `make dev` calls this.
#
# The split is deliberate and not negotiable: your Claude auth headers pass
# through the proxy to api.anthropic.com, so the proxy must never run in a
# container we could accidentally ship or log from (DIRECTION.md §2).
#
#   make dev                       # demo: hash embeds + forced tau (visible trims)
#   EMBED=dasein PARSEC_SERVE_TAU= make dev
#                                  # real scores; needs the cluster embedder:
#                                  #   kubectl port-forward svc/dasein-embed 18080:80
set -euo pipefail
cd "$(dirname "$0")/.."

CKPT="${PARSEC_CKPT:-$HOME/.parsec/brain/curator_v4_prod.pt}"
BRAIN_URL="${PARSEC_BRAIN_URL:-http://127.0.0.1:8090}"

if [ ! -f "$CKPT" ]; then
  cat >&2 <<EOF
checkpoint missing: $CKPT

  mkdir -p ~/.parsec/brain
  gcloud storage cp gs://dasein-473321-ac-learning/rulehead/curator_v4_prod.pt ~/.parsec/brain/

The brain self-validates the bundle at startup, so without it the container
never goes healthy.
EOF
  exit 1
fi

docker compose up -d --build brain

# The bundle load (36MB R-GCN + tree-sitter grammars) takes a few seconds; the
# compose healthcheck has a 90s start period, so poll rather than assume.
printf '── waiting for brain'
for _ in $(seq 1 60); do
  if curl -sf "$BRAIN_URL/health" >/dev/null 2>&1; then
    echo " ok"
    break
  fi
  printf '.'
  sleep 2
done
if ! curl -sf "$BRAIN_URL/health" >/dev/null 2>&1; then
  echo
  echo "brain did not become healthy — check: docker compose logs brain" >&2
  exit 1
fi

curl -s "$BRAIN_URL/v1/bundle" | python3 -c '
import json, sys
b = json.load(sys.stdin)
print(f"── brain: ckpt {b[\"checkpoint_id\"][:12]}… tau_q {b[\"tau_q\"]} "
      f"heads {b.get(\"heads\")} contracts {b.get(\"contracts\")}")
'

exec scripts/proxy_dev.sh "${1:-demo}"
