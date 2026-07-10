#!/usr/bin/env bash
# §8.1 golden replay against the REAL local brain — the full recorded-
# conversation invariant including the actual GNN forward (the Rust golden
# test, packages/proxy/tests/golden_conversation.rs, runs the same replay
# with a mock brain):
#   fixture turns → dasein proxy → local brain (curator_v4_prod, hash embed,
#   DASEIN_SERVE_TAU forced) → trimmed requests → mock upstream.
# Asserts byte stability + aggregate frozen:new ≥ 10:1 and prints the
# per-turn cut and ratio table. Hermetic: no cloud, no kubectl.
#
# Usage:
#   scripts/golden_replay.sh [fixture.json]   # default: the committed fixture
#
# Requires: the brain venv (packages/brain/.venv), the checkpoint at
# ~/.dasein/brain/curator_v4_prod.pt, cargo.
set -euo pipefail
cd "$(dirname "$0")/.."

FIXTURE="${1:-packages/proxy/parity/fixtures/golden_conversation.json}"
CKPT="${DASEIN_CKPT:-$HOME/.dasein/brain/curator_v4_prod.pt}"
BRAIN_PORT=8093 UPSTREAM_PORT=8094 PROXY_PORT=8095
TMP="$(mktemp -d)"
SPOOL="$TMP/upstream"
PIDS=()
cleanup() { kill "${PIDS[@]}" 2>/dev/null || true; }
trap cleanup EXIT

[ -f "$FIXTURE" ] || { echo "fixture missing: $FIXTURE"; exit 1; }
[ -f "$CKPT" ] || { echo "checkpoint missing: $CKPT (gsutil cp gs://dasein-473321-ac-learning/rulehead/curator_v4_prod.pt \$HOME/.dasein/brain/)"; exit 1; }

echo "── building proxy"
cargo build -q --bin dasein

echo "── mock upstream :$UPSTREAM_PORT (spool $SPOOL)"
python3 scripts/mock_upstream.py --port $UPSTREAM_PORT --spool "$SPOOL" & PIDS+=($!)

# DASEIN_SERVE_TAU forces the operating point so the replay is deterministic
# regardless of what the model thinks of the (sanitized) conversation — the
# replay proves the §8.1 MACHINERY end to end with the real forward in the
# loop, not the checkpoint's taste. Export GOLDEN_TAU="" for calibrated tau.
GOLDEN_TAU="${GOLDEN_TAU-0.999}"
TAU_ENV=()
[ -n "$GOLDEN_TAU" ] && TAU_ENV=(DASEIN_SERVE_TAU="$GOLDEN_TAU")
echo "── brain :$BRAIN_PORT (embed=hash, tau=${GOLDEN_TAU:-calibrated})"
( cd packages/brain && env DASEIN_EMBED_BACKEND=hash "${TAU_ENV[@]}" DASEIN_CKPT="$CKPT" \
    .venv/bin/python -m uvicorn --factory dasein_brain.app:create_app --host 127.0.0.1 \
    --port $BRAIN_PORT --log-level warning ) & PIDS+=($!)
for i in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$BRAIN_PORT/health" >/dev/null && break
  [ "$i" = 60 ] && { echo "brain never came up"; exit 1; }
  sleep 1
done

echo "── proxy :$PROXY_PORT"
env DASEIN_PROXY_PORT=$PROXY_PORT DASEIN_UPSTREAM="http://127.0.0.1:$UPSTREAM_PORT" \
    DASEIN_BRAIN_URL="http://127.0.0.1:$BRAIN_PORT" DASEIN_BRAIN_DEV_RAW=1 \
    DASEIN_BRAIN_TIMEOUT_MS=120000 \
    HOME="$TMP" ./target/debug/dasein proxy & PIDS+=($!)
sleep 1

echo "── replaying $FIXTURE"
python3 - "$FIXTURE" "$PROXY_PORT" "$SPOOL" "$TMP/.dasein/ledger.jsonl" <<'EOF'
import json, pathlib, sys, urllib.request

fixture, port, spool, ledger = sys.argv[1:5]
turns = json.load(open(fixture))["turns"]
for i, body in enumerate(turns, 1):
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/v1/messages", data=data,
        headers={"content-type": "application/json", "x-api-key": "golden"})
    with urllib.request.urlopen(req, timeout=300) as resp:
        assert resp.status == 200, f"turn {i}: HTTP {resp.status}"
    print(f"   turn {i}/{len(turns)} served", end="\r")
print()

reqs = sorted(pathlib.Path(spool).glob("req_*.json"))
assert len(reqs) == len(turns), f"{len(reqs)} upstream requests for {len(turns)} turns"
sent = [json.loads(p.read_bytes()) for p in reqs]

def strip_cc(v):
    if isinstance(v, dict):
        return {k: strip_cc(x) for k, x in v.items() if k != "cache_control"}
    if isinstance(v, list):
        return [strip_cc(x) for x in v]
    return v

rows = [json.loads(l) for l in pathlib.Path(ledger).read_text().splitlines() if l.strip()]
assert len(rows) == len(turns), f"{len(rows)} ledger rows for {len(turns)} turns"

# (a) byte stability: every previously-served message is byte-identical.
for k in range(1, len(sent)):
    prev = [strip_cc(m) for m in sent[k - 1]["messages"]]
    cur = [strip_cc(m) for m in sent[k]["messages"]]
    assert len(prev) <= len(cur), f"turn {k+1} lost messages"
    for j, (a, b) in enumerate(zip(prev, cur)):
        assert a == b, f"turn {k+1} message {j} changed bytes vs turn {k}"

# (b) per-turn cut + simulated cache ratio table.
tok = lambda msgs: len(json.dumps(msgs)) // 4
frozen_tot = new_tot = 0
prev = []
print(f"{'turn':>4} {'msgs':>5} {'frozen_tok':>10} {'new_tok':>8} {'cut_tok':>8} {'brain_ms':>9}")
for k, (s, row) in enumerate(zip(sent, rows), 1):
    msgs = [strip_cc(m) for m in s["messages"]]
    anchor = 0
    while anchor < min(len(msgs), len(prev)) and msgs[anchor] == prev[anchor]:
        anchor += 1
    frozen, new = tok(msgs[:anchor]), tok(msgs[anchor:])
    frozen_tot += frozen
    new_tot += new
    print(f"{k:>4} {len(msgs):>5} {frozen:>10} {new:>8} "
          f"{row.get('freeze_cut_tokens', 0):>8} {row.get('brain_ms', 0):>9}")
    prev = msgs
ratio = frozen_tot / max(new_tot, 1)
print(f"aggregate frozen:new = {frozen_tot}:{new_tot} = {ratio:.1f}:1")

# (c) real brain consulted + content trimmed; (d) no fail-open of any kind.
assert any(r.get("checkpoint_id") for r in rows), "no row carries checkpoint_id"
assert sum(r.get("freeze_cut_tokens", 0) for r in rows) > 0, "nothing trimmed"
assert any(" omitted ...]" in json.dumps(s["messages"]) for s in sent), "no omission marker"
for i, r in enumerate(rows):
    assert not r.get("fail_open"), f"row {i} request fail-open"
    assert not r.get("scorer_fail_opens"), f"row {i} scorer fail-open"
assert ratio >= 10.0, f"cache ratio {ratio:.2f}:1 < 10:1"

ck = next(r["checkpoint_id"] for r in rows if r.get("checkpoint_id"))
print(f"✅ golden replay PASSED · {len(turns)} turns · ratio {ratio:.1f}:1 · ckpt {ck[:12]}…")
EOF
