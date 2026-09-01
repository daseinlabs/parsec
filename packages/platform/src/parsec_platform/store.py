"""Storage seam for the platform service.

DIRECTION.md §7c picks Supabase Postgres as the v1 database for accounts,
entitlements, and savings-ledger rows. This module is the seam that makes that
a swap, not a rewrite: routes talk only to the `Store` interface; `SQLiteStore`
is the zero-infrastructure default (path from PARSEC_PLATFORM_DB, in-memory if
unset) and a Postgres-backed implementation replaces it later without touching
the routers. Deliberately no ORM and no async driver — §7c says thin.
"""

from __future__ import annotations

import abc
import json
import os
import sqlite3
import threading
import time
from datetime import datetime
from typing import Any

# Core ledger columns (typed, aggregated/billed on) — everything else in a row
# lands in the `extra` JSONB/JSON blob so new capture/governor seams need no DDL.
# `contract_version` is the constant discriminator, not telemetry, so it's core.
_CORE_LEDGER_KEYS = frozenset(
    {
        "contract_version",
        "request_id",
        "conv_id",
        "ts",
        "counterfactual_input_tokens",
        "billed_input_tokens",
        "billed_output_tokens",
        "billed_cache_read_tokens",
        "billed_cache_write_tokens",
        "cache_prefix_sha8",
        "fail_open",
    }
)


def extra_fields(row: dict[str, Any]) -> dict[str, Any]:
    """The non-core keys of a ledger row — model, session_id, brain_ms, tools_*,
    gov_*, … — persisted verbatim in the `extra` column (account_id is passed
    separately and never appears in `row`)."""
    return {k: v for k, v in row.items() if k not in _CORE_LEDGER_KEYS}


def cost_usd(
    sums: tuple[int, int, int, int],
    prices: tuple[float | None, float | None, float | None, float | None] | None,
) -> float | None:
    """Dollar cost from per-type token sums (input, output, cache_read,
    cache_write) and their per-MTok prices. None when pricing is unknown for the
    model — cost is a hole, never a fabricated zero (same honesty rule as the
    NULL counterfactual)."""
    if prices is None or any(p is None for p in prices):
        return None
    bi, bo, br, bw = sums
    # float() each price: Postgres NUMERIC columns arrive as Decimal, and a
    # Decimal result would JSON-serialize as a string instead of a number.
    pi, po, pr, pw = (float(p) for p in prices)  # type: ignore[arg-type]
    return round((bi * pi + bo * po + br * pr + bw * pw) / 1_000_000, 6)


def cost_saved_usd(
    tokens_saved: int,
    sums: tuple[int, int, int, int],
    prices: tuple[float | None, float | None, float | None, float | None] | None,
) -> float | None:
    """Dollar value of `tokens_saved`, priced at the *blended input-side rate the
    account actually paid* for this model.

    Saved tokens were never sent, so no ledger row says what they would have been
    billed as. The three candidate rates differ by 10x, so the choice IS the
    number: full input price overstates warm Claude Code sessions (which bill
    nearly all input as cache reads at 0.1x), and the cache-read floor understates
    cold traffic. The blend — Σ(billed input-side tokens × their price) ÷ Σ(billed
    input-side tokens) — is self-calibrating: a cache-heavy account gets a
    cache-weighted rate, a cold one lands near list input price, and it derives
    entirely from that account's own measured mix rather than an assumed one.

    Output tokens are excluded: curation only removes input, so output price has
    no bearing on what the dropped tokens would have cost. None when the model is
    unpriced (a hole, never a fabricated zero — same rule as `cost_usd`)."""
    if prices is None or any(p is None for p in prices):
        return None
    bi, _bo, br, bw = sums
    # float() for the same Decimal-vs-JSON reason as cost_usd.
    pi, _po, pr, pw = (float(p) for p in prices)  # type: ignore[arg-type]
    input_side = bi + br + bw
    # No input-side tokens billed at all ⇒ no observed mix to blend. Nothing was
    # served from cache, so list input price is the honest rate.
    rate = (bi * pi + br * pr + bw * pw) / input_side if input_side else pi
    return round(tokens_saved * rate / 1_000_000, 6)


def _model_row(r: dict[str, Any]) -> dict[str, Any]:
    """Shape one grouped row (token sums + joined price columns, keys as named in
    both stores' by-model queries) into the API's per-model entry, with cost_usd
    and cost_saved_usd computed from the four price columns (None when the model
    is unpriced)."""
    sums = (
        int(r["billed_input_tokens"]),
        int(r["billed_output_tokens"]),
        int(r["billed_cache_read_tokens"]),
        int(r["billed_cache_write_tokens"]),
    )
    prices = (
        r["input_per_mtok"],
        r["output_per_mtok"],
        r["cache_read_per_mtok"],
        r["cache_write_per_mtok"],
    )
    tokens_saved = int(r["tokens_saved"])
    return {
        "model": r["model"],  # None for rows with no model captured
        "rows_count": int(r["rows_count"]),
        "measured_rows": int(r["measured_rows"]),
        "tokens_saved": tokens_saved,
        "billed_input_tokens": sums[0],
        "billed_output_tokens": sums[1],
        "billed_cache_read_tokens": sums[2],
        "billed_cache_write_tokens": sums[3],
        "cost_usd": cost_usd(sums, prices),
        "cost_saved_usd": cost_saved_usd(tokens_saved, sums, prices),
        "currency": r.get("currency") or "USD",
    }


