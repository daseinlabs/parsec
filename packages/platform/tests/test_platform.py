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
from parsec_platform.models import InstallReport, LedgerRow
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
# The authoritative schema itself — compared field-for-field against the
# pydantic mirror, so a new proxy field cannot reach ingest unmirrored.
CONTRACTS_SCHEMA = (
    CONTRACTS_EXAMPLE.parents[1] / "savings-ledger.schema.json"
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
    assert row.counterfactual_source == "local_bpe"
    assert row.freeze_cut_roles == example["freeze_cut_roles"]
    assert row.freeze_cut_protected_tokens == example["freeze_cut_protected_tokens"]


def test_schema_and_mirror_have_identical_field_sets() -> None:
    """Structural drift guard: every schema property must exist in the mirror
    and vice versa.

    The example-based guards above only catch a field the example happens to
    carry, so they pass VACUOUSLY for a field nobody remembered to add — which
    is exactly how counterfactual_source, freeze_cut_roles and
    freeze_cut_protected_tokens shipped from the proxy while both mirrors were
    unaware of them, 422-ing every curated row at ingest. This compares the
    sets directly, so the next such field fails here instead of in production.
    """
    schema = json.loads(CONTRACTS_SCHEMA.read_text())
    schema_props = set(schema["properties"])
    mirror_props = {
        f.alias or name for name, f in LedgerRow.model_fields.items()
    }
    assert schema_props == mirror_props, (
        f"schema-only: {sorted(schema_props - mirror_props)} · "
        f"mirror-only: {sorted(mirror_props - schema_props)}"
    )


def test_governor_example_covers_every_optional_schema_field() -> None:
    """The seams example must exercise every optional field, or the guards
    that validate it silently stop covering the ones it omits."""
    schema = json.loads(CONTRACTS_SCHEMA.read_text())
    optional = set(schema["properties"]) - set(schema.get("required", []))
    example = json.loads(CONTRACTS_GOVERNOR_EXAMPLE.read_text())
    missing = optional - set(example)
    assert not missing, f"governor example does not exercise: {sorted(missing)}"


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
    keys: dict[str, str] = {}
    for acct, fill in (("acct-a", "d"), ("acct-b", "e")):
        keys[acct] = client.post("/keys", headers=auth(mint_jwt(acct))).json()["key"]
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
            client.post(
                "/ledger", json=row, headers={"X-Parsec-Key": keys[acct]}
            ).status_code
            == 201
        )
    # A failed-probe row is a hole (§8.4): counted, but adds no savings.
    hole = dict(
        example,
        request_id="req_" + "f" * 32,
        model="claude-sonnet-5",
        counterfactual_input_tokens=None,
        fail_open=True,
    )
    assert (
        client.post(
            "/ledger", json=hole, headers={"X-Parsec-Key": keys["acct-a"]}
        ).status_code
        == 201
    )
    # A row with no model captured: its tokens count, its cost is a hole.
    # (The contracts example itself carries no model field.)
    modelless = dict(example, request_id="req_" + "9" * 32)
    assert (
        client.post(
            "/ledger", json=modelless, headers={"X-Parsec-Key": keys["acct-b"]}
        ).status_code
        == 201
    )
    modelless_saved = example["counterfactual_input_tokens"] - (
        example["billed_input_tokens"]
        + example["billed_cache_read_tokens"]
        + example["billed_cache_write_tokens"]
    )

    resp = client.get(
        "/savings/public", headers={"Origin": "https://getparsec.ai"}
    )
    assert resp.status_code == 200
    assert resp.headers["access-control-allow-origin"] == "https://getparsec.ai"
    public = resp.json()
    # Both accounts' savings, valued at the model's blended input-side rate;
    # the modelless row adds tokens but no cost (unpriced = hole, never zero).
    assert public["tokens_saved"] == 60_000 + modelless_saved
    # The hole row's billed tokens still shape the model's blended rate (it
    # billed real input; only its savings are unmeasured) — same as the old
    # full-ledger scan.
    bi = 2 * 500 + example["billed_input_tokens"]
    br = 2 * 60_000 + example["billed_cache_read_tokens"]
    bw = 2 * 9_500 + example["billed_cache_write_tokens"]
    blended = (bi * 3.0 + br * 0.3 + bw * 3.75) / (bi + br + bw)
    assert public["cost_saved_usd"] == pytest.approx(
        round(60_000 * blended / 1_000_000, 6)
    )
    assert public["measured_rows"] == 3
    assert public["currency"] == "USD"
    # Nothing per-account leaks through the public shape.
    assert "by_model" not in public and "account_id" not in public


