"""parsec-bench — cc-bench harness, grader, and arms.

Sits at the top of the dependency graph: bench -> proxy -> engine. It drives
the `parsec` binary as a black box and is never imported by product code.

Modules:
  arm            — the Arm adapter contract + registry (baseline built in)
  arms           — concrete arms (parsec); importing registers them
  cc_runner      — the headless-Claude-Code runner (real spend; see README)
  replay         — replay recorded conversations, no credits (the smoke mode)
  usage_gateway  — passthrough bottom bridge; per-run usage JSONL
  ledger         — savings-ledger accounting (§8.4 counterfactual math)
  pricing        — cache-aware price frames
  schema         — RunRecord / CallUsage / AggResult
  grader         — official SWE-bench grading seam
  prepare_repos  — per-instance worktree + test-env provisioning
  proxy_bin      — locate/drive the parsec binary (black box)
  mock_upstream  — in-process Anthropic-shaped mock (replay + tests)
"""

__version__ = "0.1.0"

__all__ = ["arm", "arms", "cc_runner", "grader", "ledger", "mock_upstream",
           "prepare_repos", "pricing", "proxy_bin", "replay", "schema",
           "usage_gateway"]
