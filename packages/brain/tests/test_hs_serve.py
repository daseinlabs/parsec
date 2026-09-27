"""HS serve path (brain-api/v3): read columns, graph widths, the v2 adapter, per-kind taus, and the
HTTP app. Hermetic by default; the real checkpoint path runs when PARSEC_HS_CKPT is set, and the
gnn-train parity check runs when a gnn-train checkout is found (GNN_TRAIN, default ../../../gnn-train
relative to the repo)."""
import json
import os
import random
import sys
from pathlib import Path
from types import SimpleNamespace as N

import numpy as np
import pytest

# The HS model code is supplied by the private image build, not committed (vendored_hs/__init__.py).
# Without it — public CI — there is nothing here to run.
for _m in ("pyg_model", "read_columns", "rereq"):
    pytest.importorskip(f"parsec_brain.vendored_hs.{_m}",
                        reason="HS model code not present (supplied at image build)")

from parsec_brain import hs_taus  # noqa: E402
from parsec_brain.app_hs import fold_kind_tau, v2_decided_to_v3  # noqa: E402
from parsec_brain.scorer_hs import (READ_COLS, adapt_v2_nodes, build_graph, node_read_columns)
from parsec_brain.vendored_hs.read_columns import READCOLS_EXTS, read_columns  # noqa: E402
from parsec_brain.vendored_hs.rereq import rereq_features_base  # noqa: E402

pytestmark = pytest.mark.no_ckpt

REPO = Path(__file__).resolve().parents[3]
SCHEMA = REPO / "packages" / "contracts" / "schemas" / "brain-api-v3.schema.json"
GNN_TRAIN = Path(os.environ.get("GNN_TRAIN", str(REPO.parent / "gnn-train")))
F16 = "0123456789abcdef"


def _node(step, kind, ext="none", text="t", cmd="", head="", file_id=None, lo=None, hi=None,
          tokens=4, rereq=(0.0,) * 6):
    return N(step=step, kind=kind, text=text, cmd=cmd, head=head, ext=ext, file_id=file_id,
             lo=lo, hi=hi, tokens=tokens, struct=[0.1] * 21 + list(rereq), cmd_id=None,
             head_id=None)


# ---- read columns: extension stand-ins ------------------------------------------------------

@pytest.mark.parametrize("ext", READCOLS_EXTS + ["other", "none"])
def test_ext_stand_in_hits_the_right_one_hot(ext):
    row = node_read_columns([_node(1, "read", ext=ext, cmd="Read file_path=/x", lo=1, hi=5)])[0]
    onehot = row[18:50]
    want = 30 if ext == "other" else 31 if ext == "none" else READCOLS_EXTS.index(ext)
    assert onehot.sum() == 1.0 and onehot[want] == 1.0


def test_reasoning_and_asst_rows_are_zero():
    rc = node_read_columns([_node(0, "reasoning"), _node(1, "asst"),
                            _node(2, "read", ext="py", cmd="Read file_path=/a.py", lo=1, hi=9)])
    assert not rc[0].any() and not rc[1].any() and rc[2].any()


@pytest.mark.skipif(not (GNN_TRAIN / "adaptive_context" / "optimizer" / "torch_curator.py").is_file(),
                    reason="gnn-train checkout not found (set GNN_TRAIN)")
def test_vendored_columns_match_gnn_train():
    sys.path[:0] = [str(GNN_TRAIN), str(GNN_TRAIN / "scripts")]
    try:
        import adaptive_context.optimizer.torch_curator as TC
    except Exception as e:  # noqa: BLE001 — the reference tree is incomplete on some checkouts
        pytest.skip(f"gnn-train torch_curator not importable: {type(e).__name__}")
    finally:
        del sys.path[:2]
    rnd = random.Random(0)
    cmds = ["Read file_path=/r/a.py", "Read file_path=/r/a.py offset=40 limit=20",
            "grep -rn foo src", "cat src/x.ts | head -5", "sed -n '10,40p' lib/y.rs", "Bash ls",
            "Edit file_path=/r/a.py", "", "Glob **/*.md", "cat README.md"]
    files = [None, "a.py", "x.ts", "y.rs", "README.md", "z.weird", "Makefile", ".env", "q.JSON"]
    for _ in range(200):
        ch = []
        for _ in range(rnd.randint(1, 30)):
            lo = rnd.choice([None, 1, 5, 40, 200])
            ch.append(N(step=rnd.randint(0, 8), cmd=rnd.choice(cmds), lo=lo,
                        hi=None if lo is None else lo + rnd.randint(0, 60),
                        tokens=rnd.randint(0, 300),
                        kind=rnd.choice(["read", "grep", "other", "reasoning", "asst"]),
                        file=rnd.choice(files), text=""))
        ch.sort(key=lambda c: c.step)
        assert np.array_equal(TC.read_columns(ch), read_columns(ch))
        b1, m1 = TC.rereq_features_base(ch)
        b2, m2 = rereq_features_base(ch)
        assert np.array_equal(b1, b2) and m1 == m2


