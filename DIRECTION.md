# Learner — Product Direction

**Status:** direction doc, 2026-07-08. This is the anchor for the `learner` monorepo — the
productization of the Dasein A3S stack (`adaptive-context-clean`). Everything here was decided
against three reference architectures studied side by side: our own wire proxy, pxpipe
(transparent local proxy, image re-encoding), and Woz (Claude Code plugin, tool replacement).

---

## 1. The product in one paragraph

A **Claude Code plugin** is the product identity and distribution vehicle. The free tier works
with zero infrastructure (exploration maps, no-reread hook, savings dashboard). The paid tier
flips on a **local proxy** that compresses the agent's context per turn using the **learned
curator (GNN)** — the proxy runs on the user's machine, model traffic rides the user's own
Anthropic credentials (subscription or API key), and the proxy calls our **hosted brain API**
for keep/cut scores. Training stays server-side, fed by **opt-in, featurized trace telemetry**.
Weights never leave our cloud; model traffic never leaves the user's machine.

## 2. The one rule that shapes everything

**Data plane local, control plane ours.**

- **Data plane** (model requests → Anthropic): always leaves from the user's machine, with the
  user's credentials. We never middleman model traffic in the default product. This is what
  makes subscription ($200 Max plan) users servable at all (their OAuth token is only legitimate
  as their own local Claude Code traffic — the pxpipe posture), and keeps token cost off our
  books entirely.
- **Control plane** (scoring, auth, savings ledger, rule updates, telemetry): always our backend.
  The GNN weights live only here. The local proxy sends chunk **text + structural features** and
  receives scores; it applies the keep/cut decision itself.

⚠️ **Revised 2026-07-20** (see `docs/server-side-embedding.md`). This rule previously read
"the proxy sends vectors, never raw text," enforced by a contract in which text was
structurally unrepresentable. Embedding now runs server-side in the brain, so chunk text does
cross the wire. What survives — and is still absolute — is the **data-plane** half: model
requests always leave from the user's machine with the user's own credentials, and
subscription OAuth tokens are never routed through our cloud.

The hosted BYOK gateway is not a different product — it is the same proxy **deployed in our
cloud** for teams who explicitly choose it (API keys only, never subscription tokens; zero
local install, centralized reporting). One codebase, two topologies.

## 3. The tiers

| Tier | What runs | Who pays for tokens | Revenue |
|---|---|---|---|
| **Free (plugin only)** | codescout exploration maps as a `dasein:explore` agent, no-reread PreToolUse hook, savings status line, `/dasein-savings` | user (their plan/key) | none — distribution + flywheel |
| **Pro (plugin + local proxy)** | full curator via hosted scoring API, tool-schema prune, governor, rule gate | user (their plan/key) | seat subscription for the brain API |
| **Team (hosted BYOK gateway)** | same engine deployed in our cloud; customer API keys pass through | user (their key) | seat / metered on measured savings |
| **Enterprise (self-host)** | licensed checkpoints + containerized stack + **on-prem fine-tuning** on their traces from our base checkpoint | them | license |

Notes:
- The free tier competes directly with Woz (our cc-bench map arm beat their arm). It is a real
  product, not a demo — Woz proved this wedge works as a business.
- On subscriptions there is no per-token bill, so the pitch is "−X% usage against your 5-hour /
  weekly rate limits" — more Claude Code out of the same $200.
- Never route subscription OAuth tokens through our cloud. Local rewriting of a user's own
  traffic is fine; a hosted relay carrying subscription credentials is ToS exposure. Cloud =
  BYOK API keys only.

## 4. Architecture

```
┌─ user's machine ──────────────────────────────────────────────┐
│  Claude Code ── plugin (agents, hooks, status line, settings) │
│      │  ANTHROPIC_BASE_URL (paid tier)                        │
│      ▼                                                        │
│  local proxy                                                  │
│    • chunking + structural featurization (no embedder)        │
│    • applies keep/cut, tool-prune, cache-safe splicing        │
│    • deterministic quantized freezing (no session-state trap) │
│    • count_tokens counterfactual measurement                  │
│      │ text+features           │ curated request,             │
│      ▼                         ▼ user's own auth headers      │
│  [brain API] ◄─────────   api.anthropic.com                   │
└───────│───────────────────────────────────────────────────────┘
        ▼  our cloud (control plane)
   brain API (in-process GPU embedder + GNN inference)
   platform API (auth/accounts · billing · savings ledger ·
                 telemetry intake: quarantine → validate → corpus)
   trainer (server-side; produces versioned checkpoint bundles)
```

