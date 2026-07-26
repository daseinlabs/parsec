"""Auth dependencies (DIRECTION.md §7c: "Supabase Auth for signup/login/orgs
— services verify its JWTs, ~20 lines").

Two credentials exist in the platform:

- A **Supabase JWT** (Authorization: Bearer ...) identifies a human/account for
  the dashboard-facing endpoints, `aud=authenticated`, `sub` = account id.
  Two verification modes, picked by env: SUPABASE_JWKS_URL (asymmetric
  ES256/RS256 signing keys — what new Supabase projects issue; the JWKS
  client caches keys and refetches on unknown kid) or SUPABASE_JWT_SECRET
  (legacy HS256 shared secret). JWKS wins when both are set.
- A **brain-API key** (`psc_...`, X-Parsec-Key header) identifies the local
  proxy for machine-to-machine calls like ledger ingest. It is opaque and
  stored hashed — this is the "is this key entitled?" seam §7 names. The
  pre-rename forms (`dsn_...`, X-Dasein-Key) are still accepted; see
  `require_key_account`.
"""

from __future__ import annotations

import hashlib
import os

import jwt
from fastapi import Header, HTTPException, Request

SUPABASE_AUDIENCE = "authenticated"

# One JWKS client per URL, module-cached: PyJWKClient memoizes the key set
# and refetches on an unknown kid, so key rotation needs no restart.
_jwks_clients: dict[str, jwt.PyJWKClient] = {}


def hash_key(key: str) -> str:
    """Keys are opaque bearer secrets; only the SHA-256 digest is stored."""
    return hashlib.sha256(key.encode()).hexdigest()


def _decode(token: str) -> dict:
    """Verify with the configured mode (see module doc); raises PyJWTError."""
    jwks_url = os.environ.get("SUPABASE_JWKS_URL")
    if jwks_url:
        client = _jwks_clients.get(jwks_url)
        if client is None:
            client = jwt.PyJWKClient(jwks_url, cache_keys=True)
            _jwks_clients[jwks_url] = client
        try:
            key = client.get_signing_key_from_jwt(token).key
        except jwt.PyJWTError:
            raise
        except Exception as e:  # JWKS fetch/parse failure (URLError etc.)
            raise jwt.InvalidTokenError(f"jwks unavailable: {e}")
        return jwt.decode(
            token, key, algorithms=["ES256", "RS256"], audience=SUPABASE_AUDIENCE
        )
    secret = os.environ.get("SUPABASE_JWT_SECRET")
    if not secret:
        # Misconfiguration must not become an open door.
        raise jwt.InvalidTokenError("auth not configured")
    return jwt.decode(token, secret, algorithms=["HS256"], audience=SUPABASE_AUDIENCE)


def require_account(authorization: str | None = Header(default=None)) -> str:
    """FastAPI dependency: verify the Supabase JWT, return the account id.

    Returns 401 cleanly on a missing header, a non-Bearer scheme, or any
    verification failure (bad signature, expired, wrong audience, JWKS
    fetch failure).
    """
    if not authorization or not authorization.startswith("Bearer "):
        raise HTTPException(status_code=401, detail="missing bearer token")
    try:
        claims = _decode(authorization.removeprefix("Bearer "))
    except jwt.PyJWTError:
        raise HTTPException(status_code=401, detail="invalid token")
    sub = claims.get("sub")
    if not sub:
        raise HTTPException(status_code=401, detail="token has no subject")
    return sub


def require_key_account(
    request: Request,
    x_parsec_key: str | None = Header(default=None),
    x_dasein_key: str | None = Header(default=None),
) -> str:
    """FastAPI dependency: resolve a brain-API key to its account id.

    Both the header name and the key prefix moved in the dasein→parsec rename,
    and both legacy forms are still accepted: already-shipped binaries send
    `X-Dasein-Key`, and keys are stored SHA-256-only, so `dsn_` keys minted
    before the rename cannot be rewritten server-side. FastAPI derives the
    header name from the parameter name — renaming these params changes the
    wire contract, so do not "clean them up".
    """
    key = x_parsec_key or x_dasein_key
    if not key or not (key.startswith("psc_") or key.startswith("dsn_")):
        raise HTTPException(status_code=401, detail="missing api key")
    account_id = request.app.state.store.account_for_key(hash_key(key))
    if account_id is None:
        raise HTTPException(status_code=401, detail="unknown api key")
    return account_id
