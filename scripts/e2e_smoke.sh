#!/usr/bin/env bash
# Local end-to-end smoke of the real-scorer path — NO cloud deploy involved:
#   Claude-Code-shaped traffic → dasein proxy → local brain (curator_v4_prod)
#   → trimmed request → mock upstream, with the savings ledger written.
#
# Usage:
#   scripts/e2e_smoke.sh              # hermetic: hash embeddings
#   scripts/e2e_smoke.sh real-embed   # full fidelity: real bge vectors via
#                                     #   kubectl port-forward svc/dasein-embed
#
# Requires: the brain venv (packages/brain/.venv), the checkpoint at
# ~/.dasein/brain/curator_v4_prod.pt, cargo.
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-hash}"
CKPT="${DASEIN_CKPT:-$HOME/.dasein/brain/curator_v4_prod.pt}"
BRAIN_PORT=8090 UPSTREAM_PORT=8091 PROXY_PORT=8092
TMP="$(mktemp -d)"
LEDGER="$TMP/ledger.jsonl"
SPOOL="$TMP/upstream"
PIDS=()
cleanup() { kill "${PIDS[@]}" 2>/dev/null || true; }
trap cleanup EXIT

[ -f "$CKPT" ] || { echo "checkpoint missing: $CKPT (gsutil cp gs://dasein-473321-ac-learning/rulehead/curator_v4_prod.pt \$HOME/.dasein/brain/)"; exit 1; }

echo "── building proxy"
cargo build -q --bin dasein

echo "── mock upstream :$UPSTREAM_PORT (spool $SPOOL)"
python3 scripts/mock_upstream.py --port $UPSTREAM_PORT --spool "$SPOOL" & PIDS+=($!)

echo "── brain :$BRAIN_PORT (embed=$MODE)"
if [ "$MODE" = "real-embed" ]; then
  kubectl port-forward svc/dasein-embed 18080:80 >/dev/null 2>&1 & PIDS+=($!)
  sleep 2
  EMBED_ENV=(DASEIN_EMBED_URL=http://127.0.0.1:18080/embed)
else
  EMBED_ENV=(DASEIN_EMBED_BACKEND=hash)
fi
# DASEIN_SERVE_TAU forces the cut so the smoke is deterministic regardless of
# what the model thinks of this synthetic conversation — the smoke proves the
# MACHINERY (parse parity, forward, quantize, splice, fold, ledger), not the
# operating point. Export SMOKE_TAU="" to serve the calibrated tau instead.
SMOKE_TAU="${SMOKE_TAU-0.999}"
TAU_ENV=()
[ -n "$SMOKE_TAU" ] && TAU_ENV=(DASEIN_SERVE_TAU="$SMOKE_TAU")
( cd packages/brain && env "${EMBED_ENV[@]}" "${TAU_ENV[@]}" DASEIN_CKPT="$CKPT" \
    .venv/bin/python -m uvicorn --factory dasein_brain.app:create_app --host 127.0.0.1 \
    --port $BRAIN_PORT --log-level warning ) & PIDS+=($!)
for i in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$BRAIN_PORT/health" >/dev/null && break
  [ "$i" = 60 ] && { echo "brain never came up"; exit 1; }
  sleep 1
done
curl -s "http://127.0.0.1:$BRAIN_PORT/v1/bundle" | python3 -m json.tool | sed 's/^/   /' | head -15

echo "── proxy :$PROXY_PORT"
env DASEIN_PROXY_PORT=$PROXY_PORT DASEIN_UPSTREAM="http://127.0.0.1:$UPSTREAM_PORT" \
    DASEIN_BRAIN_URL="http://127.0.0.1:$BRAIN_PORT" DASEIN_BRAIN_DEV_RAW=1 \
    HOME="$TMP" ./target/debug/dasein proxy & PIDS+=($!)
sleep 1

# ── drive a CC-shaped conversation ──────────────────────────────────────────
py_file() { python3 -c "
import json
lines = '\n'.join(f'{i}: value_{i} = compute_step_{i}(input_{i})' for i in range(1, 41))
print(json.dumps(lines))"; }
FILE_TEXT=$(py_file)

turn() { # $1 = messages json array
  curl -s -X POST "http://127.0.0.1:$PROXY_PORT/v1/messages" \
    -H 'content-type: application/json' -H 'x-api-key: smoke' \
    -d "{\"model\":\"claude-sonnet-5\",\"max_tokens\":128,\"system\":\"be terse\",\"messages\":$1}" >/dev/null
}

MSGS1="[
  {\"role\":\"user\",\"content\":\"Fix the failing test in parser.py\"},
  {\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Let me read the file.\"},{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Read\",\"input\":{\"file_path\":\"src/parser.py\"}}]},
  {\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":$FILE_TEXT}]}
]"
MSGS2="${MSGS1%]} ,
  {\"role\":\"assistant\",\"content\":\"The bug is on line 12.\"},
  {\"role\":\"user\",\"content\":\"apply the fix\"}
]"

echo "── turn 1"; turn "$MSGS1"
echo "── turn 2"; turn "$MSGS2"

# ── assertions ──────────────────────────────────────────────────────────────
python3 - "$SPOOL" "$TMP/.dasein/ledger.jsonl" <<'EOF'
import json, pathlib, sys
spool, ledger = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
reqs = sorted(spool.glob("req_*.json"))
assert len(reqs) == 2, f"expected 2 upstream requests, saw {len(reqs)}"
r1, r2 = (json.loads(p.read_bytes()) for p in reqs)

obs = json.dumps(r1["messages"][2])
assert "omitted" in obs, f"turn-1 observation NOT trimmed:\n{obs[:400]}"
assert "re-read parser.py" in obs, "no recovery pointer in the digest"

def strip_cc(v):
    if isinstance(v, dict):
        return {k: strip_cc(x) for k, x in v.items() if k != "cache_control"}
    if isinstance(v, list):
        return [strip_cc(x) for x in v]
    return v
for j in range(3):
    a, b = strip_cc(r1["messages"][j]), strip_cc(r2["messages"][j])
    assert a == b, f"resident turn {j} changed bytes across calls"

assert ledger.exists(), f"no ledger at {ledger}"
rows = [json.loads(l) for l in ledger.read_text().splitlines() if l.strip()]
assert rows, "no ledger rows written"
assert rows[0].get("checkpoint_id"), "ledger row missing checkpoint_id"
assert rows[0].get("freeze_cut_tokens", 0) > 0, "ledger shows nothing trimmed"
assert not rows[0].get("fail_open"), "request fail-opened"

print("✅ e2e smoke PASSED")
print(f"   turn-1 digest: {json.dumps(r1['messages'][2])[:160]}…")
print(f"   ledger[0]: cut≈{rows[0]['freeze_cut_tokens']} tok · brain {rows[0].get('brain_ms','?')}ms · ckpt {rows[0]['checkpoint_id'][:12]}…")
EOF