Key architecture decisions (each learned from a reference system):

1. **Plugin as the integration surface** (from Woz). `/plugin install` beats "change your base
   URL"; agents/hooks/MCP give us map delivery, no-reread, and stop-governor seams natively.
   Map delivery moves from the proxy's force-spawn trick to a sanctioned `dasein:explore`
   agent backed by codescout running locally — works on arbitrary repos, no wire surgery.
2. **Cache stability by determinism, not state** (from pxpipe). Replace the in-process
   `served_folds` session dict with quantized, append-only freezing where served bytes are a
   pure function of (conversation prefix, checkpoint, config). Recomputable ⇒ restart-safe ⇒
   the same engine runs as a laptop process or a replicated cloud service without sticky
   routing.
3. **Honest measurement as a product surface** (from pxpipe). Per-request free `count_tokens`
   probe on the original body vs actually-billed usage — same event row, no counterfactual
   model. This replaces the miscalibrated `BASELINE_STEP_INFLATION` savings estimate and is
   simultaneously the billing basis (Team), the upsell proof (Free→Pro), and the trust
   artifact (Enterprise). Also adopt: `cachePrefixSha8` bust attribution, static-tag churn
   canary.
4. **The learned curator is the moat and stays server-side.** Everything else (tool-prune,
   no-reread, cache machinery, maps) is replicable — pxpipe and Woz each rebuilt a slice of
   it. The checkpoint + the outcome-labeled trace corpus are not. Open-source the
   deterministic shell; sell the brain.

## 5. Local inference — what runs on the user's machine

- **No local embedder** (revised 2026-07-20; see `docs/server-side-embedding.md`). Embedding
  runs in the brain, in-process on a GPU. The client ships no ONNX model, no `ort`, and no
  1.3GB download. This deletes the retrain that used to be the Pro long pole — the existing
  checkpoints are already matched to dasein-embed `bge-large-en-v1.5` vectors, which is exactly
  what we now serve ourselves.
- **Chunking + structural featurization stay local.** The freezer must chunk to know what to
  drop and render, so this is not optional. The proxy sends chunk text plus the struct rows and
  readout it computes locally.
- **Scoring**: remote (brain API) by default. Latency budget: decisions are per-chunk-at-birth
  and frozen, so one round-trip per turn, cacheable.
- **Decisions are applied locally.** The brain returns scores and a τ; the proxy applies the
  coverage budget and the cut. The brain never learns what was dropped.
- **No local training.** Local *adaptation* instead: self-calibrating per-config τ (streaming
  score-quantile sketch per harness/model config — designed in the handoff docs, unbuilt) and
  per-repo localization indexes. Cheap, deterministic, can't corrupt the model. Rationale
  against per-user local training: too few traces per user (overfit/drift), the cross-trace
  neighbor signal needs a corpus, embedder coupling, and no eval gate on uncontrolled machines.
- **Enterprise exception**: on-prem fine-tuning from our base checkpoint is a licensed,
  containerized pipeline with the eval gate built in (collect → label → fine-tune → validate
  against held-out slice → promote only on pass). Orgs have the trace volume and the GPUs;
  "learns your codebase, never sends us a byte" closes the self-host tier.

## 6. Server-side training + the opt-in mechanism

Training stays central (trainer + corpus + eval gates), fed by opt-in telemetry from the plugin.

**Data tiers — granular, default off:**

- **Tier 0 — off.** Fully functional product. Consent by degradation is not consent.
- **Tier 1 — metrics** (the default ask): tokens saved, cut %, cache read/write ratio,
  outcome signal, harness version, latency. No content, no paths, no prompts.
- **Tier 2 — featurized traces** (the training tier): chunk **embeddings** + trace graph
  structure + tool names + token counts + mechanically-computed `is_needed` labels + outcome.
  No source code, no prompts. The GNN consumes embeddings + features, so this is sufficient to
  train on. Disclose honestly: vectors are a *mitigation, not anonymity* (embedding inversion
  exists). Note this describes what the **training corpus** persists — since 2026-07-20 the
  serving path does carry chunk text to the brain (§2), so this is no longer a claim about the
  system as a whole.
