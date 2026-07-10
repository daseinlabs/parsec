# Manual testing — real Claude Code session through the full stack

The e2e scripts prove the machinery; this is how to *feel* it: a real Claude
Code session, through the local proxy, scored by the brain in Docker, with
the savings inspectable live. Nothing here deploys anything — the brain runs
in a local container, the proxy is a host process, and your Anthropic
credentials only ever pass through the host proxy to api.anthropic.com.

```
Claude Code ──ANTHROPIC_BASE_URL──▶ dasein proxy (host :8082)
   + plugin (hooks/skills/mcp)          │ trims context, prunes tools,
                                        │ writes ~/.dasein/ledger.jsonl
                    scores (v1 vectors) │            │ your auth, curated body
                                        ▼            ▼
                          brain (docker :8090)   api.anthropic.com
```

## 0. One-time prereqs

```sh
# the checkpoint (36MB, not in the repo):
mkdir -p ~/.dasein/brain
gsutil cp gs://dasein-473321-ac-learning/rulehead/curator_v4_prod.pt ~/.dasein/brain/

cargo build --release --bin dasein     # the proxy/plugin binary
```

## 1. Bring up the backend

```sh
docker compose up -d brain && docker compose logs -f brain   # watch the bundle self-validate
curl -s localhost:8090/v1/bundle | python3 -m json.tool      # ckpt sha, tau_q, heads, flags
```

Two modes:

| | embeddings | tau | what you'll see |
|---|---|---|---|
| **demo** (default) | deterministic hash | forced 0.999 | aggressive, visible trimming — exercises every seam regardless of content |
| **real** | bge-large from the in-cluster embedder | calibrated 0.70-cov (0.3153) | the honest operating point — trims what the GNN actually scores stale |

```sh
# real mode (needs cluster access):
kubectl port-forward svc/dasein-embed 18080:80 &
EMBED=dasein DASEIN_SERVE_TAU= docker compose up -d brain
```

## 2. The local proxy — auto-started or manual

**Auto-start (the plugin manages it):** the plugin's SessionStart hook spawns
`dasein proxy` automatically whenever the session is routed through a local
port that isn't listening yet. The one thing a plugin *cannot* do is set
`ANTHROPIC_BASE_URL` itself — routing must exist when `claude` launches. Put
it (plus the proxy's config, which the spawned process inherits) in the env
block of `.claude/settings.json` in the repo you're testing from, or your
user `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8082",
    "DASEIN_BRAIN_URL": "http://127.0.0.1:8090",
    "DASEIN_BRAIN_CONTRACT": "v1",
    "DASEIN_EMBED_BACKEND": "hash"
  }
}
```

Then plain `claude --plugin-dir …/packages/plugin` shows
`⌁ dasein proxy auto-started on 127.0.0.1:8082 …` at session start.

The managed lifecycle is symmetric: **on** at session start (only when the
routed port isn't already listening — it never double-spawns or clobbers a
manually-started proxy), **off** by itself after 30 minutes without traffic
(in-flight SSE streams keep it alive; the next session start revives it).
Conversation memos are TTL/cap-bounded in between (1h/512, the reference
dials) — safe because they're pure caches: eviction or restart costs replay
round trips and one provider-cache re-seed, never bytes. Logs at
`~/.dasein/proxy.log` (`RUST_LOG` to tune). Dials:
`DASEIN_PROXY_AUTOSTART=0` (never spawn), `DASEIN_PROXY_IDLE_EXIT_S`
(override the 30 min; manual `dasein proxy` runs default to run-forever),
`DASEIN_SESSION_TTL_S` / `DASEIN_SESSION_MAX`.

**Manual (separate terminal, logs visible):**

```sh
scripts/proxy_dev.sh          # demo: v1 contract, hash client embeddings
scripts/proxy_dev.sh real     # v1 contract, client embeds via the port-forward
scripts/proxy_dev.sh dev-raw  # legacy raw-text contract (server-side embedding)
```

Note demo/real use the **v1 contract**: only vectors + features + opaque ids
reach the brain — you can verify no conversation text crosses by watching
`docker compose logs -f brain` while you chat. Optional: export
`DASEIN_RECORD_DIR=~/.dasein/recordings` first to capture the session's wire
bodies (fixture material for `scripts/record_to_fixture.py`).

## 3. Open a Claude Code session with the plugin

```sh
cd <any repo you want to work on>
ANTHROPIC_BASE_URL=http://127.0.0.1:8082 \
  claude --plugin-dir /Users/surya/code/daseinlabs/learner/packages/plugin
```

Subscription auth works — the proxy forwards your auth headers verbatim and
never stores them. Then generate some context traffic: ask it to read a few
large files, run builds, grep around — anything observation-heavy. Re-ask it
to read a file it already read (the plugin's no-reread hook should deny the
re-read; the insist valve serves narrow re-reads of trimmed ranges in full).

## 4. Inspect the savings

**In-session:** `/dasein-savings` (skill), and the status line if configured
(`packages/plugin/bin/README.md` has the settings.json line — plugins can't
set the status line themselves).

**From another terminal:**

```sh
tail -f ~/.dasein/proxy.log              # LIVE per-request line:
#   INFO … request served — ~245 input tok avoided conv=… model="…"
#          counterfactual_in=465 billed_in=163 cache_read=50 cache_write=7 …
./target/release/dasein savings          # aggregate roll-up, per-model breakdown
tail -f ~/.dasein/ledger.jsonl           # the raw §8.4 rows (now incl. model)
```

Each request writes one honest row: `counterfactual_input_tokens` (the free
count_tokens probe on the ORIGINAL body — null if the probe failed, never
estimated) vs `billed_*` (what actually got charged), plus the capture seams —
`freeze_cut_tokens` (what the freezer trimmed, diagnostic units),
`tools_total`/`tools_kept` (schema prune), `checkpoint_id`, `brain_ms`, and
`scorer_fail_opens`/`fail_open` (§8.3 — nonzero means the brain was
unreachable and the proxy served passthrough; savings pause, correctness
never does). Savings claims come ONLY from counterfactual − billed (§8.4).

**Cache health:** across turns, `billed_cache_read_tokens` should dwarf
`billed_cache_write_tokens` (the golden replay's floor is 10:1) and
`cachePrefixSha8` should stay stable within a conversation.

## 5. Poke the failure modes (optional but satisfying)

```sh
docker compose stop brain     # mid-session: requests keep working (passthrough),
                              # ledger rows show scorer_fail_opens; restart it and
                              # the skipped birth steps get re-decided next turn
DASEIN_FREEZE=off scripts/proxy_dev.sh   # escape hatch: proxy runs pure passthrough
```

## Teardown

```sh
docker compose down
# ledger/recordings persist in ~/.dasein — delete at will
```

## Troubleshooting

- **Brain container exits immediately** — the bundle self-validation failed;
  `docker compose logs brain` names the mismatch (usually the ckpt mount).
- **No trimming in real mode** — expected on short/fresh conversations: the
  calibrated tau keeps what the model scores useful; check
  `freeze_cut_tokens` after some genuinely stale observations accumulate, or
  fall back to demo mode to see the machinery.
- **`fail_open: true` on every row** — the proxy can't reach the brain
  (`curl localhost:8090/health`) or the v1 handshake 409s (checkpoint
  changed under a live conversation — restart the proxy).
- **Everything looks passthrough** — check the proxy terminal: without
  `DASEIN_BRAIN_URL` (or with `DASEIN_FREEZE=off`) it runs the v0
  passthrough curation by design.
