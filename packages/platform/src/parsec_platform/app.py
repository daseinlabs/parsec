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

from fastapi import Depends, FastAPI, HTTPException, Query, Request

from parsec_platform.auth import hash_key, require_account, require_key_account
from parsec_platform.models import LedgerRow
from parsec_platform.store import SQLiteStore, Store
from parsec_platform.stripe_webhook import verify_stripe_signature


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

    @app.get("/ledger/summary")
    def ledger_summary(account_id: str = Depends(require_account)) -> dict:
        """Per-account usage + savings reporting (§7c): the billing basis, the
        upsell proof, the trust artifact — all from count_tokens counterfactual
        rows (§8.4), never a modeled baseline. Carries a per-model/cost
        breakdown (`by_model`) plus the account-wide total `cost_usd` summed from
        the priced models (unpriced-model tokens simply add no cost)."""
        summary = app.state.store.ledger_summary(account_id)
        priced = [m["cost_usd"] for m in summary.get("by_model", []) if m["cost_usd"] is not None]
        summary["cost_usd"] = round(sum(priced), 6)
        summary["currency"] = "USD"
        return summary

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
