#!/usr/bin/env bash
# parsec — one-line installer:
#
#   curl -fsSL https://raw.githubusercontent.com/daseinlabs/plugins/main/install.sh | bash
#
# Auto-detects the coding agents on this machine (Claude Code, Codex CLI,
# opencode) and activates parsec for each. Claude Code gets the plugin
# (`claude plugin install parsec@parsec-marketplace` — the same binary plus
# status line, hooks, and skills); codex/opencode get this platform's parsec
# binary downloaded into ~/.parsec/bin/parsec — the stable path the shims,
# skills, and hooks probe — followed by `parsec setup <tool>`. Also puts
# ~/.parsec/bin on PATH (one guarded line appended to your shell rc) so
# `parsec` is callable from anywhere. Nothing else is written outside
# ~/.parsec and the tools' own config dirs; no npm, no sudo.
# Undo: `parsec disable codex|opencode`, `claude plugin uninstall parsec`.
#
# Explicit selection instead of auto-detect, and Codex API-key mode:
#
#   … | bash -s -- codex            # just codex
#   … | bash -s -- opencode         # just opencode
#   … | bash -s -- claude           # just the Claude Code plugin
#   … | bash -s -- codex --byok     # codex with OPENAI_API_KEY instead of
#                                   # ChatGPT-subscription routing
#
# Source of truth: scripts/install.sh in the parsec repo; release.yml
# publishes it next to the binaries it references, so script and binaries
# always ship from the same commit.
set -euo pipefail

BASE="${PARSEC_INSTALL_BASE:-https://raw.githubusercontent.com/daseinlabs/plugins/main}"
MARKETPLACE_URL="${PARSEC_MARKETPLACE_URL:-https://github.com/daseinlabs/plugins}"

# ── arguments ────────────────────────────────────────────────────────────────
tools="" # space-separated; empty ⇒ auto-detect
byok=0
for a in "$@"; do
  case "$a" in
    codex | opencode | claude) tools="$tools $a" ;;
    claude-code) tools="$tools claude" ;;
    --byok) byok=1 ;;
    *)
      echo "unknown argument: $a (expected: claude, codex, opencode, --byok)" >&2
      exit 1
      ;;
  esac
done

# ── auto-detect ──────────────────────────────────────────────────────────────
if [ -z "$tools" ]; then
  # Claude Code needs its CLI present (the plugin installs through it);
  # config-dir detection alone would catch machines it was removed from.
  if command -v claude >/dev/null 2>&1; then
    tools="$tools claude"
  fi
  if command -v codex >/dev/null 2>&1 || [ -d "${CODEX_HOME:-$HOME/.codex}" ]; then
    tools="$tools codex"
  fi
  if command -v opencode >/dev/null 2>&1 || [ -d "${XDG_CONFIG_HOME:-$HOME/.config}/opencode" ]; then
    tools="$tools opencode"
  fi
  if [ -z "$tools" ]; then
    echo "no supported coding agent found (looked for: claude, codex, opencode)." >&2
    echo "install one first, or pick explicitly: … | bash -s -- codex" >&2
    exit 1
  fi
  echo "detected:$tools"
fi

if [ "$byok" = 1 ] && ! printf '%s' "$tools" | grep -qw codex; then
  echo "--byok only applies to codex" >&2
  exit 1
fi

# ── platform binary (codex/opencode only — the Claude Code plugin ships its
#    own) ────────────────────────────────────────────────────────────────────
dest="$HOME/.parsec/bin/parsec"
path_hint=""
if printf '%s' "$tools" | grep -qwE 'codex|opencode'; then
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) plat=darwin-arm64 ;;
    Linux-x86_64) plat=linux-x64 ;;
    *)
      echo "unsupported platform: $(uname -s) $(uname -m)" >&2
      echo "(Windows / other: install the Claude Code plugin instead, or build from source)" >&2
      exit 1
      ;;
  esac
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
  # A proxy that predates this install keeps serving the OLD binary —
  # restart so the fresh one owns the port (identity-checked: a foreign
  # process on the port is never killed). In-flight requests from other
  # sessions see one brief blip and recover on their next request.
  "$dest" up --restart

  # ── put ~/.parsec/bin on PATH so `parsec` works from any shell ────────────
  bin_dir="$HOME/.parsec/bin"
  case ":$PATH:" in
    *":$bin_dir:"*) ;; # already on PATH — nothing to do
    *)
      # $SHELL, not the shell running this script — `curl | bash` is always
      # bash even for zsh/fish users.
      case "$(basename "${SHELL:-sh}")" in
        zsh) rc="${ZDOTDIR:-$HOME}/.zshrc" ;;
        bash) rc="$HOME/.bashrc" ;;
        fish) rc="${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/parsec.fish" ;;
        *) rc="$HOME/.profile" ;;
      esac
      if ! grep -qs '\.parsec/bin' "$rc"; then
        mkdir -p "$(dirname "$rc")"
        if [ "${rc##*.}" = fish ]; then
          printf '\n# parsec\nfish_add_path --prepend "%s"\n' "$bin_dir" >>"$rc"
        else
          printf '\n# parsec\nexport PATH="%s:$PATH"\n' "$bin_dir" >>"$rc"
        fi
        echo "added $bin_dir to PATH in $rc"
      fi
      path_hint="restart your shell (or: export PATH=\"$bin_dir:\$PATH\") to use \`parsec\` directly."
      ;;
  esac
fi

# ── per-tool setup ───────────────────────────────────────────────────────────
for t in $tools; do
  echo
  echo "── setting up $t ──"
  case "$t" in
    claude)
      # The plugin route: same binary plus the status line, hooks, and
      # skills. marketplace add is idempotent-ish — tolerate "already
      # added" and let install be the arbiter.
      claude plugin marketplace add "$MARKETPLACE_URL" 2>/dev/null \
        || echo "(marketplace already added — continuing)"
      if claude plugin install parsec@parsec-marketplace; then
        echo "Claude Code plugin installed — get a key at https://app.getparsec.ai and run /parsec:key in a session."
      else
        echo "plugin install failed — do it manually:" >&2
        echo "  claude plugin marketplace add $MARKETPLACE_URL" >&2
        echo "  claude plugin install parsec@parsec-marketplace" >&2
      fi
      ;;
    codex)
      if [ "$byok" = 1 ]; then
        "$dest" setup codex --byok
      else
        "$dest" setup codex
      fi
      ;;
    opencode)
      "$dest" setup opencode
      ;;
  esac
done

echo
case "$tools" in *codex* | *opencode*) echo "installed $("$dest" --version) at $dest" ;; esac
case "$tools" in *claude*) echo "claude: restart Claude Code (or start a new session) — setup runs automatically." ;; esac
case "$tools" in *codex*) echo "codex: start (or restart) codex — every session routes through parsec; type \$ and pick parsec-savings." ;; esac
case "$tools" in *opencode*) echo "opencode: restart opencode to activate (Anthropic API-key providers only); /parsec-savings shows the ledger." ;; esac
[ -n "$path_hint" ] && echo "$path_hint"
echo "undo: parsec disable codex|opencode · claude plugin uninstall parsec"
