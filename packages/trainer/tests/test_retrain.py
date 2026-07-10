"""The identity retrain loop, end to end: the promoted bundle must load through
the brain's own self-validation and score EXACTLY like the base GNN — that is
the whole point of the stub (same GNN out, pipeline shape real)."""

import json

import pytest

from dasein_trainer.retrain import collect, eval_gate, promote, retrain


def test_identity_retrain_end_to_end(base_ckpt, tmp_path):
    result = retrain(base_ckpt, tmp_path / "bundles", version="test-identity")
    m = result.manifest

    assert m["trained"] is False
    assert m["checkpoint_id"] == m["base_checkpoint_id"]  # identity: same bytes
    assert m["eval"]["passed"] and m["eval"]["identical_to_base"]
    assert (result.bundle_dir / "curator.pt").is_file()
    assert (result.bundle_dir / "rules.json").is_file()
    assert json.loads((result.bundle_dir / "bundle.json").read_text())["version"] == "test-identity"

    # The loop closure: the brain loads the PROMOTED artifact (its own rules/
    # changeprone copies) and reproduces the gate's exact quantized scores.
    from dasein_trainer.retrain import _score

    served = _score(result.bundle_dir / "curator.pt")
    assert served["scores_q"] == m["eval"]["scores_q"]
    assert served["tau_q"] == m["eval"]["tau_q"]
    assert served["checkpoint_id"] == m["checkpoint_id"]


def test_promote_refuses_version_overwrite(base_ckpt, tmp_path):
    out = tmp_path / "bundles"
    retrain(base_ckpt, out, version="v-dup")
    with pytest.raises(FileExistsError):
        promote(base_ckpt, out, "v-dup", base_ckpt, {}, {"passed": True})


def test_eval_gate_rejects_corruption(base_ckpt, tmp_path):
    bad = tmp_path / "corrupt.pt"
    bad.write_bytes(base_ckpt.read_bytes()[: 1 << 20])  # truncated = unloadable
    with pytest.raises(Exception):
        eval_gate(bad, base_ckpt)


def test_collect_inventories_capture_rows(tmp_path):
    spool = tmp_path / "corpus"
    spool.mkdir()
    rows = [
        {"contract_version": "savings-ledger/v0", "request_id": "req_" + "0" * 32},
        {"contract_version": "savings-ledger/v0", "request_id": "req_" + "1" * 32},
        {"contract_version": "tcv2-community/v0"},
    ]
    (spool / "a.jsonl").write_text("\n".join(json.dumps(r) for r in rows) + "\nnot json\n")
    report = collect(spool)
    assert report["ledger_rows"] == 2
    assert report["tcv2_rows"] == 1
    assert collect(None) == {"dir": None, "ledger_rows": 0, "tcv2_rows": 0}
