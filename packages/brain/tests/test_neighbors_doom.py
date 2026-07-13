"""Neighbors (hoods) + doom head + v1 rules + logging (hermetic: hash embed backend).

The synthetic hoods artifact is generated DETERMINISTICALLY inline (hash-backend embeddings
of fixed strings — no unseeded randomness, <100KB) so every invariant is reproducible:

  - DASEIN_HOODS_PKL unset  -> bit-identical v0 serving (the committed goldens still hold);
  - set-but-missing/corrupt -> the bundle REFUSES TO START (train/serve-skew guard);
  - mounted                 -> /v1/neighbors serves the cost median (null under 4 samples),
                               blocks attach into the trace graph (scores move — sanity),
                               and dev == v1 parity STILL holds with blocks attached;
  - gf on a trace request   -> doom_q on BOTH contracts (identical — same graph, same gf);
                               absent without gf;
  - /v1/score/rules         -> v1 contract scores the server roster, parity with dev;
  - logging                 -> request lines present, fail-opens counted, no raw text.
"""
from __future__ import annotations

import json
import logging
import os
import pickle
import sys
from pathlib import Path

# self-contained bootstrap (same pattern as test_service.py)
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
os.environ.setdefault("DASEIN_EMBED_BACKEND", "hash")

import numpy as np
import pytest
from fastapi.testclient import TestClient

from dasein_brain._log import COUNTERS
from dasein_brain.app import create_app
from dasein_brain.bundle import BundleError, load_bundle
from dasein_brain.scorer import TraceScorer, parse_internal
from dasein_brain.vendored.embedding import EmbeddingClient
from dasein_brain.vendored.pyg_model import edges
from test_service import (GOLDEN_MASK, GOLDEN_SCORES_Q, GOLDEN_TAU_Q, MESSAGES, TOOLS,
                          _trace_payload)
from test_rules_gate import GOLDEN_ROSTER_SCORES_Q, LAST_STEP
from test_v1 import _f, _v1_trace_payload, _newborn_mask, _ZED

GF = [0.5, 0.44, 0.17, 0.83]        # a plausible loop_feats vector (any 4 floats in [-1, 1])
GRID = 1_000_000


def _hash_embed(texts: list[str]) -> list[np.ndarray]:
    """The exact vectors the hash backend serves (content-addressed: any client agrees)."""
    ec = EmbeddingClient(cfg={"models": {"embedder": {"dim": 1024, "backend": "hash"}}},
                         backend="hash")
    return [np.asarray(v, dtype=np.float32) for v in ec.embed(texts, as_query=False)]


def _make_hoods(path: Path, n_costs: int = 5) -> Path:
    """Deterministic synthetic artifact: 5 anchors, 2-node blocks, n_costs cost entries.
    anchor-00's task_emb IS the golden conversation's task text embedding, so the anchor
    search has an exact-cosine winner on the fixture conversation."""
    golden_task = parse_internal(MESSAGES, 10).task_text
    task_texts = [golden_task] + [f"synthetic anchor task {i}: refactor module m{i}"
                                  for i in range(1, 5)]
    iids = [f"anchor-{i:02d}" for i in range(5)]
    task_embs = {iid: v for iid, v in zip(iids, _hash_embed(task_texts))}
    blocks = {}
    for i, iid in enumerate(iids):
        bemb = np.stack(_hash_embed([f"trace anchor outcome {i}", f"neighbor chunk {i}"]))
        extras = np.asarray([[1.0, 0.0, 1.0], [1.0, 1.0, 0.0]], dtype=np.float32)
        # block-internal edges: membership (rel 5) chunk->anchor + temporal (rel 1)
        blocks[iid] = (bemb, extras, [1], [0], [5])
    cost = {iid: 100.0 * (i + 1) for i, iid in enumerate(iids[:n_costs])}
    path.write_bytes(pickle.dumps({"task_embs": task_embs, "blocks": blocks, "cost": cost}))
    assert path.stat().st_size < 100_000, "fixture must stay small"
    return path


@pytest.fixture(scope="module")
def hoods_pkl(tmp_path_factory):
    return _make_hoods(tmp_path_factory.mktemp("hoods") / "hoods_test.pkl")


