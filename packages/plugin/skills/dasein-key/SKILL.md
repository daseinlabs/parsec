---
name: dasein-key
description: Set the Dasein API key so your token savings show up in your dashboard. Use when the user wants to connect their account, add/paste their dsn_ API key, see which key is configured, or stop reporting.
---

The Dasein proxy reports each request's measured savings to the user's dashboard
only when it has their per-account API key. This skill sets/shows/clears that key
via the `dasein` binary — the key lands in `~/.dasein/credentials.json` (mode
0600) and takes effect on the **next request, with no restart**.

The key is a `dsn_…` token the user mints in the dashboard under **Account →
Brain API key**. If they don't have one, tell them to mint it there first, then
paste it here.

## Setting the key

When the user gives you a `dsn_…` key, run:

```
${CLAUDE_PLUGIN_ROOT}/bin/dasein key set <the-key>
```

Then confirm with:

```
${CLAUDE_PLUGIN_ROOT}/bin/dasein key show
```

Report back conversationally: the command prints a **masked** key and whether
shipping is now active. **Never echo the full key back to the user** — it's a
secret; refer to it only by the masked form the command prints.

If `dasein key show` reports *shipping: inactive — need … a platform URL*, this
is a dev/local build with no baked platform URL. Ask the user for their platform
URL and pass it through:

```
${CLAUDE_PLUGIN_ROOT}/bin/dasein key set <the-key> --platform-url <their-platform-url>
```

Released plugin builds bake the platform URL, so on those the key alone is enough.

## Showing / clearing

- `dasein key show` — the configured key (masked) and where it resolves from
  (a `DASEIN_API_KEY` environment variable overrides the stored file).
- `dasein key clear` — remove the stored key and stop dashboard reporting.

After setting the key, savings from the next request onward appear in the
dashboard (per-model and per-day). The local `/dasein-savings` report is
unaffected — it always reads the on-disk ledger regardless of this key.
