"""A usage-logging passthrough gateway for the Anthropic Messages API.

Port of adaptive-context-clean/bench/usage_gateway.py, PASSTHROUGH mode only.

WHY THIS EXISTS
---------------
The bench drives headless Claude Code as the ONE fixed agent for every arm. The
SDK reports a run-level cost but not the PER-REQUEST cache split
(cache_creation / cache_read) that cache-aware pricing wants. The provider DOES
report that split on every /v1/messages response — we just never see it when
Claude Code talks to the model directly.

So we interpose ONE gateway at the BOTTOM of every chain:

    baseline : Claude Code ── ANTHROPIC_BASE_URL=gateway ──> gateway ──> upstream
    parsec   : Claude Code ──> parsec proxy (curates; its PARSEC_UPSTREAM is the
               gateway) ──> gateway ──> upstream

Because the gateway sits below the arm's proxy it always observes the REAL
post-compression usage the model billed. It forwards each request VERBATIM
(urllib) to the configured upstream — https://api.anthropic.com by default,
carrying the client's own auth headers untouched — and tees one
schema.CallUsage row per request into a per-run JSONL keyed by the
`x-ccb-run-id` header (or the gateway's default run id).

The parsec arm's SAVINGS accounting does not come from here — it comes from
the proxy's own ~/.parsec/ledger.jsonl (see parsec_bench.ledger), which holds
the count_tokens counterfactual (§8.4). The gateway rows drive pricing only.

CUT vs the reference (clearly-marked seam): the reference's MODE_VERTEX — an
AnthropicVertex native bridge (litellm/anthropic SDK + ADC, beta-header
filtering, transient-retry) — required the GCP runner-box setup and is not
ported. The new bench bottoms out at api.anthropic.com with the user's own
key. To re-add a bridge, reintroduce a `mode` switch in make_handler and give
UsageGateway a completion_fn seam (see the reference file for the shape).

ROBUSTNESS: every failure mode degrades safely — a parse error, a non-JSON
body, a missing usage object never break the proxied call; the client gets the
upstream's bytes back unchanged and we merely fail to LOG a row. Streaming
responses are streamed through; usage is reconstructed from a teed copy.

Pure stdlib (http.server, urllib, json, threading).
"""

from __future__ import annotations

import json
import re
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Optional

# CallUsage is a TypedDict (pure typing) — importing it pulls in no heavy deps.
from parsec_bench.schema import CallUsage

DEFAULT_UPSTREAM = "https://api.anthropic.com"

# ── header for the run-id tag ────────────────────────────────────────────────
# Claude Code forwards ANTHROPIC_CUSTOM_HEADERS onto every model request, so
# the runner sets a run-id header there and the gateway reads it to TAG each
# usage row into the right per-run JSONL. The parsec proxy reads the SAME
# header for its conv_id prefix (server.rs conversation_id) but does NOT
# forward it upstream — a per-run gateway's default_run_id covers that leg.
RUN_ID_HEADER = "x-ccb-run-id"

# Hop-by-hop headers we must NOT forward (RFC 7230 §6.1) — plus Host (we set
# our own) and Content-Length (recomputed by urllib from the body).
_HOP_BY_HOP = {
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "te", "trailers", "transfer-encoding", "upgrade",
    "host", "content-length", "accept-encoding",
}


# ── usage extraction (the cache split, normalized) ───────────────────────────
def _i(d: dict, key: str) -> int:
    """Read an int field off a usage dict, treating missing/None as 0."""
    v = d.get(key)
    return int(v or 0)


_MODEL_RE = re.compile(r'"model"\s*:\s*"([^"]+)"')


def _model_from_text(text: str) -> str:
    """Served model id from a response body/SSE text (message_start carries it)."""
    m = _MODEL_RE.search(text or "")
    return m.group(1) if m else ""


