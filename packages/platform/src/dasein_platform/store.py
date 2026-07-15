"""Storage seam for the platform service.

DIRECTION.md §7c picks Supabase Postgres as the v1 database for accounts,
entitlements, and savings-ledger rows. This module is the seam that makes that
a swap, not a rewrite: routes talk only to the `Store` interface; `SQLiteStore`
is the zero-infrastructure default (path from DASEIN_PLATFORM_DB, in-memory if
unset) and a Postgres-backed implementation replaces it later without touching
the routers. Deliberately no ORM and no async driver — §7c says thin.
"""

from __future__ import annotations

import abc
import os
import sqlite3
import threading
from typing import Any

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
    fail_open                   INTEGER NOT NULL
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


class SQLiteStore(Store):
    """Default zero-infrastructure store. One connection, one lock — the
    platform skeleton does not need connection pooling (§7c: thin)."""

    def __init__(self, path: str | None = None) -> None:
        self._path = path or os.environ.get("DASEIN_PLATFORM_DB", ":memory:")
        self._conn = sqlite3.connect(self._path, check_same_thread=False)
        self._conn.row_factory = sqlite3.Row
        self._lock = threading.Lock()
        with self._lock:
            self._conn.executescript(_SCHEMA)

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
                "fail_open) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        with self._lock:
            measured = self._conn.execute(
                "SELECT COUNT(*) AS measured_rows, "
                "COALESCE(SUM(counterfactual_input_tokens - billed_input_tokens), 0) AS tokens_saved "
                "FROM ledger WHERE account_id = ? AND counterfactual_input_tokens IS NOT NULL",
                (account_id,),
            ).fetchone()
        summary["measured_rows"] = measured["measured_rows"]
        summary["tokens_saved"] = measured["tokens_saved"]
        return summary
