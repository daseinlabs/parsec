#!/usr/bin/env python3
"""Minimal Anthropic-shaped mock upstream for the local e2e smoke
(scripts/e2e_smoke.sh). Stdlib only. Records every /v1/messages body to
--spool so the smoke can assert on what the proxy actually forwarded."""

import argparse
import json
import pathlib
from http.server import BaseHTTPRequestHandler, HTTPServer


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8091)
    ap.add_argument("--spool", required=True)
    args = ap.parse_args()
    spool = pathlib.Path(args.spool)
    spool.mkdir(parents=True, exist_ok=True)
    seq = {"n": 0}

    class H(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802
            raw = self.rfile.read(int(self.headers.get("content-length", 0)))
            if self.path == "/v1/messages/count_tokens":
                body = json.dumps({"input_tokens": max(1, len(raw) // 4)})
            elif self.path == "/v1/messages":
                seq["n"] += 1
                (spool / f"req_{seq['n']:03d}.json").write_bytes(raw)
                body = json.dumps(
                    {
                        "id": f"msg_mock_{seq['n']}",
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "ok"}],
                        "stop_reason": "end_turn",
                        "usage": {
                            "input_tokens": max(1, len(raw) // 4),
                            "output_tokens": 5,
                            "cache_creation_input_tokens": 7,
                            "cache_read_input_tokens": 50,
                        },
                    }
                )
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

    HTTPServer(("127.0.0.1", args.port), H).serve_forever()


if __name__ == "__main__":
    main()
