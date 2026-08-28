#!/usr/bin/env bash
# Cut a release: bump the workspace version, commit, tag — one step, so the
# tag and the version the binary reports can never drift. The root
# Cargo.toml's [workspace.package] version is the single source of truth:
# it is what `parsec --version`, /health, and hook.rs's plugin-updated check
# all report, and release.yml refuses to publish a tag that disagrees with it.
#
#   make release VERSION=0.2.0    # vX.Y.0 — stable, rolled out to everyone
#   make release VERSION=0.2.1    # anything else — patch, opt-in only
#
# Channel semantics: docs/release-channels.md. Pushing is deliberately left
# to you (that is the moment the release actually happens):
#   git push origin <branch> vX.Y.Z
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${VERSION:-${1:-}}"
if [ -z "$VERSION" ]; then
  echo "usage: make release VERSION=X.Y.Z" >&2
  exit 1
fi
case "$VERSION" in
  v*)
    echo "drop the leading v — the tag gets it: VERSION=${VERSION#v}" >&2
    exit 1
    ;;
esac
# Semver, optional pre-release suffix. A suffix never matches the stable
# pattern below, so pre-releases are always patch-channel (opt-in) — which is
# what a pre-release should be.
if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'; then
  echo "not a semver version: $VERSION" >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "working tree not clean — commit or stash first" >&2
  exit 1
fi
if git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
  echo "tag v$VERSION already exists" >&2
  exit 1
fi
# Same classification release.yml applies — say it now, not after the push.
if printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.0$'; then
  channel="stable — rolled out to everyone"
else
  channel="patch — opt-in only"
fi
branch="$(git symbolic-ref --short HEAD)"
if [ "$branch" != main ]; then
  echo "note: releasing from '$branch', not main" >&2
fi

# First `version = "…"` in the root Cargo.toml is [workspace.package]; every
# crate inherits it via version.workspace = true. perl, not sed -i: BSD and
# GNU sed disagree on in-place flags, and only the FIRST match may change
# (dependency tables carry their own version keys).
V="$VERSION" perl -pi -e '$done ||= s/^version = ".*"$/version = "$ENV{V}"/' Cargo.toml
if ! grep -q "^version = \"$VERSION\"$" Cargo.toml; then
  echo "failed to stamp Cargo.toml — bump [workspace.package] version by hand" >&2
  exit 1
fi
# Sync Cargo.lock's workspace-member entries (touches nothing else); a stale
# lock would otherwise dirty the tree on the next build.
cargo update --workspace --quiet

git add Cargo.toml Cargo.lock
git commit -m "release v$VERSION"
git tag "v$VERSION"
echo "committed + tagged v$VERSION ($channel)"
echo "publish with: git push origin $branch v$VERSION"
