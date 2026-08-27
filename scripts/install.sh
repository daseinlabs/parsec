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
# ~/.parsec and the tools' own config dirs; no npm.
#
# Claude Desktop is detected and set up too (macOS). It is the one surface
# with no endpoint setting, so parsec reaches it by process-scoped TLS
# interception, which means installing mitmproxy (brew) and trusting its CA
# in the System keychain — the one step that asks for `sudo`. A launchd boot
# service is installed too, so interception survives reboots (skip that one
# with --no-autostart). Skip Desktop entirely with --no-desktop, or take it
# without the CA with --no-ca. macOS also gates
# mitmproxy behind a Network Extension approval that CANNOT be scripted, so
# Desktop is a two-pass install: this script stops with instructions, you
# approve in System Settings, then re-run `parsec setup desktop`.
# Undo: `parsec disable codex|opencode|desktop`, `claude plugin uninstall parsec`.
#
# Explicit selection instead of auto-detect, and Codex API-key mode:
#
#   … | bash -s -- codex            # just codex
#   … | bash -s -- opencode         # just opencode
#   … | bash -s -- claude           # just the Claude Code plugin
#   … | bash -s -- codex --byok     # codex with OPENAI_API_KEY instead of
#                                   # ChatGPT-subscription routing
#   … | bash -s -- desktop          # just Claude Desktop interception
#   … | bash -s -- --no-desktop     # auto-detect, but leave Desktop alone
#   … | bash -s -- desktop --no-ca  # set Desktop up, do not trust the CA
#   … | bash -s -- desktop --no-autostart  # no boot service — interception
#                                   # stops at reboot until `parsec desktop start`
#
# (or set PARSEC_NO_DESKTOP=1 / PARSEC_NO_CA=1 / PARSEC_NO_AUTOSTART=1 before
# the plain curl | bash form.)
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
no_desktop="${PARSEC_NO_DESKTOP:-0}"
no_ca="${PARSEC_NO_CA:-0}"
no_autostart="${PARSEC_NO_AUTOSTART:-0}"
for a in "$@"; do
  case "$a" in
    codex | opencode | claude | desktop) tools="$tools $a" ;;
    claude-code) tools="$tools claude" ;;
    --byok) byok=1 ;;
    --no-desktop) no_desktop=1 ;;
    --no-ca) no_ca=1 ;;
    --no-autostart) no_autostart=1 ;;
    *)
      echo "unknown argument: $a (expected: claude, codex, opencode, desktop, --byok, --no-desktop, --no-ca, --no-autostart)" >&2
      exit 1
      ;;
  esac
done

# ── Claude Desktop discovery ─────────────────────────────────────────────────
# Presence only. mitmproxy's local mode matches the process by NAME
# (`--mode local:Claude`), so an install we cannot locate on disk is still
# captured once Desktop runs — which is why an explicit `desktop` overrides a
# miss here rather than being blocked by it.
find_claude_desktop() {
  for app in "/Applications/Claude.app" "$HOME/Applications/Claude.app"; do
    if [ -d "$app" ]; then
      printf '%s' "$app"
      return 0
    fi
  done
  # Spotlight catches a relocated install; absent where indexing is off.
  if command -v mdfind >/dev/null 2>&1; then
    hit="$(mdfind "kMDItemFSName == 'Claude.app'" 2>/dev/null | head -n1 || true)"
    if [ -n "$hit" ]; then
      printf '%s' "$hit"
      return 0
    fi
  fi
  return 1
}

# ── platform ─────────────────────────────────────────────────────────────────
# Which prebuilt binary this machine can run. Decided BEFORE auto-detect so a
# Desktop found on a platform with no binary (an Intel Mac, say) is dropped
# with a warning instead of aborting an install that could still set Claude
# Code up — the same degradation install.ps1 applies on ARM64 Windows.
plat=""
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) plat=darwin-arm64 ;;
  Linux-x86_64) plat=linux-x64 ;;
esac

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
  # Claude Desktop. macOS only in auto-detect: the Linux redirector needs root
  # and is untested. --no-desktop opts out, because this is the one tool whose
  # setup installs mitmproxy and trusts a CA.
  desktop_app=""
  if [ "$no_desktop" != 1 ] && [ "$(uname -s)" = Darwin ]; then
    desktop_app="$(find_claude_desktop || true)"
    if [ -n "$desktop_app" ]; then
      if [ -n "$plat" ]; then
        tools="$tools desktop"
      else
        # AUTO-detected only: an explicit `desktop` still hits the hard error
        # below — that was a request, not a guess.
        echo "skipping Claude Desktop: it needs the parsec binary, which is not published for $(uname -s) $(uname -m)" >&2
        desktop_app=""
      fi
    fi
  fi
  if [ -z "$tools" ]; then
    echo "no supported Claude client found (looked for: claude, codex, opencode, Claude Desktop)." >&2
    echo "install one first, or pick explicitly: … | bash -s -- codex" >&2
    exit 1
  fi
  echo "detected:$tools"
  if [ -n "$desktop_app" ]; then
    echo "  Claude Desktop at $desktop_app"
    echo "  desktop needs mitmproxy + a CA in the System keychain (sudo); skip it with --no-desktop"
  fi
