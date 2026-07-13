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
import time
from dataclasses import dataclass, field
from pathlib import Path

import torch

from ._log import get_logger
from .neighbors import ArtifactFetcher, HoodsError
from .vendored.pyg_model import PyGCurator

_PKG_MODELS = Path(__file__).resolve().parents[2] / "models"   # packages/brain/models (repo-tracked)
_DEFAULT_CKPT = "~/.dasein/brain/curator_v4_prod.pt"           # the 36MB .pt stays OUT of the repo
_EXPECTED_RULES = 16

log = get_logger("bundle")


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
    hoods: ArtifactFetcher | None = None   # cross-trace neighborhoods; None = nf OFF (v0 default)

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
    # doom_gf derives from the doom head's input width. A ckpt WITHOUT the head could never
    # serve anyway (PyGCurator always constructs doom_head; the strict state-dict check in
    # load_bundle would reject it as missing params) — refuse HERE with a real message
    # instead of a raw KeyError (a required weight, not an optional sidecar like rules/
    # changeprone, so the warn-only precedent does not apply).
    dw = sd.get("doom_head.0.weight")
    if dw is None:
        raise BundleError("checkpoint has no doom_head.0.weight — not a curator-lineage "
                          "checkpoint (every servable ckpt carries the doom head; doom_gf "
                          "derives from its input width)")
    doom_gf = max(0, int(dw.shape[1] - sd["head.0.weight"].shape[1]))
    emb_dim = int(sd["emb_proj.weight"].shape[1])
    ew = int(sd["emb_proj.weight"].shape[0])
    struct_dim = int(sd["in_proj.weight"].shape[1]) - ew
    return dict(hidden=hidden, layers=layers, nrel=nrel, read_struct=read_struct,
                doom_gf=doom_gf, emb_dim=emb_dim, ew=ew, struct_dim=struct_dim)


