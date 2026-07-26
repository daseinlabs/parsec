"""brain-api/v2 <-> v1 <-> dev CROSS-CONTRACT PARITY (the server-side-embedding gate).

v2 carries TEXT where v1 carried vectors (docs/server-side-embedding.md). The assertion here
is the same shape as test_v1.py's: v2 == v1 == dev scores_q/tau_q on the same conversation,
built from the SAME vendored featurization so nothing new is pinned.

Two things this gate exists to catch, both silent-score-drift classes:

  1. THE DUPCOS HANDOFF. decided_struct cols 47-48 (AC_DUPCOS) are the one readout block a
     client cannot compute without an embedder. A v2 client sends them ZEROED and the server
     refills them from its own content embeddings. Every payload here zeroes them on purpose —
     if the server ever stops filling, parity breaks loudly instead of drifting quietly.
  2. EMPTY-STRING SEMANTICS. An empty cmd/head must become a zero vector, never the embedding
     of "". test_v2_empty_cmd_head_are_zero_vectors pins it directly.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
os.environ.setdefault("PARSEC_EMBED_BACKEND", "hash")

import numpy as np
import pytest
from fastapi.testclient import TestClient

from parsec_brain.app import create_app
from parsec_brain.bundle import load_bundle
from parsec_brain.scorer import TraceScorer, parse_internal
from parsec_brain.vendored.pyg_model import edges
from test_service import MESSAGES, TOOLS
from test_v1 import (_dev_trace_payload, _f, _newborn_mask, _v1_nodes, _v1_trace_payload,
                     to_internal)

REPO = Path(__file__).resolve().parents[3]
GOLDEN_FIXTURE = REPO / "packages/proxy/parity/fixtures/golden_conversation.json"
SCHEMA_V2 = REPO / "packages/contracts/schemas/brain-api-v2.schema.json"
FIXTURE_TURNS = (2, 5, 13, 21)


@pytest.fixture(scope="module")
def client():
    return TestClient(create_app())


@pytest.fixture(scope="module")
def fz():
    return TraceScorer(load_bundle())


@pytest.fixture(scope="module")
def ckpt_id(fz):
    return fz.bundle.checkpoint_id


def _v1_node_to_v2(node: dict, chunk) -> dict:
    """The v1 node with its three vector slots replaced by the exact strings they encoded."""
    out = {k: v for k, v in node.items() if k not in ("emb_text", "emb_cmd", "emb_head")}
    out["text"] = (chunk.text or "")[:2000]
    out["cmd"] = getattr(chunk, "cmd", "") or ""                 # UNTRUNCATED, as v1 embedded it
    out["head"] = (getattr(chunk, "head", "") or "")[:240]
    return out


def _zero_dupcos(row: list[float]) -> list[float]:
    """What a v2 client actually sends: the readout row with the trailing dupcos pair zeroed."""
    return list(row[:-2]) + [0.0, 0.0]


def _v2_trace_payload(fz, ckpt_id, parsed, live_gi, mask, conv_id="v2-parity") -> dict:
    lc = [parsed.chunks[g] for g in live_gi]
    nodes, emb = _v1_nodes(fz, lc)
    ei, et = edges(lc, emb, causal=True)
    sup = [[int(a), int(b)] for a, b, t in
           zip(ei[0].tolist(), ei[1].tolist(), et.tolist()) if t == 4]
    ds = fz._het_readout(lc, list(mask), parsed.recent_cmds, parsed.task_text, 0.0, emb,
                         parsed.cur_step, parsed.het_steps, parsed.T)
    payload = {
        "contract": "brain-api/v2",
        "conv_id": conv_id,
        "checkpoint_id": ckpt_id,
        "cur_step": int(parsed.cur_step),
        "nodes": [_v1_node_to_v2(nodes[i], c) for i, c in enumerate(lc)],
        "task_text": parsed.task_text,
        "mask": list(mask),
        "decided_struct": [_zero_dupcos(_f(row)) for row in ds],
        "edges_supersession": sup,
    }
    if parsed.sys_text:
        payload["sys_text"] = parsed.sys_text
    return payload


def _assert_trace_parity_v2(client, fz, ckpt_id, messages, conv_id, live_gi=None):
    parsed = parse_internal(messages, 10)
    live_gi = list(range(len(parsed.chunks))) if live_gi is None else live_gi(parsed)
    mask = _newborn_mask(parsed, live_gi)

    dev = client.post("/v1/score/trace",
                      json=_dev_trace_payload(parsed, messages, live_gi, mask, conv_id))
    assert dev.status_code == 200, dev.text
    v1 = client.post("/v1/score/trace",
                     json=_v1_trace_payload(fz, ckpt_id, parsed, live_gi, mask, conv_id))
    assert v1.status_code == 200, v1.text
    v2 = client.post("/v1/score/trace",
                     json=_v2_trace_payload(fz, ckpt_id, parsed, live_gi, mask, conv_id))
    assert v2.status_code == 200, v2.text

    d, a, b = dev.json(), v1.json(), v2.json()
    assert b["scores_q"] == d["scores_q"], (
        f"{conv_id}: v2 diverged from dev\n dev={d['scores_q']}\n v2 ={b['scores_q']}")
    assert b["scores_q"] == a["scores_q"], (
        f"{conv_id}: v2 diverged from v1\n v1={a['scores_q']}\n v2={b['scores_q']}")
    assert b["tau_q"] == d["tau_q"] == a["tau_q"]
    assert b["checkpoint_id"] == ckpt_id
    return mask, d["scores_q"]


def test_v2_trace_parity_handcrafted(client, fz, ckpt_id):
    mask, scores = _assert_trace_parity_v2(client, fz, ckpt_id, MESSAGES, "v2-handcrafted")
    assert mask, "the handcrafted conversation must decide at least one newborn"
    assert len(set(scores)) > 1, "degenerate scores would make parity vacuous"


def test_v2_trace_parity_alive_subset(client, fz, ckpt_id):
    """Registries applied: a live subset, not the full parse — the serve-path shape."""
    _assert_trace_parity_v2(client, fz, ckpt_id, MESSAGES, "v2-subset",
                            live_gi=lambda p: [g for g in range(len(p.chunks)) if g % 3 != 1])


@pytest.mark.parametrize("turn", FIXTURE_TURNS)
def test_v2_trace_parity_golden_conversation(client, fz, ckpt_id, turn):
    if not GOLDEN_FIXTURE.is_file():
        pytest.skip(f"golden fixture absent: {GOLDEN_FIXTURE}")
    import json
    body = json.loads(GOLDEN_FIXTURE.read_text())["turns"][turn - 1]
    _assert_trace_parity_v2(client, fz, ckpt_id, to_internal(body), f"v2-golden-{turn}")


def test_v2_server_fills_dupcos(client, fz, ckpt_id):
    """The handoff itself: zeroed dupcos in must NOT equal what the server scores with.

    If the server silently accepted the client's zeros, v2 would still return 200 and would
    quietly diverge from dev. This asserts the divergence exists when the fill is bypassed —
    i.e. that cols 47-48 actually move scores on this conversation, so the parity assertions
    above are load-bearing rather than accidentally true."""
    parsed = parse_internal(MESSAGES, 10)
    live_gi = list(range(len(parsed.chunks)))
    mask = _newborn_mask(parsed, live_gi)
    lc = [parsed.chunks[g] for g in live_gi]
    _, emb = _v1_nodes(fz, lc)
    ds = fz._het_readout(lc, list(mask), parsed.recent_cmds, parsed.task_text, 0.0, emb,
                         parsed.cur_step, parsed.het_steps, parsed.T)
    real = np.asarray([_f(r) for r in ds], np.float32)
    assert np.abs(real[:, -2:]).sum() > 0, (
        "dupcos is all-zero on this conversation — pick a fixture where it is non-trivial or "
        "the v2 parity tests prove nothing about the fill")

    # v1 payload carrying the ZEROED readout = what the server would score if it never filled
    bad = _v1_trace_payload(fz, ckpt_id, parsed, live_gi, mask, "v2-dupcos-control")
    bad["decided_struct"] = [_zero_dupcos(r) for r in bad["decided_struct"]]
    unfilled = client.post("/v1/score/trace", json=bad)
    assert unfilled.status_code == 200, unfilled.text

    good = client.post("/v1/score/trace",
                       json=_v2_trace_payload(fz, ckpt_id, parsed, live_gi, mask,
                                              "v2-dupcos-real"))
    assert good.status_code == 200, good.text
    assert good.json()["scores_q"] != unfilled.json()["scores_q"], (
        "scoring with zeroed dupcos matched scoring with filled dupcos — the fill is a no-op "
        "here, so these tests cannot detect it regressing")


def test_v2_empty_cmd_head_are_zero_vectors(client, fz, ckpt_id):
    """"" must become a ZERO VECTOR, never embed(""). A v2 node with empty cmd/head must score
    identically to the v1 node that carried explicit zero vectors in those slots."""
    parsed = parse_internal(MESSAGES, 10)
    live_gi = list(range(len(parsed.chunks)))
    mask = _newborn_mask(parsed, live_gi)
    lc = [parsed.chunks[g] for g in live_gi]

    v1p = _v1_trace_payload(fz, ckpt_id, parsed, live_gi, mask, "v2-empty")
    zed = [0.0] * 1024
    for n in v1p["nodes"]:                       # force every cmd/head slot to zeros
        n["emb_cmd"] = zed
        n["emb_head"] = zed
        n["cmd_id"] = None
        n["head_id"] = None
    a = client.post("/v1/score/trace", json=v1p)
    assert a.status_code == 200, a.text

    v2p = _v2_trace_payload(fz, ckpt_id, parsed, live_gi, mask, "v2-empty")
    for n in v2p["nodes"]:                       # ...and every v2 cmd/head to ""
        n["cmd"] = ""
        n["head"] = ""
        n["cmd_id"] = None
        n["head_id"] = None
    b = client.post("/v1/score/trace", json=v2p)
    assert b.status_code == 200, b.text
    assert b.json()["scores_q"] == a.json()["scores_q"], (
        "empty cmd/head diverged from explicit zero vectors — the server is embedding \"\"")


def test_v2_tools_parity(client, fz, ckpt_id):
    """Tool-schema head: v2 schema_text must score identically to v1's precomputed emb."""
    from parsec_brain.vendored.trace_contract import tool_schema_chunks
    parsed = parse_internal(MESSAGES, 10)
    lc = parsed.chunks
    nodes, _ = _v1_nodes(fz, lc)
    specs = tool_schema_chunks(TOOLS)
    texts = [s["text"] for s in specs]
    embs = fz._embed(texts)

    common = {"conv_id": "v2-tools", "checkpoint_id": ckpt_id, "task_text": parsed.task_text}
    v1p = {"contract": "brain-api/v1", "conv_id": "v2-tools", "checkpoint_id": ckpt_id,
           "nodes": nodes, "task_emb": _f(fz._embed([parsed.task_text])[0]),
           "tools": [{"name": s["name"], "emb": _f(embs[i]),
                      "tokens": max(1, len(texts[i]) // 4)} for i, s in enumerate(specs)]}
    v2p = {"contract": "brain-api/v2", **common,
           "nodes": [_v1_node_to_v2(nodes[i], c) for i, c in enumerate(lc)],
           "tools": [{"name": s["name"], "schema_text": texts[i],
                      "tokens": max(1, len(texts[i]) // 4)} for i, s in enumerate(specs)]}

    a = client.post("/v1/score/tools", json=v1p)
    b = client.post("/v1/score/tools", json=v2p)
    assert a.status_code == 200, a.text
    assert b.status_code == 200, b.text
    assert b.json()["names"] == a.json()["names"]
    assert b.json()["scores_q"] == a.json()["scores_q"], "v2 tools diverged from v1"


def test_v2_checkpoint_mismatch_409(client, fz, ckpt_id):
    """The §8.2 matched-pair guard is the reason v2 derives from v1, not from dev — pin it."""
    parsed = parse_internal(MESSAGES, 10)
    live_gi = list(range(len(parsed.chunks)))
    mask = _newborn_mask(parsed, live_gi)
    p = _v2_trace_payload(fz, ckpt_id, parsed, live_gi, mask)
    p["checkpoint_id"] = "0" * 64
    r = client.post("/v1/score/trace", json=p)
    assert r.status_code == 409, r.text
    assert r.json()["detail"]["server_checkpoint_id"] == ckpt_id


def test_v2_schema_validates_payload(client, fz, ckpt_id):
    """The emitted payload must validate against the committed v2 schema (drift guard)."""
    jsonschema = pytest.importorskip("jsonschema")
    import json
    schema = json.loads(SCHEMA_V2.read_text())
    parsed = parse_internal(MESSAGES, 10)
    live_gi = list(range(len(parsed.chunks)))
    mask = _newborn_mask(parsed, live_gi)
    p = _v2_trace_payload(fz, ckpt_id, parsed, live_gi, mask)
    jsonschema.validate(p, {**schema, "oneOf": [schema["$defs"]["score_trace_request"]],
                            "$defs": schema["$defs"]})
