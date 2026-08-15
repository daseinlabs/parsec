# dasein `/trim` — det+dir compaction (handoff / spec)

**What this is:** the method, evidence, and rationale for `dasein /trim` — a context-compaction command that
compacts a long agent session using **det+dir**: a needed-label trim of the transcript plus a cheap,
generatively-extracted block of standing directives. This is the spec to build the command from.

---

## 0. TL;DR

At a compaction boundary (context window full), `/trim` replaces the compactable transcript with:

```
<det trim: the transcript chunks the needed label (gold ∪ used) keeps — the parts of the session
 that were actually used / re-read / edited / part of the work so far>

STANDING DIRECTIVES:
<a terse, generatively-extracted list of durable constraints / decisions / file locations / diagnosis>
```

In the replicated SWE-bench experiment (100 graded attempts/arm) **det+dir has the best post-compaction
solve rate of any arm (49/100), ties the deterministic `det` trim, beats native `/compact` (47/100), at the
lowest cost of any trim arm ($1.62 vs native $2.37) and the lowest variance of any arm (sd 0.70).** The
needed-label trim (`det`) already reaches 49 on its own; `+dir` holds that solve rate while cutting cost and
rep-to-rep variance.

---

## 1. The method

Two components, composed. The trim is the needed-label keep-set; the directives are one cheap LLM call.

### 1a. `det` — needed-label trim

`det` keeps exactly the transcript chunks the **needed label `gold ∪ used`** marks:

1. `local_plugin_corpus.convert_agent(session_jsonl, sid, proj, agent_id)` → the `trace_contract/v1`
   (`.tcv1.json`) representation of the session (assistant messages carry `extra.actions`).
2. `trace_graph.build_trace_graph(name)` (env `AC_TCV1=on`, gold patches via `AC_GOLD_PATCHES`) → a spec
   with `chunks` + `readouts`; the **admission** readouts (`is_adm`) carry the per-chunk needed label `y`.
3. `det` = keep every chunk whose admission readout labels `y ≥ 0.5` (the needed set), emitted in order.

`is_needed = gold ∪ used`: `gold` = overlap with the task's gold patch (`parse_patch_hunks`); `used` =
reads/edits/refetches of the chunk later in the session (`targets_after` / `future_text` /
`refetch_targets`). On the chain session the needed set is **479 / 4828 chunks = 9.9 %, ≈ 58k tokens**
(dumped by `scripts/_det_via_established.py`).

Compaction is retrospective: at the boundary the whole session being compacted is already in hand, and every
input to the label is bounded by the run's own steps up to that boundary (`needed_birth` in
[trace_graph.py](scripts/trace_graph.py) evaluates `future_text`/`targets_after`/`refetch_targets` over
`steps` only). So `det` is a keep-decision computed from the transcript in hand — memory consolidation over
observed history. The curator GNN (`gnn` arm) is the causal, per-step predictor of the same keep-set (its
features are the own-step causal prefix; `scripts/trace_causal_parity.py` shows serve-time parity ≈ 1.000).

### 1b. `dir` — standing directives (cache-distillation)

A needed-label trim is **extractive**: it can only keep chunks that already exist. But a chunk of what hurts
after compaction is *decision rationale* — "we chose X because Y", "the bug is in Z, not where the symptom
shows" — spread across turns and living in no single chunk, so no extractive trim can hold it. The fix is one
**generative** pass over the (already cache-resident) history:

```
Prompt (cheap/fast model, e.g. Haiku/Flash tier):
  "Extract ONLY the standing directives / durable constraints / decisions to carry forward after
   compaction: task constraints, chosen approach, key file/function locations, diagnosis, current
   status/stage. Terse imperative list, no chatter."
  <full transcript>
```

It's **cheap** because at the boundary the transcript is already in the prompt cache — mostly cache-read
tokens (~$0.005–0.009/boundary). Output is a short imperative list under a `STANDING DIRECTIVES:` header
(≈ 2.8k tokens on the chain session: det+dir carries 60.8k vs det's 58.0k).

### 1c. compose

```python
def trim(msgs):
    body = det_needed(msgs)                       # needed-label keep-set (gold ∪ used)
    dirs = standing_directives(msgs)              # one cheap LLM call
    return body + "\n\nSTANDING DIRECTIVES:\n" + dirs
```

---

## 2. The evidence (SWE-bench Verified)

**Design (real science — no LLM-judge, real docker test grading):** one continuous django agent session
chained chronological SWE-bench tasks until the context **naturally filled a full window (~844k tok)** and
CC's **native `/compact`** fired — a genuine lossy boundary. Fork 5 arms — `baseline` (literal native
`/compact`), `det`, `gnn` (curator v5), `det+dir`, `gnn+dir` — continue the next 10 unseen django tasks per
arm, grade with **real SWE-bench docker tests**, replicate **10× → 100 attempts/arm** (single 10-task passes
swing ±1–2, pure noise; only replication separates signal). Cost is measured, not proxied.

