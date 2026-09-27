"""HS-curator model code — supplied at image build time, not committed to this repository.

The HS checkpoint (curator_hs_t3_cand.pt.ep3.selected) is weights only; the brain also needs the code
it was trained under. These modules are verbatim copies from daseinlabs/gnn-train @ 590a852 and are
gitignored here: the private image build copies them into this directory. Without them the brain's
HS path does not import and its tests skip.

  pyg_model.py    — adaptive_context/optimizer/pyg_model.py, import paths made package-relative
  enc_pool.py     — hs/enc_pool.py
  encgnn.py       — hs/encgnn_train.py L628-676 (EncGNNCurator), with package-relative imports of
                    EncoderPool and PyGCurator
  read_columns.py — scripts/fight_targets_helpers.py (_norm, _bash_read_targets) + torch_curator.py
                    (PATH_V9 helpers and the v13 READ-STRUCT block)
  rereq.py        — torch_curator.py re-request block; used only by the v2 adapter (on v3 the client
                    computes these columns, engine rereq.rs)

Committed here:
  attribution.py  — PARSEC-PATCH shim re-exporting the legacy `vendored.attribution.spans`: the HS
                    tree's attribution.py is not in the gnn-train sync. Replace with the verbatim file
                    when it is.

Byte-close policy (parity by construction, as for `vendored/`): the only changes vs the sources are
package-relative import rewrites and deletions of training-only code; anything else is marked
'# PARSEC-PATCH:'.

`parsec_brain._flags_hs` MUST be imported before anything here: PyGCurator reads AC_PERX /
AC_PERX_K / AC_HEAD at construction time.
"""
from .. import _flags_hs  # noqa: F401  (construction pins before any vendored module body runs)
