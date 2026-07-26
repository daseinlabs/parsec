"""Stripe webhook signature verification (DIRECTION.md §7c: "Stripe Checkout +
Customer Portal + webhooks" — the only billing code we write is
webhook → entitlement flag).

Implements Stripe's documented v1 scheme directly (HMAC-SHA256 over
"{timestamp}.{raw_payload}" with the endpoint's signing secret) so the
platform needs no `stripe` SDK dependency — buy the billing service, not
another dependency tree.
"""

from __future__ import annotations

import hmac
import time
from hashlib import sha256

DEFAULT_TOLERANCE_SECONDS = 300  # Stripe's recommended replay window.


def verify_stripe_signature(
    payload: bytes,
    signature_header: str | None,
    secret: str,
    tolerance_seconds: int = DEFAULT_TOLERANCE_SECONDS,
    now: float | None = None,
) -> bool:
    """Return True iff the Stripe-Signature header authenticates `payload`.

    Header format: `t=<unix_ts>,v1=<hex hmac>[,v1=...,v0=...]`. Any valid v1
    signature within the replay tolerance accepts.
    """
    if not signature_header:
        return False
    timestamp: str | None = None
    candidates: list[str] = []
    for part in signature_header.split(","):
        name, _, value = part.strip().partition("=")
        if name == "t":
            timestamp = value
        elif name == "v1":
            candidates.append(value)
    if timestamp is None or not candidates:
        return False
    try:
        ts = int(timestamp)
    except ValueError:
        return False
    if abs((now if now is not None else time.time()) - ts) > tolerance_seconds:
        return False
    expected = hmac.new(
        secret.encode(), f"{timestamp}.".encode() + payload, sha256
    ).hexdigest()
    return any(hmac.compare_digest(expected, candidate) for candidate in candidates)


def sign_payload(payload: bytes, secret: str, timestamp: int | None = None) -> str:
    """Build a Stripe-Signature header for `payload` (used by tests)."""
    ts = timestamp if timestamp is not None else int(time.time())
    mac = hmac.new(secret.encode(), f"{ts}.".encode() + payload, sha256).hexdigest()
    return f"t={ts},v1={mac}"
