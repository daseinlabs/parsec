"""Usage extraction + the passthrough gateway end-to-end (mock upstream)."""

from __future__ import annotations

import json
import urllib.request

from dasein_bench.mock_upstream import MockUpstream
from dasein_bench.usage_gateway import (
    RUN_ID_HEADER,
    UsageGateway,
    extract_usage,
    usage_from_sse,
)


# ── extract_usage normalization ───────────────────────────────────────────────
def test_extract_usage_folds_cache_into_prompt_tokens():
    row = extract_usage({"input_tokens": 100, "output_tokens": 5,
                         "cache_creation_input_tokens": 7,
                         "cache_read_input_tokens": 50}, latency_s=1.234)
    assert row["prompt_tokens"] == 157       # FULL billable input
    assert row["completion_tokens"] == 5
    assert row["cache_creation_input_tokens"] == 7
    assert row["cache_read_input_tokens"] == 50
    assert row["latency_s"] == 1.234


def test_extract_usage_omits_absent_cache_keys():
    row = extract_usage({"input_tokens": 10, "output_tokens": 1})
    assert "cache_creation_input_tokens" not in row
    assert "cache_read_input_tokens" not in row


def test_extract_usage_no_data_returns_none():
    assert extract_usage(None) is None
    assert extract_usage({}) is None
    assert extract_usage({"input_tokens": 0, "output_tokens": 0}) is None


# ── SSE usage reconstruction ──────────────────────────────────────────────────
SSE = "\n".join([
    'event: message_start',
    'data: {"type":"message_start","message":{"model":"claude-mock",'
    '"usage":{"input_tokens":10,"output_tokens":1,'
    '"cache_creation_input_tokens":3,"cache_read_input_tokens":90}}}',
    '',
    'event: message_delta',
    'data: {"type":"message_delta","usage":{"output_tokens":21}}',
    '',
    'event: message_delta',
    'data: {"type":"message_delta","usage":{"output_tokens":42}}',
    '',
    'data: [DONE]',
    'not-a-data-line',
])


def test_usage_from_sse_merges_start_and_last_delta():
    u = usage_from_sse(SSE)
    assert u == {"input_tokens": 10, "output_tokens": 42,
                 "cache_creation_input_tokens": 3, "cache_read_input_tokens": 90}


def test_usage_from_sse_empty_stream():
    assert usage_from_sse("event: ping\ndata: {}\n") is None


# ── passthrough gateway e2e ───────────────────────────────────────────────────
def test_gateway_passthrough_relays_and_logs(tmp_path):
    with MockUpstream(cache_write=7, cache_read=50) as mock:
        gw = UsageGateway(mock.base_url, log_dir=str(tmp_path / "usage"),
                          default_run_id="dflt").start()
        try:
            body = json.dumps({"model": "m", "messages": [
                {"role": "user", "content": "hello"}]}).encode()
            req = urllib.request.Request(
                gw.base_url + "/v1/messages", data=body, method="POST",
                headers={"content-type": "application/json",
                         RUN_ID_HEADER: "runX"})
            with urllib.request.urlopen(req, timeout=10) as resp:
                relayed = json.loads(resp.read())
            # body relayed intact (the mock's canned message)
            assert relayed["role"] == "assistant"
            assert relayed["usage"]["cache_read_input_tokens"] == 50
            # row tagged by the run-id header, not the default
            rows = [json.loads(l) for l in
                    gw.usage_path("runX").read_text().splitlines()]
            assert len(rows) == 1
            r = rows[0]
            assert r["prompt_tokens"] == (relayed["usage"]["input_tokens"] + 7 + 50)
            assert r["completion_tokens"] == 5
            assert r["model"] == "claude-mock"
            assert not gw.usage_path("dflt").exists()
            # the upstream saw the body verbatim, minus the run-id tag header
            seen = mock.messages()[0]
            assert seen.raw == body
            assert RUN_ID_HEADER not in {k.lower() for k in seen.headers}
        finally:
            gw.stop()


def test_gateway_default_run_id_used_without_header(tmp_path):
    with MockUpstream() as mock:
        gw = UsageGateway(mock.base_url, log_dir=str(tmp_path / "usage"),
                          default_run_id="perrun").start()
        try:
            body = json.dumps({"messages": []}).encode()
            req = urllib.request.Request(
                gw.base_url + "/v1/messages", data=body, method="POST",
                headers={"content-type": "application/json"})
            urllib.request.urlopen(req, timeout=10).read()
            assert gw.usage_path("perrun").exists()
        finally:
            gw.stop()
