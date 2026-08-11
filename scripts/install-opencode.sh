#!/usr/bin/env bash
# parsec for opencode — one-line installer for users without Claude Code:
#
#   curl -fsSL https://raw.githubusercontent.com/daseinlabs/claude-plugins/main/install-opencode.sh | bash
#
# Downloads this platform's parsec binary from the public plugin repo into
# ~/.parsec/bin/parsec — the stable path the opencode shim probes — then runs
# `parsec setup opencode`, which installs the opencode plugin as a managed
# file drop and warms the local proxy. No npm, no sudo, nothing written
# outside ~/.parsec and ~/.config/opencode. Undo: `parsec disable opencode`.
#
# Source of truth: scripts/install-opencode.sh in the parsec repo;
# release.yml publishes it next to the binaries it references, so script and
# binaries always ship from the same commit.
set -euo pipefail

BASE="${PARSEC_INSTALL_BASE:-https://raw.githubusercontent.com/daseinlabs/claude-plugins/main}"

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

"$dest" setup opencode

echo
echo "installed $("$dest" --version) at $dest"
echo "restart opencode to activate (Anthropic API-key providers only)."
echo "undo anytime: $dest disable opencode"