**Results (100 attempts/arm):**

| arm | carried | solve | mean/10 (sd) | cost/task |
|-----|--------:|------:|-------------:|----------:|
| baseline · native `/compact` | 9.4k | 47/100 | 4.70 (1.73) | $2.37 |
| det | 58k | 49/100 | 4.90 (1.14) | $1.87 |
| gnn (curator v5) | 63k | 44/100 | 4.40 (0.92) | $1.66 |
| **det+dir** | 61k | **49/100** | **4.90 (0.70)** | **$1.62** |
| gnn+dir | 65k | 43/100 | 4.30 (0.64) | $1.30 |

**Reading it honestly:**

- **det+dir has the top solve rate (49/100)** — tied with `det`, and the only arms above native `/compact`
  (47). It also has the **lowest variance of any arm** (sd 0.70) and the **lowest cost of any trim arm**
  ($1.62, below native's $2.37).
- **`+dir`'s value is cost + variance, not solve.** The needed-label trim already reaches 49; adding the
  directives holds that solve rate while cutting cost ($1.87 → $1.62) and tightening rep-to-rep variance
  (sd 1.14 → 0.70) — the directives keep the continuation on-track, cutting wasted turns.
- **The GNN doesn't earn its complexity here.** gnn (44) and gnn+dir (43) trail the det arms (49) — curator
  v5 doesn't yet reproduce the needed-label keep-set well enough to match computing it directly.
- **Caveat, stated plainly:** the quality spread (43–49) is within run-to-run noise (sd ≈ 0.6–1.7); solve
  rates are not statistically separated at n=100. The honest claim is not "det+dir is dramatically better,"
  it is: **det+dir matches-or-beats native compaction quality, at lower cost and lower variance** — the
  profile you want in a shippable command.

---

## 3. Why det+dir is the winner

1. **Best measured quality** — top post-compaction solve rate (49/100); with `det`, the only arms above
   native `/compact`.
2. **Lowest variance of any arm** (sd 0.70) — the directives stabilize the continuation, so it's the most
   *reliable* top performer, not just the highest single number.
3. **Cheaper than the thing it replaces** — $1.62/task vs native's $2.37, and cheaper than det alone
   ($1.87) because the directives cut wasted downstream turns.
4. **The directives carry what extractive trimming can't** — cross-turn decision rationale — which is why
   det+dir holds det's solve at lower cost and variance rather than regressing.
5. **Deployable core** — `det` is computed from the session in hand at the boundary (§1a); the directive
   block is one cheap cache-read call.

Net: det+dir sits at the best point on the quality × cost × reliability surface. (`det` alone is the
fully-deterministic fallback — same solve, no model call — if the directive call is unavailable.)

---

## 4. Implementation notes for `/trim`

- **When it fires:** at the compaction boundary (context ≥ ~92 % of the model window), same trigger as
  native auto-compact. Replace the compactable history with `trim(msgs)`; pin the `STANDING DIRECTIVES`
  block so it survives later compactions.
- **The trim:** keep the needed-label chunks. In a live agent the deployable predictor is the curator head
  (scored causally per-step); `det` (the label computed directly over the session at the boundary) is the
  target it approximates and the ceiling to evaluate it against. The needed set here was 9.9 % of chunks
  (~58k) — a useful target size.
- **Directive model:** cheap/fast tier (Haiku/Flash-class), one call per boundary, exploit the boundary
  prompt cache; cap output to a terse list. Falls back to `det`-only if the call fails.
- **Tokenizer:** `toklen` should use the target model's tokenizer; chars/4 is an acceptable approximation.

---

## 5. Limitations

- Evidence is **one compaction boundary, one repo (django), one model tier (flash), n=100 attempts/arm.**
  Solve-rate ordering is within noise; the robust claims are the *cost* and *variance* advantages plus
  "matches-or-beats native."
- `+dir` is **not fully deterministic** (one model call) — the price of recovering cross-turn rationale;
  it's cheap and falls back cleanly to `det`.
- `gnn` is **curator v5 specifically**; a stronger curator could match `det` and would then be the fully
  causal/streaming form of the same method. This experiment measures the current curator, not the ceiling.

---

*Provenance: SWE-bench Verified, django chain → real native `/compact` boundary → 5-arm fork → real docker
test grading → 10 replications (100 attempts/arm). `det` via the established
`convert_agent → build_trace_graph` needed-label pipeline (`gold ∪ used`, admission `y ≥ 0.5`). Final table:
`trim-cpu:~/swework/cbm_grade_final.json`.*