_ROLLUP_RECOMPUTE_SQL = (
    # The pre-rollup full-ledger aggregation, at the rollup's (account, model)
    # grain — the §8.4 ground truth the rollup must always equal.
    "SELECT account_id, COALESCE(json_extract(extra, '$.model'), '') AS model, "
    "COUNT(*), COUNT(counterfactual_input_tokens), "
    "COALESCE(SUM(CASE WHEN counterfactual_input_tokens IS NOT NULL "
    "  THEN counterfactual_input_tokens - (billed_input_tokens "
    "    + billed_cache_read_tokens + billed_cache_write_tokens) END), 0), "
    "COALESCE(SUM(counterfactual_input_tokens), 0), "
    "COALESCE(SUM(billed_input_tokens), 0), "
    "COALESCE(SUM(billed_output_tokens), 0), "
    "COALESCE(SUM(billed_cache_read_tokens), 0), "
    "COALESCE(SUM(billed_cache_write_tokens), 0), "
    "COALESCE(SUM(fail_open), 0) "
    "FROM ledger "
    "GROUP BY account_id, COALESCE(json_extract(extra, '$.model'), '') "
    "ORDER BY account_id, model"
)


def _assert_rollup_matches_ledger(store: SQLiteStore) -> None:
    rollup = store._conn.execute(
        "SELECT account_id, model, rows_count, measured_rows, tokens_saved, "
        "counterfactual_input_tokens, billed_input_tokens, billed_output_tokens, "
        "billed_cache_read_tokens, billed_cache_write_tokens, fail_open_count "
        "FROM savings_rollup ORDER BY account_id, model"
    ).fetchall()
    # A fully-deleted (account, model) leaves an all-zero rollup row where the
    # recompute has none — harmless, but any *non-zero* counter there is drift.
    live = [t for t in (tuple(r) for r in rollup) if any(t[2:])]
    recomputed = [tuple(r) for r in store._conn.execute(_ROLLUP_RECOMPUTE_SQL)]
    assert live == recomputed


def test_savings_rollup_matches_ledger_recompute(tmp_path: Path) -> None:
    """savings_rollup is derived state: §8.4 honesty holds only if it stays
    byte-equal to a from-scratch recompute of the ledger. Exercise every
    trigger path (insert with model / without model / NULL probe, delete) and
    the init-time backfill of a pre-rollup DB file."""
    db = tmp_path / "platform.db"
    store = SQLiteStore(str(db))
    example = json.loads(CONTRACTS_EXAMPLE.read_text())
    rows = [
        ("acct-a", dict(example, request_id="req_" + "1" * 32, model="claude-sonnet-5")),
        ("acct-a", dict(example, request_id="req_" + "2" * 32, model="claude-haiku-4-5")),
        ("acct-b", dict(example, request_id="req_" + "3" * 32, model="claude-sonnet-5")),
        # NULL probe: a hole — rows_count only.
        (
            "acct-b",
            dict(
                example,
                request_id="req_" + "4" * 32,
                model="claude-sonnet-5",
                counterfactual_input_tokens=None,
                fail_open=True,
            ),
        ),
        # No model captured (the contracts example carries no model field).
        ("acct-b", dict(example, request_id="req_" + "5" * 32)),
    ]
    for acct, row in rows:
        store.add_ledger_row(acct, LedgerRow.model_validate(row).model_dump())
    # Idempotent re-send: no double count.
    store.add_ledger_row("acct-a", LedgerRow.model_validate(rows[0][1]).model_dump())
    _assert_rollup_matches_ledger(store)

    # Delete trigger keeps the rollup exact when ledger rows are removed.
    store._conn.execute("DELETE FROM ledger WHERE request_id = ?", ("req_" + "2" * 32,))
    store._conn.commit()
    _assert_rollup_matches_ledger(store)

    # Backfill: a DB file from before the rollup existed (simulated by
    # emptying it) is reconstructed exactly on the next open.
    store._conn.execute("DELETE FROM savings_rollup")
    store._conn.commit()
    store._conn.close()
    reopened = SQLiteStore(str(db))
    _assert_rollup_matches_ledger(reopened)
    # And the public read comes from the rollup alone.
    assert reopened.savings_public()["measured_rows"] == 3


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


