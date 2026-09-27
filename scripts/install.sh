#!/usr/bin/env bash
# parsec — one-line installer:
#
#   curl -fsSL https://raw.githubusercontent.com/daseinlabs/parsec/main/scripts/install.sh | bash
#
# Auto-detects the coding agents on this machine (Claude Code, Codex CLI,
# opencode, pi) and activates parsec for each. Claude Code gets the plugin
# (`claude plugin install parsec@parsec-marketplace` — the same binary plus
# status line, hooks, and skills); codex/opencode/pi get this platform's parsec
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
# Undo: `parsec disable codex|opencode|pi|desktop`, `claude plugin uninstall parsec`.
#
# Explicit selection instead of auto-detect, and Codex API-key mode:
#
#   … | bash -s -- codex            # just codex
#   … | bash -s -- opencode         # just opencode
#   … | bash -s -- pi               # just pi
#   … | bash -s -- claude           # just the Claude Code plugin
#   … | bash -s -- codex --byok     # codex with OPENAI_API_KEY instead of
#                                   # ChatGPT-subscription routing
#   … | bash -s -- desktop          # just Claude Desktop interception
#   … | bash -s -- --no-desktop     # auto-detect, but leave Desktop alone
#   … | bash -s -- desktop --no-ca  # set Desktop up, do not trust the CA
#   … | bash -s -- desktop --no-autostart  # no boot service — interception
#                                   # stops at reboot until `parsec desktop start`
#   … | bash -s -- --key psc_…      # store the dashboard API key too, so
#                                   # savings report from the first request
#                                   # (the app's first-run screen emits this form)
#
#   … | bash -s -- --no-login      # do not open the browser sign-in at the end
#
# (or set PARSEC_NO_DESKTOP=1 / PARSEC_NO_CA=1 / PARSEC_NO_AUTOSTART=1 /
# PARSEC_NO_LOGIN=1 / PARSEC_API_KEY=psc_… before the plain curl | bash form.)
#
# Source of truth: scripts/install.sh in the parsec repo, served from main;
# the binaries it downloads are that repo's GitHub Release assets.
set -euo pipefail

# Binaries come from the GitHub Release assets on daseinlabs/parsec; the
# marketplace is that repository itself (.claude-plugin/marketplace.json).
# PARSEC_RELEASE_BASE points test installs at a directory mirroring the
# GitHub layout (download/<tag>/<asset>, latest/download/<asset>); the
# plugin's bin/parsec shim honours the same variable.
RELEASE_BASE="${PARSEC_RELEASE_BASE:-https://github.com/daseinlabs/parsec/releases}"
MARKETPLACE_URL="${PARSEC_MARKETPLACE_URL:-https://github.com/daseinlabs/parsec}"

# ── arguments ────────────────────────────────────────────────────────────────
tools="" # space-separated; empty ⇒ auto-detect
byok=0
no_desktop="${PARSEC_NO_DESKTOP:-0}"
no_ca="${PARSEC_NO_CA:-0}"
no_autostart="${PARSEC_NO_AUTOSTART:-0}"
# The install ends by signing in (`parsec login`: browser handoff, no key to
# paste) when it can — a terminal to talk to and no key already stored.
# CI and scripted installs opt out; a baked --key skips it on its own.
no_login="${PARSEC_NO_LOGIN:-0}"
# The per-account psc_ key. `--key` wins over the env var; the env var is
# what the proxy itself honours, so it is unset below once the key is on
# disk — otherwise the proxy restarted by this script would carry the env
# copy and `parsec key clear` could never switch it off.
api_key="${PARSEC_API_KEY:-}"
want_key=0
for a in "$@"; do
  if [ "$want_key" = 1 ]; then
    api_key="$a"
    want_key=0
    continue
  fi
  case "$a" in
    codex | opencode | pi | claude | desktop) tools="$tools $a" ;;
    claude-code) tools="$tools claude" ;;
    --byok) byok=1 ;;
    --no-desktop) no_desktop=1 ;;
    --no-ca) no_ca=1 ;;
    --no-autostart) no_autostart=1 ;;
    --no-login) no_login=1 ;;
    --key) want_key=1 ;;
    --key=*) api_key="${a#--key=}" ;;
    *)
      echo "unknown argument: $a (expected: claude, codex, opencode, pi, desktop, --byok, --no-desktop, --no-ca, --no-autostart, --no-login, --key <psc_…>)" >&2
      exit 1
      ;;
  esac
