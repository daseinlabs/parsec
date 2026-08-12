#!/usr/bin/env bash
# parsec — one-line installer for users without Claude Code:
#
#   curl -fsSL https://raw.githubusercontent.com/daseinlabs/claude-plugins/main/install.sh | bash
#
# Auto-detects the coding agents on this machine (Codex CLI, opencode) and
# activates parsec for each: downloads this platform's parsec binary from the
# public plugin repo into ~/.parsec/bin/parsec — the stable path the shims,
# skills, and hooks probe — then runs `parsec setup <tool>` per detected
# tool. Nothing is written outside ~/.parsec and the tools' own config dirs;
# no npm, no sudo. Undo per tool: `parsec disable codex|opencode`.
#
# Explicit selection instead of auto-detect, and Codex API-key mode:
#
#   … | bash -s -- codex            # just codex
#   … | bash -s -- opencode         # just opencode
#   … | bash -s -- codex --byok     # codex with OPENAI_API_KEY instead of
#                                   # ChatGPT-subscription routing
#
# Claude Code users: skip this script — install the plugin instead
# (`claude plugin install parsec@parsec-marketplace`), which ships the same
# binary plus the status line, hooks, and skills.
#
# Source of truth: scripts/install.sh in the parsec repo; release.yml
# publishes it next to the binaries it references, so script and binaries
# always ship from the same commit.
set -euo pipefail

BASE="${PARSEC_INSTALL_BASE:-https://raw.githubusercontent.com/daseinlabs/claude-plugins/main}"

# ── arguments ────────────────────────────────────────────────────────────────
tools="" # space-separated; empty ⇒ auto-detect
byok=0
for a in "$@"; do
  case "$a" in
    codex | opencode) tools="$tools $a" ;;
    --byok) byok=1 ;;
    *)
      echo "unknown argument: $a (expected: codex, opencode, --byok)" >&2
      exit 1
      ;;
  esac
done

# ── auto-detect ──────────────────────────────────────────────────────────────
if [ -z "$tools" ]; then
  if command -v codex >/dev/null 2>&1 || [ -d "${CODEX_HOME:-$HOME/.codex}" ]; then
    tools="$tools codex"
  fi
  if command -v opencode >/dev/null 2>&1 || [ -d "${XDG_CONFIG_HOME:-$HOME/.config}/opencode" ]; then
    tools="$tools opencode"
  fi
  if [ -z "$tools" ]; then
    echo "no supported coding agent found (looked for: codex, opencode)." >&2
    echo "install one first, or pick explicitly: … | bash -s -- codex" >&2
    exit 1
  fi
  echo "detected:$tools"
fi

if [ "$byok" = 1 ] && ! printf '%s' "$tools" | grep -qw codex; then
  echo "--byok only applies to codex" >&2
  exit 1
fi

# ── platform binary ──────────────────────────────────────────────────────────
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) plat=darwin-arm64 ;;
  Linux-x86_64) plat=linux-x64 ;;
  *)
    echo "unsupported platform: $(uname -s) $(uname -m)" >&2
    echo "(Windows / other: install the Claude Code plugin instead, or build from source)" >&2
    exit 1
    ;;
esac

dest="$HOME/.parsec/bin/parsec"
mkdir -p "$(dirname "$dest")"
# Download beside the destination (same filesystem), verify it runs, then
# atomically rename onto a FRESH inode — overwriting an existing binary
# in place trips the macOS code-sign cache (SIGKILL on next exec).
tmp="$(mktemp "$dest.XXXXXX")"
trap 'rm -f "$tmp"' EXIT
echo "downloading parsec ($plat)…"
curl -fsSL "$BASE/plugins/parsec/bin/$plat/parsec" -o "$tmp"
chmod 755 "$tmp"
"$tmp" --version >/dev/null # refuse to install a binary that cannot run
mv "$tmp" "$dest"
trap - EXIT

# ── per-tool setup ───────────────────────────────────────────────────────────
for t in $tools; do
  echo
  echo "── setting up $t ──"
  if [ "$t" = codex ] && [ "$byok" = 1 ]; then
    "$dest" setup codex --byok
  else
    "$dest" setup "$t"
  fi
done

if command -v claude >/dev/null 2>&1; then
  echo
  echo "note: Claude Code detected — for parsec there, install the plugin:"
  echo "  claude plugin marketplace add https://github.com/daseinlabs/claude-plugins"
  echo "  claude plugin install parsec@parsec-marketplace"
fi

echo
echo "installed $("$dest" --version) at $dest"
case "$tools" in *codex*) echo "codex: start (or restart) codex — every session routes through parsec; type \$ and pick parsec-savings." ;; esac
case "$tools" in *opencode*) echo "opencode: restart opencode to activate (Anthropic API-key providers only); /parsec-savings shows the ledger." ;; esac
echo "undo anytime: $dest disable codex|opencode"
