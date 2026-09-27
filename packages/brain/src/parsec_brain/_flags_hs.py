"""Env pins for the HS curator (curator_hs_t3_cand.pt.ep3.selected, spec v6828t3).

The vendored HS model reads its construction flags from the environment, so a
wrong value builds a different module tree (a missing perx_head, a wider
read_head) and the strict load in `bundle_hs` refuses it. These pins are the
ones that change what `PyGCurator.__init__` builds; they are FORCED, not
setdefault, because the legacy `_flags` module (imported first by the package
__init__) pops some of them for the bge lineage.

Scope: construction only. The featurization flags (AC_REREQ, AC_READCOLS,
AC_DUPCOS, the graph-shape flags, ...) are pinned alongside the vendored
feature code, from the resolved training env of the HS run
(gnn-train hs/env_hs_t3.sh and the envs it sources).
"""
import os

# Forced: each one changes the module tree the checkpoint must load into.
PINS = {
    "AC_PERX": "on",       # gated query-bank read head (perx_head.{Q,gate,b})
    "AC_PERX_K": "8",      # perx_head.Q.weight / gate.weight rows
    "AC_HEAD": "mlp",      # plain read_head (the PLR head is a different tree)
}

# Must be unset: each adds a module (think_head, field_head) or widens a head
# (AC_SKIP doubles the read_head input) relative to the trained tree.
UNSET = (
    "AC_SKIP",
    "AC_THINK",
    "AC_CAND_FIELDS",
    "AC_CAND_HID",
    "AC_CAND_DEPTH",
    "AC_SPAN_SKIP",
)

for _k, _v in PINS.items():
    os.environ[_k] = _v
for _k in UNSET:
    os.environ.pop(_k, None)

# ENCGNN_MODEL: where the encoder's Qwen3.5-0.8B-Base config + tokenizer load
# from (a local directory, or the HF repo id). The fine-tuned encoder weights
# come from the checkpoint, which overwrites the base weights at load. Deploy
# may point this at a baked-in local copy; defaults to the HF repo id.
os.environ.setdefault("ENCGNN_MODEL", "Qwen/Qwen3.5-0.8B-Base")


def snapshot() -> dict[str, str]:
    """Effective construction flags, for /v1/bundle provenance."""
    out = {k: os.environ.get(k, "") for k in PINS}
    out.update({k: os.environ.get(k, "") for k in UNSET})
    out["ENCGNN_MODEL"] = os.environ.get("ENCGNN_MODEL", "")
    return out
