"""Platform skeleton tests (DIRECTION.md §7c) via fastapi TestClient.

Covers the seams the spec names: Supabase JWT verification (401 + roundtrip),
Stripe webhook signature accept/reject → entitlement flag, brain-API key
mint → validate, and savings-ledger ingest → summary against the authoritative
contracts example.
"""

from __future__ import annotations

import json
import os
import time
import uuid
from pathlib import Path

import jwt
import pytest
from fastapi.testclient import TestClient

from parsec_platform import create_app
from parsec_platform.auth import hash_key
from parsec_platform.models import LedgerRow
from parsec_platform.store import SQLiteStore
from parsec_platform.stripe_webhook import sign_payload

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


def stripe_event(event_type: str, account_id: str | None = ACCOUNT, **obj_extra) -> bytes:
    """account_id=None mimics real subscription events, which carry only the
    customer id — never client_reference_id (checkout-session-only field)."""
    obj = {**obj_extra}
    if account_id is not None:
        obj["client_reference_id"] = account_id
    return json.dumps({"type": event_type, "data": {"object": obj}}).encode()


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
    assert resp.json()["key"].startswith("psc_")


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


def post_event(client: TestClient, payload: bytes) -> dict:
    resp = client.post(
        "/webhooks/stripe",
        content=payload,
        headers={"Stripe-Signature": sign_payload(payload, STRIPE_SECRET)},
    )
    assert resp.status_code == 200, resp.text
    return resp.json()


def test_stripe_webhook_flips_entitlement(client: TestClient) -> None:
    """Realistic event shapes: checkout carries client_reference_id +
    customer; subscription events carry ONLY the customer id and must resolve
    through the mapping recorded at checkout."""
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    assert client.get(f"/keys/validate/{key}").json() == {
        "valid": True,
        "entitled": False,
    }

    body = post_event(
        client, stripe_event("checkout.session.completed", customer="cus_123")
    )
    assert body["handled"] is True
    assert client.get(f"/keys/validate/{key}").json() == {
        "valid": True,
        "entitled": True,
    }

    # Cancellation: no client_reference_id, no metadata — customer id only.
    body = post_event(
        client,
        stripe_event("customer.subscription.deleted", account_id=None, customer="cus_123"),
    )
    assert body["handled"] is True
    assert client.get(f"/keys/validate/{key}").json() == {
        "valid": True,
        "entitled": False,
    }

    # Reactivation via subscription.updated, same resolution path.
    body = post_event(
        client,
        stripe_event(
            "customer.subscription.updated",
            account_id=None,
            customer="cus_123",
            status="active",
        ),
    )
    assert body["handled"] is True
    assert client.get(f"/keys/validate/{key}").json()["entitled"] is True


def test_stripe_subscription_event_for_unknown_customer_is_flagged(
    client: TestClient,
) -> None:
    """A subscription event we cannot map to an account is an entitlement
    leak — it must be flagged in the response, not silently swallowed."""
    body = post_event(
        client,
        stripe_event("customer.subscription.deleted", account_id=None, customer="cus_ghost"),
    )
    assert body == {"received": True, "handled": False, "unresolved_account": True}


def test_validate_unknown_key(client: TestClient) -> None:
    resp = client.get("/keys/validate/psc_definitely-not-minted")
    assert resp.json() == {"valid": False, "entitled": False}


def test_auto_entitle_on_mint(monkeypatch: pytest.MonkeyPatch) -> None:
    """PARSEC_AUTO_ENTITLE=1 (pre-billing) entitles an account the moment it
    mints a key — no Stripe event needed."""
    monkeypatch.setenv("SUPABASE_JWT_SECRET", JWT_SECRET)
    monkeypatch.setenv("PARSEC_AUTO_ENTITLE", "1")
    client = TestClient(create_app(store=SQLiteStore(":memory:")))
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    assert client.get(f"/keys/validate/{key}").json() == {"valid": True, "entitled": True}


