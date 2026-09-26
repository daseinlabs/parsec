"""Self-validating bundle for the HS curator (gnn-train hs/README.md §"Load assert").

The HS checkpoint records almost no config (select_metric, best*, tau_calib_kind, preliminary,
state), so unlike `bundle.load_bundle` there is no recorded-vs-derived cross-check. Instead every
dim is derived from the weights, the model is built from those dims, the state loads STRICTLY, and
the result is checked against the handoff's fixed load assert. Any deviation raises BundleError and
the brain refuses to start: a partially-loaded HS model scores, but scores wrong.
"""
from __future__ import annotations

from . import _flags_hs  # noqa: F401  construction pins before the vendored import below

import os
import time
from dataclasses import dataclass, field

import torch

from ._log import get_logger
from .bundle import BundleError, _resolve_ckpt, _sha256_file

log = get_logger("bundle_hs")

# The handoff's load assert (gnn-train hs/README.md §1; HS_SERVE_HANDOFF_9-24 §1). Fixed numbers,
# not derived: a checkpoint that differs in any of them is not the model the battery measured.
EXPECT_TENSORS = 378
EXPECT_ENCODER_TENSORS = 320
EXPECT_SHAPES = {
    "in_proj.weight": (384, 234),        # emb_proj 128 + node struct 106
    "read_head.0.weight": (384, 488),    # hidden 384 + decided struct 104
    "perx_head.Q.weight": (8, 488),
    "perx_head.gate.weight": (8, 488),
    "perx_head.b": (1,),
}
EXPECT_NODE_STRUCT = 106
EXPECT_READ_STRUCT = 104
ENCODER_PREFIX = "encpool."


@dataclass
class HSBundle:
    model: torch.nn.Module
    checkpoint_id: str           # sha256 hex of the checkpoint file
    ckpt_path: str
    hidden: int
    layers: int
    nrel: int
    emb_dim: int                 # encoder hidden width (per-token features into emb_proj)
    node_struct: int             # xs width (in_proj input minus emb_proj output)
    read_struct: int             # decided_struct width
    doom_gf: int
    cand_struct_w: int
    perx_k: int
    best_ep: int | None
    preliminary: bool
    flags: dict[str, str] = field(default_factory=dict)


def _derive_dims(sd: dict) -> dict:
    """Every shape the model constructor needs, read off the state weights."""
    def shape(k: str) -> tuple:
        if k not in sd:
            raise BundleError(f"checkpoint has no {k} — not an HS-lineage curator")
        return tuple(sd[k].shape)

    hidden = shape("head.0.weight")[0]
    layers = sum(1 for k in sd
                 if k.startswith("convs.") and k.endswith(".weight") and sd[k].dim() == 3)
    nrel = shape("convs.0.weight")[0]
    ew, emb_dim = shape("emb_proj.weight")
    node_struct = shape("in_proj.weight")[1] - ew
    read_struct = shape("read_head.0.weight")[1] - hidden     # AC_SKIP off: no h0 half
    doom_gf = shape("doom_head.0.weight")[1] - hidden
    cand_struct_w = shape("cand_head.0.weight")[1] - 2 * hidden
    perx_k = shape("perx_head.Q.weight")[0]
    return dict(hidden=hidden, layers=layers, nrel=nrel, emb_dim=emb_dim, ew=ew,
                node_struct=node_struct, read_struct=read_struct, doom_gf=doom_gf,
                cand_struct_w=cand_struct_w, perx_k=perx_k)


def check_state(sd: dict) -> dict:
    """The handoff load assert over the raw state dict (no model build). Returns the derived dims.
    Split out so the assert is testable without loading the 0.8B encoder."""
    n = len(sd)
    if n != EXPECT_TENSORS:
        raise BundleError(f"checkpoint has {n} state tensors, expected {EXPECT_TENSORS}")
    n_enc = sum(1 for k in sd if k.startswith(ENCODER_PREFIX))
    if n_enc != EXPECT_ENCODER_TENSORS:
        raise BundleError(f"checkpoint has {n_enc} encoder tensors, expected "
                          f"{EXPECT_ENCODER_TENSORS} — encoder not fine-tuned into the ckpt")
    for k, want in EXPECT_SHAPES.items():
        got = tuple(sd[k].shape) if k in sd else None
        if got != want:
            raise BundleError(f"{k}: shape {got}, expected {want}")
    d = _derive_dims(sd)
    if d["node_struct"] != EXPECT_NODE_STRUCT:
        raise BundleError(f"node struct width {d['node_struct']}, expected {EXPECT_NODE_STRUCT}")
    if d["read_struct"] != EXPECT_READ_STRUCT:
        raise BundleError(f"decided struct width {d['read_struct']}, expected {EXPECT_READ_STRUCT}")
    if int(os.environ.get("AC_PERX_K", "0")) != d["perx_k"]:
        raise BundleError(f"AC_PERX_K={os.environ.get('AC_PERX_K')} but perx_head has "
                          f"{d['perx_k']} experts")
    return d