# ---- graph -----------------------------------------------------------------------------------

def test_graph_node_row_is_106_and_texts_align():
    nodes = [_node(0, "reasoning", text="plan"),
             _node(1, "read", "py", "def a(): pass", "Read file_path=/a.py", "", F16, 1, 9),
             _node(1, "read", "py", "def b(): pass", "Read file_path=/a.py", "", F16, 10, 19),
             _node(2, "grep", "none", "a.py:3: x", "grep -rn x .", "exit 0"),
             _node(3, "asst", text="done")]
    emb = np.random.RandomState(0).rand(len(nodes), 1024).astype(np.float32)
    rc = node_read_columns(nodes)
    xe, xs, ei, et, ntexts, n_own, step_rows = build_graph(
        nodes, emb, "task", np.zeros(1024), "sys", np.zeros(1024), [(1, 2)], rc)
    assert xs.shape[1] == 106 and xs.shape[0] == xe.shape[0] == len(ntexts)
    assert n_own == len(nodes) and sorted(step_rows) == [0, 1, 2, 3]
    assert ntexts[n_own] == ["task"]                           # task node
    assert ["Read file_path=/a.py"] in ntexts and ["exit 0"] in ntexts and ["sys"] in ntexts
    # rel-4 comes only from the client pairs
    r4 = ei[:, et == 4].T.tolist()
    assert r4 == [[1, 2]]


# ---- brain-api/v2 adapter --------------------------------------------------------------------

def test_v2_decided_row_layout():
    row = [float(i) for i in range(49)]                        # value == its v2 column index
    out = v2_decided_to_v3(row, [9.0] * 6)
    assert len(out) == 104
    assert out[:16] == row[:16]
    assert out[16:22] == [9.0] * 6
    assert out[READ_COLS] == [0.0] * 50
    assert out[72:98] == row[16:42]                             # age .. centrality
    assert 42.0 not in out                                      # changeprone dropped
    assert out[98:102] == row[43:47]                            # scout
    assert out[102:] == [0.0, 0.0]                              # dupcos: brain-filled


def test_adapt_v2_nodes_adds_rereq_and_no_ext():
    v2 = [N(text="a", cmd="", head="", struct=[0.0] * 21, step=1, kind="read", tokens=3,
            file_id=F16, lo=1, hi=10),
          N(text="a", cmd="", head="", struct=[0.0] * 21, step=2, kind="read", tokens=3,
            file_id=F16, lo=5, hi=12)]
    out = adapt_v2_nodes(v2)
    assert all(len(n.struct) == 27 and n.ext == "none" for n in out)
    assert out[1].struct[21] == 1.0 and out[1].struct[25] == 1.0   # n_prior_serves, is_rereq
    assert out[1].struct[22] == 0.0 and out[1].struct[26] == 0.0   # no cut state on v2


def test_fold_kind_tau_matches_the_per_kind_rule():
    tau = {"read": 400_000, "other": 100_000, "grep": 200_000, "reasoning": 300_000}
    t = tau["read"]
    kinds = ["read", "other", "grep", "reasoning", "asst", "read"]
    q = [399_999, 100_000, 150_000, 900_000, 5, 1_000_000]
    folded = fold_kind_tau(q, kinds, tau, t)
    for qq, k, f in zip(q, kinds, folded):
        per_kind_keep = k == "asst" or qq >= tau[k]
        assert (f >= t) == per_kind_keep
    assert folded[4] == 1_000_000 and all(0 <= f <= 1_000_000 for f in folded)


