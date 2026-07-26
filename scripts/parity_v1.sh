#!/usr/bin/env bash
# THE CROSS-CONTRACT PARITY GATE (brain-api-dev/v0 == brain-api/v1, end to
# end): replay the committed golden fixture TWICE through fresh proxies —
# once on the dev contract (raw internal view, server-side featurization) and
# once on the v1 contract (client featurization: local hash embedder + engine
# node/readout structs + rel-4 pairs; no raw text on the wire) — against ONE
# local brain, and assert every forwarded upstream turn is byte-IDENTICAL
# across the two runs, with zero fail-opens in both ledgers. This is the
# proof that the Rust client featurization + the brain's v1 graph builder
# reproduce the dev scoring path exactly where it matters: served bytes.
#
# Hermetic: no cloud, no kubectl. PARSEC_SERVE_TAU is forced (like
# golden_replay.sh) so the gate pins the MACHINERY at a deterministic
# operating point; export PARITY_TAU="" to run the calibrated tau instead.
#
# The fast per-query twin of this gate is the #[ignore] Rust test — with the
# brain from this script (or golden_replay.sh) still running:
#   PARSEC_BRAIN_URL=http://127.0.0.1:8096 \
#     cargo test -p parsec-proxy --test proxy_brain_v1 -- --ignored
#
# Usage:
#   scripts/parity_v1.sh [fixture.json]   # default: the committed fixture
#
# Requires: the brain venv (packages/brain/.venv), the checkpoint at
# ~/.parsec/brain/curator_v4_prod.pt, cargo.
set -euo pipefail
cd "$(dirname "$0")/.."

FIXTURE="${1:-packages/proxy/parity/fixtures/golden_conversation.json}"
CKPT="${PARSEC_CKPT:-$HOME/.parsec/brain/curator_v4_prod.pt}"
BRAIN_PORT=8096 UP_DEV_PORT=8097 UP_V1_PORT=8098 PROXY_DEV_PORT=8099 PROXY_V1_PORT=8100
TMP="$(mktemp -d)"
PIDS=()
cleanup() { kill "${PIDS[@]}" 2>/dev/null || true; }
trap cleanup EXIT

[ -f "$FIXTURE" ] || { echo "fixture missing: $FIXTURE"; exit 1; }
[ -f "$CKPT" ] || { echo "checkpoint missing: $CKPT (gsutil cp gs://dasein-473321-ac-learning/rulehead/curator_v4_prod.pt \$HOME/.parsec/brain/)"; exit 1; }

echo "── building proxy"
cargo build -q --bin parsec

echo "── mock upstreams :$UP_DEV_PORT (dev spool) / :$UP_V1_PORT (v1 spool)"
python3 scripts/mock_upstream.py --port $UP_DEV_PORT --spool "$TMP/up_dev" & PIDS+=($!)
python3 scripts/mock_upstream.py --port $UP_V1_PORT --spool "$TMP/up_v1" & PIDS+=($!)

# ONE brain serves both contracts (app.py dispatches on the request's
# `contract` field). Hash embed on BOTH sides: the brain embeds the dev run's
# text with _hash_embed, the proxy embeds the v1 run's text with the Rust
# port (bit-parity pinned by tests/parity_embed.rs) — same vectors.
PARITY_TAU="${PARITY_TAU-0.999}"
TAU_ENV=()
[ -n "$PARITY_TAU" ] && TAU_ENV=(PARSEC_SERVE_TAU="$PARITY_TAU")
echo "── brain :$BRAIN_PORT (embed=hash, tau=${PARITY_TAU:-calibrated})"
( cd packages/brain && env PARSEC_EMBED_BACKEND=hash "${TAU_ENV[@]}" PARSEC_CKPT="$CKPT" \
    .venv/bin/python -m uvicorn --factory parsec_brain.app:create_app --host 127.0.0.1 \
    --port $BRAIN_PORT --log-level warning ) & PIDS+=($!)
for i in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$BRAIN_PORT/health" >/dev/null && break
  [ "$i" = 60 ] && { echo "brain never came up"; exit 1; }
  sleep 1
done

replay() { # $1 label  $2 proxy_port  $3 upstream_port  $4 home  $5.. extra env
  local label="$1" pport="$2" uport="$3" home="$4"; shift 4
  echo "── proxy :$pport ($label)"
  env PARSEC_PROXY_PORT="$pport" PARSEC_UPSTREAM="http://127.0.0.1:$uport" \
      PARSEC_BRAIN_URL="http://127.0.0.1:$BRAIN_PORT" PARSEC_BRAIN_TIMEOUT_MS=120000 \
      "$@" HOME="$home" ./target/debug/parsec proxy & local pid=$!
  PIDS+=($pid)
  sleep 1
  echo "── replaying $FIXTURE ($label)"
  python3 - "$FIXTURE" "$pport" <<'EOF'
import json, sys, urllib.request
fixture, port = sys.argv[1:3]
turns = json.load(open(fixture))["turns"]
for i, body in enumerate(turns, 1):
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/v1/messages", data=json.dumps(body).encode(),
        headers={"content-type": "application/json", "x-api-key": "parity"})
    with urllib.request.urlopen(req, timeout=300) as resp:
        assert resp.status == 200, f"turn {i}: HTTP {resp.status}"
    print(f"   turn {i}/{len(turns)} served", end="\r")
