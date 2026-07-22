# demo — the dasein "cost race"

Standalone Next.js app for the cost-savings demo we record for video. Kept
**separate from `packages/frontend`** (the prod dashboard) so the demo never
ships in a prod deployment and can iterate on its own.

Everything shown is replayed from `public/demo-tape.json`, which is built from
the **real** on-disk dasein ledger + hook state — no estimates (honors the
repo's measurement-honesty invariant).

## Run it

```bash
bun install
bun run dev        # http://localhost:3001  (port 3001 to coexist with the prod frontend on 3000)
```

## Rebuild the tape from real measured data

Reads `~/.dasein/ledger.jsonl` and `~/.dasein/sessions/`, and shells out to the
`dasein` binary (`../plugin/bin/dasein`, or set `DASEIN_BIN`) for the
counterfactual token counts:

```bash
bun run tape                       # auto-pick the best real conversation
node scripts/build-demo-tape.mjs --conv <id>   # pin a conversation
```

Output: `public/demo-tape.json`.