def load_bundle(ckpt_path: str | None = None, rules_path: str | None = None,
                changeprone_path: str | None = None, hoods_path: str | None = None) -> Bundle:
    t_load0 = time.perf_counter()
    ckpt_file = Path(ckpt_path or os.environ.get("DASEIN_CKPT", _DEFAULT_CKPT)).expanduser()
    if not ckpt_file.is_file():
        raise BundleError(f"checkpoint not found: {ckpt_file} (set DASEIN_CKPT)")
    checkpoint_id = _sha256_file(ckpt_file)
    log.info("loading checkpoint path=%s sha8=%s", ckpt_file, checkpoint_id[:8])
    ck = torch.load(str(ckpt_file), map_location="cpu", weights_only=False)
    if not isinstance(ck, dict) or "state" not in ck:
        raise BundleError(f"{ckpt_file}: not a curator checkpoint dict (no 'state')")
    sd = ck["state"]
    d = _derive_dims(sd)
    log.info("derived dims hidden=%d layers=%d nrel=%d emb_dim=%d struct_dim=%d read_struct=%d "
             "doom_gf=%d | head shapes %s", d["hidden"], d["layers"], d["nrel"], d["emb_dim"],
             d["struct_dim"], d["read_struct"], d["doom_gf"],
             {k.rsplit(".", 2)[0]: tuple(sd[k].shape)
              for k in sd if k.endswith("head.0.weight")})

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
    log.info("tau table %s | target_cov=%s -> nearest key %.2f -> calib_tau=%.7f",
             {k: round(v, 7) for k, v in sorted(tbl.items())}, target_cov, key, calib_tau)
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
        log.info("DASEIN_SERVE_TAU override active: tau = %s", calib_tau)

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
    log.info("model built arch=%s head_kind=%s params=%d",
             ck.get("arch", "rgcn"), head_kind, sum(p.numel() for p in model.parameters()))

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
        log.warning("rules roster has %d rules (expected %d) — rule head is deferred, continuing",
                    len(rules), _EXPECTED_RULES)
    else:
        log.info("rules roster loaded path=%s rules=%d", rules_file, len(rules))

    # ---- changeprone sidecar: readout col 42; absent -> None (scorer zero-fills, PATCH 2) ----
    cp_file = Path(changeprone_path or os.environ.get("AC_CHANGEPRONE_PKL",
                                                      str(_PKG_MODELS / "changeprone.pkl")))
    changeprone = None
    try:
        _d = pickle.load(open(cp_file, "rb"))
        _cc = _d["cnt"] if isinstance(_d, dict) else _d[0]
        _ct = max((_d.get("total", 1) if isinstance(_d, dict) else _d[1]), 1)
        changeprone = (_cc, _ct)
        log.info("changeprone sidecar loaded path=%s files=%d total=%d", cp_file, len(_cc), _ct)
    except Exception as e:
        log.warning("changeprone sidecar unavailable (%s) — readout col zero-filled", e)

    # ---- symbol-graph liveness (§8.2-adjacent): the ckpt TRAINED on real python parsing for the
    # centrality readout cols (37-41). A dead grammar silently fail-opens to zero symbols — the
    # scores would be quietly off the trained distribution, so assert liveness LOUDLY at startup.
    from .vendored.symbol_graph import python_parser_backend
    ts_backend = python_parser_backend()
    if ts_backend == "none":
        log.warning("tree-sitter python grammar UNAVAILABLE — symbol-graph "
                    "centrality (readout cols 37-41) serves ZEROS while the checkpoint trained on "
                    "real parsing; scores are quietly off the trained distribution. Fix the env "
                    "(tree-sitter-language-pack or the tree-sitter-python wheel).")
    else:
        log.info("symbol-graph python parser LIVE via %s", ts_backend)

    # ---- hoods artifact (cross-trace neighborhoods): DASEIN_HOODS_PKL unset = nf OFF, exactly
    # today's serving. Set-but-missing/corrupt REFUSES TO START (train/serve-skew guard —
    # reference neighbors.py semantics; no gsutil auto-fetch, explicit mount only).
    hoods = None
    hoods_pkl = hoods_path or os.environ.get("DASEIN_HOODS_PKL")
    if hoods_pkl:
        try:
            hoods = ArtifactFetcher(
                hoods_pkl,
                x=int(os.environ.get("DASEIN_NEIGHBORS_X", "2")),   # trained value — do not change
                k=int(os.environ.get("DASEIN_NEIGHBORS", "16")))
        except (HoodsError, ValueError) as e:
            raise BundleError(f"DASEIN_HOODS_PKL={hoods_pkl}: {e}")
    else:
        log.info("neighbors OFF (DASEIN_HOODS_PKL unset) — nf=None, +3 zero block-parity cols")

    log.info("bundle READY %s sha256=%s… hidden=%d layers=%d nrel=%d read_struct=%d "
             "struct_dim=%d emb_dim=%d doom_gf=%d neighbors=%s | tau[%s -> nearest %.2f] "
             "= %.7f | load_ms=%.1f",
             ckpt_file.name, checkpoint_id[:12], d["hidden"], d["layers"], d["nrel"],
             d["read_struct"], d["struct_dim"], d["emb_dim"], d["doom_gf"],
             hoods is not None, target_cov, key, calib_tau,
             (time.perf_counter() - t_load0) * 1000.0)
    return Bundle(model=model, checkpoint_id=checkpoint_id, ckpt_path=str(ckpt_file),
                  emb_dim=d["emb_dim"], hidden=d["hidden"], layers=d["layers"], nrel=d["nrel"],
                  read_struct=d["read_struct"], struct_dim=d["struct_dim"], doom_gf=d["doom_gf"],
                  chunk_lines=int(ck["chunk_lines"]), tau_table=dict(tau_table),
                  target_cov=target_cov, calib_tau=calib_tau, changeprone=changeprone,
                  rules=rules, flags=_flags.snapshot(), hoods=hoods)