print()
EOF
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

replay "dev contract"  $PROXY_DEV_PORT $UP_DEV_PORT "$TMP/home_dev" PARSEC_BRAIN_DEV_RAW=1
replay "v1 contract"   $PROXY_V1_PORT  $UP_V1_PORT  "$TMP/home_v1"  \
       PARSEC_BRAIN_CONTRACT=v1 PARSEC_EMBED_BACKEND=hash

echo "── comparing forwarded turns (dev vs v1)"
python3 - "$TMP" <<'EOF'
import json, pathlib, sys

tmp = pathlib.Path(sys.argv[1])
dev = sorted((tmp / "up_dev").glob("req_*.json"))
v1 = sorted((tmp / "up_v1").glob("req_*.json"))
assert len(dev) == len(v1) and dev, f"turn counts differ: dev={len(dev)} v1={len(v1)}"

def first_divergence(a, b, path="$"):
    """(path, a_side, b_side) of the first structural difference."""
    if type(a) is not type(b):
        return path, f"type {type(a).__name__}", f"type {type(b).__name__}"
    if isinstance(a, dict):
        for k in list(a) + [k for k in b if k not in a]:
            if k not in a or k not in b:
                return f"{path}.{k}", ("<present>" if k in a else "<absent>"), ("<present>" if k in b else "<absent>")
            r = first_divergence(a[k], b[k], f"{path}.{k}")
            if r:
                return r
        return None
    if isinstance(a, list):
        for i in range(max(len(a), len(b))):
            if i >= len(a) or i >= len(b):
                return f"{path}[{i}]", ("<len {}>".format(len(a))), ("<len {}>".format(len(b)))
            r = first_divergence(a[i], b[i], f"{path}[{i}]")
            if r:
                return r
        return None
    if a != b:
        return path, repr(a)[:300], repr(b)[:300]
    return None

fail = False
for k, (fa, fb) in enumerate(zip(dev, v1), 1):
    ba, bb = fa.read_bytes(), fb.read_bytes()
    if ba == bb:
        continue
    fail = True
    print(f"✗ turn {k}: forwarded bytes DIFFER ({len(ba)} vs {len(bb)} bytes)")
    ja, jb = json.loads(ba), json.loads(bb)
    # per-message first divergence — the diagnosis view
    ma, mb = ja.get("messages", []), jb.get("messages", [])
    for j in range(max(len(ma), len(mb))):
        if j >= len(ma) or j >= len(mb):
            print(f"    message[{j}]: only in {'dev' if j < len(ma) else 'v1'} run")
            break
        if ma[j] != mb[j]:
            d = first_divergence(ma[j], mb[j], f"messages[{j}]")
            print(f"    first diverging message: {d[0]}\n      dev: {d[1]}\n      v1 : {d[2]}")
            break
    else:
        d = first_divergence(ja, jb)
        if d:
            print(f"    non-message divergence: {d[0]}\n      dev: {d[1]}\n      v1 : {d[2]}")
    break  # first diverging turn is the diagnosis target; stop there

if fail:
    sys.exit(1)
print(f"   all {len(dev)} forwarded turns byte-identical across contracts")

# zero fail-opens in BOTH ledgers (a fail-open would hide a divergence by
# serving the original bytes on both sides).
for run in ("home_dev", "home_v1"):
    ledger = tmp / run / ".parsec" / "ledger.jsonl"
    rows = [json.loads(l) for l in ledger.read_text().splitlines() if l.strip()]
    assert len(rows) == len(dev), f"{run}: {len(rows)} ledger rows for {len(dev)} turns"
    for i, r in enumerate(rows):
        assert not r.get("fail_open"), f"{run} row {i}: request fail-open"
        assert not r.get("scorer_fail_opens"), f"{run} row {i}: scorer fail-open"
    assert any(r.get("checkpoint_id") for r in rows), f"{run}: no checkpoint_id (brain never consulted)"
    cut = sum(r.get("freeze_cut_tokens", 0) for r in rows)
    print(f"   {run}: {len(rows)} rows, 0 fail-opens, freeze_cut_tokens={cut}")
    assert cut > 0, f"{run}: nothing trimmed — the gate proved nothing"

ck = None
for run in ("home_dev", "home_v1"):
    rows = [json.loads(l) for l in (tmp / run / ".parsec" / "ledger.jsonl").read_text().splitlines() if l.strip()]
    c = next(r["checkpoint_id"] for r in rows if r.get("checkpoint_id"))
    assert ck is None or ck == c, "runs scored against different checkpoints"
    ck = c
print(f"✅ cross-contract parity PASSED · {len(dev)} turns byte-identical · ckpt {ck[:12]}…")
EOF
