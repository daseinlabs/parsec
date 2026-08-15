"""Postgres Store — the Supabase-backed implementation DIRECTION.md §7c names.

Same seam as `store.SQLiteStore`, same thinness: psycopg3 sync + a small
connection pool, no ORM. Point PARSEC_PLATFORM_DB_URL at Supabase's
TRANSACTION POOLER endpoint (port 6543) — Cloud Run instances churn, and
direct Postgres connections would exhaust the server. Schema lives in
migrations/*.sql (applied via the Supabase CLI), NOT here: a stateless
service must not race itself running DDL at startup.

Import is lazy-guarded: the platform package works without psycopg installed
(SQLite tests, local dev); `pip install parsec-platform[postgres]` adds it.
"""

from __future__ import annotations

import os
from typing import Any

from parsec_platform.store import (
    _SAVINGS_PUBLIC_SQL,
    Store,
    _model_row,
    extra_fields,
    fold_daily,
    fold_public,
)

try:
    from psycopg_pool import ConnectionPool
    from psycopg.types.json import Jsonb
except ImportError:  # pragma: no cover - exercised only without the extra
    ConnectionPool = None  # type: ignore[assignment,misc]
    Jsonb = None  # type: ignore[assignment,misc]

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
    "extra",
)


class PostgresStore(Store):
    """Store over Supabase Postgres (or any Postgres) via psycopg3."""

    def __init__(self, url: str | None = None, *, min_size: int = 0, max_size: int = 4):
        if ConnectionPool is None:
            raise RuntimeError(
                "psycopg is not installed - pip install 'parsec-platform[postgres]'"
            )
        url = url or os.environ["PARSEC_PLATFORM_DB_URL"]
        # open=True validates the URL at boot (fail fast, not on first
        # request); min_size=0 lets an idle Cloud Run instance hold nothing.
        # prepare_threshold=None: the transaction pooler hands each txn a
        # different server connection, so auto-prepared statements ("_pg3_n")
        # vanish between uses and fail with SQLSTATE 26000.
        self._pool = ConnectionPool(
            url, min_size=min_size, max_size=max_size, open=True,
            kwargs={"prepare_threshold": None},
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
            Jsonb(extra_fields(row)),
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
            # Saved subtracts ALL billed input-side tokens (uncached + cache
            # read + cache write); see the SQLiteStore comment for why omitting
            # the cache terms overstates savings by >2x on real ledgers.
            measured = conn.execute(
                "SELECT COUNT(*), "
                "COALESCE(SUM(counterfactual_input_tokens - (billed_input_tokens "
                "  + billed_cache_read_tokens + billed_cache_write_tokens)), 0) "
                "FROM ledger WHERE account_id = %s "
                "AND counterfactual_input_tokens IS NOT NULL",
                (account_id,),
            ).fetchone()
        # Per-model, per-cost breakdown (dashboard model/cost granularity). Group
        # on the JSONB-extracted model; LEFT JOIN pricing so unpriced models still
        # appear (cost null). measured_rows keeps tokens_saved §8.4-honest.
        with self._pool.connection() as conn:
            rows = conn.execute(
                "SELECT l.extra->>'model' AS model, "
                "COUNT(*) AS rows_count, "
                "COUNT(l.counterfactual_input_tokens) AS measured_rows, "
                "COALESCE(SUM(l.counterfactual_input_tokens - (l.billed_input_tokens "
                "  + l.billed_cache_read_tokens + l.billed_cache_write_tokens)) "
                "  FILTER (WHERE l.counterfactual_input_tokens IS NOT NULL), 0) "
                "  AS tokens_saved, "
                "COALESCE(SUM(l.billed_input_tokens), 0) AS billed_input_tokens, "
                "COALESCE(SUM(l.billed_output_tokens), 0) AS billed_output_tokens, "
                "COALESCE(SUM(l.billed_cache_read_tokens), 0) AS billed_cache_read_tokens, "
                "COALESCE(SUM(l.billed_cache_write_tokens), 0) AS billed_cache_write_tokens, "
                "p.input_per_mtok, p.output_per_mtok, p.cache_read_per_mtok, "
                "p.cache_write_per_mtok, p.currency "
                "FROM ledger l "
                "LEFT JOIN model_pricing p ON p.model = l.extra->>'model' "
                "WHERE l.account_id = %s "
                "GROUP BY l.extra->>'model', p.input_per_mtok, p.output_per_mtok, "
                "  p.cache_read_per_mtok, p.cache_write_per_mtok, p.currency "
                "ORDER BY billed_input_tokens DESC",
                (account_id,),
            ).fetchall()
        cols = (
            "model",
            "rows_count",
            "measured_rows",
            "tokens_saved",
            "billed_input_tokens",
            "billed_output_tokens",
            "billed_cache_read_tokens",
            "billed_cache_write_tokens",
            "input_per_mtok",
            "output_per_mtok",
            "cache_read_per_mtok",
            "cache_write_per_mtok",
            "currency",
        )
        by_model = [_model_row(dict(zip(cols, r))) for r in rows]
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
            "by_model": by_model,
        }

    def usage_daily(self, account_id: str, days: int) -> list[dict[str, Any]]:
        with self._pool.connection() as conn:
            rows = conn.execute(
                "SELECT (l.ts AT TIME ZONE 'UTC')::date AS day, "
                "l.extra->>'model' AS model, "
                "COUNT(*) AS rows_count, "
                "COUNT(l.counterfactual_input_tokens) AS measured_rows, "
                "COALESCE(SUM(l.counterfactual_input_tokens - (l.billed_input_tokens "
                "  + l.billed_cache_read_tokens + l.billed_cache_write_tokens)) "
                "  FILTER (WHERE l.counterfactual_input_tokens IS NOT NULL), 0) "
                "  AS tokens_saved, "
                "COALESCE(SUM(l.billed_input_tokens), 0) AS billed_input_tokens, "
                "COALESCE(SUM(l.billed_output_tokens), 0) AS billed_output_tokens, "
                "COALESCE(SUM(l.billed_cache_read_tokens), 0) AS billed_cache_read_tokens, "
                "COALESCE(SUM(l.billed_cache_write_tokens), 0) AS billed_cache_write_tokens, "
                "p.input_per_mtok, p.output_per_mtok, p.cache_read_per_mtok, "
                "p.cache_write_per_mtok "
                "FROM ledger l "
                "LEFT JOIN model_pricing p ON p.model = l.extra->>'model' "
                "WHERE l.account_id = %s AND l.ts >= now() - make_interval(days => %s) "
                "GROUP BY (l.ts AT TIME ZONE 'UTC')::date, l.extra->>'model', "
                "  p.input_per_mtok, p.output_per_mtok, p.cache_read_per_mtok, "
                "  p.cache_write_per_mtok",
                (account_id, int(days)),
            ).fetchall()
        cols = (
            "day",
            "model",
            "rows_count",
            "measured_rows",
            "tokens_saved",
            "billed_input_tokens",
            "billed_output_tokens",
            "billed_cache_read_tokens",
            "billed_cache_write_tokens",
            "input_per_mtok",
            "output_per_mtok",
            "cache_read_per_mtok",
            "cache_write_per_mtok",
        )
        return fold_daily([dict(zip(cols, r)) for r in rows])

    def savings_public(self) -> dict[str, Any]:
        # Site-wide totals from the (account, model) rollup — a users×models-row
        # scan, never the ledger (the full-ledger GROUP BY took ~7.5s at 562k
        # rows). The rollup is maintained by the ledger trigger in
        # migrations/0003_savings_rollup.sql; the SQL is shared with SQLiteStore.
        with self._pool.connection() as conn:
            rows = conn.execute(_SAVINGS_PUBLIC_SQL).fetchall()
        cols = (
            "model",
            "rows_count",
            "measured_rows",
            "tokens_saved",
            "billed_input_tokens",
            "billed_output_tokens",
            "billed_cache_read_tokens",
            "billed_cache_write_tokens",
            "input_per_mtok",
            "output_per_mtok",
            "cache_read_per_mtok",
            "cache_write_per_mtok",
            "currency",
        )
        return fold_public([dict(zip(cols, r)) for r in rows])