@pytest.fixture(scope="module")
def hoods_client(hoods_pkl):
    """An app whose bundle mounted the synthetic artifact (env scoped to construction)."""
    os.environ["DASEIN_HOODS_PKL"] = str(hoods_pkl)
    try:
        client = TestClient(create_app())
    finally:
        del os.environ["DASEIN_HOODS_PKL"]
    return client


@pytest.fixture(scope="module")
def client():
    return TestClient(create_app())     # hoods OFF — today's serving


@pytest.fixture(scope="module")
def fz():
    return TraceScorer(load_bundle())   # featurizer for v1 payloads (hash = content-addressed)


@pytest.fixture(scope="module")
def ckpt_id(fz):
    return fz.bundle.checkpoint_id


# ---- bundle guard: set-but-missing/corrupt hoods refuses to start ------------------------------
def test_bundle_refuses_missing_or_corrupt_hoods(tmp_path, monkeypatch):
    monkeypatch.setenv("DASEIN_HOODS_PKL", str(tmp_path / "nope.pkl"))
    with pytest.raises(BundleError, match="MISSING"):
        load_bundle()
    bad = tmp_path / "corrupt.pkl"
    bad.write_bytes(b"not a pickle")
    monkeypatch.setenv("DASEIN_HOODS_PKL", str(bad))
    with pytest.raises(BundleError, match="unreadable"):
        load_bundle()
    shaped = tmp_path / "badshape.pkl"
    shaped.write_bytes(pickle.dumps({"task_embs": {"a": np.zeros(8, np.float32)}, "blocks": {}}))
    monkeypatch.setenv("DASEIN_HOODS_PKL", str(shaped))
    with pytest.raises(BundleError, match="shape"):
        load_bundle()


def test_bundle_refuses_malformed_block_edges(tmp_path, monkeypatch):
    """Value-range validation at LOAD (adversarial-review finding): an artifact with
    out-of-range node indices or a foreign edge relation would pass a lengths-only check
    and instead crash inside attach_blocks on EVERY trace forward — the silent-degradation
    shape the loud loader exists to prevent."""
    def _art(block):
        return {"task_embs": {"a": _hash_embed(["some task"])[0]},
                "blocks": {"a": block}, "cost": {}}

    bemb = np.stack(_hash_embed(["anchor outcome", "chunk body"]))   # M=2: valid rows 0..1
    extras = np.zeros((2, 3), np.float32)
    cases = [
        ((bemb, extras, [2], [0], [5]), "out of range"),     # (a) src >= M
        ((bemb, extras, [1], [-1], [5]), "out of range"),    #     negative dst
        ((bemb, extras, [1], [0], [6]), "relation"),         # (b) rel 6 is attach-time only
        ((bemb, extras, [1], [0], [0]), "relation"),         #     rel 0 never block-internal
        ((bemb, extras, [1], [0], ["x"]), "non-integer"),    #     junk typ array
    ]
    for i, (block, msg) in enumerate(cases):
        p = tmp_path / f"bad{i}.pkl"
        p.write_bytes(pickle.dumps(_art(block)))
        monkeypatch.setenv("DASEIN_HOODS_PKL", str(p))
        with pytest.raises(BundleError, match=msg):
            load_bundle()
    # empty edge arrays are LEGAL (a single-node block has no internal edges)
    ok = tmp_path / "ok.pkl"
    ok.write_bytes(pickle.dumps(_art((bemb, extras, [], [], []))))
    monkeypatch.setenv("DASEIN_HOODS_PKL", str(ok))
    b = load_bundle()
    assert b.hoods is not None and len(b.hoods._blocks) == 1


def test_bundle_refuses_ckpt_without_doom_head(tmp_path):
    """A doom-headless checkpoint cannot serve (PyGCurator always constructs the head; the
    strict state-dict check would reject it) — the loader must say so with a real message,
    never a raw KeyError."""
    import torch
    from conftest import CKPT
    ck = torch.load(str(CKPT), map_location="cpu", weights_only=False)
    for k in [k for k in ck["state"] if k.startswith("doom_head.")]:
        del ck["state"][k]
    bad = tmp_path / "nodoom.pt"
    torch.save(ck, str(bad))
    with pytest.raises(BundleError, match="doom_head"):
        load_bundle(ckpt_path=str(bad))


