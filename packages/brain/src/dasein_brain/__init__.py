"""dasein-brain — hosted scoring API (control plane, private).

Receives chunk vectors + structural features (never raw text), returns
keep/cut scores. Checkpoint bundles version the ckpt and its matched dials as
one immutable artifact — a mismatch is a load-time error, never silent
(DIRECTION.md §7, invariant §8.2: brain-API score == trainer-forward score at
the checkpoint's calib_tau on identical chunks).
"""
