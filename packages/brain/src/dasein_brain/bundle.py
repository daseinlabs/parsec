"""Self-validating checkpoint bundle (docs/brain-serving-v0.md §"Bundle self-validation").

The brain refuses to start unless the checkpoint is exactly what the vendored scoring path
expects: every dim DERIVED from the state-dict weights must equal the ckpt's RECORDED config,
the calibration table must resolve the target coverage, and the chunker dials must match the
engine defaults. Any mismatch raises BundleError at load — never a silently-wrong score.
"""
from __future__ import annotations

from . import _flags  # noqa: F401  parity pins before the vendored import below

import hashlib
import json
import os
import pickle
from dataclasses import dataclass, field
from pathlib import Path

import torch

from .vendored.pyg_model import PyGCurator

_PKG_MODELS = Path(__file__).resolve().parents[2] / "models"   # packages/brain/models (repo-tracked)
_DEFAULT_CKPT = "~/.dasein/brain/curator_v4_prod.pt"           # the 36MB .pt stays OUT of the repo
_EXPECTED_RULES = 16


class BundleError(RuntimeError):
    """Checkpoint/config mismatch — the bundle must not serve."""


@dataclass
class Bundle:
    model: PyGCurator
    checkpoint_id: str           # sha256 hex of the .pt file
    ckpt_path: str
    # dims (derived from weights AND cross-checked against the recorded config)
    emb_dim: int
    hidden: int
    layers: int
    nrel: int
    read_struct: int
    struct_dim: int
    doom_gf: int
    chunk_lines: int
    # tau
    tau_table: dict[str, float]  # string keys, as stored in the ckpt
    target_cov: str
    calib_tau: float
    # sidecars
    changeprone: tuple | None    # (cnt: dict, total: int) or None -> zero-filled readout col
    rules: list[dict]            # rules.json roster (rule head deferred; roster validated only)
    flags: dict[str, str] = field(default_factory=dict)

    @property
    def tau_q(self) -> int:
        return int(round(self.calib_tau * 1_000_000))


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for blk in iter(lambda: f.read(1 << 20), b""):
            h.update(blk)
    return h.hexdigest()


def _derive_dims(sd: dict) -> dict:
    """Every shape self-derives from the state weights (curator.py L122-178 / model-arch.md §2)."""
    hidden = int(sd["head.0.weight"].shape[0])
    layers = sum(1 for k in sd
                 if k.startswith("convs.") and k.endswith(".weight") and sd[k].dim() == 3)
    nrel = next((int(v.shape[0]) for k, v in sd.items()
                 if k.endswith("convs.0.weight") and v.dim() == 3), None)
    rs = sd.get("read_head.0.weight")
    read_struct = int(rs.shape[1] - rs.shape[0]) if rs is not None else 16
    doom_gf = max(0, int(sd["doom_head.0.weight"].shape[1] - sd["head.0.weight"].shape[1]))
    emb_dim = int(sd["emb_proj.weight"].shape[1])
    ew = int(sd["emb_proj.weight"].shape[0])
    struct_dim = int(sd["in_proj.weight"].shape[1]) - ew
    return dict(hidden=hidden, layers=layers, nrel=nrel, read_struct=read_struct,
                doom_gf=doom_gf, emb_dim=emb_dim, ew=ew, struct_dim=struct_dim)


