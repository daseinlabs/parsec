"""The gzip request-decompression ASGI middleware: downstream must see the
plain bytes with corrected headers; garbage gzip is a 400, never a crash."""

from __future__ import annotations

import asyncio
import gzip
import json

import pytest

pytestmark = pytest.mark.no_ckpt


def _mw():
    pytest.importorskip("torch")  # parsec_brain.app import chain
    from parsec_brain.app import _GunzipRequests
    return _GunzipRequests


def test_gunzip_roundtrip():
    cls = _mw()
    got: dict = {}

    async def inner(scope, receive, send):
        body = b""
        while True:
            m = await receive()
            body += m.get("body", b"")
            if not m.get("more_body", False):
                break
        got["body"] = body
        got["headers"] = dict(scope["headers"])
        await send({"type": "http.response.start", "status": 200, "headers": []})
        await send({"type": "http.response.body", "body": b"ok"})

    payload = json.dumps({"hello": "world" * 50}).encode()
    gz = gzip.compress(payload)
    scope = {"type": "http", "headers": [
        (b"content-encoding", b"gzip"),
        (b"content-length", str(len(gz)).encode()),
        (b"content-type", b"application/json"),
    ]}
    msgs = [{"type": "http.request", "body": gz, "more_body": False}]

    async def receive():
        return msgs.pop(0)

    sent: list = []

    async def send(m):
        sent.append(m)

    asyncio.run(cls(inner)(scope, receive, send))
    assert got["body"] == payload
    assert got["headers"][b"content-length"] == str(len(payload)).encode()
    assert b"content-encoding" not in got["headers"]
    assert sent[0]["status"] == 200


def test_gunzip_rejects_garbage_with_400():
    cls = _mw()

    async def inner(scope, receive, send):  # pragma: no cover
        raise AssertionError("garbage gzip must never reach routing")

    scope = {"type": "http", "headers": [(b"content-encoding", b"gzip")]}

    async def receive():
        return {"type": "http.request", "body": b"definitely not gzip", "more_body": False}

    sent: list = []

    async def send(m):
        sent.append(m)

    asyncio.run(cls(inner)(scope, receive, send))
    assert sent[0]["status"] == 400


def test_identity_requests_untouched():
    cls = _mw()
    seen: dict = {}

    async def inner(scope, receive, send):
        seen["receive"] = receive
        await send({"type": "http.response.start", "status": 200, "headers": []})

    async def receive():
        return {"type": "http.request", "body": b"plain", "more_body": False}

    sent: list = []

    async def send(m):
        sent.append(m)

    asyncio.run(cls(inner)({"type": "http", "headers": []}, receive, send))
    assert seen["receive"] is receive  # untouched pass-through
