"""Postgres Store — the Supabase-backed implementation DIRECTION.md §7c names.

Same seam as `store.SQLiteStore`, same thinness: psycopg3 sync + a small
connection pool, no ORM. Point DASEIN_PLATFORM_DB_URL at Supabase's
TRANSACTION POOLER endpoint (port 6543) — Cloud Run instances churn, and
direct Postgres connections would exhaust the server. Schema lives in
migrations/*.sql (applied via the Supabase CLI), NOT here: a stateless
service must not race itself running DDL at startup.

Import is lazy-guarded: the platform package works without psycopg installed
(SQLite tests, local dev); `pip install dasein-platform[postgres]` adds it.
"""

from __future__ import annotations

import os
from typing import Any

from dasein_platform.store import Store

try:
    from psycopg_pool import ConnectionPool
except ImportError:  # pragma: no cover - exercised only without the extra
    ConnectionPool = None  # type: ignore[assignment,misc]

_LEDGER_COLUMNS = (
    "request_id",
    "account_id",
    "conv_id",
    "ts",
    "counterfactual_input_tokens",
    "billed_input_tokens",
    "billed_output_tokens",
    "billed_cache_read_tokens",
    "billed_cache_write_tokens",
    "cache_prefix_sha8",
    "fail_open",
)


class PostgresStore(Store):
    """Store over Supabase Postgres (or any Postgres) via psycopg3."""

    def __init__(self, url: str | None = None, *, min_size: int = 0, max_size: int = 4):
        if ConnectionPool is None:
            raise RuntimeError(
                "psycopg is not installed - pip install 'dasein-platform[postgres]'"
            )
        url = url or os.environ["DASEIN_PLATFORM_DB_URL"]
        # open=True validates the URL at boot (fail fast, not on first
        # request); min_size=0 lets an idle Cloud Run instance hold nothing.
        self._pool = ConnectionPool(
            url, min_size=min_size, max_size=max_size, open=True
        )

    def close(self) -> None:
        self._pool.close()

    def set_entitlement(self, account_id: str, entitled: bool) -> None:
        with self._pool.connection() as conn:
            conn.execute(
                "INSERT INTO entitlements (account_id, entitled) VALUES (%s, %s) "
                "ON CONFLICT (account_id) DO UPDATE SET entitled = EXCLUDED.entitled",
                (account_id, entitled),
            )

    def is_entitled(self, account_id: str) -> bool:
        with self._pool.connection() as conn:
            row = conn.execute(
                "SELECT entitled FROM entitlements WHERE account_id = %s",
                (account_id,),
            ).fetchone()
        return bool(row and row[0])

    def add_key(self, key_hash: str, account_id: str) -> None:
        with self._pool.connection() as conn:
            conn.execute(
                "INSERT INTO api_keys (key_hash, account_id) VALUES (%s, %s)",
                (key_hash, account_id),
            )

    def account_for_key(self, key_hash: str) -> str | None:
        with self._pool.connection() as conn:
            row = conn.execute(
                "SELECT account_id FROM api_keys WHERE key_hash = %s", (key_hash,)
            ).fetchone()
        return row[0] if row else None

    def link_customer(self, customer_id: str, account_id: str) -> None:
        with self._pool.connection() as conn:
            conn.execute(
                "INSERT INTO stripe_customers (customer_id, account_id) VALUES (%s, %s) "
                "ON CONFLICT (customer_id) DO UPDATE SET account_id = EXCLUDED.account_id",
                (customer_id, account_id),
            )

    def account_for_customer(self, customer_id: str) -> str | None:
        with self._pool.connection() as conn:
            row = conn.execute(
                "SELECT account_id FROM stripe_customers WHERE customer_id = %s",
                (customer_id,),
            ).fetchone()
        return row[0] if row else None

    def add_ledger_row(self, account_id: str, row: dict[str, Any]) -> None:
        values = (
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
            bool(row["fail_open"]),
        )
        with self._pool.connection() as conn:
            conn.execute(
                f"INSERT INTO ledger ({', '.join(_LEDGER_COLUMNS)}) "
                f"VALUES ({', '.join(['%s'] * len(_LEDGER_COLUMNS))}) "
                "ON CONFLICT (request_id) DO NOTHING",
                values,
            )

    def ledger_summary(self, account_id: str) -> dict[str, Any]:
        with self._pool.connection() as conn:
            totals = conn.execute(
                "SELECT COUNT(*), "
                "COALESCE(SUM(counterfactual_input_tokens), 0), "
                "COALESCE(SUM(billed_input_tokens), 0), "
                "COALESCE(SUM(billed_output_tokens), 0), "
                "COALESCE(SUM(billed_cache_read_tokens), 0), "
                "COALESCE(SUM(billed_cache_write_tokens), 0), "
                "COALESCE(SUM(fail_open::int), 0) "
                "FROM ledger WHERE account_id = %s",
                (account_id,),
            ).fetchone()
            # §8.4: savings only over rows where the probe succeeded — NULL
            # probes are holes, never zeros (same query shape as SQLiteStore).
            measured = conn.execute(
                "SELECT COUNT(*), "
                "COALESCE(SUM(counterfactual_input_tokens - billed_input_tokens), 0) "
                "FROM ledger WHERE account_id = %s "
                "AND counterfactual_input_tokens IS NOT NULL",
                (account_id,),
            ).fetchone()
        return {
            "rows_count": totals[0],
            "counterfactual_input_tokens": int(totals[1]),
            "billed_input_tokens": int(totals[2]),
            "billed_output_tokens": int(totals[3]),
            "billed_cache_read_tokens": int(totals[4]),
            "billed_cache_write_tokens": int(totals[5]),
            "fail_open_count": int(totals[6]),
            "measured_rows": measured[0],
            "tokens_saved": int(measured[1]),
        }
