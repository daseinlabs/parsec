<div align="center">

<img src="assets/parsec-mark.png" alt="parsec" width="360">

# parsec

### 2× the context. ½ the cost.

Context savings for Claude Code — **measured, never modeled.**

</div>

---

## Install

```sh
claude plugin marketplace add https://github.com/daseinlabs/claude-plugins
claude plugin install parsec@parsec-marketplace
```

Then get a key at **[app.getparsec.ai](https://app.getparsec.ai)** and paste it
into a Claude Code session:

```
/parsec:key
```

That's the whole setup. No build step, no postinstall, no model download — the
`parsec` binary ships prebuilt and the next session starts saving.

> **Until a key is set, parsec saves nothing.** Claude Code keeps working
> exactly as before; parsec stays pure passthrough until it is entitled.

<details>
<summary>Prefer the interactive installer?</summary>

`/plugin` → **Marketplaces** → add `daseinlabs/claude-plugins`.

Use the full HTTPS URL rather than the `owner/repo` shorthand — the shorthand
clones over SSH, which fails for anyone without a GitHub SSH key.

</details>

---

## What you get

**Explore with a map, not a bulk read.** Scout tools — `repo_map`,
`file_outline`, `find_symbol` — plus a `parsec:explore` subagent that answers
"how does X work" from signatures and line-anchored key lines instead of
dumping whole files into your context.

**Stop paying for the same bytes twice.** A `PreToolUse` hook denies re-reads of
file ranges already in context and breaks repeated identical commands, with an
insist valve for when the file really did change.

**See the number.** Every save is written to a local ledger and rolled up in
your status line. Ask for it any time:

| Skill | What it does |
|---|---|
| `/parsec:savings` | Measured savings — by request, conversation, and session |
| `/parsec:setup` | Activate parsec: routing env, status line, proxy — or retry a failed first run |
| `/parsec:proxy` | Restart the local proxy if it was killed mid-session |
| `/parsec:key` | Set, show, or clear your `psc_…` API key |
| `/parsec:share` | Opt-in telemetry: preview the exact bytes, or turn it off |
| `/parsec:uninstall` | Clean removal — routing, proxy, local data, plugin |

---

## Measurement honesty

Savings numbers are a **per-request `count_tokens` counterfactual** against
actually-billed usage — never a modeled baseline, never an extrapolation. Hook
rows are blocked re-reads × the on-disk bytes of the denied range. If the ledger
is empty, the report says so.

## Your data

- **Telemetry is off by default.** The product is fully functional with it off —
  consent by degradation is not consent.
- **Model traffic never leaves your machine.** Requests to Anthropic ride your
  own credentials, from your own machine.
- `/parsec:share --preview` dumps the exact bytes that would ever be uploaded,
  locally and human-readable, before anything is sent.

## Platforms

`darwin-arm64` · `linux-x64` · `win-x64`. Intel macOS is not yet supported.

---

## What's in this repo

`plugins/parsec` — the plugin's markdown/JSON surfaces plus the prebuilt
per-platform `parsec` binaries.

This repo is assembled and force-pushed by CI on every release of the private
monorepo. **Do not commit here by hand** — history is intentionally squashed to
keep clones small.

<div align="center">

[getparsec.ai](https://getparsec.ai) · [dashboard](https://app.getparsec.ai) ·
built by [Dasein Labs](mailto:support@getparsec.ai)

</div>
