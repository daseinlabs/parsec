#!/usr/bin/env bash
# Build + deploy the frontend (dashboard) to Cloud Run. Thin wrapper over
# packages/frontend/cloudbuild.yaml. The frontend is server-rendered Next 16
# (SSR + route handlers + the platform.ts BFF), so it ships as a live node
# server, not a static export.
#
#   PLATFORM_URL=https://dasein-platform-xxx.run.app \
#   SUPABASE_URL=https://<proj>.supabase.co \
#   SUPABASE_PUBLISHABLE_KEY=sb_publishable_… \
#   scripts/deploy_frontend.sh
#
# Override anything via env: PROJECT, REGION, REPO, SERVICE, PLATFORM_URL,
# SUPABASE_URL, SUPABASE_PUBLISHABLE_KEY, STRIPE_CHECKOUT_URL, STRIPE_PORTAL_URL,
# CONCURRENCY, CPU, MEMORY, MIN_INSTANCES, MAX_INSTANCES, TIMEOUT.
#
# NEXT_PUBLIC_* are inlined at BUILD time — changing SUPABASE_URL etc. requires a
# rebuild, not just a redeploy. PLATFORM_URL is a runtime env var.
set -euo pipefail
cd "$(dirname "$0")/.."

PROJECT="${PROJECT:-$(gcloud config get-value project 2>/dev/null)}"
REGION="${REGION:-us-central1}"
REPO="${REPO:-dasein-images}"
SERVICE="${SERVICE:-dasein-frontend}"
PLATFORM_URL="${PLATFORM_URL:-}"
SUPABASE_URL="${SUPABASE_URL:-}"
SUPABASE_PUBLISHABLE_KEY="${SUPABASE_PUBLISHABLE_KEY:-}"
STRIPE_CHECKOUT_URL="${STRIPE_CHECKOUT_URL:-}"
STRIPE_PORTAL_URL="${STRIPE_PORTAL_URL:-}"
CONCURRENCY="${CONCURRENCY:-80}"
CPU="${CPU:-1}"
MEMORY="${MEMORY:-512Mi}"
MIN_INSTANCES="${MIN_INSTANCES:-1}"
MAX_INSTANCES="${MAX_INSTANCES:-10}"
TIMEOUT="${TIMEOUT:-60}"

if [ -z "$PROJECT" ]; then
  echo "no GCP project — run: gcloud config set project <id>  (or PROJECT=<id> $0)" >&2
  exit 1
fi
if [ -z "$PLATFORM_URL" ]; then
  echo "PLATFORM_URL is required (the deployed platform's URL the BFF calls)." >&2
  echo "  get it: gcloud run services describe dasein-platform --region $REGION --format='value(status.url)'" >&2
  exit 1
fi
if [ -z "$SUPABASE_URL" ] || [ -z "$SUPABASE_PUBLISHABLE_KEY" ]; then
  echo "warning: SUPABASE_URL / SUPABASE_PUBLISHABLE_KEY unset — the built image will" >&2
  echo "         render the 'setup needed' notice instead of a working login." >&2
fi

echo "── deploying $SERVICE to Cloud Run"
echo "   project=$PROJECT region=$REGION repo=$REPO"
echo "   platform-url=$PLATFORM_URL supabase=${SUPABASE_URL:-<none>}"

gcloud builds submit packages/frontend \
  --project "$PROJECT" \
  --config packages/frontend/cloudbuild.yaml \
  --substitutions="_REGION=$REGION,_REPO=$REPO,_SERVICE=$SERVICE,_PLATFORM_URL=$PLATFORM_URL,_SUPABASE_URL=$SUPABASE_URL,_SUPABASE_PUBLISHABLE_KEY=$SUPABASE_PUBLISHABLE_KEY,_STRIPE_CHECKOUT_URL=$STRIPE_CHECKOUT_URL,_STRIPE_PORTAL_URL=$STRIPE_PORTAL_URL,_CONCURRENCY=$CONCURRENCY,_CPU=$CPU,_MEMORY=$MEMORY,_MIN_INSTANCES=$MIN_INSTANCES,_MAX_INSTANCES=$MAX_INSTANCES,_TIMEOUT=$TIMEOUT"

URL="$(gcloud run services describe "$SERVICE" --project "$PROJECT" --region "$REGION" --format='value(status.url)' 2>/dev/null || true)"
cat <<EOF

── done. next: set the Supabase Auth redirect + Stripe checkout return URL to
   ${URL:-<frontend-url>}/auth/callback so login round-trips back here.
EOF
