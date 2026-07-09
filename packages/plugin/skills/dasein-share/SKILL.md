---
name: dasein-share
description: Manage opt-in telemetry sharing (off by default). Supports --preview (show exact bytes before anything uploads), off, and purge.
---

Telemetry is **off by default** and the product is fully functional without it
(Tier 0). Handle subcommands via `${CLAUDE_PLUGIN_ROOT}/bin/dasein`:

- `--preview` — dump the exact featurized trace that would be uploaded from
  `~/.dasein/spool/`, locally, human-readable. Show the bytes.
- `on [tier]` — enable Tier 1 (metrics) or Tier 2 (featurized traces). State
  plainly that vectors are a mitigation, not anonymity. A tier or schema
  change must re-prompt — consent never silently expands.
- `off` — stop instantly.
- `purge` — file deletion: removed from corpus and excluded from all future
  training runs; true erasure from an already-trained checkpoint means
  retraining, and we say so plainly.
