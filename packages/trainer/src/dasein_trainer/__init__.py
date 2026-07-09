"""dasein-trainer — server-side training + eval gates + checkpoint promotion.

Consumes the scripts/trace_graph + trace_train lineage from adaptive-context.
The temporal-firewall / eval-exclusion discipline extends to community data:
the trainer refuses to run without the exclusion list (DIRECTION.md §6).
Promotion pipeline: collect -> label -> train -> validate against held-out
slice -> promote only on pass. The enterprise on-prem fine-tune is this same
pipeline, containerized.
"""
