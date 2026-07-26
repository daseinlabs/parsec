"""parsec-trainer — server-side training + eval gates + checkpoint promotion.

v0 (inference-first, docs/brain-serving-v0.md): `retrain.py` is the IDENTITY
stub — the collect -> train -> eval-gate -> promote pipeline runs for real,
but the train stage re-emits the base GNN unchanged (`trained: false` in the
bundle manifest). It exists so the promote -> brain-reload loop is testable
locally without a GPU.

The real trainer consumes the scripts/trace_graph + trace_train lineage from
adaptive-context. The temporal-firewall / eval-exclusion discipline extends
to community data: the trainer refuses to run without the exclusion list
(DIRECTION.md §6). Promotion pipeline: collect -> label -> train -> validate
against held-out slice -> promote only on pass. The enterprise on-prem
fine-tune is this same pipeline, containerized. Training code must never be
a runtime dependency of serving (§7): the promoted bundle DIRECTORY is the
only interface the brain sees.
"""