def load_bundle(ckpt_path: str | None = None, rules_path: str | None = None,
                changeprone_path: str | None = None) -> Bundle:
    ckpt_file = Path(ckpt_path or os.environ.get("DASEIN_CKPT", _DEFAULT_CKPT)).expanduser()
    if not ckpt_file.is_file():
        raise BundleError(f"checkpoint not found: {ckpt_file} (set DASEIN_CKPT)")
    checkpoint_id = _sha256_file(ckpt_file)
    ck = torch.load(str(ckpt_file), map_location="cpu", weights_only=False)
    if not isinstance(ck, dict) or "state" not in ck:
        raise BundleError(f"{ckpt_file}: not a curator checkpoint dict (no 'state')")
    sd = ck["state"]
    d = _derive_dims(sd)

    # ---- derived-vs-recorded cross-check: the ckpt is self-describing; both views must agree ----
    def _chk(name: str, derived, recorded):
        if recorded is None:
            raise BundleError(f"{ckpt_file.name}: recorded config missing '{name}'")
        if derived != recorded:
            raise BundleError(f"{ckpt_file.name}: {name} mismatch — weights say {derived}, "
                              f"recorded config says {recorded}")

    _chk("hidden", d["hidden"], int(ck.get("hidden", 0)) or None)
    _chk("layers", d["layers"], int(ck.get("layers", 0)) or None)
    _chk("nrel", d["nrel"], int(ck.get("nrel", 0)) or None)
    _chk("emb_dim", d["emb_dim"], int(ck.get("emb_dim", 0)) or None)
    if not ck.get("trace_level"):
        raise BundleError(f"{ckpt_file.name}: not a trace-level ckpt (per-decision path not served)")
    if not ck.get("hetgraph"):
        raise BundleError(f"{ckpt_file.name}: hetgraph=False — the v0 scorer builds only the "
                          f"typed het graph")
    if ck.get("chunk_mode") != "fixed":
        raise BundleError(f"{ckpt_file.name}: chunk_mode={ck.get('chunk_mode')!r} != 'fixed' "
                          f"(engine chunker parity)")
    if int(ck.get("chunk_lines") or 0) != int(os.environ["AC_CHUNK_LINES"]):
        raise BundleError(f"{ckpt_file.name}: chunk_lines={ck.get('chunk_lines')} != "
                          f"AC_CHUNK_LINES={os.environ['AC_CHUNK_LINES']} (engine chunker parity)")
    head_kind = ck.get("head_kind", "mlp")
    if head_kind != "mlp":
        raise BundleError(f"{ckpt_file.name}: head_kind={head_kind!r} unsupported (read_struct "
                          f"derivation + AC_HEAD pin assume the plain mlp read head)")
    # tri-embedding sanity: emb_dim must be 3x the embedder dim the wire protocol serves (1024)
    if d["emb_dim"] % 3 != 0:
        raise BundleError(f"{ckpt_file.name}: emb_dim={d['emb_dim']} not a content|action|head "
                          f"tri-embedding multiple")

    # ---- calibration table: STRING keys, nearest-key resolution (curator.py L84-95) ----
    tau_table = ck.get("tau_calib")
    if not tau_table:
        raise BundleError(f"{ckpt_file.name}: no tau_calib table — cannot serve a calibrated tau")
    target_cov = os.environ.get("DASEIN_TARGET_COV", os.environ.get("AC_TARGET_COV", "0.70"))
    try:
        tbl = {float(k): float(v) for k, v in tau_table.items()}
        want = float(target_cov)
    except (TypeError, ValueError) as e:
        raise BundleError(f"tau_calib resolution failed for DASEIN_TARGET_COV={target_cov!r}: {e}")
    key = min(tbl, key=lambda c: abs(c - want))          # nearest calibrated target coverage
    calib_tau = float(tbl[key])
    # DASEIN_SERVE_TAU: the reference's AC_SERVE_TAU dial (curator.py L99-104,
    # highest priority), namespaced so _flags can keep popping stray AC_* env.
    # An explicit operating-point override for smokes/per-harness calibration;
    # /v1/bundle reports it so the ledger's checkpoint provenance stays honest.
    serve_tau_override = os.environ.get("DASEIN_SERVE_TAU")
    if serve_tau_override:
        try:
            calib_tau = float(serve_tau_override)
        except ValueError as e:
            raise BundleError(f"DASEIN_SERVE_TAU={serve_tau_override!r}: {e}")
        print(f"[bundle] DASEIN_SERVE_TAU override active: tau = {calib_tau}", flush=True)

    # ---- model build: derived dims + explicit head_kind (never trust stray env for shape) ----
    model = PyGCurator(d["emb_dim"], d["struct_dim"], use_emb=True,
                       arch=ck.get("arch", "rgcn"), read_struct=d["read_struct"],
                       nrel=d["nrel"], doom_gf=d["doom_gf"], hidden=d["hidden"],
                       layers=d["layers"], head_kind=head_kind)
    missing, unexpected = model.load_state_dict(sd, strict=False)
    if unexpected:
        raise BundleError(f"{ckpt_file.name}: state keys the model did not consume: {unexpected}")
    if missing:
        raise BundleError(f"{ckpt_file.name}: model params absent from the ckpt: {missing}")
    model.eval()

    # ---- rules roster: must parse; count is warn-only (rule head deferred in v0) ----
    rules_file = Path(rules_path or os.environ.get("DASEIN_RULES_JSON",
                                                   str(_PKG_MODELS / "rules.json")))
    try:
        rules = json.loads(rules_file.read_text())
    except Exception as e:
        raise BundleError(f"rules roster unreadable at {rules_file}: {e}")
    if not isinstance(rules, list) or not all(isinstance(r, dict) and r.get("eid") and r.get("text")
                                              for r in rules):
        raise BundleError(f"rules roster malformed at {rules_file}: expected [{{eid, text, ...}}]")
    if len(rules) != _EXPECTED_RULES:
        print(f"[bundle] WARNING: rules roster has {len(rules)} rules (expected "
              f"{_EXPECTED_RULES}) — rule head is deferred, continuing", flush=True)

    # ---- changeprone sidecar: readout col 42; absent -> None (scorer zero-fills, PATCH 2) ----
    cp_file = Path(changeprone_path or os.environ.get("AC_CHANGEPRONE_PKL",
                                                      str(_PKG_MODELS / "changeprone.pkl")))
    changeprone = None
    try:
        _d = pickle.load(open(cp_file, "rb"))
        _cc = _d["cnt"] if isinstance(_d, dict) else _d[0]
        _ct = max((_d.get("total", 1) if isinstance(_d, dict) else _d[1]), 1)
        changeprone = (_cc, _ct)
    except Exception as e:
        print(f"[bundle] WARNING: changeprone sidecar unavailable ({e}) — "
              f"readout col zero-filled", flush=True)

    # ---- symbol-graph liveness (§8.2-adjacent): the ckpt TRAINED on real python parsing for the
    # centrality readout cols (37-41). A dead grammar silently fail-opens to zero symbols — the
    # scores would be quietly off the trained distribution, so assert liveness LOUDLY at startup.
    from .vendored.symbol_graph import python_parser_backend
    ts_backend = python_parser_backend()
    if ts_backend == "none":
        print("[bundle] WARNING: tree-sitter python grammar UNAVAILABLE — symbol-graph "
              "centrality (readout cols 37-41) serves ZEROS while the checkpoint trained on "
              "real parsing; scores are quietly off the trained distribution. Fix the env "
              "(tree-sitter-language-pack or the tree-sitter-python wheel).", flush=True)
    else:
        print(f"[bundle] symbol-graph python parser LIVE via {ts_backend}", flush=True)

    print(f"[bundle] {ckpt_file.name} sha256={checkpoint_id[:12]}… hidden={d['hidden']} "
          f"layers={d['layers']} nrel={d['nrel']} read_struct={d['read_struct']} "
          f"struct_dim={d['struct_dim']} emb_dim={d['emb_dim']} | tau[{target_cov} -> "
          f"nearest {key:.2f}] = {calib_tau:.7f}", flush=True)
    return Bundle(model=model, checkpoint_id=checkpoint_id, ckpt_path=str(ckpt_file),
                  emb_dim=d["emb_dim"], hidden=d["hidden"], layers=d["layers"], nrel=d["nrel"],
                  read_struct=d["read_struct"], struct_dim=d["struct_dim"], doom_gf=d["doom_gf"],
                  chunk_lines=int(ck["chunk_lines"]), tau_table=dict(tau_table),
                  target_cov=target_cov, calib_tau=calib_tau, changeprone=changeprone,
                  rules=rules, flags=_flags.snapshot())
