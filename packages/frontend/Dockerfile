# dasein frontend — Cloud Run image (server-rendered Next 16, standalone output).
# Build from packages/frontend:  docker build -t dasein-frontend packages/frontend
#
# CRITICAL Next.js gotcha: NEXT_PUBLIC_* are INLINED into the client bundle at
# `next build`, NOT read at runtime. So they arrive here as --build-arg and must
# be present before the build step. Only PLATFORM_URL (server-side, read per
# request in lib/platform.ts) is a real runtime env var. All four public vars
# are non-secret (publishable key / public Stripe links), so baking is fine.

FROM oven/bun:1 AS deps
WORKDIR /app
COPY package.json bun.lock ./
RUN bun install --frozen-lockfile

FROM oven/bun:1 AS build
WORKDIR /app
COPY --from=deps /app/node_modules ./node_modules
COPY . .
ARG NEXT_PUBLIC_SUPABASE_URL
ARG NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY
ARG NEXT_PUBLIC_STRIPE_CHECKOUT_URL
ARG NEXT_PUBLIC_STRIPE_PORTAL_URL
ENV NEXT_PUBLIC_SUPABASE_URL=$NEXT_PUBLIC_SUPABASE_URL \
    NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY=$NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY \
    NEXT_PUBLIC_STRIPE_CHECKOUT_URL=$NEXT_PUBLIC_STRIPE_CHECKOUT_URL \
    NEXT_PUBLIC_STRIPE_PORTAL_URL=$NEXT_PUBLIC_STRIPE_PORTAL_URL \
    NEXT_TELEMETRY_DISABLED=1
RUN bun run build

# Runtime: the standalone server is plain node — no bun, no dev deps needed.
FROM node:22-slim AS runtime
WORKDIR /app
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1 PORT=8080 HOSTNAME=0.0.0.0
RUN useradd --create-home app
USER app
# standalone bundles server.js + a pruned node_modules; static/ and public/ are
# NOT included by design and must be copied alongside it.
COPY --from=build --chown=app:app /app/.next/standalone ./
COPY --from=build --chown=app:app /app/.next/static ./.next/static
COPY --from=build --chown=app:app /app/public ./public
EXPOSE 8080
CMD ["node", "server.js"]