# ---- /v1/neighbors -----------------------------------------------------------------------------
def test_neighbors_endpoint_dev(hoods_client):
    task_text = parse_internal(MESSAGES, 10).task_text
    r = hoods_client.post("/v1/neighbors", json={
        "contract": "brain-api-dev/v0", "conv_id": "nbr-dev", "task_text": task_text})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["neighbors_active"] is True
    assert body["nbr_count"] == 5                       # all 5 anchors carry cost (k=16 > 5)
    assert body["nbr_cost_median"] == 300.0             # median(100..500)
    assert len(body["checkpoint_id"]) == 64
    # bundle info reflects the mounted artifact
    b = hoods_client.get("/v1/bundle").json()
    assert b["neighbors"] is True and b["hoods_anchors"] == 5
    # empty task text: no anchor query possible -> inert
    r = hoods_client.post("/v1/neighbors", json={
        "contract": "brain-api-dev/v0", "conv_id": "nbr-dev", "task_text": ""})
    assert r.json()["nbr_cost_median"] is None and r.json()["nbr_count"] == 0


def test_neighbors_endpoint_v1(hoods_client, fz, ckpt_id):
    task_text = parse_internal(MESSAGES, 10).task_text
    vec = _f(np.asarray(fz._embed([task_text])[0], np.float32))
    r = hoods_client.post("/v1/neighbors", json={
        "contract": "brain-api/v1", "conv_id": "nbr-v1",
        "checkpoint_id": ckpt_id, "task_vec": vec})
    assert r.status_code == 200, r.text
    assert r.json()["nbr_cost_median"] == 300.0 and r.json()["nbr_count"] == 5
    r = hoods_client.post("/v1/neighbors", json={
        "contract": "brain-api/v1", "conv_id": "nbr-v1",
        "checkpoint_id": "0" * 64, "task_vec": vec})
    assert r.status_code == 409                          # checkpoint handshake, like score/trace


def test_neighbors_median_needs_four_costs(tmp_path):
    os.environ["DASEIN_HOODS_PKL"] = str(_make_hoods(tmp_path / "h3.pkl", n_costs=3))
    try:
        client3 = TestClient(create_app())
    finally:
        del os.environ["DASEIN_HOODS_PKL"]
    r = client3.post("/v1/neighbors", json={
        "contract": "brain-api-dev/v0", "conv_id": "nbr-3",
        "task_text": parse_internal(MESSAGES, 10).task_text})
    assert r.json()["nbr_cost_median"] is None           # 3 < 4: runaway stays inert
    assert r.json()["nbr_count"] == 3


def test_neighbors_hoods_off_inert(client):
    r = client.post("/v1/neighbors", json={
        "contract": "brain-api-dev/v0", "conv_id": "nbr-off", "task_text": "anything"})
    assert r.json() == {"nbr_cost_median": None, "nbr_count": 0, "neighbors_active": False,
                        "checkpoint_id": r.json()["checkpoint_id"]}


# ---- doom head (gf -> doom_q) ------------------------------------------------------------------
def test_trace_gf_doom_dev_and_v1_parity(client, fz, ckpt_id):
    # no gf -> exactly today's response (golden ints, no doom_q)
    base = client.post("/v1/score/trace", json=_trace_payload()).json()
    assert base["scores_q"] == GOLDEN_SCORES_Q and "doom_q" not in base
    # gf -> same scores (doom rides the same forward, touches nothing) + doom_q
    dev = client.post("/v1/score/trace", json=_trace_payload() | {"gf": GF}).json()
    assert dev["scores_q"] == GOLDEN_SCORES_Q and dev["tau_q"] == GOLDEN_TAU_Q
    assert isinstance(dev["doom_q"], int) and 0 <= dev["doom_q"] <= GRID
    # a different trajectory conditions the head (gf is load-bearing)
    hot = client.post("/v1/score/trace",
                      json=_trace_payload() | {"gf": [1.0, 1.0, 0.5, 1.0]}).json()
    assert hot["doom_q"] != dev["doom_q"]
    # v1: same conversation, same gf -> the SAME doom_q (cross-contract parity)
    parsed = parse_internal(MESSAGES, 10)
    gi = list(range(len(parsed.chunks)))
    p = _v1_trace_payload(fz, ckpt_id, parsed, gi, _newborn_mask(parsed, gi), "doom-v1")
    v1 = client.post("/v1/score/trace", json=p | {"gf": GF}).json()
    assert v1["scores_q"] == GOLDEN_SCORES_Q
    assert v1["doom_q"] == dev["doom_q"]
    v1_no = client.post("/v1/score/trace", json=p).json()
    assert "doom_q" not in v1_no
    # schema bounds enforced: wrong arity / out-of-range values never reach the head
    assert client.post("/v1/score/trace",
                       json=_trace_payload() | {"gf": [0.1, 0.2, 0.3]}).status_code == 422
    assert client.post("/v1/score/trace",
                       json=_trace_payload() | {"gf": [0.1, 0.2, 0.3, 2.0]}).status_code == 422