# ---- per-kind taus ---------------------------------------------------------------------------

def test_derive_hits_the_coverage_per_kind():
    rnd = np.random.RandomState(1)
    n = 4000
    kinds = np.array(rnd.choice(list(hs_taus.KINDS), n))
    y = (rnd.rand(n) < 0.4).astype(float)
    sc = np.clip(y * 0.3 + rnd.rand(n) * 0.7, 0, 1)
    tok = rnd.randint(1, 200, n).astype(float)
    tau = hs_taus.derive(sc, y, tok, kinds, 0.90)
    rep = hs_taus.report(sc, y, tok, np.zeros(n, bool), kinds, tau, force_fought=False)
    for k in hs_taus.KINDS:
        assert 0.899 <= rep[k]["needed_cov"] <= 0.93


def test_load_refuses_bad_tables(tmp_path, monkeypatch):
    ck = str(tmp_path / "c.pt")
    with pytest.raises(hs_taus.TauError, match="not found"):
        hs_taus.load(ck)
    p = tmp_path / "c.pt.taus.json"
    p.write_text(json.dumps({"coverage": 0.9, "tau": {"read": 0.4}}))
    with pytest.raises(hs_taus.TauError, match="exactly the kinds"):
        hs_taus.load(ck)
    good = {"read": 0.43, "other": 0.18, "grep": 0.19, "reasoning": 0.21}
    p.write_text(json.dumps({"coverage": 0.9, "tau": good}))
    t = hs_taus.load(ck)
    assert t.tau_q == {"read": 430000, "other": 180000, "grep": 190000, "reasoning": 210000}
    alt = tmp_path / "alt.json"
    alt.write_text(json.dumps({"coverage": 0.95, "tau": good}))
    monkeypatch.setenv("PARSEC_HS_TAUS", str(alt))
    assert hs_taus.load(ck).coverage == 0.95


# ---- real checkpoint: HTTP app end to end ----------------------------------------------------

@pytest.mark.skipif(not os.environ.get("PARSEC_HS_CKPT"),
                    reason="set PARSEC_HS_CKPT (+ <ckpt>.taus.json) to run the real HS app")
def test_real_app_serves_v3_and_v2():
    import jsonschema
    from fastapi.testclient import TestClient

    from parsec_brain.app import create_app
    defs = json.loads(SCHEMA.read_text())["$defs"]

    def val(name, obj):
        jsonschema.validate(obj, {"$ref": f"#/$defs/{name}", "$defs": defs})
    c = TestClient(create_app())
    b = c.get("/v1/bundle").json()
    val("bundle_response", b)
    nodes = [dict(text="plan", cmd="", head="", struct=[0.1] * 27, step=0, kind="reasoning",
                  tokens=2, ext="none"),
             dict(text="def a(): pass", cmd="Read file_path=/a.py", head="",
                  struct=[0.1] * 27, step=1, kind="read", tokens=4, ext="py", file_id=F16,
                  lo=1, hi=9),
             dict(text="done", cmd="", head="", struct=[0.1] * 27, step=2, kind="asst",
                  tokens=1, ext="none")]
    req = dict(contract="brain-api/v3", conv_id="t", checkpoint_id=b["checkpoint_id"],
               cur_step=2, nodes=nodes, task_text="fix it", mask=[0, 1, 2],
               decided_struct=[[0.0] * 104] * 3, edges_supersession=[])
    r = c.post("/v1/score/trace", json=req)
    assert r.status_code == 200
    val("score_trace_response", r.json())
    assert c.post("/v1/score/trace", json={**req, "checkpoint_id": "0" * 64}).status_code == 409
    v2 = {**req, "contract": "brain-api/v2", "decided_struct": [[0.0] * 49] * 3,
          "nodes": [{k: v for k, v in n.items() if k != "ext"} | {"struct": [0.1] * 21}
                    for n in nodes]}
    r2 = c.post("/v1/score/trace", json=v2).json()
    assert r2["tau_q"] == b["tau_q_by_kind"]["read"] and r2["scores_q"][2] == 1_000_000
