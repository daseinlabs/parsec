"""`spans` for vendored_hs.pyg_model.edges (its rel-4 supersession block imports it).

PARSEC-PATCH: re-export of the legacy vendored copy, not a gnn-train file. gnn-train @ 590a852 does
not carry adaptive_context/optimizer/attribution.py; replace this shim with
the verbatim HS-tree copy when it lands.

Why the shim is score-neutral on the serve path: scorer_hs passes stand-in chunks with text="", so
`spans("")` is the empty set and edges() emits only its same-file line-overlap rel-4 subset, which
scorer_hs drops and replaces with the client's `edges_supersession` pairs (the v1graph pattern).
"""
from __future__ import annotations

from ..vendored.attribution import spans

__all__ = ["spans"]
