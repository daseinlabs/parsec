# Beyond cost: the proxy as a learned process controller

**Status:** direction note, 2026-07-31. Follows from `docs/context-management-research.md`
(and supersedes its framing). That doc's harsh read concluded "context management as a
proxy product does not survive scrutiny." This doc records the reframe that does:
stop optimizing for cost alone; make the proxy **learn the user's workflows and
enforce them** — proving learning on faster / cheaper / better, any one axis.
Cache invalidation is explicitly demoted from invariant to trade-off in this
direction: breaking cache is acceptable when the result is better.

## 1. The reframe

Three positions were considered and rejected on the way here:

- **Context management (injection/memory)** — rejected in the research doc:
  injection doesn't need the wire, evidence is weak, no training signal.
- **Proxy-side retrieval instead of agent grep** (kNN + GNN prune, blind
  injection) — rejected: agentic search is closed-loop (bad grep → refined
  grep), injection is open-loop (no error signal anywhere); cosine similarity
  retrieves look-alike code, not the caller/type/config/test an edit actually
  needs; nobody — not even Cursor, the best-resourced believer in code
  retrieval — blind-injects; they route retrieval through a model-invoked tool.
- **Memory systems** — rejected for the right reason: memory is a *suggestion*.
  The model must choose to consult it. Hit or miss.

What makes parsec's pruning credible is that it is **enforced** — the model
cannot see what was cut. The direction: find the other interventions with that
same enforced character, load them with learned knowledge of this user's / this
repo's workflows, and measure the win in outcomes, not tokens.

The target is not "smaller context." It is the **environment re-discovery tax**:
agents burn turns re-learning the same repo facts every session — which test
command works, which files matter for which kind of task, which commands always
fail here, what was already grepped. That tax is real, measurable, and a
wire-level observer is uniquely positioned to learn it — the proxy sees every
tool call, result, and outcome, across every session, per repo.

## 2. The enforcement spectrum

Not all interventions are equal. Ranked by how much the model can ignore them:

```
blocked action        model never gets to do it            (PreToolUse deny / Stop gate)
  > synthetic result  model sees what we chose to show     (short-circuited tool call)
  > tool description  re-read at every decision point      (wire: rewrite tools[])
  > injected context  in attention, ignorable              (wire: request splice)
  > memory tool       requires model initiative            (the "hit or miss" tier)
```

"Memory is a suggestion" is true; injection sits mid-spectrum, not at the
bottom; the top two tiers are absolute. Note the top tier doesn't even need the
wire — hooks can deny. The wire is needed for tiers 2–4 (synthetic results on
the response path, tool-description rewriting, context splicing).

## 3. The interventions, ranked by enforcement × learnability

### 3.1 Cross-session action gate — generalize no-reread into a workflow governor
The shipped no-reread hook is the degenerate case (same-session, same-file,
deterministic). The general case: block or short-circuit actions that history
says are redundant or doomed, serving an informative synthetic result instead:

- *"You ran this exact grep last session; nothing under `src/` changed since;
  here's the digest."* (redundancy: checkable — same command, unchanged inputs
  via mtime/content hash)
- *"`npm test` has failed 4/4 times in this repo — the working invocation is
  `make test`."* (failure: observed — exit codes)

Absolute enforcement; faster (no execution, no round-trip); cheaper (no
2,000-line output billed); better (agent stops rediscovering the environment).
Labels are natural, not counterfactual. Staleness is the risk — validity checks
(mtime, content hash) must gate every served cache, and anything uncheckable
doesn't get gated.

### 3.2 Completion gate — enforce the user's definition of done
The Stop-hook governor seam already exists in the plugin design. Learned
version: this user never accepts work without lint + targeted tests; the agent
tries to finish without them; the gate bounces it back naming the missing step.
"Better" axis, hard enforcement — the agent cannot declare done wrong. This is
exactly where memory-as-suggestion fails and a gate cannot.

### 3.3 Workflow priming — injection re-aimed at turn elimination
The expensive unit is not tokens; it is **turns** — each exploration turn is a
full model round-trip. High-precision priming at task start (correct
build/test/run commands, the 3 files the last five similar tasks touched,
environment gotchas) deletes exploration turns; wall-clock and cost both
collapse regardless of cache. Discipline carried over from the research doc:
the open-loop problem is still real, so inject only high-confidence, *small*
patterns — commands and file identifiers, not code bodies. Precision over
volume.

