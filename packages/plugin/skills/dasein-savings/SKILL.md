---
name: dasein-savings
description: Show measured token savings for this session and account — from the count_tokens counterfactual ledger, never a modeled baseline.
---

Run `${CLAUDE_PLUGIN_ROOT}/bin/dasein statusline --report` and present the
savings report to the user: tokens saved, cut %, cache read/write ratio, and —
on subscription plans — usage reduction framed against their rate limits.
Every number must come from the local savings ledger (count_tokens probe vs
actually-billed usage). If the ledger is empty, say so; never estimate.
