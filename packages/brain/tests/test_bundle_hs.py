"""HS load assert (bundle_hs.check_state) over synthetic state dicts — hermetic: no checkpoint file,
no encoder download. The real-checkpoint load is exercised by setting PARSEC_HS_CKPT (skipped
otherwise)."""
import os

import pytest
import torch

from parsec_brain.bundle import BundleError
from parsec_brain.bundle_hs import (EXPECT_ENCODER_TENSORS, EXPECT_TENSORS, check_state,
                                    load_hs_bundle)

# Nothing here needs the legacy bge checkpoint the conftest gates on.
pytestmark = pytest.mark.no_ckpt


def _state(**over):
    """A state dict with the HS checkpoint's head shapes, 320 encoder tensors, and filler params up
    to 378 total. `over` replaces or (value None) deletes keys."""
    E = lambda *s: torch.empty(*s)  # noqa: E731
    sd = {
        "emb_proj.weight": E(128, 1024),
        "in_proj.weight": E(384, 234),
        "convs.0.weight": E(14, 384, 384),
        "convs.1.weight": E(14, 384, 384),
        "convs.2.weight": E(14, 384, 384),
        "head.0.weight": E(384, 384),
        "doom_head.0.weight": E(384, 388),
        "read_head.0.weight": E(384, 488),
        "cand_head.0.weight": E(384, 778),
        "perx_head.Q.weight": E(8, 488),
        "perx_head.gate.weight": E(8, 488),
        "perx_head.b": E(1),
    }
    for i in range(EXPECT_ENCODER_TENSORS):
        sd[f"encpool.enc.backbone.p{i}"] = E(1)
    i = 0
    while len(sd) < EXPECT_TENSORS:
        sd[f"filler.{i}"] = E(1)
        i += 1
    for k, v in over.items():
        if v is None:
            sd.pop(k, None)
        else:
            sd[k] = v
    return sd


def test_accepts_the_trained_shapes():
    d = check_state(_state())
    assert (d["hidden"], d["layers"], d["nrel"]) == (384, 3, 14)
    assert (d["emb_dim"], d["node_struct"], d["read_struct"]) == (1024, 106, 104)
    assert (d["doom_gf"], d["cand_struct_w"], d["perx_k"]) == (4, 10, 8)


def test_refuses_a_checkpoint_without_the_encoder():
    sd = {k: v for k, v in _state().items() if not k.startswith("encpool.")}
    with pytest.raises(BundleError, match="state tensors"):
        check_state(sd)


def test_refuses_a_partial_encoder():
    sd = _state()
    sd.pop("encpool.enc.backbone.p0")
    sd["filler.extra"] = torch.empty(1)          # keep the total at 378
    with pytest.raises(BundleError, match="encoder tensors"):
        check_state(sd)


def test_refuses_the_wrong_node_struct_width():
    with pytest.raises(BundleError, match="in_proj.weight"):
        check_state(_state(**{"in_proj.weight": torch.empty(384, 128 + 56)}))


def test_refuses_a_dropped_perx_head():
    sd = _state(**{"perx_head.Q.weight": None})
    sd["filler.extra"] = torch.empty(1)
    with pytest.raises(BundleError, match="perx_head.Q.weight"):
        check_state(sd)


def test_refuses_a_mismatched_expert_count(monkeypatch):
    monkeypatch.setenv("AC_PERX_K", "4")
    with pytest.raises(BundleError, match="AC_PERX_K"):
        check_state(_state())


@pytest.mark.skipif(not os.environ.get("PARSEC_HS_CKPT"),
                    reason="PARSEC_HS_CKPT not set (real checkpoint + encoder download)")
def test_real_checkpoint_loads_strictly():
    b = load_hs_bundle()
    assert len(b.model.state_dict()) == EXPECT_TENSORS
    assert (b.node_struct, b.read_struct, b.perx_k) == (106, 104, 8)
    assert not b.model.training