def extract_usage(usage: Optional[dict], latency_s: float = 0.0) -> Optional[CallUsage]:
    """One CallUsage row from an Anthropic ``usage`` object.

    Anthropic's usage shape:
        {"input_tokens": N,                  # uncached NEW input only
         "output_tokens": M,
         "cache_creation_input_tokens": W,   # cache WRITE
         "cache_read_input_tokens": R}       # cache READ

    We emit ``prompt_tokens`` as the FULL billable input (uncached + write +
    read), the contract pricing.real_cache_cost expects. Cache keys are emitted
    only when the provider reported them (presence is the signal pricing uses
    to pick the real-cache path). Returns None when usage carries no token
    data at all — the caller then logs nothing (passthrough).
    """
    if not isinstance(usage, dict):
        return None
    inp = _i(usage, "input_tokens")
    out = _i(usage, "output_tokens")
    has_write = usage.get("cache_creation_input_tokens") is not None
    has_read = usage.get("cache_read_input_tokens") is not None
    if inp == 0 and out == 0 and not has_write and not has_read:
        return None

    write = _i(usage, "cache_creation_input_tokens")
    read = _i(usage, "cache_read_input_tokens")
    row: CallUsage = {
        "prompt_tokens": inp + write + read,
        "completion_tokens": out,
        "latency_s": round(latency_s, 3),
    }
    if has_write:
        row["cache_creation_input_tokens"] = write
    if has_read:
        row["cache_read_input_tokens"] = read
    return row


def usage_from_sse(body_text: str) -> Optional[dict]:
    """Reconstruct the final Anthropic ``usage`` from a raw SSE stream body.

    The Messages API streams usage across two events: ``message_start``
    carries the input side (input_tokens + the cache split) in
    ``message.usage``; ``message_delta`` carries the cumulative
    ``output_tokens`` in a top-level ``usage`` (the LAST one wins). Robust to
    interleaved non-data lines, the ``data: [DONE]`` sentinel, and unparseable
    JSON (skipped). Returns None if the stream carried no usage.
    """
    merged: dict = {}
    saw = False
    for line in body_text.splitlines():
        line = line.strip()
        if not line.startswith("data:"):
            continue
        payload = line[len("data:"):].strip()
        if not payload or payload == "[DONE]":
            continue
        try:
            evt = json.loads(payload)
        except (ValueError, TypeError):
            continue
        etype = evt.get("type")
        if etype == "message_start":
            u = (evt.get("message", {}) or {}).get("usage")
            if isinstance(u, dict):
                merged.update(u)
                saw = True
        elif etype == "message_delta":
            u = evt.get("usage")
            if isinstance(u, dict):
                for k in ("output_tokens", "cache_creation_input_tokens",
                          "cache_read_input_tokens", "input_tokens"):
                    if u.get(k) is not None:
                        merged[k] = u[k]
                saw = True
    return merged if saw else None


# ── the per-run usage sink (thread-safe append to JSONL) ──────────────────────
class UsageSink:
    """Appends CallUsage rows to a per-run JSONL, keyed by run id.

    One JSONL file per run id under ``log_dir`` (``<run_id>.usage.jsonl``).
    Writes are serialized by a lock (the gateway is multithreaded). A write
    failure is swallowed (logged to stderr) so it never breaks the proxied call.
    """

    def __init__(self, log_dir: str) -> None:
        self.log_dir = Path(log_dir)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self._lock = threading.Lock()

    def path_for(self, run_id: str) -> Path:
        # sanitize: keep the filename a single safe component
        safe = "".join(c if (c.isalnum() or c in "._-") else "_" for c in (run_id or "default"))
        return self.log_dir / f"{safe}.usage.jsonl"

    def write(self, run_id: str, row: CallUsage) -> None:
        try:
            with self._lock:
                with self.path_for(run_id).open("a", encoding="utf-8") as f:
                    f.write(json.dumps(row) + "\n")
        except Exception as e:  # noqa: BLE001 — never break the proxied call on a log write
            print(f"  usage_gateway WARN: usage write failed for run {run_id!r}: "
                  f"{type(e).__name__}: {str(e)[:160]}", flush=True)