def test_no_auto_entitle_by_default(client: TestClient) -> None:
    """Default (flag unset): minting a key does NOT entitle — Stripe still owns
    the flag, so the existing billing flow is unchanged."""
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    assert client.get(f"/keys/validate/{key}").json()["entitled"] is False


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
    assert row.tools_unfrozen == example["tools_unfrozen"]
    assert row.curator_insists == example["curator_insists"]


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
        resp = client.post("/ledger", json=row, headers={"X-Parsec-Key": key})
        assert resp.status_code == 201, resp.text

    # No key / unknown key -> 401.
    assert client.post("/ledger", json=example).status_code == 401
    assert (
        client.post(
            "/ledger", json=example, headers={"X-Parsec-Key": "psc_unknown"}
        ).status_code
        == 401
    )
    # Extra fields are unrepresentable (additionalProperties: false mirror).
    bad = dict(example, prompt_text="sneaky")
    assert (
        client.post("/ledger", json=bad, headers={"X-Parsec-Key": key}).status_code
        == 422
    )

    summary = client.get("/ledger/summary", headers=auth(mint_jwt())).json()
    assert summary["rows_count"] == 2
    assert summary["counterfactual_input_tokens"] == example["counterfactual_input_tokens"] + 1000
    assert summary["billed_input_tokens"] == example["billed_input_tokens"] + 400
    # Saved subtracts ALL billed input-side tokens, cache read/write included —
    # they are billed input, so treating them as free would overstate savings.
    # Valid to derive from the summary sums here because both rows are measured.
    assert summary["tokens_saved"] == (
        summary["counterfactual_input_tokens"]
        - summary["billed_input_tokens"]
        - summary["billed_cache_read_tokens"]
        - summary["billed_cache_write_tokens"]
    )
    assert summary["fail_open_count"] == 1

    # Another account sees an empty ledger.
    other = client.get("/ledger/summary", headers=auth(mint_jwt("acct-other"))).json()
    assert other["rows_count"] == 0


def test_cache_tokens_are_not_free_savings(client: TestClient) -> None:
    """Cache reads/writes are BILLED input and must reduce reported savings.

    The regression this pins: summing `counterfactual − billed_input_tokens`
    alone treats every cache-served token as free. A warm Claude Code session
    bills almost all of its input as cache reads, so that formula reported
    ~2.2x the real figure on a 406-row production ledger, and disagreed with
    what `parsec savings` printed locally from the very same rows.
    """
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    example = json.loads(CONTRACTS_EXAMPLE.read_text())
    # Warm-session shape: tiny uncached input, almost all of it cache reads.
    row = dict(
        example,
        request_id="req_" + "a" * 32,
        model="claude-sonnet-5",  # the contracts example omits it; by_model needs it
        # The example's fixed ts ages out of the 30-day usage window; the
        # cross-check below needs the row to land in the series.
        ts=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        counterfactual_input_tokens=100_000,
        billed_input_tokens=500,
        billed_cache_read_tokens=60_000,
        billed_cache_write_tokens=9_500,
    )
    assert (
        client.post("/ledger", json=row, headers={"X-Parsec-Key": key}).status_code
        == 201
    )

    summary = client.get("/ledger/summary", headers=auth(mint_jwt())).json()
    assert summary["tokens_saved"] == 30_000  # 100000 − (500 + 60000 + 9500)
    # The naive formula would have claimed 99,500 — 3.3x the truth.
    assert summary["tokens_saved"] != (
        row["counterfactual_input_tokens"] - row["billed_input_tokens"]
    )
    # Same definition must hold in the per-model and per-day breakdowns, which
    # are separate queries and so can drift independently.
    by_model = {m["model"]: m for m in summary["by_model"]}
    assert by_model[row["model"]]["tokens_saved"] == 30_000
    usage = client.get("/ledger/usage", headers=auth(mint_jwt())).json()
    assert sum(d["tokens_saved"] for d in usage["days"]) == 30_000