# ---- neighbors wired into the trace forward ----------------------------------------------------
def test_nf_on_moves_scores_nf_off_stays_golden(client, hoods_client):
    off = client.post("/v1/score/trace", json=_trace_payload()).json()
    assert off["scores_q"] == GOLDEN_SCORES_Q            # committed goldens: bit-identical
    on = hoods_client.post("/v1/score/trace", json=_trace_payload()).json()
    assert on["tau_q"] == GOLDEN_TAU_Q                   # tau is per-ckpt, not per-graph
    assert len(on["scores_q"]) == len(GOLDEN_SCORES_Q)
    assert [s for i, s in enumerate(on["scores_q"]) if i not in GOLDEN_MASK] == \
           [GRID] * (len(GOLDEN_SCORES_Q) - len(GOLDEN_MASK))   # non-mask rows still never-cut
    assert on["scores_q"] != GOLDEN_SCORES_Q, "blocks attached but scores did not move"


def test_nf_on_dev_v1_parity(hoods_client, fz, ckpt_id):
    """dev == v1 must SURVIVE neighbors: both paths query the same anchors with the same task
    vector and attach the same blocks at the same graph position."""
    parsed = parse_internal(MESSAGES, 10)
    gi = list(range(len(parsed.chunks)))
    mask = _newborn_mask(parsed, gi)
    dev = hoods_client.post("/v1/score/trace", json=_trace_payload() | {"gf": GF}).json()
    p = _v1_trace_payload(fz, ckpt_id, parsed, gi, mask, "nf-v1")
    v1 = hoods_client.post("/v1/score/trace", json=p | {"gf": GF}).json()
    assert v1["scores_q"] == dev["scores_q"]
    assert v1["doom_q"] == dev["doom_q"]                 # doom pools own rows only, gf equal


# ---- /v1/score/rules on the v1 contract ---------------------------------------------------------
def _v1_rules_payload(fz, ckpt_id, internal, tools, step, conv_id="rules-v1"):
    """Featurize the TOOL-SPEC pipeline view (the same view the v1 tools request carries) —
    the dev rules path scores over build_tool_spec's chunks, not parse_internal's."""
    from dasein_brain.vendored.trace_graph import build_tool_spec
    from test_v1 import _v1_nodes
    spec = build_tool_spec(internal, tools, "serve")
    assert spec is not None
    nodes, emb = _v1_nodes(fz, spec["chunks"])
    ei, et = edges(spec["chunks"], emb, causal=True)
    sup = [[int(a), int(b)] for a, b, t in
           zip(ei[0].tolist(), ei[1].tolist(), et.tolist()) if t == 4]
    payload = {
        "contract": "brain-api/v1",
        "conv_id": conv_id,
        "checkpoint_id": ckpt_id,
        "cur_step": int(step),
        "nodes": nodes,
        "task_emb": _f(np.asarray(fz._embed([spec["task_text"]])[0], np.float32)
                       if spec["task_text"] else _ZED),
        "edges_supersession": sup,
    }
    st = (spec.get("sys_text", "") or "")[:2000]
    if st:
        payload["sys_emb"] = _f(np.asarray(fz._embed([st])[0], np.float32))
    return payload


