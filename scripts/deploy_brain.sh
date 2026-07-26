#!/usr/bin/env bash
# Build + deploy the brain to Cloud Run. Thin wrapper over
# packages/brain/cloudbuild.yaml — the build fetches the checkpoint from GCS,
# bakes it into the image (digest pins the bundle), and deploys a stateless,
# round-robin-safe service. Cloud Run IS the routing layer (no nginx to run).
#
#   scripts/deploy_brain.sh                       # local backend: in-process bge on an L4 GPU
#   EMBED_BACKEND=dasein EMBED_URL=… deploy_brain.sh   # external embed pod (CPU service)
#   EMBED_BACKEND=hash scripts/deploy_brain.sh    # pipeline smoke test (TEST vectors, CPU)
#
# Override anything via env: PROJECT, REGION, REPO, SERVICE, CKPT_GCS,
# EMBED_BACKEND, EMBED_URL, EMBED_MODEL, TORCH_INDEX, MIN_INSTANCES,
# CONCURRENCY, CPU, MEMORY, TIMEOUT, BRAIN_KEY_SECRET.
#
# Prereqs (one-time):
#   gcloud services enable run.googleapis.com cloudbuild.googleapis.com artifactregistry.googleapis.com
#   gcloud artifacts repositories create "$REPO" --repository-format=docker --location="$REGION"
set -euo pipefail
cd "$(dirname "$0")/.."

PROJECT="${PROJECT:-$(gcloud config get-value project 2>/dev/null)}"
REGION="${REGION:-us-central1}"
REPO="${REPO:-dasein-images}"
SERVICE="${SERVICE:-dasein-brain}"
CKPT_GCS="${CKPT_GCS:-gs://dasein-473321-ac-learning/rulehead/curator_v4_prod.pt}"
EMBED_BACKEND="${EMBED_BACKEND:-local}"
EMBED_URL="${EMBED_URL:-}"
EMBED_MODEL="${EMBED_MODEL:-BAAI/bge-large-en-v1.5}"
# GPU=true attaches an L4 (needs Cloud Run L4 quota); false runs the in-process
# bge on CPU — slower but no quota, the safe first deploy.
GPU="${GPU:-false}"
MIN_INSTANCES="${MIN_INSTANCES:-1}"
# Must be <= the project's L4 quota on the GPU path: the quota check reads
# maxScale, so leaving cloudbuild's default (10) fails a 3-GPU project with
# "requested: 100/10". See docs/deploy-cloud-run.md "GPU deploy gotchas".
MAX_INSTANCES="${MAX_INSTANCES:-3}"
CONCURRENCY="${CONCURRENCY:-1}"
CPU="${CPU:-4}"
# bge-large (~1.3GB) resident on CPU wants headroom; GPU path pins 16Gi itself.
MEMORY="${MEMORY:-8Gi}"
TIMEOUT="${TIMEOUT:-300}"
BRAIN_KEY_SECRET="${BRAIN_KEY_SECRET:-}"
# Platform base URL for per-user psc_ key validation (/keys/validate). Set it to
# gate scoring on entitlement; empty keeps the static-key/open behavior.
PLATFORM_URL="${PLATFORM_URL:-}"

if [ -z "$PROJECT" ]; then
  echo "no GCP project — run: gcloud config set project <id>  (or PROJECT=<id> $0)" >&2
  exit 1
fi

# Real scores need a reachable embedder; the in-process GPU backend isn't built
# yet, so 'dasein' must point at the embed service. Warn loudly if it can't.
if [ "$EMBED_BACKEND" = "dasein" ] && [ -z "$EMBED_URL" ]; then
  cat >&2 <<EOF
EMBED_BACKEND=dasein but EMBED_URL is empty.

The deployed brain would fall back to the in-cluster default
(http://dasein-embed.default.svc.cluster.local/embed), which a Cloud Run
service cannot reach without a VPC connector to your GKE cluster. Either:

  * EMBED_URL=https://<reachable-embed-endpoint>/embed  scripts/deploy_brain.sh
  * EMBED_BACKEND=hash scripts/deploy_brain.sh          # TEST vectors, wiring only

(hash vectors validate the request path end to end but say NOTHING about score
quality — use a real embedder for anything score-related.)
EOF
  exit 1
fi

echo "── deploying $SERVICE to Cloud Run"
echo "   project=$PROJECT region=$REGION repo=$REPO"
echo "   ckpt=$CKPT_GCS embed=$EMBED_BACKEND${EMBED_URL:+ url=$EMBED_URL}"
if [ "$GPU" = "true" ]; then
  echo "   in-process bge ($EMBED_MODEL) on nvidia-l4 GPU (4 cpu / 16Gi)"
else
  echo "   embed=$EMBED_BACKEND on CPU — concurrency=$CONCURRENCY min-instances=$MIN_INSTANCES cpu=$CPU mem=$MEMORY"
fi

exec gcloud builds submit packages/brain \
  --project "$PROJECT" \
  --config packages/brain/cloudbuild.yaml \
  --substitutions="_REGION=$REGION,_REPO=$REPO,_SERVICE=$SERVICE,_CKPT_GCS=$CKPT_GCS,_EMBED_BACKEND=$EMBED_BACKEND,_EMBED_URL=$EMBED_URL,_EMBED_MODEL=$EMBED_MODEL,_GPU=$GPU,_CONCURRENCY=$CONCURRENCY,_MIN_INSTANCES=$MIN_INSTANCES,_MAX_INSTANCES=$MAX_INSTANCES,_CPU=$CPU,_MEMORY=$MEMORY,_TIMEOUT=$TIMEOUT,_BRAIN_KEY_SECRET=$BRAIN_KEY_SECRET,_PLATFORM_URL=$PLATFORM_URL"
