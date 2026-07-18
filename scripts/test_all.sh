#!/usr/bin/env bash
# Run EVERYTHING locally — every suite, every e2e gate, and the closed
# product loop (proxy → brain → ledger → identity trainer → promoted bundle
# → brain serves it again). No cloud, no deployments, no credits.
#
# Prereqs (one-time): the package venvs (brain/bench/platform/trainer — see
# each README), the checkpoint at ~/.dasein/brain/curator_v4_prod.pt, cargo.
set -uo pipefail
cd "$(dirname "$0")/.."

PASS=() FAIL=()
step() { # $1 name, rest = command
  local name="$1"; shift
  echo; echo "━━━ $name"
  if "$@"; then PASS+=("$name"); else FAIL+=("$name"); echo "✗ $name FAILED"; fi
}
py() { local pkg="$1"; shift; (cd "packages/$pkg" && .venv/bin/python -m pytest -q -p no:warnings "$@"); }

# ── static + unit/integration suites ────────────────────────────────────────
step "rust: fmt"        make fmt
step "rust: clippy"     make clippy
step "rust: tests"      make rust-test
step "brain: pytest"    py brain
step "bench: pytest"    py bench
step "platform: pytest" py platform
step "trainer: pytest"  py trainer
if command -v claude >/dev/null 2>&1; then
  step "plugin: validate" claude plugin validate packages/plugin
else
  echo "━━━ plugin: validate — SKIPPED (claude CLI not on PATH)"
fi

# ── end-to-end gates (each spawns brain + proxy + mock upstream) ────────────
step "e2e: smoke (hash embed)"          ./scripts/e2e_smoke.sh
step "e2e: golden replay (§8.1)"        ./scripts/golden_replay.sh
step "e2e: cross-contract parity (v1)"  ./scripts/parity_v1.sh

# ── the closed loop: identity trainer → promoted bundle → brain serves it ──
loop_closure() {
  local out; out="$(mktemp -d)/bundles"
  packages/trainer/.venv/bin/python -m dasein_trainer.retrain \
    --out "$out" --version battery || return 1
  # The §8.1 gate must hold VERBATIM on the promoted artifact (identity ⇒
  # same checkpoint_id, same 10.9:1 table).
  DASEIN_CKPT="$out/battery/curator.pt" \
  DASEIN_RULES_JSON="$out/battery/rules.json" ./scripts/golden_replay.sh
}
step "loop: trainer promote → golden replay on promoted bundle" loop_closure

# ── summary ─────────────────────────────────────────────────────────────────
echo; echo "════════════════════════════════════════════"
for s in "${PASS[@]}"; do echo "  ✅ $s"; done
for s in "${FAIL[@]:-}"; do [ -n "$s" ] && echo "  ❌ $s"; done
echo "════════════════════════════════════════════"
if [ "${#FAIL[@]}" -gt 0 ]; then echo "❌ ${#FAIL[@]} step(s) failed"; exit 1; fi
echo "✅ ALL LOCAL SUITES AND GATES GREEN"
