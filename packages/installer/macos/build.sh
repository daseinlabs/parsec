#!/usr/bin/env bash
# Build the macOS installer: parsec-<version>-macos-arm64.pkg
#
#   packages/installer/macos/build.sh --binary target/release/parsec [--out DIR]
#                                     [--version X.Y.Z] [--sign] [--notarize]
#
# Runs on a dev Mac and in CI (release.yml `installers` job). Seven component
# packages (pkgbuild) under one Distribution (productbuild): core carries the
# payload, the other six are script-only (five choices and the hidden sign-in step).
#
# Signing is opt-in and gated on what is in the environment, so the same
# script builds unsigned until credentials exist:
#   --sign      needs DEVELOPER_ID_APPLICATION ("Developer ID Application: …")
#               for the binary (codesign, hardened runtime, timestamp) and
#               DEVELOPER_ID_INSTALLER ("Developer ID Installer: …") for the
#               product (productsign). Notarization rejects a pkg whose Mach-O
#               payload is not Developer ID signed, so both are required.
#   --notarize  needs APPLE_NOTARY_KEY_ID, APPLE_NOTARY_ISSUER_ID,
#               APPLE_NOTARY_KEY_P8_PATH (App Store Connect API key)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
BIN=""
VERSION=""
OUT="$HERE/build"
SIGN=0
NOTARIZE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BIN="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --sign) SIGN=1; shift ;;
    --notarize) NOTARIZE=1; shift ;;
    -h | --help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done
[ -n "$BIN" ] || { echo "--binary is required" >&2; exit 1; }
[ -x "$BIN" ] || { echo "$BIN is not executable" >&2; exit 1; }
case "$(file "$BIN")" in
  *arm64*) ;;
  *) echo "$BIN is not an arm64 binary (only darwin-arm64 is published)" >&2; exit 1 ;;
esac

# The binary's own version is the single source of truth (CARGO_PKG_VERSION,
# what /health and the plugin-updated check compare against). --version may
# restate it, never contradict it — the same rule release.yml applies to tags.
reported="$("$BIN" --version | awk '{print $2}')"
if [ -n "$VERSION" ] && [ "$VERSION" != "$reported" ]; then
  echo "binary reports '$reported', --version says '$VERSION'" >&2
  exit 1
fi
VERSION="$reported"
# pkg versions must be numeric dotted; a -rc suffix goes in the file name only.
PKG_VERSION="${VERSION%%-*}"

STAGE="$OUT/stage"
PKGS="$OUT/pkgs"
rm -rf "$STAGE" "$PKGS"
mkdir -p "$STAGE/core-root/usr/local/parsec/bin" "$STAGE/resources" "$STAGE/scripts" "$PKGS"

# ── payload ──────────────────────────────────────────────────────────────────
install -m 755 "$BIN" "$STAGE/core-root/usr/local/parsec/bin/parsec"
if [ "$SIGN" = 1 ]; then
  # The binary must carry a Developer ID Application signature with the
  # hardened runtime and a secure timestamp before it goes into the payload;
  # productsign only covers the pkg wrapper and notarytool checks the Mach-O
  # inside. --force replaces the ad-hoc signature the linker puts on arm64.
  : "${DEVELOPER_ID_APPLICATION:?--sign needs DEVELOPER_ID_APPLICATION (e.g. 'Developer ID Application: Dasein Labs (TEAMID)')}"
  codesign --force --sign "$DEVELOPER_ID_APPLICATION" --options runtime --timestamp \
    "$STAGE/core-root/usr/local/parsec/bin/parsec"
  codesign --verify --strict --verbose=2 "$STAGE/core-root/usr/local/parsec/bin/parsec"
fi
# The menu-bar app's bundle, shipped pre-signed. `parsec tray install` used
# to assemble parsec.app on the user's machine around the (Developer ID,
# hardened-runtime) binary above — and a signed binary inside a bundle with
# no resource seal is what Gatekeeper calls "damaged". The bundle is
# assembled here by the binary itself (one source of truth for its layout)
# and signed as a whole; install copies it verbatim when versions match and
# otherwise falls back to an ad-hoc seal of its own.
PAYLOAD_BIN="$STAGE/core-root/usr/local/parsec/bin/parsec"
APP="$STAGE/core-root/usr/local/parsec/parsec.app"
"$PAYLOAD_BIN" tray bundle --out "$APP" --binary "$PAYLOAD_BIN" >/dev/null
if [ "$SIGN" = 1 ]; then
  codesign --force --deep --sign "$DEVELOPER_ID_APPLICATION" --options runtime --timestamp "$APP"
else
  codesign --force --deep --sign - "$APP"
fi
codesign --verify --deep --strict --verbose=2 "$APP"
install -m 755 "$HERE/payload/uninstall.sh" "$STAGE/core-root/usr/local/parsec/uninstall.sh"
printf '%s\n' "$VERSION" > "$STAGE/core-root/usr/local/parsec/VERSION"