# ── the passthrough request handler ───────────────────────────────────────────
def make_handler(upstream_base: str, sink: UsageSink,
                 default_run_id: str = "", default_headers: Optional[dict] = None,
                 timeout_s: float = 600.0):
    """Build a BaseHTTPRequestHandler subclass bound to one upstream + sink.

    upstream_base   : the base URL requests are forwarded to verbatim.
    sink            : where CallUsage rows are written.
    default_run_id  : run-id tag used when a request carries no RUN_ID_HEADER.
    default_headers : extra headers MERGED onto every forwarded request;
                      incoming client headers win on conflict.
    timeout_s       : per-request upstream timeout.
    """
    base = upstream_base.rstrip("/")
    extra_headers = dict(default_headers or {})

    class _Handler(BaseHTTPRequestHandler):
        # silence the default per-request stderr logging (noisy under a pool);
        # real failures are printed explicitly below.
        def log_message(self, *args) -> None:  # noqa: D401
            return

        def _proxy(self) -> None:
            t0 = time.time()
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else b""
            run_id = self.headers.get(RUN_ID_HEADER) or default_run_id
            self._passthrough(body, run_id, t0)

        # ── forward the request verbatim to upstream_base ─────────────────────
        def _passthrough(self, body: bytes, run_id: str, t0: float) -> None:
            # Build forwarded headers: drop hop-by-hop + our run-id tag;
            # preserve auth / anthropic-* untouched; fill from default_headers
            # only where the client didn't set the key.
            fwd_headers: dict[str, str] = {}
            for k, v in self.headers.items():
                if k.lower() in _HOP_BY_HOP or k.lower() == RUN_ID_HEADER:
                    continue
                fwd_headers[k] = v
            for k, v in extra_headers.items():
                fwd_headers.setdefault(k, v)
            # ask the upstream for an unencoded body so we can tee/parse it.
            fwd_headers["Accept-Encoding"] = "identity"

            url = base + self.path
            req = urllib.request.Request(url, data=body, method=self.command,
                                         headers=fwd_headers)
            try:
                resp = urllib.request.urlopen(req, timeout=timeout_s)
            except urllib.error.HTTPError as e:
                # The upstream returned a non-2xx: forward it FAITHFULLY
                # (status + body) so the SDK sees the real error.
                self._relay_error(e)
                return
            except Exception as e:  # noqa: BLE001 — connection refused / timeout / etc.
                self._gateway_error(504, f"upstream unreachable: {type(e).__name__}: {str(e)[:160]}")
                return

            ctype = (resp.headers.get("Content-Type") or "").lower()
            try:
                if "text/event-stream" in ctype:
                    self._relay_stream(resp, run_id, t0)
                else:
                    self._relay_buffered(resp, run_id, t0)
            except (BrokenPipeError, ConnectionError):
                # client (SDK) hung up mid-response — nothing to do, never crash.
                return

        # ── non-streaming: buffer, log usage, then relay the body ─────────────
        def _relay_buffered(self, resp, run_id: str, t0: float) -> None:
            data = resp.read()
            # log usage BEFORE writing the body back so the row is durable by
            # the time the client's request returns — no read-after-write race
            # for the runner, which reads the JSONL right after the run.
            self._log_json_usage(resp.headers, data, run_id, time.time() - t0)
            self.send_response(resp.status)
            self._send_passthrough_headers(resp.headers, len(data))
            self.end_headers()
            if data:
                self.wfile.write(data)

        # ── streaming: tee bytes to the client AND to an SSE buffer ───────────
        def _relay_stream(self, resp, run_id: str, t0: float) -> None:
            self.send_response(resp.status)
            # streamed: don't set Content-Length; preserve chunked/SSE semantics.
            self._send_passthrough_headers(resp.headers, content_length=None)
            self.end_headers()
            chunks: list[bytes] = []
            while True:
                chunk = resp.read(8192)
                if not chunk:
                    break
                chunks.append(chunk)
                self.wfile.write(chunk)
                self.wfile.flush()
            # reconstruct usage from the buffered SSE body (off the hot path).
            try:
                body_text = b"".join(chunks).decode("utf-8", "replace")
                usage = usage_from_sse(body_text)
                row = extract_usage(usage, time.time() - t0)
                if row is not None:
                    mdl = _model_from_text(body_text)
                    if mdl:
                        row["model"] = mdl
                    sink.write(run_id, row)
            except Exception as e:  # noqa: BLE001 — logging must never break the run
                print(f"  usage_gateway WARN: SSE usage parse failed: "
                      f"{type(e).__name__}: {str(e)[:160]}", flush=True)

        def _log_json_usage(self, headers, data: bytes, run_id: str, latency_s: float) -> None:
            try:
                obj = json.loads(data.decode("utf-8", "replace")) if data else None
                usage = obj.get("usage") if isinstance(obj, dict) else None
                row = extract_usage(usage, latency_s)
                if row is not None:
                    mdl = str(obj.get("model") or "") if isinstance(obj, dict) else ""
                    if mdl:
                        row["model"] = mdl
                    sink.write(run_id, row)
            except Exception as e:  # noqa: BLE001 — non-JSON body / no usage: passthrough only
                # Not an error — count_tokens responses, error bodies, etc.
                # carry no usage. Stay quiet unless it looked like a message.
                ct = (headers.get("Content-Type") or "").lower()
                if "application/json" in ct and b"usage" in (data or b""):
                    print(f"  usage_gateway WARN: JSON usage parse failed: "
                          f"{type(e).__name__}: {str(e)[:120]}", flush=True)

        # ── header relay helpers ──────────────────────────────────────────────
        def _send_passthrough_headers(self, headers, content_length: Optional[int]) -> None:
            for k, v in headers.items():
                if k.lower() in _HOP_BY_HOP:
                    continue
                self.send_header(k, v)
            if content_length is not None:
                self.send_header("Content-Length", str(content_length))

        def _relay_error(self, e: urllib.error.HTTPError) -> None:
            data = e.read() or b""
            self.send_response(e.code)
            self._send_passthrough_headers(e.headers, len(data))
            self.end_headers()
            if data:
                try:
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionError):
                    pass

        def _gateway_error(self, code: int, msg: str) -> None:
            print(f"  usage_gateway: {code} {msg}", flush=True)
            body = json.dumps({"type": "error",
                               "error": {"type": "gateway_error", "message": msg}}).encode()
            try:
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionError):
                pass

        # all proxied verbs route through _proxy
        def do_POST(self) -> None:  # noqa: N802
            self._proxy()

        def do_GET(self) -> None:  # noqa: N802
            self._proxy()

    return _Handler


