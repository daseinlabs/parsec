"""Replay mode end-to-end: the 2-turn synthetic fixture through a REAL arm
proxy (the parsec binary, passthrough mode — no brain needed) + the mock
upstream. This is the machinery smoke: proxy spawn, 1:1 forwarding, ledger
rows, and the counterfactual-vs-served math on one consistent chars/4 scale.
"""

from __future__ import annotations

import json

import pytest

from parsec_bench.replay import ReplayReport, main as replay_main, replay

pytestmark = pytest.mark.filterwarnings("ignore::ResourceWarning")


@pytest.fixture(scope="module")
def report(request) -> ReplayReport:
    fixture = request.path.parent / "fixtures" / "replay_2turn.json"
    # brain_url=None -> passthrough-shaped curation: the full proxy machinery
    # (wire freeze, folds, count_tokens probe, ledger) with no cuts.
    return replay(fixture, brain_url=None)


def test_replay_runs_both_turns(report):
    assert report.arm == "baseline"
    assert len(report.turns) == 2
    assert [t.turn for t in report.turns] == [0, 1]


def test_passthrough_forwards_everything(report):
    for t in report.turns:
        # no tool pruning without a brain
        assert t.tools_original == 4
        assert t.tools_forwarded == 4
        assert t.tools_pruned == 0
        # forwarded body ~= original (the proxy only places cache anchors);
        # anchors add bytes, so the delta is small and non-positive.
        assert t.forwarded_bytes >= t.original_bytes
        assert t.forwarded_bytes - t.original_bytes < 500
    assert report.tools_pruned_total == 0


def test_counterfactual_vs_served_math(report):
    # mock scale: count_tokens(original) = orig_bytes//4, billed input =
    # fwd_bytes//4, cache split 0 — one consistent estimate scale.
    for t in report.turns:
        assert t.counterfactual_input_tokens == max(1, t.original_bytes // 4)
        assert t.served_input_tokens == max(1, t.forwarded_bytes // 4)
        assert t.est_tokens_saved == t.counterfactual_input_tokens - t.served_input_tokens
        assert not t.fail_open
    # §8.4 rollup: every turn was probed (mock count_tokens always answers)
    assert report.probed_turns == 2
    assert report.null_probe_turns == 0
    assert report.est_tokens_saved == sum(t.est_tokens_saved for t in report.turns)
    assert report.fail_opens == 0


def test_growing_conversation(report):
    # turn 2 extends turn 1, so both sides of the wire must grow
    assert report.turns[1].original_bytes > report.turns[0].original_bytes
    assert report.turns[1].forwarded_bytes > report.turns[0].forwarded_bytes


def test_report_json_roundtrip(report):
    d = report.to_json()
    assert d["arm"] == "baseline"
    assert len(d["turns"]) == 2
    json.dumps(d)  # fully serializable


def test_cli_writes_json(tmp_path, replay_fixture_path, capsys):
    out = tmp_path / "report.json"
    rc = replay_main(["--fixture", str(replay_fixture_path),
                      "--json", str(out)])
    assert rc == 0
    d = json.loads(out.read_text())
    assert len(d["turns"]) == 2
    printed = capsys.readouterr().out
    assert "est saved" in printed
    assert "§8.4" in printed