# ── scripts: one dir per component, lib.sh beside each postinstall ──────────
for c in core claude codex opencode pi desktop tray signin; do
  mkdir -p "$STAGE/scripts/$c"
  install -m 755 "$HERE/scripts/$c/postinstall" "$STAGE/scripts/$c/postinstall"
  install -m 644 "$HERE/scripts/lib.sh" "$STAGE/scripts/$c/lib.sh"
done

# ── component packages ───────────────────────────────────────────────────────
pkgbuild --root "$STAGE/core-root" --install-location / --ownership recommended \
  --identifier rocks.dasein.parsec.core --version "$PKG_VERSION" \
  --scripts "$STAGE/scripts/core" "$PKGS/core.pkg" >/dev/null
for c in claude codex opencode pi desktop tray signin; do
  pkgbuild --nopayload --identifier "rocks.dasein.parsec.$c" --version "$PKG_VERSION" \
    --scripts "$STAGE/scripts/$c" "$PKGS/$c.pkg" >/dev/null
done

# ── resources: panes with the stylesheet inlined, license, backgrounds ──────
for pane in welcome readme conclusion; do
  # sed's `r` splices the file in verbatim (no escaping of the CSS needed);
  # `d` drops the placeholder line itself.
  sed -e '/@STYLE@/{' -e "r $HERE/resources/style.css" -e 'd' -e '}' \
    "$HERE/resources/$pane.html" > "$STAGE/resources/$pane.html"
  grep -q '@STYLE@' "$STAGE/resources/$pane.html" && { echo "stylesheet not inlined into $pane.html" >&2; exit 1; }
  # The placeholder must sit on its own line between <style> and </style>:
  # sed's `d` drops the whole placeholder line, and a one-line
  # `<style>@STYLE@</style>` took the tags with it — the panes then showed
  # the stylesheet as body text (v0.2.16 and earlier).
  awk '/^<style>$/{s=1} /^<\/style>$/{if(s)e=1} END{exit !(s&&e)}' "$STAGE/resources/$pane.html" \
    || { echo "<style>…</style> lost while inlining $pane.html" >&2; exit 1; }
done
cp "$ROOT/LICENSE" "$STAGE/resources/license.txt"
for img in background.png background-dark.png mark-light.png mark-dark.png; do
  [ -f "$HERE/resources/$img" ] || { echo "missing $HERE/resources/$img — run packages/installer/assets/gen.sh" >&2; exit 1; }
  cp "$HERE/resources/$img" "$STAGE/resources/$img"
done
sed "s/@VERSION@/$PKG_VERSION/g" "$HERE/Distribution.xml" > "$STAGE/Distribution.xml"

# ── product ──────────────────────────────────────────────────────────────────
UNSIGNED="$OUT/parsec-$VERSION-macos-arm64.unsigned.pkg"
FINAL="$OUT/parsec-$VERSION-macos-arm64.pkg"
rm -f "$UNSIGNED" "$FINAL"
productbuild --distribution "$STAGE/Distribution.xml" --resources "$STAGE/resources" \
  --package-path "$PKGS" --version "$PKG_VERSION" "$UNSIGNED" >/dev/null

if [ "$SIGN" = 1 ]; then
  : "${DEVELOPER_ID_INSTALLER:?--sign needs DEVELOPER_ID_INSTALLER (e.g. 'Developer ID Installer: Dasein Labs (TEAMID)')}"
  productsign --sign "$DEVELOPER_ID_INSTALLER" "$UNSIGNED" "$FINAL"
  rm -f "$UNSIGNED"
  pkgutil --check-signature "$FINAL"
  if [ "$NOTARIZE" = 1 ]; then
    : "${APPLE_NOTARY_KEY_ID:?}" "${APPLE_NOTARY_ISSUER_ID:?}" "${APPLE_NOTARY_KEY_P8_PATH:?}"
    xcrun notarytool submit "$FINAL" --key "$APPLE_NOTARY_KEY_P8_PATH" \
      --key-id "$APPLE_NOTARY_KEY_ID" --issuer "$APPLE_NOTARY_ISSUER_ID" --wait
    xcrun stapler staple "$FINAL"
    xcrun stapler validate "$FINAL"
    spctl -a -vv -t install "$FINAL"
  fi
else
  mv "$UNSIGNED" "$FINAL"
  echo "note: unsigned (no --sign) — Gatekeeper will warn on a browser download"
fi

# ── structural checks, every build ──────────────────────────────────────────
rm -rf "$OUT/expanded"
pkgutil --expand "$FINAL" "$OUT/expanded"
grep -q 'choice id="desktop"' "$OUT/expanded/Distribution"
for c in core claude codex opencode pi desktop tray signin; do
  [ -d "$OUT/expanded/$c.pkg" ] || { echo "component $c.pkg missing from the product" >&2; exit 1; }
done
# Proves the Distribution JavaScript parses and the choice defaults evaluate
# (nothing is installed; -target is required for the evaluation context).
installer -pkg "$FINAL" -target / -showChoicesXML >/dev/null
rm -rf "$OUT/expanded"

echo "built $FINAL"
shasum -a 256 "$FINAL"