def test_savings_endpoint(client: TestClient) -> None:
    """/savings is a projection of /ledger/summary — same §8.4-honest
    aggregation, so the two can never disagree — and NULL-counterfactual rows
    count toward rows_count but never toward measured_rows or tokens_saved."""
    assert client.get("/savings").status_code == 401

    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    example = json.loads(CONTRACTS_EXAMPLE.read_text())
    measured = dict(
        example,
        request_id="req_" + "b" * 32,
        model="claude-sonnet-5",
        counterfactual_input_tokens=100_000,
        billed_input_tokens=500,
        billed_cache_read_tokens=60_000,
        billed_cache_write_tokens=9_500,
    )
    hole = dict(
        example,
        request_id="req_" + "c" * 32,
        counterfactual_input_tokens=None,  # probe failed: a hole, not a zero
    )
    for row in (measured, hole):
        assert (
            client.post("/ledger", json=row, headers={"X-Parsec-Key": key}).status_code
            == 201
        )

    savings = client.get("/savings", headers=auth(mint_jwt())).json()
    summary = client.get("/ledger/summary", headers=auth(mint_jwt())).json()
    assert savings["tokens_saved"] == summary["tokens_saved"]
    assert savings["cost_saved_usd"] == summary["cost_saved_usd"]
    assert savings["rows_count"] == summary["rows_count"] == 2
    assert savings["measured_rows"] == summary["measured_rows"] == 1
    assert savings["currency"] == "USD"

    # Another account has saved nothing.
    other = client.get("/savings", headers=auth(mint_jwt("acct-other"))).json()
    assert other["tokens_saved"] == 0
    assert other["cost_saved_usd"] == 0
    assert other["rows_count"] == 0


def test_cost_helpers_return_floats_for_decimal_prices() -> None:
    """Postgres NUMERIC pricing columns arrive as Decimal, and a Decimal cost
    JSON-serializes as a string, not a number (seen live on /savings/public).
    The helpers must coerce to float; SQLite feeds them floats, so only this
    test exercises the Decimal path without a Postgres instance."""
    from decimal import Decimal

    from parsec_platform.store import cost_saved_usd, cost_usd

    prices = (Decimal("3.0"), Decimal("15.0"), Decimal("0.3"), Decimal("3.75"))
    sums = (500, 100, 60_000, 9_500)
    assert isinstance(cost_usd(sums, prices), float)
    assert isinstance(cost_saved_usd(30_000, sums, prices), float)


def test_savings_public(client: TestClient) -> None:
    """/savings/public is the landing page's counter: unauthenticated,
    aggregated across ALL accounts, and exposes no per-account detail. CORS
    headers let the static www fetch it from the browser (no BFF there)."""
    example = json.loads(CONTRACTS_EXAMPLE.read_text())
    for acct, fill in (("acct-a", "d"), ("acct-b", "e")):
        key = client.post("/keys", headers=auth(mint_jwt(acct))).json()["key"]
        row = dict(
            example,
            request_id="req_" + fill * 32,
            model="claude-sonnet-5",  # 3 / 15 / 0.3 / 3.75 per MTok
            counterfactual_input_tokens=100_000,
            billed_input_tokens=500,
            billed_cache_read_tokens=60_000,
            billed_cache_write_tokens=9_500,
        )
        assert (
            client.post("/ledger", json=row, headers={"X-Parsec-Key": key}).status_code
            == 201
        )

    resp = client.get(
        "/savings/public", headers={"Origin": "https://getparsec.ai"}
    )
    assert resp.status_code == 200
    assert resp.headers["access-control-allow-origin"] == "https://getparsec.ai"
    public = resp.json()
    # Both accounts' savings, valued at the model's blended input-side rate.
    assert public["tokens_saved"] == 60_000
    blended = (500 * 3.0 + 60_000 * 0.3 + 9_500 * 3.75) / 70_000
    assert public["cost_saved_usd"] == pytest.approx(
        round(2 * 30_000 * blended / 1_000_000, 6)
    )
    assert public["measured_rows"] == 2
    assert public["currency"] == "USD"
    # Nothing per-account leaks through the public shape.
    assert "by_model" not in public and "account_id" not in public


