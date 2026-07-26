"""Bundle self-validation: loads the real ckpt, refuses tampered configs (§8.2 — any
derived-vs-recorded mismatch is a startup error, never a silently-wrong score)."""
from __future__ import annotations

import hashlib

import pytest
import torch

from conftest import CKPT
from parsec_brain.bundle import BundleError, load_bundle


def test_loads_real_ckpt():
    b = load_bundle()
    assert (b.hidden, b.layers, b.nrel) == (384, 3, 14)
    assert (b.read_struct, b.struct_dim, b.emb_dim, b.doom_gf) == (49, 50, 3072, 4)
    assert b.chunk_lines == 10
    assert b.calib_tau == pytest.approx(0.31526511907577515)   # tau_calib["0.70"], this ckpt only
    assert b.tau_q == 315265
    assert b.checkpoint_id == hashlib.sha256(CKPT.read_bytes()).hexdigest()
    assert len(b.rules) == 16 and all(r.get("eid") and r.get("text") for r in b.rules)
    assert b.changeprone is not None                            # models/changeprone.pkl is tracked
    assert b.flags["AC_HUBPROPS"] == "off" and b.flags["AC_CHUNK_MODE"] == "fixed"


def test_nearest_key_tau_resolution(monkeypatch):
    monkeypatch.setenv("PARSEC_TARGET_COV", "0.72")             # no exact key -> nearest (0.70)
    b = load_bundle()
    assert b.calib_tau == pytest.approx(0.31526511907577515)
    monkeypatch.setenv("PARSEC_TARGET_COV", "0.90")
    assert load_bundle().calib_tau == pytest.approx(0.24759453535079956)


@pytest.mark.parametrize("tamper", [
    {"hidden": 256},            # recorded config disagrees with the weights
    {"chunk_lines": 40},        # chunker granularity != engine parity pin
    {"chunk_mode": "cst"},
    {"hetgraph": False},
    {"trace_level": False},
    {"tau_calib": None},        # no calibration table -> no serving tau
    {"head_kind": "plr_cross"},  # wrong read head shape
])
def test_rejects_tampered_config(tmp_path, tamper):
    ck = torch.load(str(CKPT), map_location="cpu", weights_only=False)
    ck.update(tamper)
    bad = tmp_path / "tampered.pt"
    torch.save(ck, str(bad))
    with pytest.raises(BundleError):
        load_bundle(ckpt_path=str(bad))


def test_rejects_missing_ckpt(tmp_path):
    with pytest.raises(BundleError, match="not found"):
        load_bundle(ckpt_path=str(tmp_path / "nope.pt"))
