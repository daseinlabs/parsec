# Security policy

parsec runs a local HTTP proxy on your machine that sits between your coding
agent and the model provider. It sees every request your agent makes, including
your provider credentials in transit. We take reports about it seriously.

## Reporting a vulnerability

Please do **not** open a public issue for security problems.

Use GitHub's private vulnerability reporting on this repository
("Security" tab → "Report a vulnerability"), or email
security@getparsec.ai. Include:

- the parsec version (`parsec --version`) and platform
- a description of the issue and its impact
- steps or a proof of concept to reproduce it

You will get an acknowledgement within 3 business days. We will keep you
informed as we triage and fix the issue, and credit you in the release notes
unless you prefer otherwise.

## Scope

In scope:

- the `parsec` binary (`packages/proxy`, `packages/engine`, `packages/mapgen`)
- the Claude Code plugin, OpenCode plugin, and pi extension (`packages/plugin`,
  `packages/opencode-plugin`, `packages/pi-extension`)
- the native installers and install scripts (`packages/installer`, `scripts/`)
- CI and release workflows in this repository

Out of scope for this repository (report to the same address, we will route
it): the hosted scoring API and the account platform.

## What we care about most

- Credential leakage: provider API keys or OAuth tokens leaving the machine
  anywhere other than the provider's own endpoint.
- Request tampering: the proxy altering a request in a way that is not a
  documented curation step.
- Determinism breaks that could poison provider-side prompt caches.
- Installer or updater paths that could be hijacked.

## Supported versions

Only the latest release receives security fixes.
