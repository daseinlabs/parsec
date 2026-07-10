"""In-process Anthropic-shaped mock upstream for the replay mode and tests.

Vendored from scripts/mock_upstream.py (the e2e smoke's mock), reshaped into an
importable, recording server:

  * POST /v1/messages             — records the raw forwarded body, answers a
    canned assistant message whose usage is SIZE-DERIVED:
        input_tokens = len(raw_body) // 4   (chars/4 of what was FORWARDED)
    so the ledger's billed side scales with what the proxy actually sent.
  * POST /v1/messages/count_tokens — answers {"input_tokens": len(raw)//4},
    the same chars/4 scale. The proxy probes THIS with the ORIGINAL body, so
    counterfactual and billed sit on one consistent (mock) scale and their
    difference is a meaningful machinery measurement.

Cache fields default to 0 (unlike the smoke's fixed 7/50) so the replay
report's served-input numbers aren't offset by canned constants; pass
cache_write/cache_read to restore the smoke's values.

Stdlib only; ephemeral port; ThreadingHTTPServer so the proxy's count_tokens
probe and the message forward can overlap.
"""

from __future__ import annotations

import json
import threading
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Optional


@dataclass
class Recorded:
    """One request the mock saw, in arrival order."""

    path: str
    headers: dict
    raw: bytes

    def body(self) -> dict:
        return json.loads(self.raw.decode("utf-8"))


@dataclass
class MockUpstream:
    """A running mock upstream. Use as a context manager or start()/stop()."""

    spool_dir: Optional[str] = None      # also write req_NNN.json files (smoke parity)
    cache_write: int = 0                 # canned usage.cache_creation_input_tokens
    cache_read: int = 0                  # canned usage.cache_read_input_tokens
    output_tokens: int = 5
    requests: list[Recorded] = field(default_factory=list)   # /v1/messages only
    count_tokens_calls: int = 0

    def __post_init__(self) -> None:
        self._lock = threading.Lock()
        self._server: Optional[ThreadingHTTPServer] = None
        self._thread: Optional[threading.Thread] = None

    @property
    def base_url(self) -> str:
        assert self._server is not None, "call start() first"
        host, port = self._server.server_address[0], self._server.server_address[1]
        return f"http://{host}:{port}"

    def messages(self) -> list[Recorded]:
        with self._lock:
            return list(self.requests)

    def start(self) -> "MockUpstream":
        mock = self

        class H(BaseHTTPRequestHandler):
            def do_POST(self) -> None:  # noqa: N802
                raw = self.rfile.read(int(self.headers.get("content-length") or 0))
                if self.path == "/v1/messages/count_tokens":
                    with mock._lock:
                        mock.count_tokens_calls += 1
                    body = json.dumps({"input_tokens": max(1, len(raw) // 4)})
                elif self.path == "/v1/messages":
                    with mock._lock:
                        mock.requests.append(Recorded(
                            path=self.path, headers=dict(self.headers), raw=raw))
                        n = len(mock.requests)
                        if mock.spool_dir:
                            d = Path(mock.spool_dir)
                            d.mkdir(parents=True, exist_ok=True)
                            (d / f"req_{n:03d}.json").write_bytes(raw)
                    body = json.dumps({
                        "id": f"msg_mock_{n}",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-mock",
                        "content": [{"type": "text", "text": "ok"}],
                        "stop_reason": "end_turn",
                        "usage": {
                            "input_tokens": max(1, len(raw) // 4),
                            "output_tokens": mock.output_tokens,
                            "cache_creation_input_tokens": mock.cache_write,
                            "cache_read_input_tokens": mock.cache_read,
                        },
                    })
                else:
                    self.send_response(404)
                    self.end_headers()
                    return
                data = body.encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *a: object) -> None:
                pass

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self._thread = threading.Thread(target=self._server.serve_forever,
                                        name="mock-upstream", daemon=True)
        self._thread.start()
        return self

    def stop(self) -> None:
        if self._server is not None:
            try:
                self._server.shutdown()
                self._server.server_close()
            except Exception:
                pass
        if self._thread is not None:
            self._thread.join(timeout=5)

    def __enter__(self) -> "MockUpstream":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()
