"""FastAPI app — the §7c control plane skeleton.

Everything here is glue over managed services: Supabase verifies identity
(auth.py), Stripe decides billing state (stripe_webhook.py), and this app only
records the consequences (store.py) and serves the two seams the rest of the
product needs — "is this key entitled?" for the brain API (§7) and the
savings ledger (§4.3/§8.4). Telemetry intake (quarantine → validate → corpus)
lands with the trainer, not this skeleton.
"""

from __future__ import annotations

import json
import os
import secrets
import time
from typing import Any

from fastapi import Depends, FastAPI, HTTPException, Query, Request
from fastapi.middleware.cors import CORSMiddleware

from parsec_platform.auth import (
    hash_key,
    optional_key_account,
    require_account,
    require_key_account,
)
from parsec_platform.models import InstallReport, LedgerRow
from parsec_platform.store import SQLiteStore, Store
from parsec_platform.stripe_webhook import verify_stripe_signature


def _priced_sum(by_model: list[dict], key: str) -> float:
    """Sum a per-model cost column across priced models — an unpriced model's
    cost is None (a hole, never zero) and contributes nothing."""
    return round(sum(m[key] for m in by_model if m[key] is not None), 6)


def create_app(store: Store | None = None) -> FastAPI:
    """Build the app. Store precedence: explicit `store` arg (tests) →
    Postgres when PARSEC_PLATFORM_DB_URL is set (Supabase, §7c) → SQLite at
    PARSEC_PLATFORM_DB (zero-infrastructure default)."""
    app = FastAPI(title="parsec-platform", version="0.1.0")
    if store is None:
        if os.environ.get("PARSEC_PLATFORM_DB_URL"):
            from parsec_platform.pgstore import PostgresStore

            store = PostgresStore()
        else:
            store = SQLiteStore()
    app.state.store = store
    # /savings/public is the only route a browser calls cross-origin: the
    # getparsec.ai landing page is a static export with no BFF (unlike the
    # dashboard, which proxies server-side). GET-only CORS for the marketing
    # origins; every authenticated route is server-to-server and unaffected.
    # Local dev overrides via PARSEC_CORS_ORIGINS in the repo-root .env.local
    # (compose loads it via env_file; bare uvicorn needs --env-file).
    app.add_middleware(
        CORSMiddleware,
        allow_origins=os.environ.get(
            "PARSEC_CORS_ORIGINS",
            "https://getparsec.ai,https://www.getparsec.ai,"
            "https://daseinlabs.ai,https://www.daseinlabs.ai,"
            "https://daseinlabs.github.io",
        ).split(","),
        allow_methods=["GET"],
    )
    # Pre-billing measure: until Stripe is wired, grant entitlement the moment a
    # user mints their key (the onboarding step) so we can gather usage patterns.
    # Off by default — the correct long-term behavior is Stripe-gated — the
    # deployment turns it on with PARSEC_AUTO_ENTITLE=1 and drops it when billing
    # goes live. See mint_key.
    auto_entitle = os.environ.get("PARSEC_AUTO_ENTITLE", "") == "1"

    @app.get("/health")
    def health() -> dict[str, str]:
        return {"status": "ok"}

    @app.post("/webhooks/stripe")
    async def stripe_webhook(request: Request) -> dict[str, bool]:
        """Stripe webhook → entitlement flag (§7c). Signature-verified with
        the endpoint secret; unsigned/forged payloads are rejected before any
        state change."""
        payload = await request.body()
        secret = os.environ.get("STRIPE_WEBHOOK_SECRET", "")
        if not secret or not verify_stripe_signature(
            payload, request.headers.get("Stripe-Signature"), secret
        ):
            raise HTTPException(status_code=400, detail="invalid signature")
        try:
            event = json.loads(payload)
        except ValueError:
            raise HTTPException(status_code=400, detail="invalid payload")
        obj = event.get("data", {}).get("object", {})
        # Only checkout.session carries client_reference_id; Subscription
        # objects carry metadata only if set at checkout and ALWAYS carry the
        # customer id — so checkout links customer -> account, and
        # subscription events resolve through that mapping.
        customer_id = obj.get("customer")
        account_id = (
            obj.get("client_reference_id")
            or obj.get("metadata", {}).get("account_id")
            or (customer_id and app.state.store.account_for_customer(customer_id))
        )
        handled = False
        if account_id:
            event_type = event.get("type", "")
            if event_type == "checkout.session.completed":
                if customer_id:
                    app.state.store.link_customer(customer_id, account_id)
                app.state.store.set_entitlement(account_id, True)
                handled = True
            elif event_type == "customer.subscription.deleted":
                app.state.store.set_entitlement(account_id, False)
                handled = True
            elif event_type == "customer.subscription.updated":
                app.state.store.set_entitlement(
                    account_id, obj.get("status") == "active"
                )
                handled = True
        # Always 2xx on a verified event so Stripe does not retry event types
        # we deliberately ignore — but an UNRESOLVABLE subscription event is
        # an entitlement leak (a cancellation we could not revoke); flag it in
        # the response so it shows in the Stripe dashboard's delivery log.
        if not handled and event.get("type", "").startswith("customer.subscription."):
            return {"received": True, "handled": False, "unresolved_account": True}
        return {"received": True, "handled": handled}

    @app.post("/keys", status_code=201)
    def mint_key(account_id: str = Depends(require_account)) -> dict[str, str]:
        """Mint the opaque brain-API credential the local proxy calls with
        (§7c). Shown once; only its hash is stored.

        When PARSEC_AUTO_ENTITLE=1 (pre-billing phase), also flip the account to
        entitled here — this is the "anyone who signs up is entitled" switch that
        lets the brain serve real users before Stripe exists. Stripe events still
        own the flag once billing is live and the switch is off."""
        key = "psc_" + secrets.token_urlsafe(32)
        app.state.store.add_key(hash_key(key), account_id)
        if auto_entitle:
            app.state.store.set_entitlement(account_id, True)
        return {"key": key}

    @app.get("/keys/validate/{key}")
    def validate_key(key: str) -> dict[str, bool]:
        """The "is this key entitled?" seam DIRECTION §7 names — what the
        brain API asks before scoring. Unknown keys are valid=False rather
        than 404 so callers get one uniform shape."""
        account_id = app.state.store.account_for_key(hash_key(key))
        if account_id is None:
            return {"valid": False, "entitled": False}
        return {"valid": True, "entitled": app.state.store.is_entitled(account_id)}

    @app.post("/ledger", status_code=201)
    def ingest_ledger_row(
        row: LedgerRow, account_id: str = Depends(require_key_account)
    ) -> dict[str, bool]:
        """Savings-ledger ingest: one row per request, key-authenticated
        (the proxy reports with its psc_ key). Idempotent on request_id."""
        app.state.store.add_ledger_row(account_id, row.model_dump())
        return {"accepted": True}

    @app.post("/installs", status_code=201)
    def register_install(
        report: InstallReport, account_id: str | None = Depends(optional_key_account)
    ) -> dict[str, bool]:
        """Install-registration ping (docs/install-tracking.md): one row per
        machine, upserted on the client-minted install_id. Keyless pings are
        accepted — an install exists before onboarding — and a later keyed
        ping links the account. Every field is pattern-gated by the
        InstallReport mirror, so this unauthenticated path cannot carry raw
        text."""
        app.state.store.record_install(report.model_dump(), account_id)
        return {"accepted": True}

    @app.get("/installs/summary")
    def installs_summary(account_id: str = Depends(require_account)) -> dict:
        """Fleet-wide install counts: total, active_7d/30d (last_seen-based),
        linked accounts, and by-version/os/harness spreads. Aggregate-only —
        no install ids or account ids leave the fold. JWT-gated; note this is
        a PRODUCT-WIDE metric (any signed-in account sees the same numbers),
        an internal dashboard view rather than per-account data."""
        return app.state.store.installs_summary()

    @app.get("/ledger/summary")
    def ledger_summary(account_id: str = Depends(require_account)) -> dict:
        """Per-account usage + savings reporting (§7c): the billing basis, the
        upsell proof, the trust artifact — all from count_tokens counterfactual
        rows (§8.4), never a modeled baseline. Carries a per-model/cost
        breakdown (`by_model`) plus the account-wide totals `cost_usd` (spend) and
        `cost_saved_usd` (the dashboard's headline number), each summed from the
        priced models — unpriced-model tokens simply add no cost. Savings are
        priced per model before summing so each model's saved tokens carry its own
        blended input-side rate (see `store.cost_saved_usd`)."""
        summary = app.state.store.ledger_summary(account_id)
        by_model = summary.get("by_model", [])
        summary["cost_usd"] = _priced_sum(by_model, "cost_usd")
        summary["cost_saved_usd"] = _priced_sum(by_model, "cost_saved_usd")
        summary["currency"] = "USD"
        return summary

    @app.get("/savings")
    def savings(account_id: str = Depends(require_account)) -> dict:
        """Current total savings for the account: the savings fields of
        /ledger/summary without the usage/billing detail. Deliberately a
        projection of the same ledger aggregation — not a running counter —
        so the number is always recomputable from per-request count_tokens
        counterfactual rows (§8.4) and cannot drift from the summary.
        tokens_saved is the primary figure; cost_saved_usd is derived from it
        via each model's blended input-side rate (see store.cost_saved_usd)
        and sums only priced models."""
        summary = app.state.store.ledger_summary(account_id)
        return {
            "tokens_saved": summary["tokens_saved"],
            "cost_saved_usd": _priced_sum(summary.get("by_model", []), "cost_saved_usd"),
            "currency": "USD",
            "rows_count": summary["rows_count"],
            "measured_rows": summary["measured_rows"],
        }

    # (value, monotonic deadline) for /savings/public — one cached dict per
    # process is enough; replicas each warming their own copy is fine.
    public_cache: dict[str, Any] = {"value": None, "until": 0.0}

    @app.get("/savings/public")
    def savings_public() -> dict:
        """Site-wide savings counter for the landing page: aggregate across
        ALL accounts, unauthenticated by design, exposing nothing per-account.
        Same §8.4 ledger aggregation as /savings (real count_tokens
        counterfactuals only — the www copy rule forbids invented numbers).
        Cached ~60s in-process so anonymous traffic can't hammer the
        full-ledger scan; staleness of a minute is irrelevant to a counter
        that only grows."""
        now = time.monotonic()
        if public_cache["value"] is None or now >= public_cache["until"]:
            totals = app.state.store.savings_public()
            public_cache["value"] = {
                "tokens_saved": totals["tokens_saved"],
                "cost_saved_usd": totals["cost_saved_usd"],
                "measured_rows": totals["measured_rows"],
                "currency": "USD",
            }
            public_cache["until"] = now + 60
        return public_cache["value"]

    @app.get("/ledger/usage")
    def ledger_usage(
        account_id: str = Depends(require_account),
        days: int = Query(default=30, ge=1, le=365),
    ) -> dict:
        """Per-day usage series for the account over the last `days` days — the
        dashboard's usage-over-time view (tokens, tokens_saved, cost per day).
        Same measurement honesty as the summary; empty days are omitted."""
        return {
            "days": app.state.store.usage_daily(account_id, days),
            "window_days": days,
            "currency": "USD",
        }

    return app