### 3.4 Tool-surface shaping — knowledge at the decision point
Rewrite tool descriptions per repo with learned corrections: the Bash tool's
description gains *"in this repo: `make test`, not `npm test`; requires
`DATABASE_URL` from `.env.local`."* The model re-reads tool descriptions every
turn at the moment of choosing an action — the most attention-privileged
placement for workflow knowledge, far stronger than a memory note. Only the
wire can do this.

### 3.5 Learned result rewriting — semantic trimming
Today's cuts are mechanical. The learned version rewrites: 2,000 lines of test
output become the 30 lines this model actually uses downstream (learned from
what gets quoted/acted on across the corpus). Enforced — the original never
reaches the model. The natural deepening of the existing product rather than a
new one.

### 3.6 Inline critic — most aggressive, hold for last
The proxy sees the response before the harness does. If a learned model
predicts the action is wrong (*"editing the generated file, not the source
template — this user reverted exactly this twice"*), suppress the response,
re-issue with a corrective system message, serve the retry. Double inference on
intervened turns, but a wrong path costs far more than 2× one turn. Real
enforcement of learned judgment — and the fastest trust-destroyer if precision
is low. Comes only after the process model has proven itself in 3.1–3.5.

## 4. Why this direction is sounder than injection

**The training-signal problem inverts.** Learned injection had no labels
(counterfactual — you never observe the un-taken path). Process learning has
abundant ones:

- next-action prediction over tool-call traces = behavior cloning, ground truth
  in every session;
- redundancy = mechanically checkable (command identity + input freshness);
- failure = observed exit codes;
- turns-to-completion = directly measurable.

And the GNN already consumes **trace graphs** — next-action / redundancy /
failure / completion-readiness heads are a far more natural extension of that
architecture than a relevance-injection head ever was. Same featurization
pipeline, same telemetry, new heads; each head powers one enforced intervention
(prime / gate / rewrite / governor).

**v1 needs no model at all.** "This exact command failed 4 times," "this grep
already ran against unchanged files," "tests always follow edits in this repo"
is deterministic pattern mining — counting, not gradients. That fits the
existing local-adaptation doctrine (no local training; cheap, deterministic,
can't corrupt the model) and generates the labeled corpus the learned version
trains on later. Same sequencing logic as keep/cut: heuristics first, flywheel,
then the learned head.

## 5. Deployment: shadow mode, precision-gated

Every intervention runs silently first: predict the block/prime/rewrite, don't
enforce, measure whether the prediction was right (did the agent's grep return
the digest we would have served? did the command fail as predicted? did the
agent end up reading the files we would have primed?). Enforce only above a
per-intervention precision threshold. This is the abstention-first principle
from the bandit literature (arXiv 2604.27283: penalize false-positive
intervention over missed intervention) applied to enforcement, and it produces
the proof artifact this direction needs.

**The metric shifts** from tokens saved to: turns-to-completion, wall-clock,
command-failure rate, retry rate — A/B'd against passthrough with the same
measurement-honesty discipline as the savings ledger (measured counterfactual,
never modeled). Token savings remains a reported axis, no longer the objective.

## 6. The harsh check on this direction

- **Risk concentrates in the gate.** A wrong *cut* degrades silently; a wrong
  *block* visibly fights the user's agent. Precision requirements are brutal —
  hence the ordering: deterministically-checkable gates first (3.1),
  high-confidence tiny priming second (3.3), judgment-based intervention last
  (3.6), all behind shadow-mode precision thresholds.
- **Staleness is the failure mode of served caches.** Every synthetic result
  must carry a validity proof (mtime/hash); anything unprovable executes
  normally.
- **Per-user/per-repo learning is local adaptation, not local training** — keep
  it to deterministic statistics client-side; learned heads train server-side
  on the opt-in corpus like everything else.
- **What this does NOT bet on:** it never bets against the model's own tool use
  (the retrieval mistake). It bets against the environment re-discovery tax —
  the agent redoing what the wire already watched it do.

## 7. Relation to existing invariants

- **Fail open, measured** — unchanged; every gate/rewrite degrades to
  passthrough, counted.
- **Determinism boundary** — gates and priming are functions of (trace history,
  pattern store version, config); the pattern store versions into the config
  hash like a checkpoint.
- **Cache stability** — demoted from invariant to *costed trade-off* in this
  direction: an intervention may break cache when its measured outcome win
  exceeds the cache cost. The ledger must price both sides (cache cost is
  exactly measurable; outcome win comes from shadow-mode/A-B deltas).
- **Measurement honesty** — extended, not relaxed: intervention value is only
  ever claimed from measured A/B deltas against passthrough, never modeled.