done
if [ "$want_key" = 1 ]; then
  echo "--key needs a value: the psc_… key from https://app.getparsec.ai" >&2
  exit 1
fi
api_key="$(printf '%s' "$api_key" | tr -d '[:space:]')"
unset PARSEC_API_KEY

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
  # pi keeps models.json and extensions/ under its agent dir; PI_CODING_AGENT_DIR
  # is pi's own override for that dir (setup_pi.rs honours the same variable).
  if command -v pi >/dev/null 2>&1 || [ -d "${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}" ]; then
    tools="$tools pi"
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
    echo "no supported Claude client found (looked for: claude, codex, opencode, pi, Claude Desktop)." >&2
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
  # codex/opencode/pi/desktop all run THROUGH the binary; claude does not.
  if printf '%s' "$tools" | grep -qwE 'codex|opencode|pi|desktop'; then
    echo "unsupported platform: $(uname -s) $(uname -m)" >&2
    echo "(Windows / other: install the Claude Code plugin instead, or build from source)" >&2
    exit 1
  fi
  echo "no parsec binary for $(uname -s) $(uname -m) — installing the Claude Code plugin only."
fi
if [ -n "$plat" ]; then
  mkdir -p "$(dirname "$dest")"
  # ── resolve the newest published version ───────────────────────────────────
  # Every vX.Y.Z tag is a GitHub Release on daseinlabs/parsec carrying the
  # per-platform binaries and a manifest.json of their sha256s. Pre-release
  # tags are marked as such, so GitHub's `releases/latest` redirect — the
  # only pointer there is — never lands on one. Resolve the manifest through
  # that redirect, then pull the binary from the SAME tag (not `latest`
  # again: a release cut between the two requests would mismatch the sha).
  # If the manifest cannot be fetched, fall back to the unverified `latest`
  # asset so a transient GitHub hiccup cannot brick the installer.
  manifest="$(curl -fsSL "$RELEASE_BASE/latest/download/manifest.json" 2>/dev/null || true)"
  # `: *` tolerates the pretty-printed "key": "value" spacing.
  release_tag="$(printf '%s' "$manifest" | grep -o '"tag": *"[^"]*"' | head -1 | cut -d'"' -f4)"
  release_ver="$(printf '%s' "$manifest" | grep -o '"version": *"[^"]*"' | head -1 | cut -d'"' -f4)"
  release_sha="$(printf '%s' "$manifest" | grep -o "\"parsec-$plat\": *\"[0-9a-f]*\"" | head -1 | cut -d'"' -f4)"
  # Download beside the destination (same filesystem), verify it runs, then
  # atomically rename onto a FRESH inode — overwriting an existing binary
  # in place trips the macOS code-sign cache (SIGKILL on next exec).
  tmp="$(mktemp "$dest.XXXXXX")"
  trap 'rm -f "$tmp"' EXIT
  if [ -n "$release_tag" ] &&
    curl -fsSL "$RELEASE_BASE/download/$release_tag/parsec-$plat" -o "$tmp" 2>/dev/null; then
    echo "downloaded parsec $release_ver ($plat, from the $release_tag release)"
    if [ -n "$release_sha" ]; then
      actual="$( (shasum -a 256 "$tmp" 2>/dev/null || sha256sum "$tmp" 2>/dev/null) | cut -d' ' -f1)"
      if [ -n "$actual" ] && [ "$actual" != "$release_sha" ]; then
        echo "sha256 mismatch for parsec-$plat: expected $release_sha, got $actual" >&2
        exit 1
      fi
    fi
  else
    echo "downloading parsec ($plat, newest release — manifest unavailable, unverified)…"
    curl -fsSL "$RELEASE_BASE/latest/download/parsec-$plat" -o "$tmp"
  fi
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

# ── API key ──────────────────────────────────────────────────────────────────
# Through the binary when there is one (`parsec key set` also re-reports the
# install so the machine is attributed to the account); otherwise straight
# into ~/.parsec/credentials.json in the shape credentials.rs reads, mode
# 0600, so a claude-only install on a platform without a binary still comes
# up keyed. Done before per-tool setup so the very first routed request
# ships its savings row.
key_saved=0
if [ -n "$api_key" ]; then
  case "$api_key" in
    psc_*) ;;
    *) echo "warning: key does not start with 'psc_' — storing it anyway" >&2 ;;
  esac
  if [ -n "$plat" ] && "$dest" key set "$api_key"; then
    key_saved=1
  else
    creds="$HOME/.parsec/credentials.json"
    mkdir -p "$(dirname "$creds")"
    tmp_creds="$creds.tmp"
    umask 077
    # Only a psc_ key from the dashboard lands here, but escape anyway so a
    # stray quote can never produce a file the proxy rejects.
    esc="$(printf '%s' "$api_key" | sed 's/\\/\\\\/g; s/"/\\"/g')"
    printf '{\n  "api_key": "%s"\n}\n' "$esc" >"$tmp_creds"
    chmod 600 "$tmp_creds"
    mv "$tmp_creds" "$creds"
    umask 022
    echo "saved API key to $creds"
    key_saved=1
  fi
