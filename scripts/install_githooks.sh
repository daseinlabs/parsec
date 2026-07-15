#!/usr/bin/env bash
# Install the repo's git hooks (pre-commit framework, .pre-commit-config.yaml).
# Idempotent: safe to run multiple times. Same pattern as Oxen's
# bin/install-prereqs steps 6–7.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v pre-commit >/dev/null 2>&1; then
  if command -v uv >/dev/null 2>&1; then
    echo "Installing pre-commit via uv..."
    uv tool install pre-commit
  elif command -v pipx >/dev/null 2>&1; then
    echo "Installing pre-commit via pipx..."
    pipx install pre-commit
  elif command -v brew >/dev/null 2>&1; then
    echo "Installing pre-commit via brew..."
    brew install pre-commit
  else
    echo "ERROR: pre-commit not found and no installer available (uv/pipx/brew)." >&2
    echo "Install it manually: https://pre-commit.com/#install" >&2
    exit 1
  fi
fi

pre-commit install
echo "✅ git hooks installed (run 'pre-commit run --all-files' to check everything now)"
