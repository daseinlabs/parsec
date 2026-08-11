# From context trimming to context management — research survey + design framing

**Status:** research note, 2026-07-31. Deep-research synthesis (papers, OSS ecosystem,
Anthropic wire mechanics) for evolving parsec from context *trimming* (keep/cut) to
context *management* (keep/cut/**inject**). No implementation decisions made here;
this is the map for making them.

> **Follow-up:** the direction that survived this doc's harsh read — reframing from
> cost/context-management to learned workflow *enforcement* — is developed in
> `docs/workflow-enforcement-direction.md`.

---

## 0. The harsh read — conclusions before the survey

The survey below can be read as a case for proxy-level injection. It shouldn't be.
The same evidence, read without a thumb on the scale:

1. **The empty intersection (§2) is empty because it's a bad position, not
   undiscovered.** Injection needs intent and task state; the harness owns both,
   the model owns judgment about what it needs, the wire owns neither. Everyone at
   the wire does subtraction because subtraction is the only operation that
   doesn't require understanding intent.
2. **Injection doesn't need the wire.** The wire is mandatory only for
   subtraction. Injection has sanctioned front doors — `UserPromptSubmit`
   additional context, SessionStart hooks, MCP, skills, CLAUDE.md — which is why
   the whole ecosystem injects at the harness layer. A parsec context-management
   play at the harness layer competes against free incumbents (claude-mem et al.)
   with no architectural moat; doing it at the proxy instead is differentiation
   as a story, fragility as engineering.
3. **Evidence that injection helps a strong coding agent is weak.** Full-context
   baselines beat the memory systems on their own benchmarks (Mem0 ~68% vs ~73%
   full-context; Zep's numbers collapsed under re-evaluation). Anthropic's own
   guidance is JIT retrieval via tools over pre-loading; injecting repo knowledge
   into an agent that can grep is a bet against the model's tool use with no
   published evidence behind it (aider's map pays off because aider lacks
   agentic exploration).
4. **The cache invariant is structurally hostile to injection.** Ephemeral tail
   injection is re-billed uncached every turn; frozen injection permanently
   occupies context the product is paid to reclaim. Trimming saves what injection
   spends.
5. **The "novel GNN inject head" is a negative signal dressed as opportunity.**
   Keep/cut labels are mechanical (`is_needed`); inject labels are counterfactual
   and unobservable. The field is stuck at bandits in toy QA settings because the
   training signal doesn't exist — not because nobody had the architecture.
6. **The platform is eating the layer.** Memory tool, context editing, server-side
   compaction, mid-conversation system messages — all shipped within ~a year.
   Features that compensate for harness/API deficiencies carry a first-party
   expiry date.

**What survives:** the only injections that *require* the proxy are repairs of
things only the proxy knows — (a) **recall of cut content** (an elision marker
with no retrieval path is a trimming bug; closing it makes aggressive trimming
safe — §5A) and (b) **compaction/discontinuity repair** (§5D). Both are trimming
insurance, not a second product. Cross-session memory, repo maps, and playbooks
(§5B–C) belong at the plugin layer if anywhere, on their merits against free
incumbents; the learned inject head (§5E) is unfunded research until A/D generate
a real training signal. "Context management" as a proxy product category does not
survive scrutiny; "trimming with a recall path" does.

---

## 1. The framing the literature gives us

The 2025 context-engineering survey (arXiv [2507.13334](https://arxiv.org/abs/2507.13334),
1400+ papers) and the Dec-2025 agent-memory survey
([2512.13564](https://arxiv.org/abs/2512.13564)) converge on a four-operation taxonomy:

| Operation | What it is | Parsec today |
|---|---|---|
| **Select** | choose what enters the window (retrieval, ranking, gating) | ✅ GNN keep/cut — but only *subtractive* select |
| **Compress** | reduce what's in the window (summarize, prune, elide) | ✅ freezing + elision digests |
| **Write** | persist state outside the window (notes, ledgers, graphs, playbooks) | ❌ nothing survives the transcript |
| **Isolate** | partition context across agents/workspaces | ◐ `parsec:explore` subagent maps |

"Context management" = adding the **write** path and the *additive* half of **select**
(injection). Two empirical constraints bind any injector:

- **Lost in the Middle** ([2307.03172](https://arxiv.org/abs/2307.03172)): position
  matters — inject near head or tail, never mid-context.
- **Context rot** ([Chroma, 2025](https://www.trychroma.com/research/context-rot)):
  every token has marginal attention cost well below the window limit — injection
  budgets must be small and high-signal. Injection is not the opposite of trimming;
  it is the same scarcity problem ("smallest set of high-signal tokens" —
  [Anthropic's context-engineering post](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)).

The 2026 frontier is **learned injection policies**: contextual bandits deciding
inject/abstain (MemCon [2607.13591](https://arxiv.org/abs/2607.13591); the
abstention-aware coding-agent bandit [2604.27283](https://arxiv.org/abs/2604.27283),
which asymmetrically penalizes *false-positive* injection), RL-trained memory
managers (Memory-R1 [2508.19828](https://arxiv.org/abs/2508.19828), Mem-α
[2509.25911](https://arxiv.org/abs/2509.25911)), and sidecar injector agents —
Meta's "Remember When It Matters" ([2607.08716](https://arxiv.org/abs/2607.08716))
is architecturally *identical* to a proxy: a separate process watches the trajectory
and per-step decides inject-or-stay-silent into an unmodified agent's context.

**Nobody has published a learned injection policy over conversation graph structure.**
The GNN curator extended with an "inject" head appears to be open territory.

## 2. The gap nobody occupies (ecosystem survey)

Where existing systems sit:

- **SDK/service above the harness** (app must cooperate): mem0 (~62k★, extract→
  ADD/UPDATE/DELETE/NOOP reconciliation), Zep/Graphiti (~29k★, bi-temporal KG, edge
  *invalidation* not deletion, **no LLM at read time** — precompiled per-session
  context block), cognee, Memobase (bounded fixed-schema profile), LangMem
  (hot-path tools vs background "subconscious" formation).
- **Harness plugin / prompt convention**: CLAUDE.md, Cline Memory Bank, Cursor rules
  (glob-conditional attachment) + Memories (sidecar observer model), claude-mem
  (hooks → typed observations → SessionStart re-inject), the entire
  handoff-document ecosystem (Continuous-Claude, cc-sessions, Amp's
  extraction-over-compaction).
- **API/model layer** (Anthropic's own): memory tool `memory_20250818`
  (model-initiated, *client-executed* file CRUD), context editing
  (`clear_tool_uses` — with `clear_at_least` as shipped "is this edit worth the
  cache bust" logic), server-side compaction beta.
- **Wire/proxy layer** — nearly empty: Headroom (lossy compression + CCR: originals
  stored locally, a `headroom_retrieve` tool injected into the request and satisfied
  *by the proxy*), supermemory's hosted Infinite Chat gateway (the only shipping
  retrieval-injection proxy — cloud-side, OpenAI-shaped, not cache-aware),
  OpenRouter middle-out (destructive), LiteLLM pre-call hooks (mechanism, no policy).
- **The one proven injection artifact in coding agents**: aider's repo map
  ([docs](https://aider.chat/docs/repomap.html)) — tree-sitter tags → reference
  graph → personalized PageRank biased by chat files/identifiers → binary-search
  to a token budget → re-sent every request. Deterministic, budgeted, client-side.
  Parsec already owns the ingredients (mapgen/codescout, tree-sitter).

**The unoccupied intersection, precisely:** local + deterministic + prompt-cache-aware
+ learned-policy + measured injection on the Anthropic `/v1/messages` wire. Every
existing injector rewrites prefix bytes each turn (cache-hostile), uses cosine
top-k or hand rules (unlearned), and reports no counterfactual value measurement.
Each of those is a parsec non-negotiable already built for trimming.

## 3. Wire mechanics — what the Anthropic API permits (verified 2026-07-31)

Docs: [prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching) ·
[context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing) ·
[compaction](https://platform.claude.com/docs/en/build-with-claude/compaction) ·
[memory tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool).

Cache facts that gate placement: exact-byte prefix match; ≤4 `cache_control`
breakpoints; 20-block lookback window per breakpoint; tiered invalidation —
`tools[]` change kills everything, `system` change kills system+messages, message
bytes kill messages-forward. Minimum cacheable prefix is model-dependent
(512 tokens on Opus 5/Fable-class).

### Injection placement verdicts

| Placement | Verdict |
|---|---|
| Append after last cached breakpoint (tail of `messages`) | **Safe** — the only unconditionally safe live position |
| `role:"system"` message appended in `messages[]` (mid-conversation system messages) | **Safe** on Opus 5 / Opus 4.8 / Fable / Mythos; 400 elsewhere → need user-turn fallback. Operator-authority, non-spoofable — best channel where available |
| `<system-reminder>`-style text block in a user turn at the tail | **Safe iff frozen once the conversation grows past it** — freezing discipline applies to injections exactly as to cuts |
| Top-level `system` mutation | **Poison** every time injected bytes change |
| `tools[]` mutation mid-conversation | **Poison** (full invalidation) — except `defer_loading` + `tool_addition` beta (`mid-conversation-tool-changes-2026-07-01`, Opus 5+); a one-time tools change at conversation start is a bounded cost |
| Anything mid-prefix that updates (e.g. a "memory block" at messages[0] that evolves) | **Poison on every update** |

### The harness-echo problem (the load-bearing constraint)

Claude Code resends the full transcript every turn and never sees proxy edits.

- **Request-side injection**: harness-blind. Turn N+1's body will *not* contain
  turn N's injection — the proxy must re-derive and re-splice it byte-identically
  every turn. This is exactly the freeze problem again: an injection, once emitted
  at a position, is frozen; served bytes stay a pure function of
  (prefix, checkpoint, config). Parsec's frozen-prefix machinery is the missing
  piece no gateway has — injections are just another deterministic rewrite in the
  fold map. Two coherent regimes:
  1. **Frozen append-only injections** — pinned into the prefix; supersede by
     appending, never editing (cf. Graphiti's edge invalidation).
  2. **Ephemeral tail injection** — re-rendered fresh at the current tail each
     turn, never frozen; always cache-safe but re-billed as uncached input every
     turn, and earlier model turns were conditioned on content that later vanishes.
  The governor-directive precedent in `docs/freeze-design.md` already chose regime
  2 ("directives must ride the moving tail") for non-recomputable content.
- **Response-side injection**: echoed by the harness forever — transcript/UI
  pollution, fabricated `tool_use` breaks the harness contract and 400s the next
  request without a matching `tool_result`. Only viable for content you *want*
  echoed. Avoid for invisible memory.
- Structural validity when splicing: every `tool_use` id needs a matching
  `tool_result` next turn; referenced tools must exist in `tools[]`; thinking
  blocks are signature-checked — pass through untouched; `role:"system"` messages
  have position constraints (must follow a user turn, text-only).
- **Auto-compact is a discontinuity event**: post-compact the incoming prefix is
  all-new bytes; detectable on the wire as "prefix extends no known frozen prefix";
  any injection state keyed to the old prefix must be refrozen. `PreCompact` hooks
  are the client-side signal — and the moment to inject a handoff payload.

### One unexploited wire trick

Anthropic's memory tool is **client-executed with no input schema**: a proxy can
inject `{"type":"memory_20250818","name":"memory"}` into `tools[]` and satisfy the
resulting `tool_use` turns *itself* against its own local store — model-directed
persistent memory, zero harness changes, on any harness. (Costs: one-time tools
cache bust at conversation start; the API auto-injects a memory protocol into the
system prompt; the tool round-trips are response-side content the harness will
echo, so they must be sentinel-tagged and idempotently recognized. Headroom's CCR
proves the proxy-satisfied-tool pattern works in production.)

## 4. Measurement honesty under injection

`count_tokens(original) − billed(actual)` can go negative with injection. The
Zep-vs-Mem0 benchmark dispute
([Zep's critique](https://blog.getzep.com/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/))
is the cautionary tale: memory systems scored without a **full-context baseline**
overstate value (Mem0's own numbers: full-context ~73% vs their best ~68% on LoCoMo).

- **Cost axis** (ledger): split `net_tokens` into `trim_savings` (≥0) and
  `injection_cost` (≤0, exact — the proxy owns the injected bytes;
  `count_tokens` on the delta gives an honest number). Never net them into one
  "savings" figure.
- **Value axis** (eval): task-success lift vs. no-injection baseline AND vs.
  full-context baseline, cc-bench style — same suites, proxy-with-injection vs
  proxy-passthrough, report resolve-rate/tokens/latency. Benchmarks:
  LongMemEval ([2410.10813](https://arxiv.org/abs/2410.10813)) is the credible
  memory benchmark (LoCoMo is known-flawed); SWE-EVO
  ([2512.18470](https://arxiv.org/abs/2512.18470)) targets exactly the
  cross-session-carryover setting where injection should pay.
- The counterfactual-utility framing ("Decision-Aware Memory Cards",
  [2606.08151](https://arxiv.org/pdf/2606.08151)) extends parsec's
  measurement ethos to quality: score injections by causal effect on outcomes.

## 5. The approaches, ordered by distance from what exists

### A. Recall of what you cut (close the trimming loop) — nearest
Parsec's elisions already print `[... ~N tokens · re-read f:Llo-hi omitted ...]` —
a retrieval *pointer* with no retrieval *path* (the model must re-run tools to get
content back, which is exactly the no-reread waste we police). Add the path:
- **Model-initiated**: inject one `parsec_recall` tool (or the memory tool);
  proxy satisfies calls from the FoldMap's dropped-chunk registry (Headroom CCR
  pattern). The model pulls back only what it decides it needs — the
  abstention-friendly default.
- **Proxy-initiated**: when scoring turn N's chunks, the brain also scores dropped
  chunks for *resurrection* against the current tail; winners ride the moving tail.
  This is the insist-valve generalized, and the first natural GNN "inject head."
Safest correctness profile of all options: injected content originated in this
very transcript — no staleness, no cross-session provenance questions.

### B. Repo-knowledge injection (aider-proven) — assets exist
Deterministic, budgeted, relevance-ranked repo map spliced at the tail; recomputed
per turn from (repo state, files-in-conversation) — mapgen/codescout already build
maps; tree-sitter is already a dependency. Cursor's glob-conditional rules are the
cheap trigger model: inject artifact X only when the conversation touches matching
files (evaluable from the request body alone, deterministic). Cache note: a
per-turn-changing map must ride the ephemeral tail, or be quantized/refrozen at
coarse boundaries to earn prefix stability.

### C. Cross-session memory (the write path) — the real "management" prize
All formation **off the serving path** (LangMem's "subconscious" split; Letta's
sleep-time compute [2504.13171](https://arxiv.org/abs/2504.13171) — the proxy has
idle time and full transcript visibility, and can run consolidation through the
user's own credentials, keeping data-plane-local intact). Store shapes worth
stealing: claude-mem's **typed few-token observations** (bugfix/discovery/decision
— cheap to inject, natural chunks for the GNN to score), ACE's **playbook with
incremental delta updates** ([2510.04618](https://arxiv.org/abs/2510.04618) —
avoids monolithic-rewrite "context collapse"), PROJECTMEM's **local-first
event-sourced log** ([2606.12329](https://arxiv.org/pdf/2606.12329) — memory as a
derived artifact, rebuild-able, which preserves determinism: memory version pins
into the config hash). Injection point: session start (frozen once) + optional
tail refreshes. Memobase's bounded fixed-schema profile is the cache-friendliest
payload shape.

### D. Compaction insurance — high user pain, cheap
Auto-compact degradation is the loudest community pain (the whole handoff-tool
ecosystem exists for it). The proxy sees the discontinuity on the wire; a ledger
maintained per-conversation (option C's store) injected immediately post-compact
recovers what the summary lost. Also: treat Anthropic's server-side compaction
beta blocks as opaque and position-stable if/when the harness adopts them — never
trim across a compaction boundary.

### E. Learned injection policy (the moat extension) — last, deliberately
Extend the curator: candidates (dropped chunks, observations, map entries) are
featurized like live chunks; the brain returns inject scores + a budget τ; the
proxy applies them deterministically. Literature guidance: outcome-supervised
reward (Memory-R1), budget-constrained sequential framing (ContextBudget
[2604.01664](https://arxiv.org/abs/2604.01664)), and **abstention-first asymmetry**
— penalize false-positive injection over missed injection
([2604.27283](https://arxiv.org/abs/2604.27283)). Sequencing logic: options A–D
run on heuristics first and *generate the labeled traces* (was the injected chunk
used? did the task succeed?) that make the learned head trainable — same flywheel
as keep/cut.

Explicitly infeasible at the proxy (for the record): gist/soft-token compression,
KV-cache cartridges ([2506.06266](https://arxiv.org/abs/2506.06266)), Self-RAG
reflection tokens — all need weight or KV access the API doesn't expose. Hard
text-level ops are the proxy's complete vocabulary.

## 6. Hard constraints any design must satisfy

1. **Injected bytes live strictly after the last cached breakpoint, or are frozen
   forever** — exact-byte prefix caching; the ≥10:1 invariant survives nothing else.
2. **Never mutate top-level `system` or `tools[]` mid-conversation** (tier-1/2
   invalidation); sanctioned channels: `role:"system"` tail messages (with 400
   fallback), one-time tool injection at conversation start, `defer_loading` beta.
3. **Injection is a pure function of (prefix, memory-store version, checkpoint,
   config)** — retrieval results checkpoint like GNN scores; the memory store
   version pins into the config hash, else replicas diverge.
4. **Harness-echo idempotency** — request-side injections re-spliced
   byte-identically every turn; anything response-side sentinel-tagged and
   recognized/stripped/refrozen on echo.
5. **API structural validity** — tool_use/tool_result pairing, tools present in
   `tools[]`, thinking blocks untouched, system-message position rules.
6. **≤4 breakpoints, 20-block lookback** — heavy injection turns can silently push
   the previous cache entry out of the lookback window.
7. **Fail open = "no *new* injections", not "strip injections"** — frozen
   injections are prefix bytes; stripping them on failure busts the cache.
   Fail-open events counted, as ever.
8. **Measurement: cost and value are separate axes** — injection_cost in the
   ledger, injection value only from eval lift vs no-injection AND full-context
   baselines. Never a modeled number.

## 7. Rust-embeddable retrieval infra (when C needs an index)

- **tantivy** (~15.6k★) — embedded BM25, incremental segment commits, no server.
- **LanceDB** — embedded, file-based vector store; the vector complement.
- **probe** (~675★, Rust) — index-free tree-sitter block extraction at ripgrep
  speed; sidesteps staleness entirely; its session-dedup idea = "don't re-inject
  what's already in the prefix."
- **cocoindex** (~11k★, Rust core) — incremental dataflow indexing, tree-sitter
  syntax-boundary chunking, only reprocesses changed data.
- **code-splitter** (~60★) — minimal tree-sitter chunker crate, directly usable.
- **BloopAI/bloop** (archived, Rust) — tantivy + tree-sitter code search; mineable
  reference for exactly this stack.

## 8. Papers shortlist (the ten worth actually reading)

1. [2507.13334](https://arxiv.org/abs/2507.13334) — Context Engineering survey (the taxonomy)
2. [2512.13564](https://arxiv.org/abs/2512.13564) — Memory in the Age of AI Agents survey (the design-space map)
3. [2607.08716](https://arxiv.org/abs/2607.08716) — Remember When It Matters (sidecar injector ≅ proxy)
4. [2607.13591](https://arxiv.org/abs/2607.13591) — MemCon (bandit inject/abstain controller, tiny, online)
5. [2604.27283](https://arxiv.org/abs/2604.27283) — abstention-aware retrieval bandit for coding agents (reward asymmetry + 16-feature schema)
6. [2510.04618](https://arxiv.org/abs/2510.04618) — ACE (playbook with delta updates; brevity-bias/context-collapse failure modes)
7. [2401.18059](https://arxiv.org/abs/2401.18059) — RAPTOR (summary tree = compress with recall path at every granularity)
8. [2512.22087](https://arxiv.org/abs/2512.22087) — Context-as-a-Tool for SWE agents (three-tier workspace; milestone-triggered compression)
9. [2504.13171](https://arxiv.org/abs/2504.13171) — Sleep-time compute (offline consolidation economics)
10. [2508.19828](https://arxiv.org/abs/2508.19828) — Memory-R1 (outcome-supervised memory ops with 152 training pairs)

Full agent reports (30+ papers, ~40 repos, all wire-mechanics citations) are
preserved in the session transcript; this doc is the distillation.
