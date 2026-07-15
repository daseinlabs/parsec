-- Platform schema v1 (DIRECTION.md §7c) — Supabase Postgres dialect.
-- Apply with the Supabase CLI (supabase db push / migration up); the service
-- itself never runs DDL. Mirrors store.py _SCHEMA (SQLite) — keep in sync.

CREATE TABLE IF NOT EXISTS entitlements (
    account_id TEXT PRIMARY KEY,
    entitled   BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS api_keys (
    key_hash   TEXT PRIMARY KEY,
    account_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS api_keys_account ON api_keys (account_id);

-- Stripe customer -> account. Written at checkout.session.completed (the only
-- event carrying client_reference_id); read by customer.subscription.* events,
-- which carry only the customer id.
CREATE TABLE IF NOT EXISTS stripe_customers (
    customer_id TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ledger (
    request_id                  TEXT PRIMARY KEY,
    account_id                  TEXT NOT NULL,
    conv_id                     TEXT NOT NULL,
    ts                          TIMESTAMPTZ NOT NULL,
    -- NULL = the probe failed on that request (§8.4: record the hole, never estimate)
    counterfactual_input_tokens BIGINT,
    billed_input_tokens         BIGINT NOT NULL,
    billed_output_tokens        BIGINT NOT NULL,
    billed_cache_read_tokens    BIGINT NOT NULL,
    billed_cache_write_tokens   BIGINT NOT NULL,
    cache_prefix_sha8           TEXT NOT NULL,
    fail_open                   BOOLEAN NOT NULL
);
CREATE INDEX IF NOT EXISTS ledger_account ON ledger (account_id, ts);

-- Row Level Security: this service connects with its own role and is the only
-- writer; Supabase's anon/authenticated roles get no direct table access.
ALTER TABLE entitlements     ENABLE ROW LEVEL SECURITY;
ALTER TABLE api_keys         ENABLE ROW LEVEL SECURITY;
ALTER TABLE stripe_customers ENABLE ROW LEVEL SECURITY;
ALTER TABLE ledger           ENABLE ROW LEVEL SECURITY;
