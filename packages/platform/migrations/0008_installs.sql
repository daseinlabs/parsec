-- One row per parsec install: the anonymous client-minted machine id
-- (contracts/schemas/install-report.schema.json) plus coarse environment
-- facts, upserted by POST /installs. account_id stays NULL until a keyed
-- report links it; the upsert COALESCEs so a later keyless ping never
-- unlinks. Mirrors store.py _SCHEMA (SQLite) — keep in sync.

CREATE TABLE IF NOT EXISTS installs (
    install_id TEXT PRIMARY KEY,
    account_id TEXT,
    version    TEXT NOT NULL,
    os         TEXT NOT NULL,
    arch       TEXT NOT NULL,
    harnesses  JSONB NOT NULL DEFAULT '[]'::jsonb,
    first_seen TIMESTAMPTZ NOT NULL,
    last_seen  TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS installs_account ON installs (account_id);

ALTER TABLE installs ENABLE ROW LEVEL SECURITY;
