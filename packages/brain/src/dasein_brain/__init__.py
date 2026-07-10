"""dasein-brain — hosted scoring API (control plane, private).

v0 (inference-first, docs/brain-serving-v0.md): serves curator_v4_prod.pt behind
/v1/score/trace (chunk keep/cut scores + calibrated tau, quantized to the 1e-6
grid) and /v1/score/tools (tool-schema keep scores), running the VENDORED
reference scoring path (dasein_brain.vendored) so parity with the trainer
forward is by construction. The bundle self-validates at startup — dims derived
from the ckpt weights must match its recorded config, tau_calib must resolve
DASEIN_TARGET_COV — and any mismatch refuses to start (§8.2: brain-API score ==
trainer-forward score at the checkpoint's calib_tau on identical chunks).

⚠️ The v0 dev contract (brain-api-dev/v0) carries RAW TEXT to our cluster —
explicit opt-in, dev/dogfood machines only. The same endpoints ALSO serve the
brain-api/v1 contract (vectors + features + opaque ids, raw text
unrepresentable by construction — dasein_brain.v1graph), dispatched on the
request `contract` field and pinned score-identical to the dev path by the
cross-path parity gate (tests/test_v1.py); the proxy migrates to it as the
client-side featurization port lands.
"""
from . import _flags  # noqa: F401  parity env pins — MUST precede any vendored import
