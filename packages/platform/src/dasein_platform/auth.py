"""Auth dependencies (DIRECTION.md §7c: "Supabase Auth for signup/login/orgs
— services verify its JWTs, ~20 lines").

Two credentials exist in the platform:

- A **Supabase JWT** (Authorization: Bearer ...) identifies a human/account for
  the dashboard-facing endpoints. Verified HS256 against SUPABASE_JWT_SECRET
  with Supabase's default `aud=authenticated`; the `sub` claim is the account
  id.
- A **brain-API key** (`dsn_...`, X-Dasein-Key header) identifies the local
  proxy for machine-to-machine calls like ledger ingest. It is opaque and
  stored hashed — this is the "is this key entitled?" seam §7 names.
"""

from __future__ import annotations

import hashlib
import os

import jwt
from fastapi import Header, HTTPException, Request

SUPABASE_AUDIENCE = "authenticated"


def hash_key(key: str) -> str:
    """Keys are opaque bearer secrets; only the SHA-256 digest is stored."""
    return hashlib.sha256(key.encode()).hexdigest()


def require_account(authorization: str | None = Header(default=None)) -> str:
    """FastAPI dependency: verify the Supabase JWT, return the account id.

    Returns 401 cleanly on a missing header, a non-Bearer scheme, or any
    verification failure (bad signature, expired, wrong audience).
    """
    if not authorization or not authorization.startswith("Bearer "):
        raise HTTPException(status_code=401, detail="missing bearer token")
    secret = os.environ.get("SUPABASE_JWT_SECRET")
    if not secret:
        # Misconfiguration must not become an open door.
        raise HTTPException(status_code=401, detail="auth not configured")
    try:
        claims = jwt.decode(
            authorization.removeprefix("Bearer "),
            secret,
            algorithms=["HS256"],
            audience=SUPABASE_AUDIENCE,
        )
    except jwt.PyJWTError:
        raise HTTPException(status_code=401, detail="invalid token")
    sub = claims.get("sub")
    if not sub:
        raise HTTPException(status_code=401, detail="token has no subject")
    return sub


def require_key_account(
    request: Request, x_dasein_key: str | None = Header(default=None)
) -> str:
    """FastAPI dependency: resolve a `dsn_` brain-API key to its account id."""
    if not x_dasein_key or not x_dasein_key.startswith("dsn_"):
        raise HTTPException(status_code=401, detail="missing api key")
    account_id = request.app.state.store.account_for_key(hash_key(x_dasein_key))
    if account_id is None:
        raise HTTPException(status_code=401, detail="unknown api key")
    return account_id
