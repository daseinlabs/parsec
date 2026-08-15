<div align="center">

<img src="assets/parsec-mark.png" alt="parsec" width="360">

# parsec

### 2× the context. ½ the cost.

Context savings for coding agents — **Claude Code · Codex CLI · opencode** —
measured, never modeled.

</div>

---

## Install

One line. The installer auto-detects the coding agents on your machine and
activates parsec for each — no build step, no npm, no sudo; nothing is written
outside `~/.parsec` and the tools' own config dirs:

```sh
curl -fsSL https://raw.githubusercontent.com/daseinlabs/plugins/main/install.sh | bash
```

Windows (PowerShell; or run the `curl | bash` line inside WSL):

```powershell
powershell -c "irm https://raw.githubusercontent.com/daseinlabs/plugins/main/install.ps1 | iex"
```

Then get a key at **[app.getparsec.ai](https://app.getparsec.ai)** and hand it
to parsec — `/parsec:key` inside a Claude Code session, or from any shell:

```sh
parsec key set psc_…
```

No slash command in your agent (Codex CLI, opencode)? Just paste the key in
chat and ask the agent to set it — it runs the same `parsec key set` for you.

> **Until a key is set, parsec saves nothing.** Your tools keep working
> exactly as before; parsec stays pure passthrough until it is entitled.

Prefer to pick instead of auto-detecting? `… | bash -s -- claude`,
`… | bash -s -- codex`, or `… | bash -s -- opencode`.

### What each tool gets

- **Claude Code**: the full plugin — scout tools, hooks, skills, status-line
  savings (see [What you get](#what-you-get) below).
- **Codex CLI**: every codex session routes through parsec with your existing
  ChatGPT sign-in (the token never leaves your machine). API-key mode instead:
  `… | bash -s -- codex --byok`. Type `$` and pick `parsec-savings` for the
  ledger.
- **opencode**: Anthropic API-key providers; `/parsec-savings` shows the
  ledger.

Undo anytime: `parsec disable codex|opencode`,
`claude plugin uninstall parsec`.

<details>
<summary>Claude Code only, via the plugin marketplace?</summary>

```sh
claude plugin marketplace add https://github.com/daseinlabs/plugins
claude plugin install parsec@parsec-marketplace
```

Or interactively: `/plugin` → **Marketplaces** → add `daseinlabs/plugins`.
Use the full HTTPS URL rather than the `owner/repo` shorthand — the shorthand
clones over SSH, which fails for anyone without a GitHub SSH key.

Already have the plugin and want the other tools too? Just run
`parsec setup codex` (or `parsec setup opencode`) — no download needed.

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

**Compact without losing the plot.** `/parsec:trim` stages a better `/compact`:
a deterministic trim keeping only the parts of the session that were actually
re-read, edited, or used later, plus a standing-directives block so your
rulings keep governing after compaction. Run it, then `/clear` — the trimmed
context is injected into the next session automatically.

**See the number.** Every save is written to a local ledger — rolled up in the
Claude Code status line, codex's `$parsec-savings`, and opencode's
`/parsec-savings`. In Claude Code, ask for it any time:

| Skill | What it does |
|---|---|
| `/parsec:savings` | Measured savings — by request, conversation, and session |
| `/parsec:trim` | A better `/compact`: stage a deterministic trim of the transcript (what was actually re-read, edited, or used later) plus your standing directives, injected automatically after `/clear`. `--level 1–5` sets aggressiveness (default 3) |
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
- **Model traffic never touches our cloud.** Requests ride your own
  credentials, straight from your own machine to your model provider.
- `/parsec:share --preview` dumps the exact bytes that would ever be uploaded,
  locally and human-readable, before anything is sent.

## Platforms

`darwin-arm64` · `linux-x64` · `win-x64`. Intel macOS is not yet supported.

---

## What's in this repo

`plugins/parsec` — the plugin's markdown/JSON surfaces plus the prebuilt
per-platform `parsec` binaries, along with the installers that ship beside
them.

This repo is assembled and force-pushed by CI on every release of the private
monorepo. **Do not commit here by hand** — history is intentionally squashed to
keep clones small.

<div align="center">

[getparsec.ai](https://getparsec.ai) · [dashboard](https://app.getparsec.ai) ·
built by [Dasein Labs](mailto:support@daseinlabs.ai)

</div>