def test_ledger_accepts_pre_rename_key_and_header(client: TestClient) -> None:
    """Binaries shipped before the dasein→parsec rename send `X-Dasein-Key`
    with a `dsn_` key. Keys are stored SHA-256-only so they cannot be rewritten
    server-side — both legacy forms must keep authenticating."""
    legacy = "dsn_" + "a" * 32
    client.app.state.store.add_key(hash_key(legacy), ACCOUNT)
    example = json.loads(CONTRACTS_EXAMPLE.read_text())

    assert (
        client.post(
            "/ledger", json=example, headers={"X-Dasein-Key": legacy}
        ).status_code
        == 201
    )
    # The new header with a legacy key, and vice versa, both resolve too.
    row2 = dict(example, request_id="req_" + "e" * 32)
    assert (
        client.post("/ledger", json=row2, headers={"X-Parsec-Key": legacy}).status_code
        == 201
    )
    # A dsn_-shaped key that was never minted is still rejected.
    assert (
        client.post(
            "/ledger", json=example, headers={"X-Dasein-Key": "dsn_unknown"}
        ).status_code
        == 401
    )

    summary = client.get("/ledger/summary", headers=auth(mint_jwt())).json()
    assert summary["rows_count"] == 2


def test_ledger_per_model_cost(client: TestClient) -> None:
    """The seams-bearing row (carries `model`) is stored in `extra` and shows up
    as a per-model, per-cost breakdown — the dashboard's model/cost granularity.
    Cost derives from token sums × the seeded model_pricing (no per-row dollars)."""
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    row = json.loads(CONTRACTS_GOVERNOR_EXAMPLE.read_text())  # model=claude-sonnet-5
    assert (
        client.post("/ledger", json=row, headers={"X-Parsec-Key": key}).status_code
        == 201
    )

    summary = client.get("/ledger/summary", headers=auth(mint_jwt())).json()
    by_model = {m["model"]: m for m in summary["by_model"]}
    assert "claude-sonnet-5" in by_model, summary["by_model"]
    m = by_model["claude-sonnet-5"]

    # tokens_saved is §8.4-honest per model: counterfactual − all billed
    # input-side tokens (uncached + cache read + cache write).
    assert m["tokens_saved"] == (
        row["counterfactual_input_tokens"]
        - row["billed_input_tokens"]
        - row["billed_cache_read_tokens"]
        - row["billed_cache_write_tokens"]
    )
    # cost = Σ(tokens × per-MTok price) / 1e6 at seeded Sonnet-5 rates
    # (3 / 15 / 0.3 / 3.75) — computed at report time, not stored.
    expected = round(
        (2110 * 3.0 + 640 * 15.0 + 27400 * 0.3 + 1980 * 3.75) / 1_000_000, 6
    )
    assert float(m["cost_usd"]) == expected
    assert m["currency"] == "USD"
    # Account-wide total cost (one model here) sums the priced breakdown.
    assert float(summary["cost_usd"]) == expected
    assert summary["currency"] == "USD"

    # Saved tokens are valued at the blended input-side rate this account
    # actually paid for the model — Σ(input-side tokens × price) ÷ Σ(input-side
    # tokens) — not at list input price. Output is excluded: curation removes
    # only input, so the output rate has no bearing on the dropped tokens.
    input_side = 2110 + 27400 + 1980
    blended = (2110 * 3.0 + 27400 * 0.3 + 1980 * 3.75) / input_side
    assert float(m["cost_saved_usd"]) == round(
        m["tokens_saved"] * blended / 1_000_000, 6
    )
    assert float(summary["cost_saved_usd"]) == float(m["cost_saved_usd"])