- **Tier 3 — full traces**: design partners only, contract-governed.

**Consent UX:**
- Ask **after the first savings report**, not at install — "help the model that just saved you
  $3.20 get better" converts; an install-time wall doesn't.
- `/dasein-share --preview` dumps the exact featurized trace that would be uploaded, locally,
  human-readable. Show the bytes.
- Persistent status-line indicator while sharing is on; `/dasein-share off` stops instantly;
  `/dasein-share purge` files deletion.
- A tier or schema change **re-prompts** — consent never silently expands.

**Pipeline:**
1. Local spool (`~/.dasein/spool/`), delayed upload — inspectable/deletable before shipping.
2. Local scrubbing **before** featurization: secret scanning (keys, `.env` shapes,
   high-entropy strings → mask or drop the chunk), repo-relative paths hashed with a local salt.
3. Provenance tagging (contributor ID + trace ID) so purge is honorable: deleted from corpus
   and excluded from all **future** training runs (true erasure from a trained checkpoint =
   retrain; say so plainly).
4. Server-side quarantine → validation (contract version, secret re-scan, dedup, outlier
   screen) → corpus promotion. The temporal-firewall / eval-exclusion discipline extends to
   community data — the trainer refuses to run without the exclusion list.
5. Versioned contribution contract (`tcv2-community`), evolving only with re-consent.

**Org policy + incentives:**
- Workspace admin sets the telemetry ceiling; users can't exceed it. Org accounts default hard
  off.
- Pay for the data: contributor discount / quota credit ("data dividend"). Publish the
  aggregate loop ("community corpus improved cov@70 by X") so contributors see the effect.

## 7. Monorepo layout (target)

```
learner/
  DIRECTION.md            ← this doc
  packages/
    plugin/               Claude Code plugin: agents (dasein:explore), hooks (no-reread,
                          stop-governor seam, telemetry), skills (/dasein-savings, /dasein-share,
                          login/settings), status line. Markdown + JSON + committed per-platform
                          `dasein` binaries (see §7b) — no TypeScript.
    proxy/                the local data-plane proxy (also deployable as the Team cloud
                          gateway): Anthropic-wire /v1/messages, cache-safe splicing with
                          deterministic quantized freezing, tool-prune, governor, passthrough
                          auth, count_tokens measurement. Rust (`dasein proxy`). Ported from
                          adaptive-context-clean/service (absorbing the gateway's upstream
                          bridge as provider adapters: anthropic-passthrough | vertex | byok).
    engine/               the client-side deterministic core the proxy uses: chunking,
                          freezing, feature extraction. Rust crate, no torch, no embedder.
                          Ported from adaptive_context/ minus the GNN.
    brain/                the hosted scoring API: GNN inference (torch), checkpoint-bundle
                          loading (ckpt + matched dials versioned as one immutable artifact —
                          a mismatch is a load-time error, never silent), τ calibration
                          endpoints. Private.
    trainer/              server-side training + eval gates + checkpoint promotion; the
                          enterprise on-prem fine-tune pipeline is this, containerized.
                          Consumes scripts/trace_graph + trace_train lineage. Private.
    platform/             accounts, billing, savings-ledger API, telemetry intake — the
                          control plane minus scoring. Thin FastAPI glue over managed
                          services (see §7c). Mints/validates the brain-API credential
                          the proxy calls with. Private; SaaS-only (never part of the
                          enterprise self-host bundle — brain/trainer are).
    contracts/            shared schemas: tcv2-community trace contract, telemetry events,
                          savings-ledger rows, brain API. Single source of truth across
                          packages and languages.
    mapgen/               codescout + map rendering (the Explore-contract map). Product
                          feature, not bench tooling — used by plugin free tier.
    bench/                cc-bench harness, grader, arms. Imports proxy as a black-box client;
                          never imported by anything above.
  docs/
```

Dependency direction (enforced): `bench → proxy → engine`; `plugin → proxy (manages)`;
`brain/trainer/platform` share `contracts` with everything but import nothing client-side.
`brain` stays pure inference: it answers "is this key entitled?" via platform (shared table
or signed entitlement token — decide at Pro build time) and does nothing else non-scoring. `scripts/`
training modules stop being a runtime dependency of serving (today `curator.py` imports
`scripts.trace_graph` on the live path — that inversion dies in the port).