def load_hs_bundle(ckpt_path: str | None = None) -> HSBundle:
    """Load, build, strictly load, verify. PARSEC_HS_CKPT names the checkpoint: a local path or
    hf://<org>/<repo>/<file> (the `bundle._resolve_ckpt` convention)."""
    from .vendored_hs.encgnn import EncGNNCurator

    t0 = time.perf_counter()
    spec = ckpt_path or os.environ.get("PARSEC_HS_CKPT")
    if not spec:
        raise BundleError("PARSEC_HS_CKPT is not set (path or hf://org/repo/file)")
    ckpt_file = _resolve_ckpt(spec)
    if not ckpt_file.is_file():
        raise BundleError(f"checkpoint not found: {ckpt_file}")
    checkpoint_id = _sha256_file(ckpt_file)
    log.info("loading HS checkpoint path=%s sha8=%s", ckpt_file, checkpoint_id[:8])
    ck = torch.load(str(ckpt_file), map_location="cpu", weights_only=False)
    if not isinstance(ck, dict) or "state" not in ck:
        raise BundleError(f"{ckpt_file}: not a curator checkpoint dict (no 'state')")
    sd = ck["state"]
    d = check_state(sd)
    log.info("HS dims hidden=%d layers=%d nrel=%d emb_dim=%d node_struct=%d read_struct=%d "
             "doom_gf=%d cand_struct_w=%d perx_k=%d", d["hidden"], d["layers"], d["nrel"],
             d["emb_dim"], d["node_struct"], d["read_struct"], d["doom_gf"], d["cand_struct_w"],
             d["perx_k"])

    # Constructor call mirrors the battery scorer (gnn-train hs/hs_score_shard.py): drop=0.0,
    # rgcn, dims from the weights. Building EncoderPool loads the Qwen base (ENCGNN_MODEL); the
    # checkpoint's 320 encoder tensors then overwrite every base weight in the strict load below.
    model = EncGNNCurator(d["emb_dim"], d["node_struct"], hidden=d["hidden"], layers=d["layers"],
                          arch="rgcn", drop=0.0, nrel=d["nrel"], doom_gf=d["doom_gf"],
                          read_struct=d["read_struct"], head_kind="mlp",
                          cand_struct_w=d["cand_struct_w"])
    missing, unexpected = model.load_state_dict(sd, strict=False)
    if unexpected:
        raise BundleError(f"state keys the model did not consume: {sorted(unexpected)[:10]}")
    if missing:
        raise BundleError(f"model params absent from the ckpt: {sorted(missing)[:10]}")
    n_model = len(model.state_dict())
    if n_model != EXPECT_TENSORS:
        raise BundleError(f"model has {n_model} state tensors, expected {EXPECT_TENSORS}")
    model.eval()
    log.info("HS bundle READY %s sha256=%s… tensors=%d/%d encoder=%d best_ep=%s preliminary=%s "
             "load_ms=%.1f", ckpt_file.name, checkpoint_id[:12], n_model, EXPECT_TENSORS,
             EXPECT_ENCODER_TENSORS, ck.get("best_ep"), ck.get("preliminary"),
             (time.perf_counter() - t0) * 1000.0)
    return HSBundle(model=model, checkpoint_id=checkpoint_id, ckpt_path=str(ckpt_file),
                    hidden=d["hidden"], layers=d["layers"], nrel=d["nrel"],
                    emb_dim=d["emb_dim"], node_struct=d["node_struct"],
                    read_struct=d["read_struct"], doom_gf=d["doom_gf"],
                    cand_struct_w=d["cand_struct_w"], perx_k=d["perx_k"],
                    best_ep=ck.get("best_ep"), preliminary=bool(ck.get("preliminary")),
                    flags=_flags_hs.snapshot())