def test_openai_models_are_priced(client: TestClient) -> None:
    """Codex/ChatGPT traffic ships OpenAI model strings; the seed must price
    them so their savings are dollars, not a hole (blended the same way as
    Claude rows)."""
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    example = json.loads(CONTRACTS_EXAMPLE.read_text())
    row = dict(
        example,
        request_id="req_" + "d" * 32,
        model="gpt-5.6-terra",  # 2 / 12 / 0.2 / 2.0 per MTok
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
    m = {r["model"]: r for r in summary["by_model"]}["gpt-5.6-terra"]
    saved = m["tokens_saved"]  # 30_000
    # cache_write blends at the BASE input rate: OpenAI has no cache-write
    # surcharge (0007_openai_pricing_correction.sql).
    blended = (500 * 2.0 + 60_000 * 0.2 + 9_500 * 2.0) / (500 + 60_000 + 9_500)
    assert float(m["cost_saved_usd"]) == round(saved * blended / 1_000_000, 6)
    assert m["cost_usd"] is not None


def test_pricing_seed_matches_the_migrations() -> None:
    """`_PRICING_SEED` and migrations/*.sql are two hand-maintained copies of
    the same table, kept together by a comment. Replay the migrations'
    model_pricing statements against SQLite (same DDL, and every statement
    involved is portable SQL) and diff the result against the seed, so a rate
    corrected in one place can't silently stay wrong in the other.

    This is the check that would have caught gpt-5.6-sol drifting: 0004 seeded
    it, 0007 corrected it, and nothing but a comment tied either to store.py."""
    import re
    import sqlite3

    from parsec_platform.store import _PRICING_SEED, _SCHEMA

    conn = sqlite3.connect(":memory:")
    conn.executescript(
        next(
            m.group(0)
            for m in re.finditer(
                r"CREATE TABLE IF NOT EXISTS model_pricing.*?\);", _SCHEMA, re.S
            )
        )
    )
    migrations = Path(__file__).resolve().parents[1] / "migrations"
    applied = 0
    for path in sorted(migrations.glob("*.sql")):
        body = path.read_text()
        # Strip comments, then take whole statements that touch model_pricing.
        body = "\n".join(l for l in body.splitlines() if not l.lstrip().startswith("--"))
        for stmt in body.split(";"):
            head = stmt.strip().upper()
            if not head.startswith(("INSERT INTO MODEL_PRICING", "UPDATE MODEL_PRICING")):
                continue
            conn.execute(stmt)
            applied += 1
    assert applied, "no model_pricing statements found — the parser drifted"

    from_migrations = {
        row[0]: tuple(round(float(v), 6) for v in row[1:])
        for row in conn.execute(
            "SELECT model, input_per_mtok, output_per_mtok, cache_read_per_mtok, "
            "cache_write_per_mtok FROM model_pricing"
        )
    }
    from_seed = {
        r[0]: tuple(round(float(v), 6) for v in r[1:]) for r in _PRICING_SEED
    }
    assert from_migrations == from_seed, (
        "store.py _PRICING_SEED and migrations/*.sql disagree; "
        f"only in migrations: {set(from_migrations) - set(from_seed)}; "
        f"only in seed: {set(from_seed) - set(from_migrations)}; "
        "differing rates: "
        + repr(
            {
                k: (from_migrations[k], from_seed[k])
                for k in set(from_migrations) & set(from_seed)
                if from_migrations[k] != from_seed[k]
            }
        )
    )


def test_gemini_models_are_priced(client: TestClient) -> None:
    """Gemini traffic ships Google model strings; the seed must price them so
    their savings are dollars, not a hole (blended the same way as Claude
    rows). Gemini has no cache-write surcharge, so write tokens blend at the
    base input rate."""
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    example = json.loads(CONTRACTS_EXAMPLE.read_text())
    row = dict(
        example,
        request_id="req_" + "e" * 32,
        model="gemini-3.5-flash",  # 1.5 / 9 / 0.15 / 1.5 per MTok
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
    m = {r["model"]: r for r in summary["by_model"]}["gemini-3.5-flash"]
    saved = m["tokens_saved"]  # 30_000
    blended = (500 * 1.5 + 60_000 * 0.15 + 9_500 * 1.5) / (500 + 60_000 + 9_500)
    assert float(m["cost_saved_usd"]) == round(saved * blended / 1_000_000, 6)
    assert m["cost_usd"] is not None


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
    migrations = Path(__file__).resolve().parents[1] / "migrations"
    with store._pool.connection() as conn:
        for path in sorted(migrations.glob("*.sql")):
            conn.execute(path.read_text())

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
        # Normalize through the contract model as the /ledger route does — the
        # raw example uses the camelCase alias (cachePrefixSha8).
        row = LedgerRow.model_validate(dict(example, request_id=rid)).model_dump()
        store.add_ledger_row(account, row)
        store.add_ledger_row(account, row)  # duplicate — must not raise
        hole = LedgerRow.model_validate(
            dict(
                example,
                request_id="req_" + uuid.uuid4().hex,
                counterfactual_input_tokens=None,
                fail_open=True,
            )
        ).model_dump()
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

        # The trigger-maintained rollup carries this account's rows at
        # (account, model) grain — same counters the ledger scan produced.
        # (The example rows carry no model field, so they land under ''.)
        with store._pool.connection() as conn:
            rollup = conn.execute(
                "SELECT model, rows_count, measured_rows, tokens_saved, "
                "fail_open_count FROM savings_rollup WHERE account_id = %s",
                (account,),
            ).fetchall()
        assert rollup == [("", 2, 1, s["tokens_saved"], 1)]
        # savings_public reads the rollup (site-wide, so only >= this account).
        assert store.savings_public()["tokens_saved"] >= s["tokens_saved"]
    finally:
        with store._pool.connection() as conn:
            # The ledger delete fires the rollup trigger back to zero; the row
            # itself is removed with the other per-account fixtures.
            conn.execute("DELETE FROM ledger WHERE account_id = %s", (account,))
            conn.execute(
                "DELETE FROM savings_rollup WHERE account_id = %s", (account,)
            )
            conn.execute("DELETE FROM api_keys WHERE account_id = %s", (account,))
            conn.execute(
                "DELETE FROM stripe_customers WHERE account_id = %s", (account,)
            )
            conn.execute("DELETE FROM entitlements WHERE account_id = %s", (account,))
        store.close()


# ── install registration (contracts/schemas/install-report.schema.json) ──────

INSTALL_EXAMPLE = CONTRACTS_EXAMPLE.parent / "install-report.example.json"
INSTALL_SCHEMA = CONTRACTS_SCHEMA.parent / "install-report.schema.json"


def _install_report(**overrides) -> dict:
    """The committed example with a fresh ts (activity windows are measured
    against real time, so a static example date would rot)."""
    report = json.loads(INSTALL_EXAMPLE.read_text())
    report["ts"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    report.update(overrides)
    return report


def test_install_report_mirror_accepts_contracts_example() -> None:
    """Same drift guard as the ledger: the pydantic mirror must accept the
    schema's committed example."""
    example = json.loads(INSTALL_EXAMPLE.read_text())
    report = InstallReport.model_validate(example)
    assert report.install_id == example["install_id"]


def test_install_schema_and_mirror_have_identical_field_sets() -> None:
    """Structural drift guard, ledger-style: a schema field the mirror lacks
    422s every ping; a mirror field the schema lacks never validates."""
    schema = json.loads(INSTALL_SCHEMA.read_text())
    assert set(schema["properties"]) == set(InstallReport.model_fields)


def test_install_register_upsert_link_and_summary(client: TestClient) -> None:
    # Keyless ping accepted: an install exists before onboarding.
    assert client.post("/installs", json=_install_report()).status_code == 201
    # Unknown key degrades to anonymous, never 401 — registration is fail-open.
    assert (
        client.post(
            "/installs",
            json=_install_report(),
            headers={"X-Parsec-Key": "psc_unknown"},
        ).status_code
        == 201
    )
    # A keyed re-ping of the SAME install links the account and refreshes
    # metadata (upsert on install_id, not a second row).
    key = client.post("/keys", headers=auth(mint_jwt())).json()["key"]
    assert (
        client.post(
            "/installs",
            json=_install_report(version="0.3.0"),
            headers={"X-Parsec-Key": key},
        ).status_code
        == 201
    )
    # A different machine, long inactive, different harness set.
    assert (
        client.post(
            "/installs",
            json=_install_report(
                install_id="ins_" + "f" * 32,
                harnesses=["codex"],
                ts="2020-01-01T00:00:00Z",
            ),
        ).status_code
        == 201
    )

    assert client.get("/installs/summary").status_code == 401  # JWT-gated
    summary = client.get("/installs/summary", headers=auth(mint_jwt())).json()
    assert summary["installs_total"] == 2
    assert summary["linked_accounts"] == 1
    assert summary["by_version"] == {"0.3.0": 1, "0.2.6": 1}
    assert summary["by_os"] == {"macos": 2}
    assert summary["by_harness"] == {"claude-code": 1, "claude-desktop": 1, "codex": 1}
    assert summary["active_7d"] == 1  # the 2020 install is not active
    assert summary["active_30d"] == 1
    # Aggregate-only: no ids leave the fold.
    assert "install_id" not in json.dumps(summary)
    assert ACCOUNT not in json.dumps(summary)

    # A later keyless ping must NOT unlink the account (COALESCE upsert).
    assert client.post("/installs", json=_install_report()).status_code == 201
    summary = client.get("/installs/summary", headers=auth(mint_jwt())).json()
    assert summary["linked_accounts"] == 1


def test_install_report_is_pattern_gated(client: TestClient) -> None:
    """Free text is unrepresentable on the (unauthenticated) install path —
    the same contract-level guarantee as the ledger's tool/session_id."""
    for bad in (
        _install_report(os="Mac OS X!"),
        _install_report(install_id="ins_not-hex"),
        _install_report(harnesses=["Claude Desktop (roaming)!"]),
        _install_report(sneaky="raw text"),
    ):
        assert client.post("/installs", json=bad).status_code == 422