**Licensing split:** `plugin`, `proxy`, `engine`, `mapgen`, `contracts` → open source (MIT or
similar). `brain`, `trainer`, `platform`, checkpoints, corpus → closed. The OSS shell is replicable anyway
(pxpipe/Woz proved it); the moat is the trained model + data flywheel, and the OSS tier is
what feeds the flywheel.

## 7b. Stack decision (2026-07-09, verified against the Claude Code plugin docs)

**Client side: Rust monoculture. Server side: Python. No TypeScript anywhere.**

Verified facts that drove it (code.claude.com/docs — plugins-reference, hooks, mcp,
plugin-marketplaces, setup, statusline):
- **Node is NOT guaranteed** on machines running Claude Code — the recommended native-binary
  install does not invoke Node. A TS plugin is a runtime bet; a compiled binary is not.
- Every plugin surface accepts **arbitrary executables**: MCP `command` = any binary (stdio);
  hooks have an **exec form** (direct spawn, no shell — avoids the Windows Git-Bash/PowerShell
  split); statusline = any program reading JSON on stdin. Agents/skills are pure markdown.
- Distribution is git-clone → local cache copy, **no build/postinstall step** ⇒ binaries must
  be committed to the plugin repo. No documented size limits. `version` in plugin.json is the
  update channel.

The shape:
- **One `dasein` binary, subcommands for every role**: `dasein mcp` (stdio MCP server —
  codescout/Search), `dasein hook <event>` (no-reread gate, session, telemetry), `dasein proxy`
  (Pro data plane; same binary deploys as the Team gateway in a scratch container),
  `dasein statusline`. Crates: axum/hyper + tokio (proxy, SSE), rmcp (MCP), tree-sitter,
  serde_json with `preserve_order`.
- **Plugin repo = markdown + JSON + committed per-platform binaries**
  (`bin/{darwin-arm64,darwin-x64,linux-x64,win-x64}/`) plus a two-line platform shim
  (`sh` stub + `.cmd`) — the only non-Rust client code.
- **Why Rust over TS** (beyond the runtime guarantee): ~10MB idle daemon vs ~100MB Node for an
  always-on background process; single static binary for non-Claude-Code harnesses later; a
  credential-handling proxy audits far better as one signed binary with a dozen crates than as
  a node_modules tree.
- **Python stays** for `brain`, `trainer`, `bench` (torch_geometric is non-negotiable;
  server-side, we control the environment).
- **The port's definition of done = the two cross-language parity suites** (see §8): Rust must
  reproduce the Python reference byte-for-byte on cache freezing (JSON key order, escaping,
  float formatting) and vector-for-vector on featurization. Divergence = the "silently wrong
  scores" failure mode.
- **Sequencing guard**: the Rust port is the Pro-tier deliverable. (It used to be gated on the
  embedder retrain running in parallel; server-side embedding removed that dependency.) If the
  free plugin must ship before the binary is ready, its v0 (maps + savings line) can ship with
  the `mcp`+`hook` subcommands only — the proxy subcommand lands with Pro.

## 7c. Platform layer decision (2026-07-09)

The §4 control plane names auth/accounts, billing, and the savings ledger, but §7 originally
gave them no package. They live in `packages/platform`: **Python + FastAPI, buy-don't-build,
keep it thin.** Not Rust — the §7b Rust rationale (no runtime guarantee on user machines,
binary auditability) is client-only; auth/billing is webhook-and-CRUD territory where managed
ecosystems win and iteration speed matters more than anything Rust buys.

- **Auth + database: Supabase.** Supabase Auth for signup/login/orgs (services verify its
  JWTs — ~20 lines), Supabase Postgres for accounts, entitlements, savings-ledger rows, and
  the telemetry consent registry. One vendor for v1; revisit WorkOS when Team-tier SSO/SCIM
  asks arrive.
- **Billing: Stripe Checkout + Customer Portal + webhooks.** No custom pricing pages, card
  forms, or subscription-management UI. Pro seats and Team metered-on-measured-savings are
  both native Stripe primitives; the "data dividend" is a Stripe coupon/credit.
- **What we actually write** (a few hundred lines): signup webhook → account row; Stripe
  webhook → entitlement flag; mint/validate the brain-API key the local proxy uses;
  savings-ledger ingest + per-account reporting; telemetry intake front door
  (quarantine → validate → corpus promotion feeding `trainer`).