def test_cost_saved_is_blended_not_list_input_price(client: TestClient) -> None:
    """The dashboard headline (`cost_saved_usd`) must not price saved tokens at
    full list input rate.

    A warm Claude Code session bills almost all input as cache reads at 0.1x, so
    valuing the un-sent tokens at the uncached input rate inflates the headline by
    ~10x. The blend derives the rate from the account's own measured mix, so this
    cache-heavy row lands far below the list-price figure and above the cache-read
    floor."""
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    example = json.loads(CONTRACTS_EXAMPLE.read_text())
    row = dict(
        example,
        request_id="req_" + "b" * 32,
        model="claude-sonnet-5",  # 3 / 15 / 0.3 / 3.75 per MTok
        # Fresh ts: the per-day cross-check needs the row inside the 30-day window.
        ts=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        counterfactual_input_tokens=100_000,
        billed_input_tokens=500,
        billed_cache_read_tokens=60_000,
        billed_cache_write_tokens=9_500,
    )
    assert (
        client.post("/ledger", json=row, headers={"X-Parsec-Key": key}).status_code
        == 201
    )

    summary = client.get("/ledger/summary", headers=auth(mint_jwt())).json()
    saved = summary["tokens_saved"]  # 30_000
    blended = (500 * 3.0 + 60_000 * 0.3 + 9_500 * 3.75) / (500 + 60_000 + 9_500)
    assert float(summary["cost_saved_usd"]) == round(saved * blended / 1_000_000, 6)
    # Bracketed by the two rates we deliberately did not use.
    at_list_input = saved * 3.0 / 1_000_000
    at_cache_read = saved * 0.3 / 1_000_000
    assert at_cache_read < summary["cost_saved_usd"] < at_list_input
    # The per-day series prices savings the same way (separate code path).
    usage = client.get("/ledger/usage", headers=auth(mint_jwt())).json()
    assert sum(d["cost_saved_usd"] for d in usage["days"]) == pytest.approx(
        summary["cost_saved_usd"]
    )


def test_cost_saved_is_null_for_unpriced_model(client: TestClient) -> None:
    """An unknown/stale model string has no pricing row, so its saved dollars are
    a hole — never a fabricated zero (same honesty rule as `cost_usd`), and it
    contributes nothing to the account total."""
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    example = json.loads(CONTRACTS_EXAMPLE.read_text())
    row = dict(
        example,
        request_id="req_" + "c" * 32,
        model="claude-not-a-real-model",
    )
    assert (
        client.post("/ledger", json=row, headers={"X-Parsec-Key": key}).status_code
        == 201
    )

    summary = client.get("/ledger/summary", headers=auth(mint_jwt())).json()
    by_model = {m["model"]: m for m in summary["by_model"]}
    m = by_model["claude-not-a-real-model"]
    assert m["tokens_saved"] > 0  # tokens are measured...
    assert m["cost_saved_usd"] is None  # ...but their dollar value is unknown
    assert m["cost_usd"] is None
    assert summary["cost_saved_usd"] == 0.0


def test_ledger_usage_series(client: TestClient) -> None:
    """The per-day usage series buckets rows by date with tokens + cost, and
    honors the `days` window bound."""
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    row = json.loads(CONTRACTS_GOVERNOR_EXAMPLE.read_text())  # ts 2026-07-10
    assert (
        client.post("/ledger", json=row, headers={"X-Parsec-Key": key}).status_code
        == 201
    )

    usage = client.get(
        "/ledger/usage", params={"days": 365}, headers=auth(mint_jwt())
    ).json()
    assert usage["window_days"] == 365
    assert len(usage["days"]) == 1
    bucket = usage["days"][0]
    assert bucket["date"] == "2026-07-10"
    assert bucket["rows_count"] == 1
    assert bucket["tokens_saved"] == (
        row["counterfactual_input_tokens"]
        - row["billed_input_tokens"]
        - row["billed_cache_read_tokens"]
        - row["billed_cache_write_tokens"]
    )
    assert bucket["cost_usd"] > 0

    # Out-of-range `days` is rejected (Query bounds).
    assert (
        client.get(
            "/ledger/usage", params={"days": 0}, headers=auth(mint_jwt())
        ).status_code
        == 422
    )


