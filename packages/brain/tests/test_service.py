"""API surface + golden scoring parity (hermetic: DASEIN_EMBED_BACKEND=hash).

The GOLDEN block pins the exact quantized outputs of the vendored scoring path on a
handcrafted internal-view conversation. Any change to chunking, graph construction,
feature composition, flag pins or the checkpoint moves at least one int — the fixture
is the serve-side parity contract until the Rust featurization port lands.

Regenerate after a DELIBERATE change:  .venv/bin/python tests/test_service.py
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path

# self-contained bootstrap so the __main__ regen helper runs without pytest/conftest
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
os.environ.setdefault("DASEIN_EMBED_BACKEND", "hash")

from fastapi.testclient import TestClient

from dasein_brain.app import create_app
from dasein_brain.scorer import chunk_checksum, parse_internal

# ---- handcrafted internal view: system + task + 3 (assistant, tool-observation) steps ----
MESSAGES = [
    {"role": "system", "content": "You are a coding agent inside a sandboxed repo."},
    {"role": "user", "content": "Fix the bug in parse_config: load() raises ValueError when the "
     "config file contains comment lines. Traceback (most recent call last):\n"
     '  File "config.py", line 12, in parse_config\nValueError: invalid literal'},
    {"role": "assistant", "content": "Let me read the config module first.",
     "reasoning_content": "I should check how parse_config handles comment lines before editing.",
     "extra": {"actions": [{"tool": "Read", "command": "sed -n '1,24p' config.py"}]}},
    {"role": "tool", "content": "returncode: 0\n" + "\n".join(
        f"line {i} of config.py body: parse_config handles token {i}" for i in range(1, 25))},
    {"role": "assistant", "content": "Now find every caller of parse_config.",
     "extra": {"actions": [{"tool": "Grep", "command": "grep 'parse_config' src"}]}},
    {"role": "tool", "content": "src/config.py:12: def parse_config(path):\n"
     "src/app.py:4: from config import parse_config\nsrc/cli.py:9: cfg = parse_config(args.path)"},
    {"role": "assistant", "content": "Run the failing test to confirm.",
     "extra": {"actions": [{"tool": "Bash", "command": "python -m pytest tests/test_config.py -x"}]}},
    {"role": "tool", "content": "returncode: 1\nFAILED tests/test_config.py::test_comments - "
     "ValueError: invalid literal for int() with base 10: '# comment'\n1 failed in 0.21s"},
]

TOOLS = [
    {"name": "Bash", "description": "Run a shell command",
     "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}},
    {"name": "Read", "description": "Read a file from disk",
     "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}}},
    {"name": "Grep", "description": "Search file contents with a regex",
     "input_schema": {"type": "object", "properties": {"pattern": {"type": "string"}}}},
]

# ---- GOLDEN (generated once via the __main__ helper below, then pinned) ----
GOLDEN_CHECKSUM = "afd5a7ad74c0b925bc2b648554db89abeeff405893600cda421516871ef7bbe8"
GOLDEN_N_CHUNKS = 11               # 3 read + 1 asst + 1 reasoning | 3 grep + 1 asst | 1 other + 1 asst
GOLDEN_MASK = [9, 10]              # cur_step(3) newborns, evict != "provider"
GOLDEN_SCORES_Q = [1000000] * 9 + [305229, 278330]
GOLDEN_TAU_Q = 315265              # tau_calib["0.70"] = 0.3152651 on the 1e-6 grid
GOLDEN_TOOL_SCORES_Q = {"Bash": 598754, "Read": 565216, "Grep": 749731}
GOLDEN_TOOL_TOKENS = {"Bash": 33, "Read": 34, "Grep": 37}


def _trace_payload() -> dict:
    return {
        "contract": "brain-api-dev/v0",
        "conv_id": "test-conv",
        "messages": MESSAGES,
        "live_gi": list(range(GOLDEN_N_CHUNKS)),
        "mask": GOLDEN_MASK,
        "cur_step": 3,
        "chunk_checksum": GOLDEN_CHECKSUM,
    }


def test_health_and_bundle():
    client = TestClient(create_app())
    h = client.get("/health").json()
    assert h["status"] == "ok"
    assert isinstance(h["fail_opens"], int) and isinstance(h["requests"], int)   # counted, alertable
    b = client.get("/v1/bundle").json()
    assert b["contract"] == "brain-api-dev/v0"
    assert b["tau_q"] == GOLDEN_TAU_Q
    assert b["grid"] == 1_000_000
    assert b["heads"] == ["curator", "tool", "rule", "gate"]
    assert b["neighbors"] is False               # DASEIN_HOODS_PKL unset in the hermetic suite
    assert b["doom"] == {"gf": 4, "served": True}
    assert "hoods_anchors" not in b
    assert b["flags"]["AC_HUBPROPS"] == "off"
    assert len(b["checkpoint_id"]) == 64


def test_golden_scoring_twice_cold():
    """Exact pinned quantized scores, from two COLD app instances (fresh caches) — the
    determinism invariant: served ints are a pure function of (prefix, ckpt, config)."""
    # the pinned fixture must describe the server's own parse (guards fixture rot)
    parsed = parse_internal(MESSAGES, 10)
    assert len(parsed.chunks) == GOLDEN_N_CHUNKS
    assert chunk_checksum(parsed.chunks) == GOLDEN_CHECKSUM
    assert [j for j, c in enumerate(parsed.chunks)
            if c.step == parsed.cur_step and c.evict != "provider"] == GOLDEN_MASK
    for run in range(2):
        client = TestClient(create_app())
        r = client.post("/v1/score/trace", json=_trace_payload())
        assert r.status_code == 200, r.text
        body = r.json()
        assert body["scores_q"] == GOLDEN_SCORES_Q, f"cold run {run} diverged"
        assert body["tau_q"] == GOLDEN_TAU_Q
        assert len(body["checkpoint_id"]) == 64
        assert set(body["timings_ms"]) == {"embed", "forward"}


def test_checksum_mismatch_409():
    client = TestClient(create_app())
    payload = _trace_payload() | {"chunk_checksum": "0" * 64}
    r = client.post("/v1/score/trace", json=payload)
    assert r.status_code == 409
    assert r.json()["detail"]["server_checksum"] == GOLDEN_CHECKSUM


def test_mask_semantics_non_mask_rows_exactly_grid():
    client = TestClient(create_app())
    mask = [0]                                   # decide only the first live row
    r = client.post("/v1/score/trace", json=_trace_payload() | {"mask": mask})
    assert r.status_code == 200, r.text
    scores = r.json()["scores_q"]
    assert len(scores) == GOLDEN_N_CHUNKS
    assert scores[0] < 1_000_000                 # a real (raw sigmoid) score
    assert all(s == 1_000_000 for i, s in enumerate(scores) if i not in mask)
    # empty mask: nothing to decide, every row never-cut
    r = client.post("/v1/score/trace", json=_trace_payload() | {"mask": []})
    assert r.json()["scores_q"] == [1_000_000] * GOLDEN_N_CHUNKS


def test_score_tools_aligned_and_fail_shape():
    client = TestClient(create_app())
    req = {"contract": "brain-api-dev/v0", "conv_id": "test-conv",
           "messages": MESSAGES, "tools": TOOLS}
    r = client.post("/v1/score/tools", json=req)
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["names"] == ["Bash", "Read", "Grep"]     # roster order, deduped last-wins
    assert len(body["names"]) == len(body["scores_q"]) == len(body["tokens"])
    assert body["scores_q"] == [GOLDEN_TOOL_SCORES_Q[n] for n in body["names"]]
    assert body["tokens"] == [GOLDEN_TOOL_TOKENS[n] for n in body["names"]]
    # empty roster -> fail-open shape: names=[] means "serve the full roster"
    r = client.post("/v1/score/tools", json=req | {"tools": []})
    assert r.json() == {"names": [], "scores_q": [], "tokens": [],
                        "checkpoint_id": body["checkpoint_id"]}


def test_bearer_auth_when_key_set(monkeypatch):
    monkeypatch.setenv("DASEIN_BRAIN_KEY", "sekrit")
    client = TestClient(create_app())
    assert client.get("/health").status_code == 200            # health stays open (probes)
    assert client.get("/v1/bundle").status_code == 401
    assert client.post("/v1/score/trace", json=_trace_payload()).status_code == 401
    ok = client.get("/v1/bundle", headers={"Authorization": "Bearer sekrit"})
    assert ok.status_code == 200


def test_platform_entitlement_gate(monkeypatch):
    """DASEIN_PLATFORM_URL set -> the bearer is a dsn_ key validated against the
    platform; only valid+entitled keys are served."""
    import dasein_brain.keyauth as keyauth
    keyauth._cache.clear()
    monkeypatch.setattr(keyauth, "_validate_remote", lambda url, key: key == "dsn_good")
    monkeypatch.setenv("DASEIN_PLATFORM_URL", "https://platform.test")
    client = TestClient(create_app())
    assert client.get("/health").status_code == 200                    # probes stay open
    assert client.get("/v1/bundle").status_code == 401                 # no key
    assert client.get("/v1/bundle",
                      headers={"Authorization": "Bearer dsn_bad"}).status_code == 401
    assert client.get("/v1/bundle",
                      headers={"Authorization": "Bearer dsn_good"}).status_code == 200


def test_platform_gate_fails_open_measured(monkeypatch):
    """A platform outage must NOT block a paying user (CLAUDE.md fail-open), and
    every fail-open is counted on /health."""
    import dasein_brain.keyauth as keyauth
    keyauth._cache.clear()

    def _down(url, key):
        raise RuntimeError("platform unreachable")

    monkeypatch.setattr(keyauth, "_validate_remote", _down)
    monkeypatch.setenv("DASEIN_PLATFORM_URL", "https://platform.test")
    client = TestClient(create_app())
    before = client.get("/health").json()["fail_opens"]
    assert client.get("/v1/bundle",
                      headers={"Authorization": "Bearer dsn_any"}).status_code == 200
    assert client.get("/health").json()["fail_opens"] == before + 1


def test_platform_gate_strict_fails_closed(monkeypatch):
    """DASEIN_BRAIN_AUTH_STRICT=1 flips the outage behavior to fail-closed."""
    import dasein_brain.keyauth as keyauth
    keyauth._cache.clear()

    def _down(url, key):
        raise RuntimeError("platform unreachable")

    monkeypatch.setattr(keyauth, "_validate_remote", _down)
    monkeypatch.setenv("DASEIN_PLATFORM_URL", "https://platform.test")
    monkeypatch.setenv("DASEIN_BRAIN_AUTH_STRICT", "1")
    client = TestClient(create_app())
    assert client.get("/v1/bundle",
                      headers={"Authorization": "Bearer dsn_any"}).status_code == 401


if __name__ == "__main__":                       # golden regeneration helper
    parsed = parse_internal(MESSAGES, 10)
    mask = [j for j, c in enumerate(parsed.chunks)
            if c.step == parsed.cur_step and c.evict != "provider"]
    client = TestClient(create_app())
    payload = _trace_payload() | {"chunk_checksum": chunk_checksum(parsed.chunks),
                                  "live_gi": list(range(len(parsed.chunks))), "mask": mask,
                                  "cur_step": parsed.cur_step}
    trace = client.post("/v1/score/trace", json=payload).json()
    tools = client.post("/v1/score/tools", json={
        "contract": "brain-api-dev/v0", "conv_id": "regen",
        "messages": MESSAGES, "tools": TOOLS}).json()
    print(json.dumps({
        "GOLDEN_CHECKSUM": chunk_checksum(parsed.chunks),
        "GOLDEN_N_CHUNKS": len(parsed.chunks),
        "GOLDEN_MASK": mask,
        "GOLDEN_SCORES_Q": trace["scores_q"],
        "GOLDEN_TAU_Q": trace["tau_q"],
        "GOLDEN_TOOL_SCORES_Q": dict(zip(tools["names"], tools["scores_q"])),
        "GOLDEN_TOOL_TOKENS": dict(zip(tools["names"], tools["tokens"])),
    }, indent=1))