class UsageGateway:
    """A running gateway: an HTTP server thread + the usage sink it writes to.

    Usage:
        gw = UsageGateway("https://api.anthropic.com", log_dir="runs/usage",
                          default_run_id=run_id).start()
        base_url = gw.base_url          # set ANTHROPIC_BASE_URL / the arm
                                        # proxy's PARSEC_UPSTREAM to this
        ...
        gw.stop()

    Binds 127.0.0.1 on an ephemeral port (port 0) by default so many gateways
    run concurrently (one per (instance, arm) solve) without a port dance.
    """

    def __init__(self, upstream_base: str = DEFAULT_UPSTREAM,
                 log_dir: str = "runs/usage", *,
                 host: str = "127.0.0.1", port: int = 0,
                 default_run_id: str = "", default_headers: Optional[dict] = None,
                 timeout_s: float = 600.0) -> None:
        self.upstream_base = upstream_base
        self.sink = UsageSink(log_dir)
        self.default_run_id = default_run_id
        handler = make_handler(upstream_base, self.sink,
                               default_run_id=default_run_id,
                               default_headers=default_headers,
                               timeout_s=timeout_s)
        self._server = ThreadingHTTPServer((host, port), handler)
        self._thread: Optional[threading.Thread] = None

    @property
    def address(self) -> tuple[str, int]:
        return self._server.server_address[0], self._server.server_address[1]

    @property
    def base_url(self) -> str:
        host, port = self.address
        return f"http://{host}:{port}"

    def usage_path(self, run_id: Optional[str] = None) -> Path:
        return self.sink.path_for(run_id or self.default_run_id)

    def start(self) -> "UsageGateway":
        self._thread = threading.Thread(target=self._server.serve_forever,
                                        name="usage-gateway", daemon=True)
        self._thread.start()
        return self

    def stop(self) -> None:
        try:
            self._server.shutdown()
        except Exception:
            pass
        try:
            self._server.server_close()
        except Exception:
            pass
        if self._thread is not None:
            self._thread.join(timeout=5)

    def __enter__(self) -> "UsageGateway":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()


# ── standalone launch (debug / smoke) ─────────────────────────────────────────
def main() -> None:
    import argparse
    import os

    ap = argparse.ArgumentParser(description="Anthropic usage-logging passthrough gateway")
    ap.add_argument("--upstream", default=os.environ.get("CCB_GATEWAY_UPSTREAM",
                                                         DEFAULT_UPSTREAM),
                    help="upstream base URL to forward to verbatim")
    ap.add_argument("--log-dir", default=os.environ.get("CCB_GATEWAY_LOG_DIR", "runs/usage"))
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--run-id", default=os.environ.get("CCB_GATEWAY_RUN_ID", "default"))
    a = ap.parse_args()

    gw = UsageGateway(a.upstream, a.log_dir, host=a.host, port=a.port,
                      default_run_id=a.run_id).start()
    print(f"usage_gateway [passthrough]: {gw.base_url} -> {a.upstream}  "
          f"(usage -> {gw.usage_path()})", flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        gw.stop()


if __name__ == "__main__":
    main()
