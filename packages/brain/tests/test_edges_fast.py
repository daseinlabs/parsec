"""Parity pin for AC_EDGES_FAST: the vectorized rel-0 (file chain) and
rel-2 (causal kNN) twins in vendored/pyg_model.edges() must be BIT-IDENTICAL
to the reference pure-Python loops — same candidate sets, same (lo, index)
tie-breaks, same np.argsort permutation on the same float32 arrays. A wrong
edge is a silently-wrong score, so this pins fast == slow on randomized
inputs across the flag combinations the serve path can see."""

from __future__ import annotations

import random
import types

import pytest

pytestmark = pytest.mark.no_ckpt


def _chunks(rng: random.Random, n: int) -> list:
    files = ["a.py", "b.py", "c.rs", None, ""]
    out = []
    step = 0
    for i in range(n):
        if rng.random() < 0.4:
            step += 1
        f = files[rng.randrange(len(files))]
        lo = rng.randrange(1, 400) if (f and rng.random() < 0.8) else None
        out.append(types.SimpleNamespace(
            step=step, file=f, lo=lo, hi=(lo + 9) if lo is not None else None,
            text=("line %d " % i) + ("tok%d " % rng.randrange(7)) * 6,
            kind="obs", tokens=10, cmd="", head=""))
    return out


@pytest.mark.parametrize("xfile,strict", [("on", None), ("off", None), ("on", "on")])
def test_fast_edges_bit_identical(monkeypatch, xfile, strict):
    torch = pytest.importorskip("torch")
    np = pytest.importorskip("numpy")
    import parsec_brain  # noqa: F401  — _flags parity pins before vendored import
    from parsec_brain.vendored.pyg_model import edges

    rng = random.Random(7)
    chunks = _chunks(rng, 80)
    emb = np.asarray([[rng.uniform(-1, 1) for _ in range(16)] for _ in chunks],
                     dtype=np.float32)

    monkeypatch.setenv("AC_KNN_XFILE", xfile)
    if strict:
        monkeypatch.setenv("AC_STRICTEDGE", strict)
    else:
        monkeypatch.delenv("AC_STRICTEDGE", raising=False)

    monkeypatch.setenv("AC_EDGES_FAST", "off")
    ei_ref, et_ref = edges(chunks, emb, causal=True)
    monkeypatch.setenv("AC_EDGES_FAST", "on")
    ei_fast, et_fast = edges(chunks, emb, causal=True)
    assert torch.equal(et_ref, et_fast), "edge types diverged"
    assert torch.equal(ei_ref, ei_fast), "edge indices diverged"

    # skip_supersession must ONLY remove rel-4 rows (order preserved).
    ei_skip, et_skip = edges(chunks, emb, causal=True, skip_supersession=True)
    keep = et_ref != 4
    assert torch.equal(et_skip, et_ref[keep])
    assert torch.equal(ei_skip, ei_ref[:, keep])
