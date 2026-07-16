# Dasein plugin marketplace

Claude Code plugin marketplace for [Dasein](https://dasein.rocks). This repo is
assembled and force-pushed by CI on every release of the private monorepo — do
not commit here by hand; history is intentionally squashed to keep clones small.

## Install

```
claude plugin marketplace add https://github.com/daseinlabs/claude-plugins
claude plugin install dasein@dasein-marketplace
```

Or interactively: `/plugin` → Marketplaces → add `daseinlabs/claude-plugins`.
(The full HTTPS URL avoids the `owner/repo` shorthand's default of cloning
over SSH, which fails for users without a GitHub SSH key.)

## What's inside

`plugins/dasein` — markdown/JSON plugin surfaces plus prebuilt `dasein`
binaries for darwin-arm64, linux-x64, and win-x64 (Intel macOS not yet
supported). There is no
build or postinstall step; the binaries ship ready to run. Savings numbers
shown by the plugin are measured per-request via `count_tokens`, never a
modeled baseline.