fi

# ── Claude Desktop ───────────────────────────────────────────────────────────
# Desktop has no endpoint setting (its embedded SDK is pinned to
# api.anthropic.com), so the only route in is process-scoped TLS interception
# via mitmproxy — see packages/proxy/src/setup_desktop.rs.
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
        if [ "$key_saved" = 1 ]; then
          echo "Claude Code plugin installed."
        else
          echo "Claude Code plugin installed — run /parsec:login in a session to link your dashboard."
        fi
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
    pi)
      "$dest" setup pi
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
# Word-bounded: no other tool name contains "pi", but a glob on it would be fragile.
case " $tools " in *" pi "*) echo "pi: restart pi to activate — its anthropic provider routes through parsec; other providers go direct." ;; esac
case "$tools" in
  *desktop*)
    echo "desktop: quit Claude Desktop completely (⌘Q, not just the window) and reopen it — mitmproxy hooks the process at launch."
    echo "         only Cowork / Agent mode is routed; the normal chat sidebar is not. Check with: parsec desktop status"
    [ "$no_autostart" = 1 ] && echo "         --no-autostart: interception stops at reboot — bring it back with: parsec desktop start"
    ;;
esac
[ -n "$path_hint" ] && echo "$path_hint"
echo "undo: parsec disable codex|opencode|pi|desktop · claude plugin uninstall parsec"

# ── sign in: the one step left ───────────────────────────────────────────────
# Green only when stdout is a terminal — `curl | bash` into a log stays clean.
if [ -t 1 ]; then grn="$(printf '\033[1;32m')" rst="$(printf '\033[0m')"; else grn="" rst=""; fi
echo
if [ "$key_saved" = 1 ]; then
  echo "${grn}✓ API key saved — savings report to https://app.getparsec.ai from your next request.${rst}"
elif [ -n "$plat" ] && [ "$no_login" != 1 ] && [ -t 1 ]; then
  # Interactive install with a binary: finish by signing in right here.
  # `parsec login` opens the dashboard and hands the key straight back to
  # this machine (packages/proxy/src/login.rs). stdin is the piped script
  # under curl | bash, so the child gets /dev/null — it never reads stdin.
  echo "${grn}➜ Signing in — a browser tab is opening at https://app.getparsec.ai; click Connect.${rst}"
  if ! "$dest" login --timeout 300 </dev/null; then
    echo "${grn}➜ Not signed in yet — run  parsec login  any time (or click Sign in in the parsec menu-bar app).${rst}"
  fi
elif [ -n "$plat" ]; then
  echo "${grn}➜ Sign in: run  parsec login  — it opens https://app.getparsec.ai and links this machine.${rst}"
else
  echo "${grn}➜ Sign in: run /parsec:login in a Claude Code session — it opens https://app.getparsec.ai and links this machine.${rst}"
fi
