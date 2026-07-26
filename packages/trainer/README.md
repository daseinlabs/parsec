# parsec-trainer

**v0 is the IDENTITY retrain stub** (docs/brain-serving-v0.md inference-first
decision): the pipeline shape — collect → train → eval gate → promote — runs
for real and is tested locally, but the train stage returns the base GNN
unchanged and the bundle manifest says so (`"trained": false`). It exists so
the full product loop (served traffic → capture seams → trainer → promoted
bundle → brain reload) is exercisable on a laptop, with the exact seams a
real trainer drops into.

```sh
cd packages/trainer
uv venv .venv && uv pip install --python .venv/bin/python \
  torch --index-url https://download.pytorch.org/whl/cpu
uv pip install --python .venv/bin/python "numpy<2" -e ../brain -e ".[test]"

# "retrain" (identity) and promote a versioned bundle:
.venv/bin/python -m parsec_trainer.retrain \
  --base-ckpt ~/.parsec/brain/curator_v4_prod.pt --out /tmp/bundles

# serve the promoted bundle:
PARSEC_CKPT=/tmp/bundles/<version>/curator.pt \
PARSEC_RULES_JSON=/tmp/bundles/<version>/rules.json ... (brain README)
```

What is REAL in the stub: the eval gate loads candidates through the brain's
own self-validating bundle loader (§8.2 — features and weights move
together), scores a fixed conversation, and refuses promotion on any
mismatch; promotion writes an immutable versioned bundle whose
`checkpoint_id` is the sha256 of the artifact. What is STUBBED: the training
step (identity), labeling, the corpus (collect() only inventories capture
rows), and the held-split thresholds (identity uses exact score equality).

The real trainer replaces `train_identity()` with the trace_graph/trace_train
lineage (~17 min on 4×A100 per PRODUCTION_SERVING_HANDOFF.md), the equality
criterion with cov@70 / within-run-AUC thresholds on a held split, and
enforces the eval-exclusion list (temporal firewall — it must refuse to run
without one). Training code never becomes a runtime dependency of serving:
the promoted bundle **directory** is the only interface the brain sees.