def test_jwks_verification(monkeypatch: pytest.MonkeyPatch) -> None:
    """The asymmetric path (SUPABASE_JWKS_URL): ES256 tokens verify against
    the JWKS signing key; HS256 tokens (legacy/forged alg) are rejected. The
    JWKS client is stubbed — no network in tests."""
    ec = pytest.importorskip("cryptography.hazmat.primitives.asymmetric.ec")

    from parsec_platform import auth as auth_mod

    private_key = ec.generate_private_key(ec.SECP256R1())

    class StubSigningKey:
        key = private_key.public_key()

    class StubJWKSClient:
        def get_signing_key_from_jwt(self, token: str) -> StubSigningKey:
            return StubSigningKey()

    url = "https://stub.supabase.test/auth/v1/.well-known/jwks.json"
    monkeypatch.setenv("SUPABASE_JWKS_URL", url)
    monkeypatch.setenv("STRIPE_WEBHOOK_SECRET", STRIPE_SECRET)
    monkeypatch.delenv("SUPABASE_JWT_SECRET", raising=False)
    monkeypatch.setitem(auth_mod._jwks_clients, url, StubJWKSClient())
    client = TestClient(create_app(store=SQLiteStore(":memory:")))

    claims = {"sub": ACCOUNT, "aud": "authenticated", "exp": int(time.time()) + 3600}
    good = jwt.encode(claims, private_key, algorithm="ES256")
    assert client.post("/keys", headers=auth(good)).status_code == 201

    hs256 = jwt.encode(claims, "some-shared-secret", algorithm="HS256")
    assert client.post("/keys", headers=auth(hs256)).status_code == 401


# ── PostgresStore (opt-in: needs a reachable Postgres) ──────────────────────
# Run with e.g. a local container:
#   docker run --rm -d -p 5433:5432 -e POSTGRES_PASSWORD=pg postgres:16
#   TEST_POSTGRES_URL=postgresql://postgres:pg@127.0.0.1:5433/postgres \
#     pytest packages/platform -k postgres


@pytest.mark.skipif(
    "TEST_POSTGRES_URL" not in os.environ,
    reason="needs TEST_POSTGRES_URL (see comment above)",
)
def test_postgres_store_roundtrip() -> None:
    from parsec_platform.pgstore import PostgresStore

    store = PostgresStore(os.environ["TEST_POSTGRES_URL"])
    migration = (
        Path(__file__).resolve().parents[1] / "migrations" / "0001_init.sql"
    ).read_text()
    with store._pool.connection() as conn:
        conn.execute(migration)

    suffix = uuid.uuid4().hex[:8]
    account = f"acct-pg-{suffix}"
    try:
        # entitlement upsert both ways
        assert store.is_entitled(account) is False
        store.set_entitlement(account, True)
        assert store.is_entitled(account) is True
        store.set_entitlement(account, False)
        assert store.is_entitled(account) is False

        # keys + customer mapping
        store.add_key(f"hash-{suffix}", account)
        assert store.account_for_key(f"hash-{suffix}") == account
        assert store.account_for_key("hash-missing") is None
        store.link_customer(f"cus_{suffix}", account)
        assert store.account_for_customer(f"cus_{suffix}") == account

        # ledger: idempotent insert, NULL-probe rows excluded from savings
        example = json.loads(CONTRACTS_EXAMPLE.read_text())
        rid = "req_" + uuid.uuid4().hex
        row = dict(example, request_id=rid)
        store.add_ledger_row(account, row)
        store.add_ledger_row(account, row)  # duplicate — must not raise
        hole = dict(
            example,
            request_id="req_" + uuid.uuid4().hex,
            counterfactual_input_tokens=None,
            fail_open=True,
        )
        store.add_ledger_row(account, hole)
        s = store.ledger_summary(account)
        assert s["rows_count"] == 2
        assert s["measured_rows"] == 1
        assert s["fail_open_count"] == 1
        # Only the example row is measured (measured_rows == 1), so the
        # expectation is that row's counterfactual minus its billed input side.
        assert s["tokens_saved"] == (
            example["counterfactual_input_tokens"]
            - example["billed_input_tokens"]
            - example["billed_cache_read_tokens"]
            - example["billed_cache_write_tokens"]
        )
    finally:
        with store._pool.connection() as conn:
            conn.execute("DELETE FROM ledger WHERE account_id = %s", (account,))
            conn.execute("DELETE FROM api_keys WHERE account_id = %s", (account,))
            conn.execute(
                "DELETE FROM stripe_customers WHERE account_id = %s", (account,)
            )
            conn.execute("DELETE FROM entitlements WHERE account_id = %s", (account,))
        store.close()
