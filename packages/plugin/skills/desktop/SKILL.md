---
name: desktop
description: Route Claude Desktop's Cowork / Agent-mode traffic through the local parsec proxy so it gets curation and savings too. Use when the user asks to use parsec with Claude Desktop, or to turn Desktop interception off.
---

Claude Desktop has no endpoint setting — its embedded SDK is hardwired to
`api.anthropic.com` — so parsec cannot capture it the way it captures Claude
Code. The only route is process-scoped TLS interception via **mitmproxy**,
which means this is opt-in and has two costs the user must agree to before
you run anything:

1. It needs **mitmproxy installed** (`brew install mitmproxy` on macOS). It is
   the one external tool parsec depends on, and only for this feature.
2. It needs **mitmproxy's CA trusted by the OS**. parsec never installs a root
   certificate silently — it prints the exact command for the user to run.

Say both out loud before the first setup run. If the user is only asking "does
this work with Claude Desktop?", answer with the scope note below and stop.

## Check state first

```
${CLAUDE_PLUGIN_ROOT}/bin/parsec setup desktop --status
```

Changes nothing. It reports mitmproxy, the CA, macOS Network Extension
approval, whether the addon and interceptor are in place, and the proxy port
Desktop would be pointed at.

## Turn it on (first time)

```
${CLAUDE_PLUGIN_ROOT}/bin/parsec setup desktop
```

Idempotent — safe to re-run after fixing whatever it complained about. It
generates the CA, writes the managed addon, verifies the platform's
interception mechanism is approved, **restarts the proxy** so it and the addon
come from the same build, and starts `mitmdump` scoped to the Claude process
only.

The restart matters: a proxy left running from an older binary does not serve
`/v1/models`, which the addon redirects — Desktop's model picker would 404.
If setup prints a `WARNING: ... does not serve /v1/models`, the installed
binary itself is stale; tell the user to install the current build before
relying on Desktop.

Flags, both off by default and both worth naming before you use them:

- `--install-ca` — parsec runs the trust-store command itself instead of
  printing it. It will prompt for the user's password. Only pass this if the
  user has said yes to trusting the CA.
- `--autostart` — keep Desktop routed across reboots by installing a boot
  service (launchd agent / systemd `--user` unit / Run key). Without it, the
  interceptor lasts until logout.

Then tell the user to **quit Claude Desktop fully (⌘Q) and relaunch it** —
mitmproxy hooks the process at launch, so a Desktop that was already running
stays unintercepted.

## Day-to-day, once it is set up

Use these instead of re-running setup — they skip the CA and approval steps:

```
${CLAUDE_PLUGIN_ROOT}/bin/parsec desktop status
${CLAUDE_PLUGIN_ROOT}/bin/parsec desktop start [--autostart]
${CLAUDE_PLUGIN_ROOT}/bin/parsec desktop stop [--keep-autostart]
${CLAUDE_PLUGIN_ROOT}/bin/parsec desktop restart
```

- `status` changes nothing — prefer it for any "is Desktop routed?" question.
- `restart` is the fix when the routed proxy port moved; it re-renders the
  addon against the current port.
- `stop` also removes the boot service unless `--keep-autostart`, so a stop
  stays stopped across a reboot.

## Errors it returns, and what they mean

- **"mitmproxy is not installed"** — give them the install command it printed;
  nothing else has changed.
- **"Network Extension is installed but not approved"** — macOS gates this.
  System Settings → General → Login Items & Extensions → Network Extensions →
  turn on "Mitmproxy Redirector". Re-run afterwards. This one matters: without
  approval `mitmdump` starts fine and captures nothing.
- **"Network Extension is not installed"** — the user runs the `mitmdump`
  command it printed once by hand so macOS shows the approval prompt.
- **"mitmdump started but exited immediately"** — read the tail of
  `~/.parsec/interceptor/mitmdump.log` and report the real error rather than
  guessing.

If Desktop shows TLS/certificate errors after setup, the CA is not trusted —
run the command `--status` prints.

## Turn it off

```
${CLAUDE_PLUGIN_ROOT}/bin/parsec disable desktop
```

Stops the interceptor, removes the boot service and the managed addon, and
prints the command to untrust the CA (it does not run that one either).
Desktop goes direct again after a relaunch.

## Scope note — be honest about this

This captures **Cowork / Agent-mode inference** — `/v1/messages*` plus
`/v1/models`. Regular Desktop chat runs on a different wire (`/api/*`) and is
deliberately untouched, as are login (`/v1/oauth/*`) and the Cowork bridge
(`/v1/environments/*`) — redirecting those would break Desktop, not curate it.

The user's own credentials are never substituted: the addon sets no auth
header, and the proxy forwards auth verbatim, exactly as for Claude Code.
Desktop traffic is tagged `x-parsec-tool: claude-desktop` so its savings are
separable in the ledger; that tag is dropped before anything leaves the
machine.

One thing to be straight about if asked: mitmproxy decrypts *everything* the
Claude process sends, because it cannot know a request's host until after the
TLS handshake. The addon only acts on the two Anthropic paths above, but the
termination itself is process-wide. That is inherent to the mechanism.
