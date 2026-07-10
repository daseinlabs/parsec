"""Vendored reference modules — the exact code that trained/validated curator_v4_prod.pt.

Provenance:
  pyg_model.py / trace_graph.py / trace_train.py  — rulehead gnn-train drop (trained the ckpt)
  chunking.py / torch_curator.py / attribution.py / symbol_graph.py / labelers.py /
  trace_contract.py / embedding.py / dasein_embed.py — adaptive-context-clean/adaptive_context

Byte-close policy (docs/brain-serving-v0.md: parity by construction): the only changes vs the
sources are (1) package-relative import rewrites, (2) deletions of training-only code (DDP loop,
corpus loaders, needed-label machinery, legacy vertex/cst branches), and (3) patches marked with a
'# DASEIN-PATCH:' comment:
  PATCH 1 (dasein_embed.py)  — JSON POST to DASEIN_EMBED_URL replaces k8s pod discovery +
                               binary /encode_bin; DASEIN_EMBED_BACKEND=hash (embedding.py's
                               deterministic backend) makes tests hermetic.
  PATCH 2 (scorer._het_readout, dasein_brain/scorer.py) — the readout ALWAYS emits all 49
                               decided_struct cols: changeprone zero-filled when the pickle is
                               absent, scout cols zero-filled when no scout record (the reference
                               skips absent blocks and crashes the read_head matmul at 433≠width).
  PATCH 3 (trace_graph.build_tool_spec) — step-0 task-text fallback, verbatim from the reference
                               SERVING copy (adaptive-context-clean/scripts/trace_graph.py); the
                               training drop lacks it and crashes on a zero-observation request.

`dasein_brain._flags` MUST be imported before anything here (chunking reads AC_CHUNK_MODE at
import time); the parent package __init__ enforces that for any `dasein_brain.*` import.
"""
from .. import _flags  # noqa: F401  (parity pins before any vendored module body runs)
