"""Rule + gate head endpoints (hermetic: PARSEC_EMBED_BACKEND=hash).

Same golden discipline as test_service.py: exact quantized ints pinned on the shared
handcrafted conversation, twice from cold. NEITHER head has a proxy consumer yet — the
rule tau_hint is advisory (bench calibration required before anything fires on it) and
the gate tau is the reference's locked 0.5.

Regenerate after a DELIBERATE change:  .venv/bin/python tests/test_rules_gate.py
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path

# self-contained bootstrap so the __main__ regen helper runs without pytest/conftest
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
os.environ.setdefault("PARSEC_EMBED_BACKEND", "hash")

from fastapi.testclient import TestClient

from parsec_brain.app import create_app
from test_service import MESSAGES, TOOLS

RULES = [
    {"eid": "r-verify", "text": "Before declaring the fix complete, run the failing test again "
     "and confirm it passes; never claim success from reasoning alone."},
    {"eid": "r-repro", "text": "Reproduce the reported failure with a minimal command before "
     "editing any file."},
]
BRIEF = ("Scout brief: parse_config in config.py chokes on comment lines; see "
         "tests/test_config.py::test_comments and the int() cast at line 12.")
# the fixture's tool-spec chunk pipeline (steps_of: one step per assistant->obs pair) spans
# steps {0, 1, 2}; 2 is the last step WITH a step node — the exact-match fire step.
LAST_STEP = 2

# ---- GOLDEN (generated once via the __main__ helper below, then pinned) ----
GOLDEN_RULE_SCORES_Q = {"r-verify": 49787, "r-repro": 47539}       # fire step = LAST_STEP
GOLDEN_RULE_SCORES_Q_STEP0 = {"r-verify": 281748, "r-repro": 332192}
GOLDEN_ROSTER_SCORES_Q = {"rule1": 17756, "rule2": 45687, "rule3": 50380,
                          "rule4": 15431, "rule6": 118902, "rule7": 133310}
GOLDEN_GATE_SCORE_Q = 487735
RULE_TAU_HINT_Q = 150_000
GATE_TAU_Q = 500_000


def _rules_payload(**over) -> dict:
    return {"contract": "brain-api-dev/v0", "conv_id": "test-conv", "messages": MESSAGES,
            "tools": TOOLS, "rules": RULES, "step": LAST_STEP} | over


def _gate_payload(**over) -> dict:
    return {"contract": "brain-api-dev/v0", "conv_id": "test-conv", "messages": MESSAGES,
            "tools": TOOLS, "brief": BRIEF} | over


def test_golden_rules_twice_cold():
    """Exact pinned quantized rule scores from two COLD app instances (determinism)."""
    for run in range(2):
        client = TestClient(create_app())
        r = client.post("/v1/score/rules", json=_rules_payload())
        assert r.status_code == 200, r.text
        body = r.json()
        assert body["scores_q"] == GOLDEN_RULE_SCORES_Q, f"cold run {run} diverged"
        assert body["tau_hint_q"] == RULE_TAU_HINT_Q
        assert "bench-calibrated" in body["description"]        # advisory tau stays labeled
        assert "No proxy consumer" in body["description"]
        assert len(body["checkpoint_id"]) == 64


def test_rule_fire_step_clamps_to_nearest_earlier_step_node():
    """A pure-reasoning trailing turn has NO step node (no obs pair -> no chunks at its step);
    firing there must clamp to the nearest EARLIER step node, not condition on garbage."""
    trailing = [{"role": "assistant", "content": "",
                 "reasoning_content": "The comment-line guard belongs before the int() cast."}]
    client = TestClient(create_app())
    r = client.post("/v1/score/rules",
                    json=_rules_payload(messages=MESSAGES + trailing, step=LAST_STEP + 1))
    assert r.json()["scores_q"] == GOLDEN_RULE_SCORES_Q         # == the LAST_STEP conditioning
    # sanity that the fire step actually conditions the score: an earlier step node differs
    r0 = client.post("/v1/score/rules", json=_rules_payload(step=0))
    assert r0.json()["scores_q"] == GOLDEN_RULE_SCORES_Q_STEP0
    assert r0.json()["scores_q"] != GOLDEN_RULE_SCORES_Q


def test_rules_roster_default_active_and_always_on_only():
    """`rules` omitted -> the bundle's rules.json subset with status active|always_on (the
    governor's live roster); candidate/retired eids are never scored unrequested."""
    client = TestClient(create_app())
    r = client.post("/v1/score/rules", json={k: v for k, v in _rules_payload().items()
                                             if k != "rules"})
    assert r.status_code == 200, r.text
    assert r.json()["scores_q"] == GOLDEN_ROSTER_SCORES_Q
    assert sorted(r.json()["scores_q"]) == ["rule1", "rule2", "rule3", "rule4", "rule6", "rule7"]


