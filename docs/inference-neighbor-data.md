# Neighbor data at inference — what crosses the wire, and the cost of sending more

**Status:** analysis note, 2026-07-28. Answers two questions: (1) does the client send
neighboring values (graph edges, neighbor embeddings, cross-trace blocks) to the brain at
inference, and (2) how big a lift would it be to start sending them. Compared against the
Python reference in `adaptive-context-clean`. No work items; this records the analysis so
we don't re-derive it.

---

## 1. Current behavior: per-node features + one edge relation

The v2 `POST /v1/score/trace` payload (`build_v2_trace_payload`,
`packages/proxy/src/featurize.rs:333`; pydantic model `ScoreTraceV2Request`,
`packages/brain/src/parsec_brain/app.py:183`) carries:

- **Per-node data only** in `nodes`: `text` / `cmd` / `head` (raw text on v2 — embedding
  moved server-side, see `docs/server-side-embedding.md`), the 21-col
  `node_struct_with_type` vector, `step`, `kind`, `tokens`, and the salted join keys
  `file_id`, `lo`, `hi`, `cmd_id`, `head_id`. The join keys are identity/span info, not
  neighbor lists.
- **One edge relation**: `edges_supersession` — the rel-4 near-duplicate pairs, computed
  client-side by `supersession_edges` in `packages/engine` (spliced in at
  `featurize.rs:329`). It rides the wire because it needs raw-text Jaccard over spans.
  The tools body (`featurize.rs:376`) ships no edges at all.

Everything else about the neighborhood is **rebuilt server-side** in `build_graph`
(`packages/brain/src/parsec_brain/v1graph.py:64`):

- **Rels 0/1/2** (same-file line-neighbor chain, temporal chain, kNN cosine) — recomputed
  by the vendored `edges()` over duck-typed `_V1Chunk` stand-ins carrying only the join
  keys. Adjacency falls out of id equality + integer ordering; rel-2 uses the server-side
  embeddings.
- **Rel-4** — the vendored emissions are discarded and replaced verbatim by the client's
  pairs, sorted lexicographically to preserve scatter order (`v1graph.py:97-104`).
- **Task node, step spine, file/command/head/system hubs** — attached server-side
  (`attach_task`, `attach_steps`, `attach_hetero`; tools/rules likewise).
- **Cross-trace "hoods" blocks** — `attach_blocks` (`v1graph.py:110-114`) splices neighbor
  embeddings + edge arrays queried from the precomputed `PARSEC_HOODS_PKL` artifact
  (`neighbors.py`, loaded at `bundle.py:234-248`). The client sends nothing about them.

Two unrelated uses of the word "neighbors" to keep straight:

1. The graph neighborhood above.
2. `POST /v1/neighbors` — the governor's cost-baseline call, once per conversation.
   Request is `{contract, conv_id, checkpoint_id, task_text}`; response is
   `nbr_cost_median` / `nbr_count` / `neighbors_active`. No graph structure either way.

## 2. The reference (`adaptive-context-clean`) never had a wire here

In the reference, the curator, GNN checkpoint, and hoods artifact all live **inside the
proxy process** (`service/app.py:50` builds `CuratingProxy` lazily; `curator.py:67` does a
local `torch.load`). `edges()` and the `attach_*` functions return in-memory torch tensors;
nothing graph-shaped is ever serialized. Hoods come from a locally-downloaded
`models/hoods_v1.pkl` (`neighbors.py:30-52`) with local numpy cosine anchor selection. The
only inference-time HTTP is text→embedding to the embed pod and the LLM upstream.

So the reference offers no precedent for sending neighbor values — client and scorer are
the same process. parsec splits that process at exactly the point the reference keeps
internal, and reconstructs the identical graph server-side rather than serializing it.
Every attach step in the reference has a parsec counterpart (including `attach_brief`, via
the vendored `pyg_model.py` and the gate path in `scorer.py`). The parity surface to watch
is the server-side re-derivation of rels 0/1/2 — the one place parsec recomputes what the
reference computed from richer in-memory objects.

## 3. Lift analysis: what would it cost to send the neighbors?

Ranges from "a couple of days" to "reversing an architecture decision," depending on which
neighbors.

### Rels 0/1 (same-file line chain, temporal chain) — small, ~1–2 days

Pure functions of `file_id`, `lo`/`hi`, `step`, and chunk index — data the client already
ships as join keys. The reference logic (`pyg_model.py:328-410`) is ~60 lines. The port
would follow the rel-4 template exactly: implement in `parsec_engine`, splice into the
payload, server discards its vendored emissions and replaces with the client's pairs.

The cost is not the port, it's **parity**: the causal chain has fiddly tie-breaking
(`max(below, key=(lo, i))`, equal-step ties broken by chunk index, the
`AC_STRICTEDGE` / `AC_FILECHAIN` / `AC_PRUNE` flag variants) that must match the Python
byte-for-byte, plus a canonical edge ordering so scatter order is preserved (as rel-4's
lexicographic sort does today).

### Rel 2 (kNN cosine) — the blocker

Needs the 1024-d content embeddings, and the v2 client is deliberately embedder-less
(`featurize.rs:327` sends `content_embs: None`; see `docs/server-side-embedding.md`,
2026-07-20). Computing kNN edges client-side means either:

- (a) putting an embedder back in the Rust client — model distribution, ORT runtime, and a
  new parity surface against the GPU embedder; this re-litigates the server-side-embedding
  decision and its "why" list (small-machine OOM, Pro retrain, install weight); or
- (b) a server→client round trip just to fetch embeddings — extra latency and a contract
  wart, defeating the purpose.

Neither is a "send a list" change.

### Hoods blocks (cross-trace neighbors) — non-starter

The client would need the hoods artifact locally, i.e. shipping training-derived
embeddings from our cloud to every user machine. That runs straight into the
**control-plane rule** (GNN weights/artifacts never leave our cloud), and the reference
explicitly consolidated on a single artifact path with no per-host fallbacks
(`neighbors.py:34-38`).

### Contract overhead (any of the above)

A v2 schema extension or v3 bump: JSON schema, the `extra="forbid"` pydantic model, the
Rust payload builder, server accept-and-replace logic. Mechanical (~1 day) but a
coordinated two-sided deploy.

## 4. Conclusion

- "Client authors all the edges it *can* author" = rels 0/1 on top of the existing rel-4.
  Small, well-precedented lift.
- "Server never builds neighborhood structure" is not reachable without undoing
  server-side embedding (rel 2) and leaking the hoods artifact. Non-starter.
- **The win is marginal either way.** The server must embed the text and run the GNN
  regardless, so shipping rels 0/1 saves it a trivial recompute of edges it can already
  derive deterministically from the same join keys. Unless a parity bug or a
  trust-boundary argument makes the client authoritative for those edges, the current
  split — per-node features + rel-4 out, graph rebuilt server-side — is the right shape,
  and it is also what keeps brain replicas stateless and round-robin-safe.
