#!/bin/bash
# Remove parsec installed by the macOS .pkg.
#
#   sudo /usr/local/parsec/uninstall.sh [--user NAME] [--remove-ca] [--yes]
#
# Reverses, as the user: the menu-bar app, Claude Desktop interception, the
# Claude Code routing + status line, the codex/opencode/pi shims, the proxy, and
# ~/.parsec data (`parsec uninstall`); then, as root, the payload and the
# package receipts. mitmproxy's root CA is left in the System keychain unless
# --remove-ca is given — a trust-store change is never silent, in either
# direction (the same posture as `parsec disable desktop`).
set -uo pipefail

USER_NAME=""
REMOVE_CA=0
YES=0
for a in "$@"; do
  case "$a" in
    --user) shift_next=1 ;;
    --remove-ca) REMOVE_CA=1 ;;
    --yes | -y) YES=1 ;;
    -h | --help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      if [ "${shift_next:-0}" = 1 ]; then
        USER_NAME="$a"
        shift_next=0
      else
        echo "unknown argument: $a" >&2
        exit 1
      fi
      ;;
  esac
done

if [ "$(id -u)" != 0 ]; then
  echo "run with sudo: sudo $0 $*" >&2
  exit 1
fi
if [ -z "$USER_NAME" ]; then
  USER_NAME="${SUDO_USER:-$(/usr/bin/stat -f %Su /dev/console 2>/dev/null || true)}"
fi
if [ -z "$USER_NAME" ] || [ "$USER_NAME" = root ]; then
  echo "could not determine the user to clean up for — pass --user NAME" >&2
  exit 1
fi
USER_HOME="$(dscl . -read "/Users/$USER_NAME" NFSHomeDirectory 2>/dev/null | awk '{print $2}')"
[ -n "$USER_HOME" ] || USER_HOME="/Users/$USER_NAME"
PARSEC="$USER_HOME/.parsec/bin/parsec"
USER_PATH="$USER_HOME/.parsec/bin:$USER_HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

as_user() {
  local uid
  uid="$(id -u "$USER_NAME")"
  if launchctl print "gui/$uid" >/dev/null 2>&1; then
    launchctl asuser "$uid" sudo -u "$USER_NAME" -H env HOME="$USER_HOME" USER="$USER_NAME" PATH="$USER_PATH" "$@"
  else
    sudo -u "$USER_NAME" -H env HOME="$USER_HOME" USER="$USER_NAME" PATH="$USER_PATH" "$@"
  fi
}

echo "This removes parsec for $USER_NAME:"
echo "  • menu-bar app, Claude Desktop interception, Claude Code routing, codex/opencode/pi shims"
echo "  • the proxy and ~/.parsec data"
echo "  • /usr/local/parsec and the package receipts"
[ "$REMOVE_CA" = 1 ] && echo "  • mitmproxy's CA from the System keychain (--remove-ca)"
if [ "$YES" != 1 ]; then
  printf 'Continue? [y/N] '
  read -r ans
  case "$ans" in y | Y | yes) ;; *) echo "aborted"; exit 1 ;; esac
fi

if [ -x "$PARSEC" ]; then
  as_user "$PARSEC" tray uninstall || true
  as_user "$PARSEC" disable desktop || true
  as_user "$PARSEC" uninstall || true
  CLAUDE="$(as_user sh -c 'command -v claude' 2>/dev/null || true)"
  if [ -n "$CLAUDE" ]; then
    as_user "$CLAUDE" plugin uninstall parsec@parsec-marketplace || true
  fi
else
  echo "$PARSEC not present — skipping the user-scope cleanup it would have done"
fi

if [ "$REMOVE_CA" = 1 ]; then
  if security delete-certificate -c mitmproxy -t /Library/Keychains/System.keychain; then
    echo "removed mitmproxy's CA from the System keychain"
  else
    echo "no mitmproxy CA found in the System keychain (or it could not be removed)"
  fi
else
  echo "mitmproxy's CA (if trusted) is still in the System keychain. Remove it with:"
  echo "  sudo security delete-certificate -c mitmproxy -t /Library/Keychains/System.keychain"
fi

rm -rf /usr/local/parsec
for c in core claude codex opencode pi desktop tray signin; do
  pkgutil --forget "rocks.dasein.parsec.$c" >/dev/null 2>&1 || true
done
echo
echo "parsec removed. Left on purpose:"
echo "  • Homebrew's mitmproxy (brew uninstall mitmproxy) and ~/.mitmproxy"
echo "  • the 'disabled' marker in $USER_HOME/.parsec (rm -rf it if you like)"
echo "  • the PATH line in your shell rc (harmless; delete the '# parsec' block by hand)"
echo "Restart any open Claude Code sessions."