# Site-wide by-model totals from the (account, model) rollup, shaped for
# fold_public. Plain SQL shared verbatim by both stores: NULLIF undoes the ''
# sentinel the rollup uses for "no model captured" so the pricing join and the
# API shape see a real NULL.
_SAVINGS_PUBLIC_SQL = (
    "SELECT NULLIF(r.model, '') AS model, "
    "SUM(r.rows_count) AS rows_count, "
    "SUM(r.measured_rows) AS measured_rows, "
    "SUM(r.tokens_saved) AS tokens_saved, "
    "SUM(r.billed_input_tokens) AS billed_input_tokens, "
    "SUM(r.billed_output_tokens) AS billed_output_tokens, "
    "SUM(r.billed_cache_read_tokens) AS billed_cache_read_tokens, "
    "SUM(r.billed_cache_write_tokens) AS billed_cache_write_tokens, "
    "p.input_per_mtok, p.output_per_mtok, p.cache_read_per_mtok, "
    "p.cache_write_per_mtok, p.currency "
    "FROM savings_rollup r "
    "LEFT JOIN model_pricing p ON p.model = r.model "
    "GROUP BY r.model, p.input_per_mtok, p.output_per_mtok, "
    "p.cache_read_per_mtok, p.cache_write_per_mtok, p.currency"
)


