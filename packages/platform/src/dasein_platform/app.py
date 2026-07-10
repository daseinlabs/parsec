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

from fastapi import Depends, FastAPI, HTTPException, Request

from dasein_platform.auth import hash_key, require_account, require_key_account
from dasein_platform.models import LedgerRow
from dasein_platform.store import SQLiteStore, Store
from dasein_platform.stripe_webhook import verify_stripe_signature


def create_app(store: Store | None = None) -> FastAPI:
    """Build the app. `store` defaults to SQLite at DASEIN_PLATFORM_DB; a
    Supabase-Postgres Store drops in here later without touching routes."""
    app = FastAPI(title="dasein-platform", version="0.1.0")
    app.state.store = store or SQLiteStore()

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
        account_id = obj.get("client_reference_id") or obj.get("metadata", {}).get(
            "account_id"
        )
        handled = False
        if account_id:
            event_type = event.get("type", "")
            if event_type == "checkout.session.completed":
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
        # we deliberately ignore.
        return {"received": True, "handled": handled}

    @app.post("/keys", status_code=201)
    def mint_key(account_id: str = Depends(require_account)) -> dict[str, str]:
        """Mint the opaque brain-API credential the local proxy calls with
        (§7c). Shown once; only its hash is stored."""
        key = "dsn_" + secrets.token_urlsafe(32)
        app.state.store.add_key(hash_key(key), account_id)
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
        (the proxy reports with its dsn_ key). Idempotent on request_id."""
        app.state.store.add_ledger_row(account_id, row.model_dump())
        return {"accepted": True}

    @app.get("/ledger/summary")
    def ledger_summary(account_id: str = Depends(require_account)) -> dict:
        """Per-account savings reporting (§7c): the billing basis, the upsell
        proof, the trust artifact — all from count_tokens counterfactual rows
        (§8.4), never a modeled baseline."""
        return app.state.store.ledger_summary(account_id)

    return app
