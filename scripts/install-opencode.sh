#!/usr/bin/env bash
# Compatibility stub — this URL shipped in earlier releases and must keep
# working. The real installer is install.sh (auto-detects codex + opencode);
# this forwards to it pinned to opencode. Both files publish side by side
# from the same commit (release.yml).
set -euo pipefail
BASE="${PARSEC_INSTALL_BASE:-https://raw.githubusercontent.com/daseinlabs/plugins/main}"
curl -fsSL "$BASE/install.sh" | bash -s -- opencode
