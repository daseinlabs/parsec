#!/usr/bin/env bash
# Build + deploy the platform (control plane) to Cloud Run. Thin wrapper over
# packages/platform/cloudbuild.yaml. UNLIKE the brain, this bakes nothing — the
# platform is stateless glue over Supabase (Postgres + Auth) and Stripe, so all
# state is external and wired in as config/secrets.
#
#   scripts/deploy_platform.sh                        # deploy with current secrets
#   MIGRATE=1 MIGRATE_DB_URL=… scripts/deploy_platform.sh   # apply migrations first
#
# Override anything via env: PROJECT, REGION, REPO, SERVICE, DB_URL_SECRET,
# SUPABASE_JWKS_URL, JWT_SECRET_SECRET, STRIPE_WEBHOOK_SECRET, CONCURRENCY, CPU,
# MEMORY, MIN_INSTANCES, MAX_INSTANCES, TIMEOUT.
#
# Prereqs (one-time):
#   gcloud services enable run.googleapis.com cloudbuild.googleapis.com artifactregistry.googleapis.com
#   gcloud artifacts repositories create "$REPO" --repository-format=docker --location="$REGION"
#   # Store the credentials as Secret Manager secrets, then grant the Cloud Run
#   # runtime service account roles/secretmanager.secretAccessor on each:
#   printf %s "$SUPAVISOR_POOLER_URL" | gcloud secrets create dasein-platform-db-url --data-file=-
#   printf %s "$STRIPE_WHSEC"         | gcloud secrets create dasein-stripe-webhook  --data-file=-
#
# The build context is the repo root (the platform Dockerfile COPYs
# packages/platform/... from root). target/ and node_modules/ are gitignored, so
# gcloud's default .gcloudignore keeps them out of the uploaded context.
set -euo pipefail
cd "$(dirname "$0")/.."

PROJECT="${PROJECT:-$(gcloud config get-value project 2>/dev/null)}"
REGION="${REGION:-us-central1}"
REPO="${REPO:-dasein-images}"
SERVICE="${SERVICE:-dasein-platform}"
# Secret Manager refs (name:version). DB URL is required.
DB_URL_SECRET="${DB_URL_SECRET:-dasein-platform-db-url:latest}"
# Supabase JWT verification — set ONE (JWKS wins if both). JWKS URL is public;
# the legacy HS256 secret is a Secret Manager ref.
SUPABASE_JWKS_URL="${SUPABASE_JWKS_URL:-}"
JWT_SECRET_SECRET="${JWT_SECRET_SECRET:-}"
STRIPE_WEBHOOK_SECRET="${STRIPE_WEBHOOK_SECRET:-}"
CONCURRENCY="${CONCURRENCY:-60}"
CPU="${CPU:-1}"
MEMORY="${MEMORY:-512Mi}"
MIN_INSTANCES="${MIN_INSTANCES:-1}"
MAX_INSTANCES="${MAX_INSTANCES:-10}"
TIMEOUT="${TIMEOUT:-60}"
# 'all' = public (required: the client POSTs /ledger from user machines).
# 'internal-and-cloud-load-balancing' only if an external HTTPS LB fronts it.
INGRESS="${INGRESS:-all}"
# Pre-billing: '1' entitles every account at key-mint (usage gathering, no
# Stripe). Set '0' when billing goes live.
AUTO_ENTITLE="${AUTO_ENTITLE:-1}"

if [ -z "$PROJECT" ]; then
  echo "no GCP project — run: gcloud config set project <id>  (or PROJECT=<id> $0)" >&2
  exit 1
fi

if [ -z "$SUPABASE_JWKS_URL" ] && [ -z "$JWT_SECRET_SECRET" ]; then
  echo "warning: neither SUPABASE_JWKS_URL nor JWT_SECRET_SECRET set —" >&2
  echo "         JWT-gated endpoints (/keys, /ledger/summary, …) will reject every" >&2
  echo "         request. Set one before the dashboard can work." >&2
fi

# Migrations run OUT-OF-BAND, before the code deploy (pgstore.py does not
# self-migrate). Expand/contract: apply additive migrations that the currently
# running code tolerates, deploy, and only drop in a later release.
if [ "${MIGRATE:-0}" = "1" ]; then
  : "${MIGRATE_DB_URL:?MIGRATE=1 needs MIGRATE_DB_URL (a DIRECT session-mode URL, not the pooler)}"
  echo "── applying migrations (direct connection)"
  for f in packages/platform/migrations/*.sql; do
    echo "   $f"
    psql "$MIGRATE_DB_URL" -v ON_ERROR_STOP=1 -1 -f "$f"
  done
else
  echo "note: migrations NOT applied (set MIGRATE=1 MIGRATE_DB_URL=… to run them,"
  echo "      or: supabase db push). Deploy assumes the schema is already current."
fi

echo "── deploying $SERVICE to Cloud Run"
echo "   project=$PROJECT region=$REGION repo=$REPO"
echo "   concurrency=$CONCURRENCY cpu=$CPU mem=$MEMORY min-instances=$MIN_INSTANCES"
echo "   db-url-secret=$DB_URL_SECRET jwks=${SUPABASE_JWKS_URL:-<none>} stripe=${STRIPE_WEBHOOK_SECRET:-<none>}"

gcloud builds submit . \
  --project "$PROJECT" \
  --config packages/platform/cloudbuild.yaml \
  --substitutions="_REGION=$REGION,_REPO=$REPO,_SERVICE=$SERVICE,_DB_URL_SECRET=$DB_URL_SECRET,_SUPABASE_JWKS_URL=$SUPABASE_JWKS_URL,_JWT_SECRET_SECRET=$JWT_SECRET_SECRET,_STRIPE_WEBHOOK_SECRET=$STRIPE_WEBHOOK_SECRET,_CONCURRENCY=$CONCURRENCY,_CPU=$CPU,_MEMORY=$MEMORY,_MIN_INSTANCES=$MIN_INSTANCES,_MAX_INSTANCES=$MAX_INSTANCES,_TIMEOUT=$TIMEOUT,_INGRESS=$INGRESS,_AUTO_ENTITLE=$AUTO_ENTITLE"

URL="$(gcloud run services describe "$SERVICE" --project "$PROJECT" --region "$REGION" --format='value(status.url)' 2>/dev/null || true)"
cat <<EOF

── done. next steps:
   1. Bake the URL into released binaries (fixes ledger shipping):
        gh variable set PARSEC_DEFAULT_PLATFORM_URL --body "${URL:-<platform-url>}"
   2. Point the frontend at it: set PLATFORM_URL=${URL:-<platform-url>} in its host env.
   3. Register the Stripe webhook endpoint: ${URL:-<platform-url>}/webhooks/stripe
EOF
