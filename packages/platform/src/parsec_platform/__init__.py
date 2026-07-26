"""parsec-platform — the control plane minus scoring (DIRECTION.md §7c).

Buy-don't-build, kept thin: Supabase owns signup/login (we only verify its
JWTs), Stripe owns billing UI (we only consume its webhooks). What we actually
write is the few hundred lines §7c enumerates — Stripe webhook → entitlement
flag, mint/validate the brain-API key the local proxy calls with, and
savings-ledger ingest + per-account reporting. Private; SaaS-only (never part
of the enterprise self-host bundle).
"""

from parsec_platform.app import create_app

__all__ = ["create_app"]
