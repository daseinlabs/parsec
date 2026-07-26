"""Config parsing: task sets, run ids, the arm registry, fixture loading."""

from __future__ import annotations

import json

import pytest

import parsec_bench.arms  # noqa: F401 — registers the parsec arm
from parsec_bench.arm import ArmKind, available_arms, get_arm
from parsec_bench.cc_runner import _resolve_run_id, load_tasks
from parsec_bench.replay import load_fixture


# ── task-set parsing ──────────────────────────────────────────────────────────
def test_load_tasks_instances_dict(tmp_path):
    p = tmp_path / "tasks.json"
    p.write_text(json.dumps({"instances": ["a__1", "b__2"]}))
    assert load_tasks(str(p)) == ["a__1", "b__2"]


def test_load_tasks_bare_list_and_dict_rows(tmp_path):
    p = tmp_path / "tasks.json"
    p.write_text(json.dumps(["x__1", {"instance_id": "y__2"}]))
    assert load_tasks(str(p)) == ["x__1", "y__2"]


def test_load_tasks_bad_shape(tmp_path):
    p = tmp_path / "tasks.json"
    p.write_text(json.dumps("nope"))
    with pytest.raises(ValueError):
        load_tasks(str(p))


# ── run-id resolution ─────────────────────────────────────────────────────────
def test_run_id_explicit_wins():
    assert _resolve_run_id("t.json", ["baseline"], " rid ") == "rid"


def test_run_id_deterministic_and_arm_order_independent():
    a = _resolve_run_id("tasks_smoke.json", ["parsec", "baseline"], None)
    b = _resolve_run_id("tasks_smoke.json", ["baseline", "parsec"], None)
    assert a == b
    assert a.startswith("tasks_smoke__baseline-parsec__")
    # different arm set -> different slug+hash
    c = _resolve_run_id("tasks_smoke.json", ["baseline"], None)
    assert c != a


# ── arm registry ──────────────────────────────────────────────────────────────
def test_registry_has_baseline_and_parsec():
    names = available_arms()
    assert "baseline" in names and "parsec" in names


def test_get_arm_unknown_raises():
    with pytest.raises(KeyError):
        get_arm("no-such-arm")


def test_baseline_always_ready():
    arm = get_arm("baseline")
    ok, reason = arm.ready()
    assert ok and arm.kind == ArmKind.BASELINE
    assert arm.start_run  # per-solve lifecycle surface exists (no-op)
    assert arm.ledger_path() is None


def test_parsec_not_ready_without_brain(monkeypatch):
    monkeypatch.delenv("PARSEC_BRAIN_URL", raising=False)
    arm = get_arm("parsec")
    ok, reason = arm.ready()
    assert not ok
    assert "PARSEC_BRAIN_URL" in reason


def test_parsec_not_ready_when_brain_unreachable(monkeypatch):
    # a closed port: /health cannot answer -> precise, actionable skip reason
    monkeypatch.setenv("PARSEC_BRAIN_URL", "http://127.0.0.1:9")
    arm = get_arm("parsec")
    ok, reason = arm.ready()
    assert not ok
    assert "/health" in reason


# ── replay fixture parsing ────────────────────────────────────────────────────
def test_load_fixture_turns_shape(replay_fixture_path):
    turns = load_fixture(replay_fixture_path)
    assert len(turns) == 2
    assert all("messages" in t for t in turns)
    # turn 2 extends turn 1 (recorded-conversation prefix property)
    assert turns[1]["messages"][: len(turns[0]["messages"])] == turns[0]["messages"]


def test_load_fixture_bare_list(tmp_path):
    p = tmp_path / "f.json"
    p.write_text(json.dumps([{"messages": [{"role": "user", "content": "hi"}]}]))
    assert len(load_fixture(p)) == 1


def test_load_fixture_rejects_non_bodies(tmp_path):
    p = tmp_path / "f.json"
    p.write_text(json.dumps({"turns": [{"nope": 1}]}))
    with pytest.raises(ValueError):
        load_fixture(p)


def test_load_fixture_rejects_empty(tmp_path):
    p = tmp_path / "f.json"
    p.write_text(json.dumps({"turns": []}))
    with pytest.raises(ValueError):
        load_fixture(p)