fi

if [ "$byok" = 1 ] && ! printf '%s' "$tools" | grep -qw codex; then
  echo "--byok only applies to codex" >&2
  exit 1
fi
if [ "$no_desktop" = 1 ] && printf '%s' "$tools" | grep -qw desktop; then
  echo "--no-desktop contradicts an explicit 'desktop' — pick one" >&2
  exit 1
fi

# ── platform binary ──────────────────────────────────────────────────────────
# Fetched whenever this platform has one, not just for the tools that cannot
# work without it: the skills, the status line, and every doc tell people to
# run `parsec …`, and a claude-only install used to leave that command missing
# until some later session created it. The Claude Code plugin still ships its
# own copy — this is the one on PATH.
dest="$HOME/.parsec/bin/parsec"
path_hint=""
if [ -z "$plat" ]; then
  # codex/opencode/desktop all run THROUGH the binary; claude does not.
  if printf '%s' "$tools" | grep -qwE 'codex|opencode|desktop'; then
    echo "unsupported platform: $(uname -s) $(uname -m)" >&2
    echo "(Windows / other: install the Claude Code plugin instead, or build from source)" >&2
    exit 1
  fi
  echo "no parsec binary for $(uname -s) $(uname -m) — installing the Claude Code plugin only (it ships its own)."
fi
if [ -n "$plat" ]; then
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

# ── Claude Desktop ───────────────────────────────────────────────────────────
# Desktop has no endpoint setting (its embedded SDK is pinned to
# api.anthropic.com), so the only route in is process-scoped TLS interception
# via mitmproxy — see docs/claude-desktop-integration.md.
install_parsec_desktop() {
  app="$(find_claude_desktop || true)"
  if [ -n "$app" ]; then
    echo "Claude Desktop: $app"
  else
    echo "Claude Desktop not found on disk — continuing anyway: mitmproxy hooks the process by name, not by path."
  fi

  if ! command -v mitmdump >/dev/null 2>&1; then
    if command -v brew >/dev/null 2>&1; then
      echo "mitmproxy not found — installing it (brew)…"
      brew install mitmproxy || echo "(brew install failed — continuing)"
    fi
  fi
  # A `curl | bash` shell may not have brew's prefix on PATH.
  if ! command -v mitmdump >/dev/null 2>&1; then
    for d in /opt/homebrew/bin /usr/local/bin; do
      if [ -x "$d/mitmdump" ]; then
        PATH="$d:$PATH"
        export PATH
        break
      fi
    done
  fi
  if ! command -v mitmdump >/dev/null 2>&1; then
    echo "mitmproxy is not installed, so Claude Desktop cannot be intercepted." >&2
    echo "  install: brew install mitmproxy   (or: pipx install mitmproxy)" >&2
    echo "  then:    parsec setup desktop" >&2
    return 0
  fi
  echo "mitmproxy: $(command -v mitmdump)"

  set -- setup desktop
  if [ "$no_ca" != 1 ]; then
    set -- "$@" --install-ca
    echo "the CA goes into the System keychain — macOS will ask for your password (sudo)."
  fi
  if [ "$no_autostart" != 1 ]; then
    # Out-of-the-box means surviving a reboot: without the boot service the
    # interceptor dies with the session and Desktop silently goes unrouted
    # (fail open) until someone runs `parsec desktop start`.
    set -- "$@" --autostart
    echo "installing the launchd boot service so interception survives reboots (skip with --no-autostart)."
  fi
  if "$dest" "$@"; then
    return 0
  fi
  # gate_extension() bails here on a first run: mitmproxy's Network Extension
  # has to be installed and approved by hand, and no flag gets past a System
  # Settings toggle. That is pass one of two, not a failure — the message
  # above says which state it stopped in.
  echo
  echo "Claude Desktop needs the manual step printed above, then re-run:"
  echo "  parsec $*"
  return 0
}

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
    desktop)
      install_parsec_desktop
      ;;
  esac
done

echo
[ -n "$plat" ] && echo "installed $("$dest" --version) at $dest"
case "$tools" in *claude*) echo "claude: restart Claude Code (or start a new session) — setup runs automatically." ;; esac
case "$tools" in *codex*) echo "codex: start (or restart) codex — every session routes through parsec; type \$ and pick parsec-savings." ;; esac
case "$tools" in *opencode*) echo "opencode: restart opencode to activate (Anthropic API-key providers only); /parsec-savings shows the ledger." ;; esac
case "$tools" in
  *desktop*)
    echo "desktop: quit Claude Desktop completely (⌘Q, not just the window) and reopen it — mitmproxy hooks the process at launch."
    echo "         only Cowork / Agent mode is routed; the normal chat sidebar is not. Check with: parsec desktop status"
    [ "$no_autostart" = 1 ] && echo "         --no-autostart: interception stops at reboot — bring it back with: parsec desktop start"
    ;;
esac
[ -n "$path_hint" ] && echo "$path_hint"
echo "undo: parsec disable codex|opencode|desktop · claude plugin uninstall parsec"
