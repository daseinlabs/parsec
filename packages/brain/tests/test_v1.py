"""brain-api/v1 <-> brain-api-dev/v0 CROSS-PATH PARITY (the migration's intermediate gate).

The v1 payload here is featurized by the VENDORED CODE ITSELF — parse -> chunks ->
node_struct_with_type + markers -> _het_readout decided_struct -> hash embeddings ->
supersession pairs via the vendored edges() — and the tests assert the v1 endpoint returns
EXACTLY the same scores_q/tau_q as the dev endpoint on the same conversation. Nothing new is
pinned: the assertion IS dev == v1, on the handcrafted golden conversation AND on turns of
packages/proxy/parity/fixtures/golden_conversation.json (converted through a small honest
mirror of anthropic_shapes.to_internal — the brain's parse takes the internal view).

Also here: the v1 contract's structural no-raw-text guarantee, the §8.2 checkpoint guard,
schema+example validation, and the symbol-graph parser-liveness regression (bugfix: a broken
tree-sitter grammar used to silently zero the centrality readout cols 37-41).
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
from pathlib import Path

# self-contained bootstrap (same pattern as test_service.py)
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
os.environ.setdefault("PARSEC_EMBED_BACKEND", "hash")

import numpy as np
import pytest
from fastapi.testclient import TestClient

from parsec_brain.app import create_app
from parsec_brain.bundle import load_bundle
from parsec_brain.scorer import TraceScorer, chunk_checksum, parse_internal
from parsec_brain.vendored.pyg_model import edges
from test_service import MESSAGES, TOOLS

REPO = Path(__file__).resolve().parents[3]
GOLDEN_FIXTURE = REPO / "packages/proxy/parity/fixtures/golden_conversation.json"
SCHEMA = REPO / "packages/contracts/schemas/brain-api.schema.json"
EXAMPLE = REPO / "packages/contracts/schemas/examples/brain-api.example.json"
FIXTURE_TURNS = (2, 5, 13, 21)      # 1-indexed prefixes of the recorded conversation
_ZED = np.zeros(1024, np.float32)


@pytest.fixture(scope="module")
def client():
    return TestClient(create_app())


@pytest.fixture(scope="module")
def fz():
    """Featurizer scorer — a TraceScorer over its own bundle (embed cache independent of the
    app's; the hash backend is content-addressed so vectors agree bit-for-bit)."""
    return TraceScorer(load_bundle())


@pytest.fixture(scope="module")
def ckpt_id(fz):
    return fz.bundle.checkpoint_id


def _hid(text: str) -> str:
    """Opaque 16-hex id for a file basename / command / head — equality-preserving, salted
    (the client's real ids are salted hashes; only equality/truthiness ride the wire)."""
    return hashlib.sha256(("v1-parity-salt:" + text).encode("utf-8")).hexdigest()[:16]


def _f(a):
    return [float(x) for x in np.asarray(a, np.float32)]


# ---- v1 featurization THROUGH the vendored code ----------------------------------------------
def _v1_nodes(fz: TraceScorer, lc: list) -> tuple[list[dict], np.ndarray]:
    """(nodes, content-emb matrix) for a live chunk list — embeddings/struct exactly as
    TraceScorer._trace_scores derives them, ids replacing the raw strings."""
    from parsec_brain.vendored.torch_curator import node_struct_with_type
    emb = np.asarray(fz._embed([c.text[:2000] for c in lc]), np.float32)
    cmds = [getattr(c, "cmd", "") or "" for c in lc]
    heads = [(getattr(c, "head", "") or "")[:240] for c in lc]
    uniq = [t for t in dict.fromkeys(cmds + heads) if t]
    if uniq:
        fz._embed(uniq)
    ns = node_struct_with_type(lc)
    nodes = []
    for i, c in enumerate(lc):
        nodes.append({
            "emb_text": _f(emb[i]),
            "emb_cmd": _f(fz.cache[cmds[i]] if cmds[i] else _ZED),
            "emb_head": _f(fz.cache[heads[i]] if heads[i] else _ZED),
            "struct": _f(ns[i]),
            "step": int(c.step),
            "kind": c.kind,
            "tokens": int(c.tokens),
            "file_id": _hid(c.file) if c.file else None,
            "lo": None if c.lo is None else int(c.lo),
            "hi": None if c.hi is None else int(c.hi),
            "cmd_id": _hid(cmds[i]) if cmds[i] else None,
            "head_id": _hid(heads[i]) if heads[i] else None,
        })
    return nodes, emb


def _v1_trace_payload(fz: TraceScorer, ckpt_id: str, parsed, live_gi, mask,
                      conv_id="v1-parity") -> dict:
    lc = [parsed.chunks[g] for g in live_gi]
    nodes, emb = _v1_nodes(fz, lc)
    ei, et = edges(lc, emb, causal=True)              # vendored rel-4 = the client's pairs
    sup = [[int(a), int(b)] for a, b, t in
           zip(ei[0].tolist(), ei[1].tolist(), et.tolist()) if t == 4]
    ds = fz._het_readout(lc, list(mask), parsed.recent_cmds, parsed.task_text, 0.0, emb,
                         parsed.cur_step, parsed.het_steps, parsed.T)
    task_emb = (np.asarray(fz._embed([parsed.task_text])[0], np.float32)
                if parsed.task_text else _ZED)
    payload = {
        "contract": "brain-api/v1",
        "conv_id": conv_id,
        "checkpoint_id": ckpt_id,
        "cur_step": int(parsed.cur_step),
        "nodes": nodes,
        "task_emb": _f(task_emb),
        "mask": list(mask),
        "decided_struct": [_f(row) for row in ds],
        "edges_supersession": sup,
    }
    if parsed.sys_text:
        payload["sys_emb"] = _f(np.asarray(fz._embed([parsed.sys_text])[0], np.float32))
    return payload


def _dev_trace_payload(parsed, messages, live_gi, mask, conv_id="v1-parity") -> dict:
    return {"contract": "brain-api-dev/v0", "conv_id": conv_id, "messages": messages,
            "live_gi": list(live_gi), "mask": list(mask), "cur_step": parsed.cur_step,
            "chunk_checksum": chunk_checksum(parsed.chunks)}


def _newborn_mask(parsed, live_gi):
    return [j for j, g in enumerate(live_gi)
            if parsed.chunks[g].step == parsed.cur_step
            and parsed.chunks[g].evict != "provider"]


def _assert_trace_parity(client, fz, ckpt_id, messages, conv_id, live_gi=None):
    parsed = parse_internal(messages, 10)
    live_gi = list(range(len(parsed.chunks))) if live_gi is None else live_gi(parsed)
    mask = _newborn_mask(parsed, live_gi)
    dev = client.post("/v1/score/trace",
                      json=_dev_trace_payload(parsed, messages, live_gi, mask, conv_id))
    assert dev.status_code == 200, dev.text
    v1 = client.post("/v1/score/trace",
                     json=_v1_trace_payload(fz, ckpt_id, parsed, live_gi, mask, conv_id))
    assert v1.status_code == 200, v1.text
    d, v = dev.json(), v1.json()
    assert v["scores_q"] == d["scores_q"], (
        f"{conv_id}: v1 diverged from dev\n dev={d['scores_q']}\n v1 ={v['scores_q']}")
    assert v["tau_q"] == d["tau_q"]
    assert v["checkpoint_id"] == d["checkpoint_id"] == ckpt_id
    assert v["timings_ms"]["embed"] == 0.0          # nothing is embedded server-side on v1
    return mask, d["scores_q"]


# ---- the small honest to_internal mirror (spec to-internal.md §1) -----------------------------
_PRJ_SHELL = {"bash", "shell", "run", "execute", "bashoutput", "bash_output"}
_PRJ_READ = {"read", "notebookread", "notebook_read", "view", "cat", "openfile", "open_file"}
_PRJ_EDIT = {"edit", "multiedit", "write", "str_replace", "str_replace_editor",
             "str_replace_based_edit_tool", "notebookedit", "notebook_edit", "create",
             "createfile"}
_PRJ_GREP = {"grep", "glob", "search", "search_code", "searchcode", "find", "ripgrep", "rg"}


def _bash_twin(tool, args):
    name = (tool or "").strip().lower()
    if not isinstance(args, dict):
        args = {}
    if name in _PRJ_SHELL:
        c = args.get("command")
        return c if isinstance(c, str) and c.strip() else None
    f = args.get("file_path") or args.get("path") or args.get("filename") or args.get("file")
    if name in _PRJ_READ and isinstance(f, str) and f:
        try:
            off = int(args["offset"]) if args.get("offset") is not None else None
            lim = int(args["limit"]) if args.get("limit") is not None else None
        except (TypeError, ValueError):
            off = lim = None
        if off and lim:
            return f"sed -n '{off},{off + max(1, lim) - 1}p' {f}"
        if off:
            return f"sed -n '{off},{off + 1999}p' {f}"
        return f"cat {f}"
    if name in _PRJ_EDIT and isinstance(f, str) and f:
        return f"edit {f.split('/')[-1]}"
    if name in _PRJ_GREP:
        pat = str(args.get("pattern") or args.get("query") or args.get("q") or "")[:200]
        path = args.get("path") or args.get("glob") or args.get("include") or ""
        return (f"grep '{pat.replace(chr(39), ' ')}' {path}").strip()
    return None


def _derive_command(tool, args):
    if not isinstance(args, dict):
        return str(args)[:300]
    if tool.lower() in {"bash", "shell", "run", "execute"} and isinstance(args.get("command"), str):
        return args["command"]
    parts = []
    for k in sorted(args):
        sv = args[k] if isinstance(args[k], str) else json.dumps(args[k], ensure_ascii=False)
        parts.append(f"{k}={sv[:160] + '…' if len(sv) > 160 else sv}")
    return (f"{tool} " + " ".join(parts)).strip()[:400]


def _derive_query(tool, args):
    for k in ("query", "pattern", "q", "search"):
        v = args.get(k) if isinstance(args, dict) else None
        if isinstance(v, str) and v.strip():
            return v
    return ""


def _content_text(c):
    if isinstance(c, str):
        return c
    if not isinstance(c, list):
        return ""
    out = []
    for b in c:
        if not isinstance(b, dict):
            continue
        if b.get("type") == "text":
            out.append(b.get("text", "") or "")
        elif b.get("type") == "tool_result":
            tc = b.get("content")
            out.append(tc if isinstance(tc, str) else
                       " ".join(p.get("text", "") for p in tc
                                if isinstance(p, dict) and p.get("type") == "text")
                       if isinstance(tc, list) else "")
    return " ".join(s for s in out if s)


def to_internal(body: dict) -> list[dict]:
    """Mirror of anthropic_shapes.to_internal, reduced to what the brain's parse consumes
    (role, flattened content text, extra.actions with bash-twin commands; tool_calls and the
    thinking blocks are irrelevant to parse_internal and deliberately not mirrored)."""
    s = body.get("system")
    sys_text = s if isinstance(s, str) else (
        "\n".join(b.get("text", "") for b in s if isinstance(b, dict) and b.get("type") == "text")
        if isinstance(s, list) else "")
    out = [{"role": "system", "content": sys_text}] if sys_text else []
    for m in body.get("messages", []):
        role = m.get("role", "user")
        im = {"role": role, "content": _content_text(m.get("content"))}
        c = m.get("content")
        if role == "assistant" and isinstance(c, list):
            tus = [b for b in c if isinstance(b, dict) and b.get("type") == "tool_use"]
            if tus:
                acts = []
                for b in tus:
                    name = (b.get("name") or "").strip()
                    args = b.get("input")
                    a = args if isinstance(args, dict) else {}
                    act = {"tool": name, "name": name}
                    if isinstance(args, dict):
                        act.update(args)
                    twin = _bash_twin(name, a)
                    act["command"] = twin if twin else _derive_command(name, a)
                    q = _derive_query(name, a)
                    if q:
                        act["query"] = q
                    acts.append(act)
                im["extra"] = {"actions": acts}
        out.append(im)
    return out


# ---- trace parity -----------------------------------------------------------------------------
def test_v1_trace_parity_handcrafted(client, fz, ckpt_id):
    mask, scores_q = _assert_trace_parity(client, fz, ckpt_id, MESSAGES, "v1-handcrafted")
    assert mask, "handcrafted golden must decide newborn rows"
    from test_service import GOLDEN_SCORES_Q
    assert scores_q == GOLDEN_SCORES_Q            # both paths still sit on the pinned golden


def test_v1_trace_parity_alive_subset(client, fz, ckpt_id):
    """live_gi = a strict subset (committed drops applied) — node indices, rel-0 chains, kNN
    candidates and the readout alive-set all shift; parity must hold on the subset graph."""
    def subset(parsed):
        n = len(parsed.chunks)
        keep = [g for g in range(n)
                if parsed.chunks[g].step == parsed.cur_step or g % 3 != 1
                or parsed.chunks[g].step == 0]
        assert 0 < len(keep) < n
        return keep
    _assert_trace_parity(client, fz, ckpt_id, MESSAGES, "v1-subset", live_gi=subset)


@pytest.mark.parametrize("turn", FIXTURE_TURNS)
def test_v1_trace_parity_golden_conversation(client, fz, ckpt_id, turn):
    """Recorded CC-shaped conversation (the proxy's §8.1 fixture), converted through the
    to_internal mirror — the exact internal view the proxy would send the dev path."""
    body = json.loads(GOLDEN_FIXTURE.read_text())["turns"][turn - 1]
    internal = to_internal(body)
    mask, _ = _assert_trace_parity(client, fz, ckpt_id, internal, f"v1-golden-t{turn}")
    if turn == FIXTURE_TURNS[-1]:
        assert mask, "the deepest fixture turn must decide newborn rows"


def test_v1_trace_parity_supersession(client, fz, ckpt_id):
    """A re-read conversation — the one case that exercises payload rel-4 (neither the
    handcrafted golden nor the sanitized proxy fixture emits supersession pairs). Also pins
    that the pairs are LOAD-BEARING (dropping them moves scores) and that the server's
    lexicographic re-sort makes client pair order irrelevant."""
    q = "\n".join(f"def f{i}(x):\n    return g{i}(x)" for i in range(6))
    msgs = [
        {"role": "user", "content": "Fix f0 in q.py"},
        {"role": "assistant", "content": "Read it.",
         "extra": {"actions": [{"tool": "Read", "command": "cat q.py"}]}},
        {"role": "tool", "content": "returncode: 0\n" + q},
        {"role": "assistant", "content": "Re-read.",
         "extra": {"actions": [{"tool": "Read", "command": "cat q.py"}]}},
        {"role": "tool", "content": "returncode: 0\n" + q},
    ]
    parsed = parse_internal(msgs, 10)
    gi = list(range(len(parsed.chunks)))
    mask = _newborn_mask(parsed, gi)
    payload = _v1_trace_payload(fz, ckpt_id, parsed, gi, mask, "v1-supersession")
    assert payload["edges_supersession"], "re-read must produce rel-4 pairs"
    _, ref = _assert_trace_parity(client, fz, ckpt_id, msgs, "v1-supersession")
    dropped = client.post("/v1/score/trace",
                          json=payload | {"edges_supersession": []}).json()["scores_q"]
    assert dropped != ref                            # the pairs are load-bearing
    shuffled = client.post("/v1/score/trace", json=payload | {
        "edges_supersession": list(reversed(payload["edges_supersession"]))}).json()["scores_q"]
    assert shuffled == ref                           # server re-sorts to the vendored order


# ---- tools parity -----------------------------------------------------------------------------
def _v1_tools_payload(fz: TraceScorer, ckpt_id: str, internal, tools, conv_id="v1-parity"):
    from parsec_brain.vendored.trace_graph import build_tool_spec
    spec = build_tool_spec(internal, tools, "serve")
    assert spec is not None and spec["tool_nodes"]
    nodes, _emb = _v1_nodes(fz, spec["chunks"])
    tool_embs = fz._embed([tn["text"] for tn in spec["tool_nodes"]])
    payload = {
        "contract": "brain-api/v1",
        "conv_id": conv_id,
        "checkpoint_id": ckpt_id,
        "nodes": nodes,
        "task_emb": _f(np.asarray(fz._embed([spec["task_text"]])[0], np.float32)
                       if spec["task_text"] else _ZED),
        "tools": [{"name": tn["name"], "emb": _f(v), "tokens": int(tn["tokens"])}
                  for tn, v in zip(spec["tool_nodes"], tool_embs)],
    }
    st = (spec.get("sys_text", "") or "")[:2000]
    if st:
        payload["sys_emb"] = _f(np.asarray(fz._embed([st])[0], np.float32))
    return payload


def _assert_tools_parity(client, fz, ckpt_id, internal, tools, conv_id):
    dev = client.post("/v1/score/tools", json={
        "contract": "brain-api-dev/v0", "conv_id": conv_id,
        "messages": internal, "tools": tools})
    assert dev.status_code == 200, dev.text
    v1 = client.post("/v1/score/tools",
                     json=_v1_tools_payload(fz, ckpt_id, internal, tools, conv_id))
    assert v1.status_code == 200, v1.text
    assert v1.json() == dev.json(), (
        f"{conv_id}: v1 tools diverged\n dev={dev.json()}\n v1 ={v1.json()}")


def test_v1_tools_parity_handcrafted(client, fz, ckpt_id):
    _assert_tools_parity(client, fz, ckpt_id, MESSAGES, TOOLS, "v1-tools-handcrafted")


def test_v1_tools_parity_golden_conversation(client, fz, ckpt_id):
    body = json.loads(GOLDEN_FIXTURE.read_text())["turns"][FIXTURE_TURNS[-1] - 1]
    _assert_tools_parity(client, fz, ckpt_id, to_internal(body), body["tools"],
                         "v1-tools-golden")


# ---- contract guards --------------------------------------------------------------------------
def test_v1_checkpoint_mismatch_409(client, fz, ckpt_id):
    parsed = parse_internal(MESSAGES, 10)
    gi = list(range(len(parsed.chunks)))
    p = _v1_trace_payload(fz, ckpt_id, parsed, gi, _newborn_mask(parsed, gi))
    r = client.post("/v1/score/trace", json=p | {"checkpoint_id": "0" * 64})
    assert r.status_code == 409
    assert r.json()["detail"]["server_checkpoint_id"] == ckpt_id
    t = _v1_tools_payload(fz, ckpt_id, MESSAGES, TOOLS)
    r = client.post("/v1/score/tools", json=t | {"checkpoint_id": "0" * 64})
    assert r.status_code == 409


def test_v1_validation_and_empty_mask(client, fz, ckpt_id):
    parsed = parse_internal(MESSAGES, 10)
    gi = list(range(len(parsed.chunks)))
    p = _v1_trace_payload(fz, ckpt_id, parsed, gi, _newborn_mask(parsed, gi))
    n = len(p["nodes"])
    assert client.post("/v1/score/trace",
                       json=p | {"mask": [n]}).status_code == 422           # row out of range
    assert client.post("/v1/score/trace",
                       json=p | {"decided_struct": [[0.0] * 48] * len(p["mask"])}
                       ).status_code == 422                                 # 48 != read_struct
    assert client.post("/v1/score/trace",
                       json=p | {"decided_struct": p["decided_struct"][:-1]}
                       ).status_code == 422                                 # rows != mask len
    assert client.post("/v1/score/trace",
                       json=p | {"edges_supersession": [[0, n]]}).status_code == 422
    assert client.post("/v1/score/trace",
                       json=p | {"nodes": [p["nodes"][0] | {"emb_text": [0.0] * 3}]}
                       ).status_code == 422                                 # wrong vector width
    r = client.post("/v1/score/trace", json=p | {"mask": [], "decided_struct": []})
    assert r.json()["scores_q"] == [1_000_000] * n                          # nothing to decide
    t = _v1_tools_payload(fz, ckpt_id, MESSAGES, TOOLS)
    r = client.post("/v1/score/tools", json=t | {"tools": []})              # fail-open shape
    assert r.json() == {"names": [], "scores_q": [], "tokens": [],
                        "checkpoint_id": ckpt_id}
    assert client.post("/v1/score/tools", json=t | {"nodes": []}).status_code == 422


def test_v1_no_raw_text_representable():
    """Structural: every string field on the v1 request models is a Literal or is
    pattern-constrained to a fixed-format id — free text has no slot, by construction."""
    import types
    from typing import Union, get_args, get_origin
    from parsec_brain.app import (NeighborsV1Request, ScoreRulesV1Request,
                                  ScoreToolsV1Request, ScoreTraceV1Request, V1Node, V1Tool)
    checked = []
    for model in (V1Node, V1Tool, ScoreTraceV1Request, ScoreToolsV1Request,
                  ScoreRulesV1Request, NeighborsV1Request):
        assert model.model_config.get("extra") == "forbid", model  # nothing rides uncontracted
        for name, f in model.model_fields.items():
            ann = f.annotation
            args = (list(get_args(ann))
                    if get_origin(ann) in (Union, types.UnionType) else [ann])
            if str in args:                     # plain-str slot (also str|None optionals)
                pats = [m.pattern for m in f.metadata if getattr(m, "pattern", None)]
                assert pats, f"{model.__name__}.{name}: unconstrained str field (raw-text slot)"
                checked.append(f"{model.__name__}.{name}")
    # the walk really saw every string slot (guards the test against annotation-shape drift)
    assert set(checked) == {
        "V1Node.file_id", "V1Node.cmd_id", "V1Node.head_id", "V1Tool.name",
        "ScoreTraceV1Request.conv_id", "ScoreTraceV1Request.checkpoint_id",
        "ScoreTraceV1Request.target_cov",
        "ScoreToolsV1Request.conv_id", "ScoreToolsV1Request.checkpoint_id",
        "ScoreRulesV1Request.conv_id", "ScoreRulesV1Request.checkpoint_id",
        "NeighborsV1Request.conv_id", "NeighborsV1Request.checkpoint_id",
    }, checked


def test_v1_schema_and_example_valid(client):
    """The committed schema matches the served models: every example instance validates
    against its $def, and the example requests PARSE at the endpoint (409 = past validation,
    stopped only by the checkpoint guard — the example carries a placeholder checkpoint)."""
    import jsonschema
    schema = json.loads(SCHEMA.read_text())
    example = json.loads(EXAMPLE.read_text())
    expected = {"score_trace_request", "score_trace_response",
                "score_tools_request", "score_tools_response",
                "score_rules_request", "score_rules_response",
                "neighbors_request", "neighbors_response"}
    assert set(example) == expected
    for key, inst in example.items():
        jsonschema.validate(inst, schema | {"$ref": f"#/$defs/{key}"})
        jsonschema.validate(inst, schema)               # the top-level oneOf accepts it too
    for ep in ("trace", "tools", "rules"):
        r = client.post(f"/v1/score/{ep}", json=example[f"score_{ep}_request"])
        assert r.status_code == 409, r.text             # parsed OK; placeholder ckpt rejected
    r = client.post("/v1/neighbors", json=example["neighbors_request"])
    assert r.status_code == 409, r.text                 # same checkpoint handshake


# ---- symbol-graph liveness regression (the tree-sitter fallback bugfix) ------------------------
def test_symbol_graph_parser_live_and_centrality_nonzero(fz):
    """A broken grammar used to fail-open to zero symbols: served centrality readout cols
    (37-41) were ZERO while the ckpt trained on real parsing. The vendored fallback must keep
    a python parser live and produce real centrality for a python read chunk."""
    from parsec_brain.vendored.symbol_graph import _py_defs_refs, python_parser_backend
    assert python_parser_backend() != "none"
    d, r = _py_defs_refs("def alpha(x):\n    return beta(x)\n")
    assert d == {"alpha"} and r == ["beta"]
    msgs = [
        {"role": "user", "content": "Fix load_batch in loader.py"},
        {"role": "assistant", "content": "Read it.",
         "extra": {"actions": [{"tool": "Read", "command": "cat loader.py"}]}},
        {"role": "tool", "content": "def load_batch(i):\n    return fetch(i)\n"
                                    "def fetch(i):\n    return load_batch(i - 1)"},
    ]
    parsed = parse_internal(msgs, 10)
    mask = _newborn_mask(parsed, list(range(len(parsed.chunks))))
    emb = np.asarray(fz._embed([c.text[:2000] for c in parsed.chunks]), np.float32)
    ds = fz._het_readout(parsed.chunks, mask, parsed.recent_cmds, parsed.task_text, 0.0,
                         emb, parsed.cur_step, parsed.het_steps, parsed.T)
    read_rows = [k for k, j in enumerate(mask) if parsed.chunks[j].kind == "read"]
    assert read_rows and np.abs(ds[read_rows][:, 37:42]).sum() > 0.0, (
        "centrality cols 37-41 are zero for a python read chunk — parser fail-open regressed")