def test_v1_rules_parity_with_dev(client, fz, ckpt_id):
    dev = client.post("/v1/score/rules", json={
        "contract": "brain-api-dev/v0", "conv_id": "rules-dev", "messages": MESSAGES,
        "tools": TOOLS, "step": LAST_STEP}).json()
    assert dev["scores_q"] == GOLDEN_ROSTER_SCORES_Q     # the committed dev golden still holds
    v1 = client.post("/v1/score/rules",
                     json=_v1_rules_payload(fz, ckpt_id, MESSAGES, TOOLS, LAST_STEP)).json()
    assert v1["scores_q"] == dev["scores_q"], (
        f"v1 rules diverged\n dev={dev['scores_q']}\n v1 ={v1['scores_q']}")
    assert v1["rules"] == dev["rules"]                   # server-owned text + p_q + fire_step
    assert v1["tau_hint_q"] == dev["tau_hint_q"] == 150_000


def test_rules_response_carries_text_p_q_fire_step(client):
    body = client.post("/v1/score/rules", json={
        "contract": "brain-api-dev/v0", "conv_id": "rules-fields", "messages": MESSAGES,
        "tools": TOOLS, "step": LAST_STEP}).json()
    assert len(body["rules"]) == len(body["scores_q"]) == 6      # active+always_on roster
    for r in body["rules"]:
        assert r["eid"] in body["scores_q"]
        assert r["p_q"] == body["scores_q"][r["eid"]]
        assert r["fire_step"] == LAST_STEP
        assert isinstance(r["text"], str) and len(r["text"]) > 20   # the directive text rides
    # fail-open keeps the additive field consistent: scores_q == {} -> rules == []
    fo = client.post("/v1/score/rules", json={
        "contract": "brain-api-dev/v0", "conv_id": "rules-fo", "messages": MESSAGES,
        "tools": [], "step": 0}).json()
    assert fo["scores_q"] == {} and fo["rules"] == []


def test_v1_rules_guards(client, fz, ckpt_id):
    p = _v1_rules_payload(fz, ckpt_id, MESSAGES, TOOLS, LAST_STEP)
    assert client.post("/v1/score/rules",
                       json=p | {"checkpoint_id": "0" * 64}).status_code == 409
    assert client.post("/v1/score/rules", json=p | {"nodes": []}).status_code == 422
    n = len(p["nodes"])
    assert client.post("/v1/score/rules",
                       json=p | {"edges_supersession": [[0, n]]}).status_code == 422


# ---- logging -----------------------------------------------------------------------------------
def test_request_log_lines_and_no_raw_text(client, caplog):
    with caplog.at_level(logging.INFO, logger="dasein_brain"):
        client.post("/v1/score/trace", json=_trace_payload() | {"gf": GF})
        client.post("/v1/neighbors", json={
            "contract": "brain-api-dev/v0", "conv_id": "log-smoke",
            "task_text": parse_internal(MESSAGES, 10).task_text})
    msgs = [r.getMessage() for r in caplog.records]
    trace_lines = [m for m in msgs if m.startswith("score/trace contract=dev")]
    assert trace_lines, msgs
    line = trace_lines[-1]
    for field in ("conv=", "n_chunks=", "tau_q=", "doom_q=", "forward_ms=", "status=200"):
        assert field in line, line
    assert any(m.startswith("neighbors contract=dev") for m in msgs)
    # the data-plane rule applies to logs: no message text / task text / conv id in the clear
    blob = "\n".join(msgs)
    for raw in ("parse_config", "config.py", "comment lines", "test-conv", "log-smoke"):
        assert raw not in blob, f"raw text {raw!r} leaked into logs"


def test_fail_open_counted_on_health(client, monkeypatch):
    def _boom(*a, **k):
        raise RuntimeError("boom")
    monkeypatch.setattr("dasein_brain.vendored.trace_graph.build_tool_spec", _boom)
    before = COUNTERS["fail_opens"]
    r = client.post("/v1/score/tools", json={
        "contract": "brain-api-dev/v0", "conv_id": "fo", "messages": MESSAGES, "tools": TOOLS})
    assert r.status_code == 200 and r.json()["names"] == []      # fail-open shape served
    h = client.get("/health").json()
    assert h["fail_opens"] == COUNTERS["fail_opens"] >= before + 1
    assert h["requests"] >= 1
