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
    pi, po, pr, pw = prices  # type: ignore[misc]
    return round((bi * pi + bo * po + br * pr + bw * pw) / 1_000_000, 6)


def _model_row(r: dict[str, Any]) -> dict[str, Any]:
    """Shape one grouped row (token sums + joined price columns, keys as named in
    both stores' by-model queries) into the API's per-model entry, with cost_usd
    computed from the four price columns (None when the model is unpriced)."""
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
    return {
        "model": r["model"],  # None for rows with no model captured
        "rows_count": int(r["rows_count"]),
        "measured_rows": int(r["measured_rows"]),
        "tokens_saved": int(r["tokens_saved"]),
        "billed_input_tokens": sums[0],
        "billed_output_tokens": sums[1],
        "billed_cache_read_tokens": sums[2],
        "billed_cache_write_tokens": sums[3],
        "cost_usd": cost_usd(sums, prices),
        "currency": r.get("currency") or "USD",
    }


def fold_daily(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Roll per-(day, model) grouped rows (same column shape as the by-model
    query, plus a `day`) up to one bucket per day: summed tokens + cost, oldest
    day first. Cost is null-aware — a day's unpriced-model tokens add no cost but
    don't erase the priced portion (the model seed covers what the client sends,
    so unpriced is only stale/unknown model strings)."""
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
            },
        )
        b["rows_count"] += int(r["rows_count"])
        b["measured_rows"] += int(r["measured_rows"])
        b["tokens_saved"] += int(r["tokens_saved"])
        for c in token_cols:
            b[c] += int(r[c])
        c = cost_usd(
            tuple(int(r[k]) for k in token_cols),  # type: ignore[arg-type]
            (
                r["input_per_mtok"],
                r["output_per_mtok"],
                r["cache_read_per_mtok"],
                r["cache_write_per_mtok"],
            ),
        )
        if c is not None:
            b["cost_usd"] += float(c)
    out = sorted(days.values(), key=lambda x: x["date"])
    for b in out:
        b["cost_usd"] = round(b["cost_usd"], 6)
    return out


# Anthropic list pricing, USD per million tokens (cache_write = 5-minute TTL =
# 1.25x input; 1-hour TTL would be 2x — the ledger stores a single
# billed_cache_write_tokens and can't distinguish TTL, so the 5m default is
# used). KEEP IN SYNC with migrations/0002_*.sql model_pricing seed.
_PRICING_SEED = (
    # model,               input, output, cache_read, cache_write(5m)
    ("claude-fable-5", 10.0, 50.0, 1.0, 12.5),
    ("claude-opus-4-8", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-7", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-6", 5.0, 25.0, 0.5, 6.25),
    ("claude-sonnet-5", 3.0, 15.0, 0.3, 3.75),
    ("claude-sonnet-4-6", 3.0, 15.0, 0.3, 3.75),
    ("claude-haiku-4-5", 1.0, 5.0, 0.1, 1.25),
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