- **Sequencing:** none of this blocks §9 steps 1–3. Platform lands with step 4 (Ship Pro) —
  the free tier needs no account, and the brain API needs auth only once it has paying
  callers.

## 8. Invariants to protect with CI from day one

1. **Cache-stability golden test**: replay a recorded multi-turn conversation; every
   previously-served turn byte-identical; per-task `cache_read/cache_write ≥ ~10:1`.
2. **Curator parity test**: brain-API score == trainer-forward score at the checkpoint's
   `calib_tau` on identical chunks. (The checkpoint/env "matched pair" trap — silent wrong
   scores — dies by making bundles self-validating.)
3. **Fail-open floor**: every layer degrades to passthrough on error — but fail-open rate is
   a first-class metric with an alert. A proxy silently serving 0% cut is indistinguishable
   from a working one without it.
4. **Measurement honesty**: savings numbers only from the per-request `count_tokens`
   counterfactual; never from a modeled baseline.

## 9. Sequencing

1. **Core first**: engine + proxy port with deterministic freezing, the two invariant tests,
   `count_tokens` measurement. Everything depends on this being solid.
2. **Ship the free plugin** (maps + no-reread hook + savings display): weeks of work, validates
   distribution, starts the flywheel before the brain API lands.
3. **Stand up the brain as a serving monolith** (revised 2026-07-20 — this step used to be
   "embedder retrain, the Pro long pole"; server-side embedding deleted the retrain). Host
   `bge-large-en-v1.5` in-process on a GPU, add the text-carrying contract, keep the
   checkpoint-bundle handshake and its 409 matched-pair guard. Single-threaded per process;
   scale by running N processes behind nginx (round-robin — determinism means no sticky
   routing). Add per-key throttling before it takes real traffic.
4. **Ship Pro** (plugin manages local proxy + brain API + the `platform` service — signup,
   entitlements, billing per §7c).
5. **Team gateway** when teams ask: same proxy deployed on GKE (reuse the serving-engine
   repo's CI/CD pattern — Artifact Registry, gated rollouts), BYOK keys only, zero-retention
   posture stated loudly; SOC2 on the roadmap before mid-market.
6. **Enterprise self-host + on-prem fine-tune** when enterprises ask.

## 10. Open questions

- ~~Local embedder choice and retrain timeline~~ / ~~low-memory machines can't load fp32
  bge-large~~ — **both closed 2026-07-20** by moving embedding server-side. The Windows
  bad-allocation failure (observed 2026-07-18) is the change's proximate cause; it cannot recur
  once the client ships no model.
- Deployment target for a GPU monolith: Cloud Run with L4 (keeps the current Cloud Run posture,
  no k8s) vs GKE (matches `docs/brain-serving-v0.md`, but no manifests exist in the repo yet).
- Per-key throttling design — quota, concurrency cap, 429 + `Retry-After`, and proxy-side
  backoff. None of this exists today, and the brain is currently open by default when
  `DASEIN_BRAIN_KEY` is unset.
- Brain API scoring latency budget per turn at p95, and offline/degraded mode (proxy falls
  back to deterministic-only when the brain is unreachable — fail-open, measured).
- Self-calibrating per-config τ design (handoff `CHAIN_HASHDUP_HANDOFF.md` direction) — needed
  before harness diversity, not for v1.
- Pricing specifics (seat vs metered vs data dividend interplay).
- What of the submit governor survives in plugin/hook form vs requiring the wire.
- Non-Anthropic providers: the cache-stability machinery is Anthropic-only today; per-provider
  cache models (OpenAI implicit prefix cache, Gemini cachedContent) are real engineering —
  explicitly **out of scope for v1**, revisit after Pro ships.

## 11. Source repos this supersedes / draws from

- `adaptive-context-clean` — the serving strip; source for proxy/engine/mapgen/bench ports.
  Stays as reference until the port completes.
- `adaptive-context` — research monorepo; trainer lineage + test suites worth porting.
- `dasein-compression-service` — older hosted-proxy repo; port its test suite and
  ARCHITECTURE.md thinking, then archive.
- `dasein-serving-engine` — not part of this product line, but its GKE CI/CD pattern is the
  deployment blueprint for `brain` and the Team gateway.