def test_rules_fail_open_and_validation():
    client = TestClient(create_app())
    # no tools -> build_tool_spec is None -> fail-open {} (caller fires ungated)
    r = client.post("/v1/score/rules", json=_rules_payload(tools=[]))
    assert r.status_code == 200 and r.json()["scores_q"] == {}
    # explicit empty candidate list -> same fail-open shape
    r = client.post("/v1/score/rules", json=_rules_payload(rules=[]))
    assert r.status_code == 200 and r.json()["scores_q"] == {}
    # malformed candidate -> 422, never a silent mis-score
    r = client.post("/v1/score/rules", json=_rules_payload(rules=[{"eid": "x"}]))
    assert r.status_code == 422


def test_golden_gate_twice_cold():
    for run in range(2):
        client = TestClient(create_app())
        r = client.post("/v1/score/gate", json=_gate_payload())
        assert r.status_code == 200, r.text
        body = r.json()
        assert body["score_q"] == GOLDEN_GATE_SCORE_Q, f"cold run {run} diverged"
        assert body["tau_q"] == GATE_TAU_Q
        assert body["fire"] is (GOLDEN_GATE_SCORE_Q >= GATE_TAU_Q)
        assert body["fail_open"] is False
        assert len(body["checkpoint_id"]) == 64


def test_gate_empty_brief_fail_shape():
    """No brief -> nothing to gate -> fail open (serve = fire), measurably flagged."""
    client = TestClient(create_app())
    for brief in ("", "   "):
        r = client.post("/v1/score/gate", json=_gate_payload(brief=brief))
        assert r.status_code == 200, r.text
        assert r.json() == {"score_q": 1_000_000, "tau_q": GATE_TAU_Q, "fire": True,
                            "fail_open": True, "checkpoint_id": r.json()["checkpoint_id"]}


def test_scoped_env_toggles_do_not_leak_into_trace_scoring():
    """AC_RULE/AC_GATE are graph-SHAPE flags: they must be on only inside assemble_trace.
    After rule+gate calls on the SAME app, the trace path must still serve its golden ints."""
    from test_service import GOLDEN_SCORES_Q, _trace_payload
    client = TestClient(create_app())
    client.post("/v1/score/rules", json=_rules_payload())
    client.post("/v1/score/gate", json=_gate_payload())
    assert os.environ.get("AC_RULE") is None and os.environ.get("AC_GATE") is None
    r = client.post("/v1/score/trace", json=_trace_payload())
    assert r.json()["scores_q"] == GOLDEN_SCORES_Q


def test_bearer_auth_covers_rule_and_gate(monkeypatch):
    monkeypatch.setenv("PARSEC_BRAIN_KEY", "sekrit")
    client = TestClient(create_app())
    assert client.post("/v1/score/rules", json=_rules_payload()).status_code == 401
    assert client.post("/v1/score/gate", json=_gate_payload()).status_code == 401
    hdrs = {"Authorization": "Bearer sekrit"}
    assert client.post("/v1/score/rules", json=_rules_payload(), headers=hdrs).status_code == 200
    assert client.post("/v1/score/gate", json=_gate_payload(), headers=hdrs).status_code == 200


if __name__ == "__main__":                       # golden regeneration helper
    client = TestClient(create_app())
    rules = client.post("/v1/score/rules", json=_rules_payload()).json()
    rules0 = client.post("/v1/score/rules", json=_rules_payload(step=0)).json()
    roster = client.post("/v1/score/rules", json={k: v for k, v in _rules_payload().items()
                                                  if k != "rules"}).json()
    gate = client.post("/v1/score/gate", json=_gate_payload()).json()
    print(json.dumps({
        "GOLDEN_RULE_SCORES_Q": rules["scores_q"],
        "GOLDEN_RULE_SCORES_Q_STEP0": rules0["scores_q"],
        "GOLDEN_ROSTER_SCORES_Q": roster["scores_q"],
        "GOLDEN_GATE_SCORE_Q": gate["score_q"],
    }, indent=1))
