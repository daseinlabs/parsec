"""Per-user `psc_` key validation against the platform's /keys/validate seam.

DIRECTION §7 names this exact seam: "is this key entitled?" is what the brain
asks before scoring. Enabled by PARSEC_PLATFORM_URL — when set, the bearer token
on every scoring request is validated against {platform}/keys/validate/{key} and
must be BOTH valid AND entitled.

Two properties keep this consistent with the rest of the brain:

  * Cached (TTL) so the platform is NOT on the per-request hot path — steady
    state does zero network work. Revoked/unentitled keys re-check sooner
    (_TTL_BAD) so a cancellation propagates within ~30s.
  * FAIL OPEN, MEASURED: on a platform transport error we serve and count the
    event (COUNTERS["fail_opens"], surfaced on /health) rather than block a
    paying user during a platform blip — the CLAUDE.md posture. Set
    PARSEC_BRAIN_AUTH_STRICT=1 to fail closed instead.

This gate is access control, NOT part of the scored bytes: it reads the wall
clock (cache TTL) but never influences a score, so the serving-path determinism
rule is untouched.
"""
from __future__ import annotations

import os
import threading
import time

import httpx

from ._log import count_fail_open, get_logger

log = get_logger("keyauth")

_TTL_OK_S = float(os.environ.get("PARSEC_BRAIN_AUTH_TTL_S", "300"))   # entitled: 5 min
_TTL_BAD_S = 30.0                                                     # denied: re-check sooner
_TIMEOUT_S = float(os.environ.get("PARSEC_BRAIN_AUTH_TIMEOUT_S", "3"))

_cache: dict[str, tuple[bool, float]] = {}   # key -> (allowed, expiry on time.monotonic())
_lock = threading.Lock()
_client: httpx.Client | None = None


def _http() -> httpx.Client:
    global _client
    if _client is None:                       # lazy: never constructed under test monkeypatch
        _client = httpx.Client(timeout=_TIMEOUT_S)
    return _client


def _validate_remote(platform_url: str, key: str) -> bool:
    """True iff the platform reports the key valid AND entitled. Raises on any
    transport/HTTP error (the caller decides fail-open vs fail-closed)."""
    resp = _http().get(f"{platform_url.rstrip('/')}/keys/validate/{key}")
    resp.raise_for_status()
    body = resp.json()
    return bool(body.get("valid")) and bool(body.get("entitled"))


def check(platform_url: str, key: str | None) -> bool:
    """Should this request be served? Cached by key; fail-open-and-count on a
    platform error unless PARSEC_BRAIN_AUTH_STRICT=1."""
    if not key:
        return False                          # no credential — always denied
    now = time.monotonic()
    with _lock:
        hit = _cache.get(key)
    if hit is not None and hit[1] > now:
        return hit[0]                          # fresh cached verdict
    try:
        allowed = _validate_remote(platform_url, key)
    except Exception as e:
        if os.environ.get("PARSEC_BRAIN_AUTH_STRICT") == "1":
            log.warning("key validation failed (%s) -> fail closed (strict)", type(e).__name__)
            return False
        # Prefer a stale verdict over guessing; else fail open. Either way, count.
        verdict = hit[0] if hit is not None else True
        count_fail_open(log, f"platform unreachable ({type(e).__name__}) -> "
                             f"{'stale verdict' if hit is not None else 'fail open'}")
        return verdict
    with _lock:
        _cache[key] = (allowed, now + (_TTL_OK_S if allowed else _TTL_BAD_S))
    return allowed
