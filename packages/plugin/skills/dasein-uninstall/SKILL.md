---
name: dasein-uninstall
description: Cleanly remove dasein from this machine — undo Claude Code routing, stop the local proxy, delete downloaded models and data, then uninstall the plugin itself.
---
Claude Code has no plugin-uninstall lifecycle hook, so cleanup must run while
the plugin (and its binary) still exists. Do the steps IN THIS ORDER and show
the user each command's output:

1. Run `${CLAUDE_PLUGIN_ROOT}/bin/dasein uninstall`. This removes the
   dasein-managed env keys from `~/.claude/settings.json` (never any user
   values), asks the local proxy to shut down (only after `/health` proves the
   listener is ours), and deletes `~/.dasein` data — models, logs, ledger,
   spool — keeping only `setup_state.json` as a "disabled" marker so auto-setup
   can't re-trigger from a still-open session.
2. Run `claude plugin uninstall dasein@dasein-marketplace` (plain
   `claude plugin uninstall dasein` if the scoped name is not found). This
   removes the plugin files, hooks, and this skill.
3. Offer — do not do it unasked — `rm -rf ~/.dasein` to remove the last
   marker file, and `claude plugin marketplace remove dasein-marketplace` if
   they want the marketplace entry gone too.

Tell the user to restart any open Claude Code sessions afterwards: routing
env is read at session start, and already-running sessions still hold the old
`ANTHROPIC_BASE_URL`. If step 1 fails, stop and report the error verbatim —
do not proceed to step 2, because after the plugin is gone the cleanup binary
is gone with it.
