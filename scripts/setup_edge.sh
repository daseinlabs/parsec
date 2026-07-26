#!/usr/bin/env bash
# One-time: front the brain's Cloud Run service with a Global External ALB +
# Cloud Armor, giving a CHEAP, COARSE, per-IP rate bottleneck at Google's edge.
#
# Why the edge and not the app: rate-limit state is per-user, mutable, and
# shared — CLAUDE.md / docs/deploy-cloud-run.md forbid it in the stateless brain
# replica. Cloud Armor keeps the counters in Google's infra (nothing to run or
# maintain), and drops floods BEFORE they reach a GPU pod. Keyed on IP: coarse
# and un-spoofable. "Leaking some compute" (bursts within the window, users
# behind shared NAT sharing a bucket) is accepted by design — this is a
# bottleneck, not a precise per-user SLA. Per-token keying can be layered on
# later (add a second rule with --enforce-on-key=HTTP-HEADER) once stable
# per-user tokens exist.
#
#   scripts/setup_edge.sh            # create the LB + Cloud Armor policy
#   LOCK_INGRESS=true scripts/setup_edge.sh   # ALSO restrict Cloud Run to LB-only
#
# Override via env: PROJECT, REGION, SERVICE, DOMAIN, POLICY, RATE_COUNT,
# RATE_INTERVAL, BAN_DURATION, LOCK_INGRESS.
set -euo pipefail

PROJECT="${PROJECT:-$(gcloud config get-value project 2>/dev/null)}"
REGION="${REGION:-us-central1}"
SERVICE="${SERVICE:-dasein-brain}"
DOMAIN="${DOMAIN:-brain.getparsec.ai}"     # DNS you'll point at the LB IP
POLICY="${POLICY:-dasein-edge}"
NEG="${SERVICE}-neg"
BACKEND="${SERVICE}-backend"

# Coarse cap: ~20 req/s AVERAGED over the window. Cloud Armor's window is not
# truly per-second, so a client can burst within it — that's the accepted leak.
# 1200 / 60s = 20/s average. Tighten the interval for a stricter cap.
RATE_COUNT="${RATE_COUNT:-1200}"
RATE_INTERVAL="${RATE_INTERVAL:-60}"       # seconds
BAN_DURATION="${BAN_DURATION:-60}"         # seconds an over-limit IP is 429'd
LOCK_INGRESS="${LOCK_INGRESS:-false}"

[ -z "$PROJECT" ] && { echo "no GCP project — gcloud config set project <id>" >&2; exit 1; }
gcloud() { command gcloud --project "$PROJECT" "$@"; }

echo "── edge for $SERVICE  (project=$PROJECT region=$REGION domain=$DOMAIN)"
echo "   coarse per-IP cap: $RATE_COUNT req / ${RATE_INTERVAL}s  (~$((RATE_COUNT / RATE_INTERVAL))/s avg)"

# 1. Serverless NEG → the Cloud Run service.
gcloud compute network-endpoint-groups create "$NEG" \
  --region="$REGION" --network-endpoint-type=serverless \
  --cloud-run-service="$SERVICE" 2>/dev/null || echo "   NEG $NEG exists"

# 2. Global backend service (the LB scheme Cloud Run serverless NEGs require).
gcloud compute backend-services create "$BACKEND" --global \
  --load-balancing-scheme=EXTERNAL_MANAGED 2>/dev/null || echo "   backend $BACKEND exists"
gcloud compute backend-services add-backend "$BACKEND" --global \
  --network-endpoint-group="$NEG" --network-endpoint-group-region="$REGION" 2>/dev/null \
  || echo "   backend already has NEG"

# 3. Cloud Armor policy + a per-IP rate-limit rule. `throttle` 429s the excess;
#    swap to `rate-based-ban` (same flags) to temporarily block a hot IP.
gcloud compute security-policies create "$POLICY" \
  --description "coarse per-IP bottleneck for $SERVICE" 2>/dev/null || echo "   policy $POLICY exists"
gcloud compute security-policies rules create 1000 \
  --security-policy="$POLICY" \
  --expression="true" \
  --action=throttle \
  --rate-limit-threshold-count="$RATE_COUNT" \
  --rate-limit-threshold-interval-sec="$RATE_INTERVAL" \
  --conform-action=allow \
  --exceed-action=deny-429 \
  --enforce-on-key=IP \
  --ban-duration-sec="$BAN_DURATION" 2>/dev/null || echo "   rule 1000 exists (delete to change)"

# 4. Attach the policy to the backend.
gcloud compute backend-services update "$BACKEND" --global --security-policy="$POLICY"

# 5. URL map → target HTTPS proxy → managed cert → forwarding rule.
gcloud compute url-maps create "${SERVICE}-lb" --default-service="$BACKEND" 2>/dev/null \
  || echo "   url-map exists"
gcloud compute ssl-certificates create "${SERVICE}-cert" --domains="$DOMAIN" --global 2>/dev/null \
  || echo "   cert exists"
gcloud compute target-https-proxies create "${SERVICE}-https" \
  --url-map="${SERVICE}-lb" --ssl-certificates="${SERVICE}-cert" 2>/dev/null \
  || echo "   https-proxy exists"
gcloud compute forwarding-rules create "${SERVICE}-fr" --global \
  --target-https-proxy="${SERVICE}-https" --ports=443 2>/dev/null || echo "   forwarding-rule exists"

IP=$(gcloud compute forwarding-rules describe "${SERVICE}-fr" --global --format='value(IPAddress)')
echo
echo "── LB IP: $IP"
echo "   Point DNS:  $DOMAIN  A  $IP   (managed cert provisions once DNS resolves)"

# 6. OPTIONAL, do LAST: stop anyone bypassing Cloud Armor via the *.run.app URL.
#    Run only after the LB serves traffic, or you lock yourself out mid-setup.
if [ "$LOCK_INGRESS" = "true" ]; then
  echo "── locking $SERVICE ingress to internal-and-cloud-load-balancing"
  gcloud run services update "$SERVICE" --region="$REGION" \
    --ingress=internal-and-cloud-load-balancing
else
  echo "   (skipping ingress lock — rerun with LOCK_INGRESS=true once the LB works,"
  echo "    else the run.app URL stays a Cloud-Armor-free back door)"
fi
