"""Platform skeleton tests (DIRECTION.md §7c) via fastapi TestClient.

Covers the seams the spec names: Supabase JWT verification (401 + roundtrip),
Stripe webhook signature accept/reject → entitlement flag, brain-API key
mint → validate, and savings-ledger ingest → summary against the authoritative
contracts example.
"""

from __future__ import annotations

import json
import time
from pathlib import Path

import jwt
import pytest
from fastapi.testclient import TestClient

from dasein_platform import create_app
from dasein_platform.models import LedgerRow
from dasein_platform.store import SQLiteStore
from dasein_platform.stripe_webhook import sign_payload

JWT_SECRET = "test-supabase-jwt-secret-0123456789abcdef"
STRIPE_SECRET = "whsec_test_secret"
ACCOUNT = "acct-11111111-2222-3333-4444-555555555555"

CONTRACTS_EXAMPLE = (
    Path(__file__).resolve().parents[2]
    / "contracts"
    / "schemas"
    / "examples"
    / "savings-ledger.example.json"
)
# The seams-bearing example (capture + governor optional fields) — the mirror
# must accept EVERY schema-optional field, not just the minimal row (the gap
# that let capture-seam rows 422 at ingest).
CONTRACTS_GOVERNOR_EXAMPLE = (
    CONTRACTS_EXAMPLE.parent / "savings-ledger.governor.example.json"
)


@pytest.fixture()
def client(monkeypatch: pytest.MonkeyPatch) -> TestClient:
    monkeypatch.setenv("SUPABASE_JWT_SECRET", JWT_SECRET)
    monkeypatch.setenv("STRIPE_WEBHOOK_SECRET", STRIPE_SECRET)
    return TestClient(create_app(store=SQLiteStore(":memory:")))


def mint_jwt(account_id: str = ACCOUNT, secret: str = JWT_SECRET, **overrides) -> str:
    claims = {
        "sub": account_id,
        "aud": "authenticated",
        "exp": int(time.time()) + 3600,
        **overrides,
    }
    return jwt.encode(claims, secret, algorithm="HS256")


def auth(token: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"}


def stripe_event(event_type: str, account_id: str = ACCOUNT, **obj_extra) -> bytes:
    return json.dumps(
        {"type": event_type, "data": {"object": {"client_reference_id": account_id, **obj_extra}}}
    ).encode()


def test_health(client: TestClient) -> None:
    assert client.get("/health").json() == {"status": "ok"}


def test_401_without_jwt(client: TestClient) -> None:
    assert client.post("/keys").status_code == 401
    assert client.get("/ledger/summary").status_code == 401


def test_401_with_bad_jwt(client: TestClient) -> None:
    forged = mint_jwt(secret="wrong-secret")
    assert client.post("/keys", headers=auth(forged)).status_code == 401
    expired = mint_jwt(exp=int(time.time()) - 10)
    assert client.post("/keys", headers=auth(expired)).status_code == 401


def test_jwt_roundtrip(client: TestClient) -> None:
    resp = client.post("/keys", headers=auth(mint_jwt()))
    assert resp.status_code == 201
    assert resp.json()["key"].startswith("dsn_")


def test_stripe_webhook_rejects_bad_signature(client: TestClient) -> None:
    payload = stripe_event("checkout.session.completed")
    assert client.post("/webhooks/stripe", content=payload).status_code == 400
    forged = sign_payload(payload, "wrong-secret")
    resp = client.post(
        "/webhooks/stripe", content=payload, headers={"Stripe-Signature": forged}
    )
    assert resp.status_code == 400
    stale = sign_payload(payload, STRIPE_SECRET, timestamp=int(time.time()) - 3600)
    resp = client.post(
        "/webhooks/stripe", content=payload, headers={"Stripe-Signature": stale}
    )
    assert resp.status_code == 400


def test_stripe_webhook_flips_entitlement(client: TestClient) -> None:
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    assert client.get(f"/keys/validate/{key}").json() == {
        "valid": True,
        "entitled": False,
    }

    payload = stripe_event("checkout.session.completed")
    resp = client.post(
        "/webhooks/stripe",
        content=payload,
        headers={"Stripe-Signature": sign_payload(payload, STRIPE_SECRET)},
    )
    assert resp.status_code == 200 and resp.json()["handled"] is True
    assert client.get(f"/keys/validate/{key}").json() == {
        "valid": True,
        "entitled": True,
    }

    payload = stripe_event("customer.subscription.deleted")
    client.post(
        "/webhooks/stripe",
        content=payload,
        headers={"Stripe-Signature": sign_payload(payload, STRIPE_SECRET)},
    )
    assert client.get(f"/keys/validate/{key}").json() == {
        "valid": True,
        "entitled": False,
    }


def test_validate_unknown_key(client: TestClient) -> None:
    resp = client.get("/keys/validate/dsn_definitely-not-minted")
    assert resp.json() == {"valid": False, "entitled": False}


def test_pydantic_model_accepts_contracts_example() -> None:
    """The JSON Schema file is authoritative; the pydantic mirror must accept
    its committed example (drift guard)."""
    example = json.loads(CONTRACTS_EXAMPLE.read_text())
    row = LedgerRow.model_validate(example)
    assert row.cache_prefix_sha8 == example["cachePrefixSha8"]


def test_pydantic_model_accepts_governor_example() -> None:
    """Every schema-OPTIONAL field (capture seams + governor seams) must pass
    the mirror too — extra="forbid" turns a missing mirror field into a 422
    at ingest, silently dropping real rows."""
    example = json.loads(CONTRACTS_GOVERNOR_EXAMPLE.read_text())
    row = LedgerRow.model_validate(example)
    assert row.governor_mode == "advise"
    assert row.gov_rule_fires == example["gov_rule_fires"]
    assert row.checkpoint_id == example["checkpoint_id"]


def test_ledger_ingest_to_summary(client: TestClient) -> None:
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    example = json.loads(CONTRACTS_EXAMPLE.read_text())

    row2 = dict(
        example,
        request_id="req_" + "f" * 32,
        counterfactual_input_tokens=1000,
        billed_input_tokens=400,
        fail_open=True,
    )
    for row in (example, row2):
        resp = client.post("/ledger", json=row, headers={"X-Dasein-Key": key})
        assert resp.status_code == 201, resp.text

    # No key / unknown key -> 401.
    assert client.post("/ledger", json=example).status_code == 401
    assert (
        client.post(
            "/ledger", json=example, headers={"X-Dasein-Key": "dsn_unknown"}
        ).status_code
        == 401
    )
    # Extra fields are unrepresentable (additionalProperties: false mirror).
    bad = dict(example, prompt_text="sneaky")
    assert (
        client.post("/ledger", json=bad, headers={"X-Dasein-Key": key}).status_code
        == 422
    )

    summary = client.get("/ledger/summary", headers=auth(mint_jwt())).json()
    assert summary["rows_count"] == 2
    assert summary["counterfactual_input_tokens"] == example["counterfactual_input_tokens"] + 1000
    assert summary["billed_input_tokens"] == example["billed_input_tokens"] + 400
    assert summary["tokens_saved"] == (
        summary["counterfactual_input_tokens"] - summary["billed_input_tokens"]
    )
    assert summary["fail_open_count"] == 1

    # Another account sees an empty ledger.
    other = client.get("/ledger/summary", headers=auth(mint_jwt("acct-other"))).json()
    assert other["rows_count"] == 0
