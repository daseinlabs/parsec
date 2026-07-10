"""Parity-critical env pins for curator_v4_prod.pt (graph-construction spec §6).

The vendored reference code composes the graph and the 49-col readout struct
from environment flags; a wrong flag is a silently-wrong-score bug, not a
crash. This module pins every flag the ckpt was trained under and MUST be
imported before any vendored module — `chunking.py` reads AC_CHUNK_MODE at
import time. `dasein_brain/__init__.py` imports it first so any
`dasein_brain.*` import gets the pins; scorer/bundle/app import it explicitly
as well.

setdefault-pinned (deploy may override only knobs that are not shape-critical;
the bundle re-validates chunk_mode/chunk_lines/hetgraph against the ckpt):
"""
import os

PINS = {
    "AC_CHUNK_MODE": "fixed",    # == ckpt chunk_mode; read at chunking import time
    "AC_CHUNK_LINES": "10",      # == ckpt chunk_lines (read-atom granularity G)
    "AC_HETGRAPH": "on",         # step spine + typed hubs, nrel=14 (ckpt hetgraph=True)
    "AC_HUBPROPS": "off",        # file hubs carry ZERO props — feeding real props shifts
    #                              every score high -> under-evict (documented parity bug)
    "AC_ISSUEMATCH": "on",       # +5 readout cols (issue-symbol span match)
    "AC_SCOUTFEAT": "on",        # +4 readout cols; "on" is no pickle path -> zeros (the
    #                              trained missing-value convention), width preserved
    "AC_FILECHAIN": "on",        # causal same-file line-neighbor chain (rel 0)
    "AC_KNN_XFILE": "on",        # kNN sim edges cross-file only (rel 2)
    "AC_HEAD": "mlp",            # == ckpt head_kind; wrong value builds a random read_head
    "AC_NSCACHE": "on",          # node-struct caching (byte-identical perf toggle)
    "AC_TARGET_COV": "0.70",     # serving operating point (bundle prefers DASEIN_TARGET_COV)
}

# Flags that must be UNSET for this ckpt: any value silently changes the graph
# shape (STEPGRAPH/STRICTEDGE/PRUNE/GATE/RULE), the read_head width (SKIP), or
# the score/tau scale (FIXED_TAUS/AC_SERVE_TAU). Popped, never inherited.
UNSET = (
    "AC_STEPGRAPH",
    "AC_STRICTEDGE",
    "AC_PRUNE",
    "AC_SKIP",
    "AC_GATE",
    "AC_RULE",
    "FIXED_TAUS",
    "AC_SERVE_TAU",
    "AC_TARGET_CUT",
)

for _k, _v in PINS.items():
    os.environ.setdefault(_k, _v)
for _k in UNSET:
    os.environ.pop(_k, None)


def snapshot() -> dict[str, str]:
    """The effective flag state, exposed on /v1/bundle (capture seam)."""
    out = {k: os.environ.get(k, "") for k in PINS}
    out.update({k: os.environ.get(k, "") for k in UNSET})
    return out