def fold_public(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """Roll by-model grouped rows (same column shape as the by-model queries)
    up to the site-wide public totals. Savings are valued per model at its own
    blended input-side rate before summing (same rule as `fold_daily`);
    unpriced models add tokens but no cost — a hole, never a fabricated zero."""
    shaped = [_model_row(r) for r in rows]
    return {
        "rows_count": sum(m["rows_count"] for m in shaped),
        "measured_rows": sum(m["measured_rows"] for m in shaped),
        "tokens_saved": sum(m["tokens_saved"] for m in shaped),
        "cost_saved_usd": round(
            sum(
                m["cost_saved_usd"]
                for m in shaped
                if m["cost_saved_usd"] is not None
            ),
            6,
        ),
    }


def _seen_epoch(v: Any) -> float:
    """last_seen as a Unix timestamp — SQLite hands back the RFC 3339 text the
    client sent, Postgres a datetime. Unparseable → 0.0 (counts as inactive,
    never crashes the summary)."""
    if isinstance(v, datetime):
        return v.timestamp()
    try:
        return datetime.fromisoformat(str(v).replace("Z", "+00:00")).timestamp()
    except ValueError:
        return 0.0


def fold_installs(rows: list[dict[str, Any]], now: float | None = None) -> dict[str, Any]:
    """Fleet summary over raw installs rows: totals, activity windows keyed on
    last_seen (refreshed by setup/key/proxy-start pings, at most daily per
    install), and by-version/os/harness spreads. Aggregate-only by design —
    no install_id or account_id leaves this fold. Python-side rather than SQL
    so SQLite and Postgres share one definition; the installs table is small
    (one row per machine)."""
    now = time.time() if now is None else now
    by_version: dict[str, int] = {}
    by_os: dict[str, int] = {}
    by_harness: dict[str, int] = {}
    active_7d = active_30d = 0
    linked: set[str] = set()
    for r in rows:
        by_version[r["version"]] = by_version.get(r["version"], 0) + 1
        by_os[r["os"]] = by_os.get(r["os"], 0) + 1
        harnesses = r["harnesses"]
        if isinstance(harnesses, str):  # SQLite stores the JSON array as text
            harnesses = json.loads(harnesses or "[]")
        for h in harnesses:
            by_harness[h] = by_harness.get(h, 0) + 1
        age = now - _seen_epoch(r["last_seen"])
        if age <= 7 * 86400:
            active_7d += 1
        if age <= 30 * 86400:
            active_30d += 1
        if r["account_id"]:
            linked.add(r["account_id"])
    return {
        "installs_total": len(rows),
        "active_7d": active_7d,
        "active_30d": active_30d,
        "linked_accounts": len(linked),
        "by_version": by_version,
        "by_os": by_os,
        "by_harness": by_harness,
    }


def fold_daily(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Roll per-(day, model) grouped rows (same column shape as the by-model
    query, plus a `day`) up to one bucket per day: summed tokens + cost, oldest
    day first. Cost is null-aware — a day's unpriced-model tokens add no cost but
    don't erase the priced portion (the model seed covers what the client sends,
    so unpriced is only stale/unknown model strings). Savings are valued per
    (day, model) *before* summing, so each model's own blended input-side rate
    applies — blending across models would price one model's saved tokens at
    another's rate."""
    days: dict[str, dict[str, Any]] = {}
    token_cols = (
        "billed_input_tokens",
        "billed_output_tokens",
        "billed_cache_read_tokens",
        "billed_cache_write_tokens",
    )
    for r in rows:
        day = str(r["day"])  # SQLite: 'YYYY-MM-DD' text; Postgres: a date object
        b = days.setdefault(
            day,
            {
                "date": day,
                "rows_count": 0,
                "measured_rows": 0,
                "tokens_saved": 0,
                **{c: 0 for c in token_cols},
                "cost_usd": 0.0,
                "cost_saved_usd": 0.0,
            },
        )
        b["rows_count"] += int(r["rows_count"])
        b["measured_rows"] += int(r["measured_rows"])
        tokens_saved = int(r["tokens_saved"])
        b["tokens_saved"] += tokens_saved
        for c in token_cols:
            b[c] += int(r[c])
        sums: tuple[int, int, int, int] = tuple(int(r[k]) for k in token_cols)  # type: ignore[assignment]
        prices = (
            r["input_per_mtok"],
            r["output_per_mtok"],
            r["cache_read_per_mtok"],
            r["cache_write_per_mtok"],
        )
        c = cost_usd(sums, prices)
        if c is not None:
            b["cost_usd"] += float(c)
        s = cost_saved_usd(tokens_saved, sums, prices)
        if s is not None:
            b["cost_saved_usd"] += float(s)
    out = sorted(days.values(), key=lambda x: x["date"])
    for b in out:
        b["cost_usd"] = round(b["cost_usd"], 6)
        b["cost_saved_usd"] = round(b["cost_saved_usd"], 6)
    return out


# Provider list pricing, USD per million tokens.
# Anthropic: cache_write = 5-minute TTL = 1.25x input; 1-hour TTL would be 2x —
# the ledger stores a single billed_cache_write_tokens and can't distinguish
# TTL, so the 5m default is used.
# OpenAI (Codex/ChatGPT traffic): cache_read = 0.1x input, and cache_write =
# the base input rate because OpenAI has NO cache-write charge — its pricing
# table is input / cached input / output only. Rates verified against
# developers.openai.com/api/docs/pricing on 2026-08-24; the short-context tier,
# since the long-context tier bills higher and a single flat row can't express
# a tier break, so long-context spend is undervalued, never overvalued.
# Gemini: cache_read = 0.1x input; no per-token cache-write surcharge (explicit
# caching bills hourly storage, not representable here), so cache_write = the
# base input rate. Pro rows are the ≤200K tier; 3.7/3.6-flash rates are
# promotional through 2026-12-31.
# KEEP IN SYNC with migrations/0002_*.sql + 0004_*.sql + 0005_*.sql +
# 0006_*.sql + 0007_*.sql model_pricing seeds. NB the seed is applied with
# INSERT OR IGNORE, so a corrected rate only reaches a SQLite DB created after
# the change; Postgres gets it from the migration's UPDATE.
_PRICING_SEED = (
    # model,               input, output, cache_read, cache_write
    ("claude-fable-5", 10.0, 50.0, 1.0, 12.5),
    ("claude-opus-5", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-8", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-7", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-6", 5.0, 25.0, 0.5, 6.25),
    ("claude-sonnet-5", 3.0, 15.0, 0.3, 3.75),
    ("claude-sonnet-4-6", 3.0, 15.0, 0.3, 3.75),
    ("claude-haiku-4-5", 1.0, 5.0, 0.1, 1.25),
    ("claude-haiku-4-5-20251001", 1.0, 5.0, 0.1, 1.25),
    ("gpt-5.6-cyber", 12.5, 75.0, 1.25, 12.5),
    ("gpt-5.6-sol", 4.0, 20.0, 0.4, 4.0),
    ("gpt-5.6-terra", 2.0, 12.0, 0.2, 2.0),
    ("gpt-5.6-luna", 0.2, 1.2, 0.02, 0.2),
    ("gpt-5.5", 5.0, 30.0, 0.5, 5.0),
    ("gpt-5.4-mini", 0.75, 4.5, 0.075, 0.75),
    ("gpt-5.4-nano", 0.2, 1.25, 0.02, 0.2),
    ("gpt-5.3-codex", 1.75, 14.0, 0.175, 1.75),
    ("gpt-5.2-codex", 1.75, 14.0, 0.175, 1.75),
    ("gpt-5-codex", 1.25, 10.0, 0.125, 1.25),
    # gpt-5-pro publishes no cached-input rate — no discount, not a guess.
    ("gpt-5-pro", 15.0, 120.0, 15.0, 15.0),
    ("gpt-5", 1.25, 10.0, 0.125, 1.25),
    ("gpt-5-mini", 0.25, 2.0, 0.025, 0.25),
    ("gpt-5-nano", 0.05, 0.4, 0.005, 0.05),
    ("gemini-3.7-flash", 0.75, 3.75, 0.075, 0.75),
    ("gemini-3.6-flash", 0.75, 3.75, 0.075, 0.75),
    ("gemini-3.5-flash", 1.5, 9.0, 0.15, 1.5),
    ("gemini-3.5-flash-lite", 0.3, 2.5, 0.03, 0.3),
    ("gemini-3.1-pro-preview", 2.0, 12.0, 0.2, 2.0),
    ("gemini-3.1-flash-lite", 0.25, 1.5, 0.025, 0.25),
    ("gemini-2.5-pro", 1.25, 10.0, 0.125, 1.25),
    ("gemini-2.5-flash", 0.3, 2.5, 0.03, 0.3),
    ("gemini-2.5-flash-lite", 0.1, 0.4, 0.01, 0.1),
    ("gemini-flash", 0.75, 3.75, 0.075, 0.75),
)

_SCHEMA = """
CREATE TABLE IF NOT EXISTS entitlements (
    account_id TEXT PRIMARY KEY,
    entitled   INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS api_keys (
    key_hash   TEXT PRIMARY KEY,
    account_id TEXT NOT NULL
);
-- Stripe customer -> account. Written at checkout.session.completed (the only
-- event that carries client_reference_id); read by customer.subscription.*
-- events, which carry only the customer id.
CREATE TABLE IF NOT EXISTS stripe_customers (
    customer_id TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS ledger (
    request_id                  TEXT PRIMARY KEY,
    account_id                  TEXT NOT NULL,
    conv_id                     TEXT NOT NULL,
    ts                          TEXT NOT NULL,
    -- NULL = the probe failed on that request (§8.4: record the hole, never estimate)
    counterfactual_input_tokens INTEGER,
    billed_input_tokens         INTEGER NOT NULL,
    billed_output_tokens        INTEGER NOT NULL,
    billed_cache_read_tokens    INTEGER NOT NULL,
    billed_cache_write_tokens   INTEGER NOT NULL,
    cache_prefix_sha8           TEXT NOT NULL,
    fail_open                   INTEGER NOT NULL,
    -- Non-core capture/governor seams (model, session_id, brain_ms, tools_*,
    -- gov_*, …) as a JSON blob: new fields need no migration. Postgres uses
    -- JSONB; SQLite stores JSON text and reads it via json_extract.
    extra                       TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS ledger_model ON ledger (json_extract(extra, '$.model'));

-- One row per parsec install (anonymous client-minted machine id —
-- contracts/schemas/install-report.schema.json). account_id is NULL until a
-- keyed report links it; updates COALESCE so a later keyless ping never
-- unlinks. KEEP IN SYNC with migrations/0008_installs.sql.
CREATE TABLE IF NOT EXISTS installs (
    install_id TEXT PRIMARY KEY,
    account_id TEXT,
    version    TEXT NOT NULL,
    os         TEXT NOT NULL,
    arch       TEXT NOT NULL,
    -- JSON array of configured harnesses (Postgres uses JSONB).
    harnesses  TEXT NOT NULL DEFAULT '[]',
    first_seen TEXT NOT NULL,
    last_seen  TEXT NOT NULL
);

-- Incremental (account, model) rollup: running totals maintained by the
-- ledger triggers below so savings reads never scan the ledger (the public
-- counter used to take ~7.5s at 562k rows). §8.4-honest: every counter
-- derives from the same per-request count_tokens rows and is recomputable
-- from the ledger. KEEP IN SYNC with migrations/0003_savings_rollup.sql.
CREATE TABLE IF NOT EXISTS savings_rollup (
    account_id                  TEXT NOT NULL,
    -- '' = the row carried no model; NULL can't be part of a primary key.
    model                       TEXT NOT NULL,
    rows_count                  INTEGER NOT NULL DEFAULT 0,
    measured_rows               INTEGER NOT NULL DEFAULT 0,
    tokens_saved                INTEGER NOT NULL DEFAULT 0,
    counterfactual_input_tokens INTEGER NOT NULL DEFAULT 0,
    billed_input_tokens         INTEGER NOT NULL DEFAULT 0,
    billed_output_tokens        INTEGER NOT NULL DEFAULT 0,
    billed_cache_read_tokens    INTEGER NOT NULL DEFAULT 0,
    billed_cache_write_tokens   INTEGER NOT NULL DEFAULT 0,
    fail_open_count             INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, model)
);

-- INSERT OR IGNORE seeds the (account, model) row, then UPDATE folds the
-- ledger row in — upsert inside a trigger body is not portable across the
-- SQLite versions in the field. A NULL counterfactual is a hole (§8.4): it
-- counts in rows_count but adds nothing to measured_rows/tokens_saved.
CREATE TRIGGER IF NOT EXISTS ledger_savings_rollup_ins AFTER INSERT ON ledger
BEGIN
    INSERT OR IGNORE INTO savings_rollup (account_id, model)
        VALUES (NEW.account_id, COALESCE(json_extract(NEW.extra, '$.model'), ''));
    UPDATE savings_rollup SET
        rows_count = rows_count + 1,
        measured_rows = measured_rows + (NEW.counterfactual_input_tokens IS NOT NULL),
        tokens_saved = tokens_saved + COALESCE(NEW.counterfactual_input_tokens
            - (NEW.billed_input_tokens + NEW.billed_cache_read_tokens
               + NEW.billed_cache_write_tokens), 0),
        counterfactual_input_tokens = counterfactual_input_tokens
            + COALESCE(NEW.counterfactual_input_tokens, 0),
        billed_input_tokens = billed_input_tokens + NEW.billed_input_tokens,
        billed_output_tokens = billed_output_tokens + NEW.billed_output_tokens,
        billed_cache_read_tokens = billed_cache_read_tokens + NEW.billed_cache_read_tokens,
        billed_cache_write_tokens = billed_cache_write_tokens + NEW.billed_cache_write_tokens,
        fail_open_count = fail_open_count + NEW.fail_open
    WHERE account_id = NEW.account_id
      AND model = COALESCE(json_extract(NEW.extra, '$.model'), '');
END;

CREATE TRIGGER IF NOT EXISTS ledger_savings_rollup_del AFTER DELETE ON ledger
BEGIN
    UPDATE savings_rollup SET
        rows_count = rows_count - 1,
        measured_rows = measured_rows - (OLD.counterfactual_input_tokens IS NOT NULL),
        tokens_saved = tokens_saved - COALESCE(OLD.counterfactual_input_tokens
            - (OLD.billed_input_tokens + OLD.billed_cache_read_tokens
               + OLD.billed_cache_write_tokens), 0),
        counterfactual_input_tokens = counterfactual_input_tokens
            - COALESCE(OLD.counterfactual_input_tokens, 0),
        billed_input_tokens = billed_input_tokens - OLD.billed_input_tokens,
        billed_output_tokens = billed_output_tokens - OLD.billed_output_tokens,
        billed_cache_read_tokens = billed_cache_read_tokens - OLD.billed_cache_read_tokens,
        billed_cache_write_tokens = billed_cache_write_tokens - OLD.billed_cache_write_tokens,
        fail_open_count = fail_open_count - OLD.fail_open
    WHERE account_id = OLD.account_id
      AND model = COALESCE(json_extract(OLD.extra, '$.model'), '');
END;

-- The app never UPDATEs ledger rows (INSERT OR IGNORE), but a hand edit must
-- not silently desync the rollup: subtract OLD, fold in NEW.
CREATE TRIGGER IF NOT EXISTS ledger_savings_rollup_upd AFTER UPDATE ON ledger
BEGIN
    UPDATE savings_rollup SET
        rows_count = rows_count - 1,
        measured_rows = measured_rows - (OLD.counterfactual_input_tokens IS NOT NULL),
        tokens_saved = tokens_saved - COALESCE(OLD.counterfactual_input_tokens
            - (OLD.billed_input_tokens + OLD.billed_cache_read_tokens
               + OLD.billed_cache_write_tokens), 0),
        counterfactual_input_tokens = counterfactual_input_tokens
            - COALESCE(OLD.counterfactual_input_tokens, 0),
        billed_input_tokens = billed_input_tokens - OLD.billed_input_tokens,
        billed_output_tokens = billed_output_tokens - OLD.billed_output_tokens,
        billed_cache_read_tokens = billed_cache_read_tokens - OLD.billed_cache_read_tokens,
        billed_cache_write_tokens = billed_cache_write_tokens - OLD.billed_cache_write_tokens,
        fail_open_count = fail_open_count - OLD.fail_open
    WHERE account_id = OLD.account_id
      AND model = COALESCE(json_extract(OLD.extra, '$.model'), '');
    INSERT OR IGNORE INTO savings_rollup (account_id, model)
        VALUES (NEW.account_id, COALESCE(json_extract(NEW.extra, '$.model'), ''));
    UPDATE savings_rollup SET
        rows_count = rows_count + 1,
        measured_rows = measured_rows + (NEW.counterfactual_input_tokens IS NOT NULL),
        tokens_saved = tokens_saved + COALESCE(NEW.counterfactual_input_tokens
            - (NEW.billed_input_tokens + NEW.billed_cache_read_tokens
               + NEW.billed_cache_write_tokens), 0),
        counterfactual_input_tokens = counterfactual_input_tokens
            + COALESCE(NEW.counterfactual_input_tokens, 0),
        billed_input_tokens = billed_input_tokens + NEW.billed_input_tokens,
        billed_output_tokens = billed_output_tokens + NEW.billed_output_tokens,
        billed_cache_read_tokens = billed_cache_read_tokens + NEW.billed_cache_read_tokens,
        billed_cache_write_tokens = billed_cache_write_tokens + NEW.billed_cache_write_tokens,
        fail_open_count = fail_open_count + NEW.fail_open
    WHERE account_id = NEW.account_id
      AND model = COALESCE(json_extract(NEW.extra, '$.model'), '');
END;

-- Model list pricing (USD per million tokens). Report-time join turns token
-- counts into cost; kept out of the ledger rows so a price change is one UPDATE
-- and never rewrites history. cache_write is the 5-minute-TTL rate.
CREATE TABLE IF NOT EXISTS model_pricing (
    model                TEXT PRIMARY KEY,
    input_per_mtok       REAL NOT NULL,
    output_per_mtok      REAL NOT NULL,
    cache_read_per_mtok  REAL NOT NULL,
    cache_write_per_mtok REAL NOT NULL,
    currency             TEXT NOT NULL DEFAULT 'USD'
);
"""


class Store(abc.ABC):
    """What the routers need from persistence — nothing more."""

    @abc.abstractmethod
    def set_entitlement(self, account_id: str, entitled: bool) -> None: ...

    @abc.abstractmethod
    def is_entitled(self, account_id: str) -> bool: ...

    @abc.abstractmethod
    def add_key(self, key_hash: str, account_id: str) -> None: ...

    @abc.abstractmethod
    def account_for_key(self, key_hash: str) -> str | None: ...

    @abc.abstractmethod
    def link_customer(self, customer_id: str, account_id: str) -> None:
        """Record the Stripe customer -> account mapping (idempotent; a
        re-checkout by the same customer re-points to the new account)."""

    @abc.abstractmethod
    def account_for_customer(self, customer_id: str) -> str | None: ...

    @abc.abstractmethod
    def add_ledger_row(self, account_id: str, row: dict[str, Any]) -> None:
        """Insert one savings-ledger row (idempotent on request_id)."""

    @abc.abstractmethod
    def ledger_summary(self, account_id: str) -> dict[str, Any]: ...

    @abc.abstractmethod
    def usage_daily(self, account_id: str, days: int) -> list[dict[str, Any]]:
        """Per-day usage buckets over the last `days` days (oldest first), each
        with summed tokens, tokens_saved (§8.4-honest), and cost."""

    @abc.abstractmethod
    def savings_public(self) -> dict[str, Any]:
        """Site-wide savings totals across ALL accounts — the landing page's
        public counter. Same §8.4 aggregation as ledger_summary minus the
        account filter; must expose nothing per-account."""

    @abc.abstractmethod
    def record_install(self, report: dict[str, Any], account_id: str | None) -> None:
        """Upsert one install-report ping (idempotent on install_id): metadata
        and last_seen refresh, first_seen keeps the original, account link is
        COALESCEd so a keyless ping never unlinks an account."""

    @abc.abstractmethod
    def installs_summary(self) -> dict[str, Any]:
        """Fleet-wide install counts (`fold_installs`) — aggregate-only."""


class SQLiteStore(Store):
    """Default zero-infrastructure store. One connection, one lock — the
    platform skeleton does not need connection pooling (§7c: thin)."""

    def __init__(self, path: str | None = None) -> None:
        self._path = path or os.environ.get("PARSEC_PLATFORM_DB", ":memory:")
        self._conn = sqlite3.connect(self._path, check_same_thread=False)
        self._conn.row_factory = sqlite3.Row
        self._lock = threading.Lock()
        with self._lock:
            self._conn.executescript(_SCHEMA)
            self._conn.executemany(
                "INSERT OR IGNORE INTO model_pricing (model, input_per_mtok, "
                "output_per_mtok, cache_read_per_mtok, cache_write_per_mtok) "
                "VALUES (?, ?, ?, ?, ?)",
                _PRICING_SEED,
            )
            # Backfill the rollup for a pre-existing DB file created before the
            # savings_rollup triggers existed. An empty rollup + non-empty
            # ledger can only mean "never backfilled": the ledger is
            # append-only and the triggers keep the rollup in step from the
            # moment the schema lands.
            if not self._conn.execute("SELECT 1 FROM savings_rollup LIMIT 1").fetchone():
                self._conn.execute(
                    "INSERT INTO savings_rollup (account_id, model, rows_count, "
                    "measured_rows, tokens_saved, counterfactual_input_tokens, "
                    "billed_input_tokens, billed_output_tokens, "
                    "billed_cache_read_tokens, billed_cache_write_tokens, fail_open_count) "
                    "SELECT account_id, COALESCE(json_extract(extra, '$.model'), ''), "
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
                    "GROUP BY account_id, COALESCE(json_extract(extra, '$.model'), '')"
                )
            self._conn.commit()

    def set_entitlement(self, account_id: str, entitled: bool) -> None:
        with self._lock:
            self._conn.execute(
                "INSERT INTO entitlements (account_id, entitled) VALUES (?, ?) "
                "ON CONFLICT(account_id) DO UPDATE SET entitled = excluded.entitled",
                (account_id, int(entitled)),
            )
            self._conn.commit()

    def is_entitled(self, account_id: str) -> bool:
        with self._lock:
            row = self._conn.execute(
                "SELECT entitled FROM entitlements WHERE account_id = ?", (account_id,)
            ).fetchone()
        return bool(row and row["entitled"])

    def add_key(self, key_hash: str, account_id: str) -> None:
        with self._lock:
            self._conn.execute(
                "INSERT INTO api_keys (key_hash, account_id) VALUES (?, ?)",
                (key_hash, account_id),
            )
            self._conn.commit()

    def account_for_key(self, key_hash: str) -> str | None:
        with self._lock:
            row = self._conn.execute(
                "SELECT account_id FROM api_keys WHERE key_hash = ?", (key_hash,)
            ).fetchone()
        return row["account_id"] if row else None

    def link_customer(self, customer_id: str, account_id: str) -> None:
        with self._lock:
            self._conn.execute(
                "INSERT INTO stripe_customers (customer_id, account_id) VALUES (?, ?) "
                "ON CONFLICT(customer_id) DO UPDATE SET account_id = excluded.account_id",
                (customer_id, account_id),
            )
            self._conn.commit()

    def account_for_customer(self, customer_id: str) -> str | None:
        with self._lock:
            row = self._conn.execute(
                "SELECT account_id FROM stripe_customers WHERE customer_id = ?",
                (customer_id,),
            ).fetchone()
        return row["account_id"] if row else None

    def add_ledger_row(self, account_id: str, row: dict[str, Any]) -> None:
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO ledger (request_id, account_id, conv_id, ts, "
                "counterfactual_input_tokens, billed_input_tokens, billed_output_tokens, "
                "billed_cache_read_tokens, billed_cache_write_tokens, cache_prefix_sha8, "
                "fail_open, extra) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    row["request_id"],
                    account_id,
                    row["conv_id"],
                    row["ts"],
                    row["counterfactual_input_tokens"],
                    row["billed_input_tokens"],
                    row["billed_output_tokens"],
                    row["billed_cache_read_tokens"],
                    row["billed_cache_write_tokens"],
                    row["cache_prefix_sha8"],
                    int(row["fail_open"]),
                    json.dumps(extra_fields(row)),
                ),
            )
            self._conn.commit()

    def record_install(self, report: dict[str, Any], account_id: str | None) -> None:
        with self._lock:
            self._conn.execute(
                "INSERT INTO installs (install_id, account_id, version, os, arch, "
                "harnesses, first_seen, last_seen) VALUES (?, ?, ?, ?, ?, ?, ?, ?) "
                "ON CONFLICT(install_id) DO UPDATE SET "
                "account_id = COALESCE(excluded.account_id, installs.account_id), "
                "version = excluded.version, os = excluded.os, arch = excluded.arch, "
                "harnesses = excluded.harnesses, last_seen = excluded.last_seen",
                (
                    report["install_id"],
                    account_id,
                    report["version"],
                    report["os"],
                    report["arch"],
                    json.dumps(report["harnesses"]),
                    report["ts"],
                    report["ts"],
                ),
            )
            self._conn.commit()

    def installs_summary(self) -> dict[str, Any]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT account_id, version, os, arch, harnesses, last_seen "
                "FROM installs"
            ).fetchall()
        return fold_installs([dict(r) for r in rows])

    def ledger_summary(self, account_id: str) -> dict[str, Any]:
        with self._lock:
            row = self._conn.execute(
                "SELECT COUNT(*) AS rows_count, "
                "COALESCE(SUM(counterfactual_input_tokens), 0) AS counterfactual_input_tokens, "
                "COALESCE(SUM(billed_input_tokens), 0) AS billed_input_tokens, "
                "COALESCE(SUM(billed_output_tokens), 0) AS billed_output_tokens, "
                "COALESCE(SUM(billed_cache_read_tokens), 0) AS billed_cache_read_tokens, "
                "COALESCE(SUM(billed_cache_write_tokens), 0) AS billed_cache_write_tokens, "
                "COALESCE(SUM(fail_open), 0) AS fail_open_count "
                "FROM ledger WHERE account_id = ?",
                (account_id,),
            ).fetchone()
        summary = dict(row)
        # §8.4 measurement honesty: savings derive from the count_tokens
        # counterfactual in the same rows — never a modeled baseline, and only
        # over rows where the probe succeeded (NULL probes are holes, not
        # zeros; imputing them would fabricate savings).
        #
        # Saved = counterfactual − ALL billed input-side tokens (uncached +
        # cache read + cache write). Cache reads and writes are billed input;
        # subtracting only billed_input_tokens counts cache-served prefix as
        # free and overstates savings — on real ledgers by >2x, since a warm
        # Claude Code session bills nearly all of its input as cache reads.
        # This must stay byte-identical in meaning to the client-side
        # aggregation in proxy/src/statusline.rs (`aggregate_ledger`), which is
        # what `parsec savings` prints; the two disagreeing is a trust bug.
        with self._lock:
            measured = self._conn.execute(
                "SELECT COUNT(*) AS measured_rows, "
                "COALESCE(SUM(counterfactual_input_tokens - (billed_input_tokens "
                "  + billed_cache_read_tokens + billed_cache_write_tokens)), 0) AS tokens_saved "
                "FROM ledger WHERE account_id = ? AND counterfactual_input_tokens IS NOT NULL",
                (account_id,),
            ).fetchone()
        summary["measured_rows"] = measured["measured_rows"]
        summary["tokens_saved"] = measured["tokens_saved"]
        # Per-model, per-cost breakdown (the dashboard's model/cost granularity).
        # Group on the JSON-extracted model; LEFT JOIN pricing so unknown models
        # still appear (with cost null). measured_rows counts probe-successful
        # rows so tokens_saved stays §8.4-honest per model.
        with self._lock:
            by_model = self._conn.execute(
                "SELECT json_extract(l.extra, '$.model') AS model, "
                "COUNT(*) AS rows_count, "
                "COUNT(l.counterfactual_input_tokens) AS measured_rows, "
                "COALESCE(SUM(CASE WHEN l.counterfactual_input_tokens IS NOT NULL "
                "  THEN l.counterfactual_input_tokens - (l.billed_input_tokens "
                "    + l.billed_cache_read_tokens + l.billed_cache_write_tokens) END), 0) "
                "  AS tokens_saved, "
                "COALESCE(SUM(l.billed_input_tokens), 0) AS billed_input_tokens, "
                "COALESCE(SUM(l.billed_output_tokens), 0) AS billed_output_tokens, "
                "COALESCE(SUM(l.billed_cache_read_tokens), 0) AS billed_cache_read_tokens, "
                "COALESCE(SUM(l.billed_cache_write_tokens), 0) AS billed_cache_write_tokens, "
                "p.input_per_mtok, p.output_per_mtok, p.cache_read_per_mtok, "
                "p.cache_write_per_mtok, p.currency "
                "FROM ledger l "
                "LEFT JOIN model_pricing p ON p.model = json_extract(l.extra, '$.model') "
                "WHERE l.account_id = ? "
                "GROUP BY json_extract(l.extra, '$.model'), p.input_per_mtok, "
                "  p.output_per_mtok, p.cache_read_per_mtok, p.cache_write_per_mtok, "
                "  p.currency "
                "ORDER BY billed_input_tokens DESC",
                (account_id,),
            ).fetchall()
        summary["by_model"] = [_model_row(dict(r)) for r in by_model]
        return summary

    def savings_public(self) -> dict[str, Any]:
        # Site-wide totals from the (account, model) rollup — a users×models-row
        # scan, never the ledger. Summed per model so each model's saved tokens
        # are valued at its own blended rate; accounts collapse here and nothing
        # per-account leaves this method.
        with self._lock:
            rows = self._conn.execute(_SAVINGS_PUBLIC_SQL).fetchall()
        return fold_public([dict(r) for r in rows])

    def usage_daily(self, account_id: str, days: int) -> list[dict[str, Any]]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT date(l.ts) AS day, "
                "json_extract(l.extra, '$.model') AS model, "
                "COUNT(*) AS rows_count, "
                "COUNT(l.counterfactual_input_tokens) AS measured_rows, "
                "COALESCE(SUM(CASE WHEN l.counterfactual_input_tokens IS NOT NULL "
                "  THEN l.counterfactual_input_tokens - (l.billed_input_tokens "
                "    + l.billed_cache_read_tokens + l.billed_cache_write_tokens) END), 0) "
                "  AS tokens_saved, "
                "COALESCE(SUM(l.billed_input_tokens), 0) AS billed_input_tokens, "
                "COALESCE(SUM(l.billed_output_tokens), 0) AS billed_output_tokens, "
                "COALESCE(SUM(l.billed_cache_read_tokens), 0) AS billed_cache_read_tokens, "
                "COALESCE(SUM(l.billed_cache_write_tokens), 0) AS billed_cache_write_tokens, "
                "p.input_per_mtok, p.output_per_mtok, p.cache_read_per_mtok, "
                "p.cache_write_per_mtok "
                "FROM ledger l "
                "LEFT JOIN model_pricing p ON p.model = json_extract(l.extra, '$.model') "
                "WHERE l.account_id = ? AND l.ts >= datetime('now', ?) "
                "GROUP BY date(l.ts), json_extract(l.extra, '$.model'), "
                "  p.input_per_mtok, p.output_per_mtok, p.cache_read_per_mtok, "
                "  p.cache_write_per_mtok",
                (account_id, f"-{int(days)} days"),
            ).fetchall()
        return fold_daily([dict(r) for r in rows])
