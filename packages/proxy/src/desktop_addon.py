# parsec-managed-interceptor — mitmproxy addon for Claude Desktop.
#
# The interception approach (mitmproxy local mode scoped to the Claude
# process, host rewrite of api.anthropic.com to a loopback proxy) follows
# CC-Router's interceptor addon: https://github.com/VictorMinemu/CC-Router
# Copyright (c) 2026 CC-Router Contributors, MIT License. See THIRD_PARTY.md.
#
# Claude Desktop embeds an Anthropic SDK hardwired to api.anthropic.com and
# honors no endpoint override, so `ANTHROPIC_BASE_URL` (how parsec captures
# Claude Code) cannot reach it. mitmproxy's process-scoped "local mode"
# intercepts only the Claude process; this addon rewrites the destination of
# the calls parsec can serve.
#
# TWO cases, both of which occur in a Desktop session:
#
#   1. Requests to api.anthropic.com — Desktop's own Cowork / Agent-mode
#      inference. Rewrite host/port/scheme to the local parsec proxy.
#   2. Requests already aimed at the parsec proxy — Claude Code running
#      *inside* Desktop (the Code tab / Cowork) when it honors
#      ~/.claude/settings.json and so already goes to ANTHROPIC_BASE_URL.
#      This addon passes those through UNTOUCHED, on purpose: there is
#      nothing to rewrite, and tagging them as Desktop traffic would
#      mis-attribute Claude Code's own savings. (CC-Router's addon does act
#      on this case, but only to inject its proxy secret — a step parsec has
#      no equivalent of.)
#
#      KNOWN GAP: recent Desktop builds inject their own
#      ANTHROPIC_BASE_URL=https://api.anthropic.com into the Code tab's
#      process, overriding the settings file. Those requests then look like
#      case 1 and get rewritten here — but the Code tab runs on Node, which
#      uses its own bundled root store rather than the macOS keychain, so it
#      rejects mitmproxy's CA ("Client TLS handshake failed" in mitmdump.log)
#      and the request fails rather than being curated. The Code tab inside
#      Desktop is NOT curated today; see the desktop skill's scope note.
#
# WHAT THIS ADDON DOES NOT DO — deliberately:
#
#   * It never adds, removes, or reads an auth header. parsec forwards the
#     user's own credentials verbatim (DIRECTION §3, "data plane local"), so
#     Desktop's OAuth token rides through untouched. There is no proxy secret
#     to inject because the parsec proxy binds 127.0.0.1 only — injecting one
#     would clobber the caller's own `x-api-key` and break the request.
#   * It never touches anything outside the paths parsec serves. Claude
#     Desktop also talks to `/v1/oauth/*` (session auth), `/v1/environments/*`
#     (the Cowork bridge), and `/api/*` (claude.ai app features). Those carry
#     the user's session token for surfaces parsec does not serve, and
#     redirecting them would break login and Cowork rather than curate them.
#
# The one header it sets is `x-parsec-tool`, parsec's existing calling-tool
# attribution seam (the opencode shim sets it too). It is charset-gated to a
# slug, it is what makes Desktop traffic distinguishable in the savings
# ledger, and it never leaves the machine: the proxy's `forward_auth_headers`
# forwards only auth headers upstream, so this one is dropped at the hop.
#
# Scope note: regular Claude Desktop chat runs over the claude.ai `/api/*`
# wire, not `/v1/messages`. What this captures is Cowork / Agent-mode
# inference — the traffic that looks like Claude Code's.

import logging
import os
import shutil
import socket
import subprocess
import sys
import time
from urllib.parse import urlparse

from mitmproxy import http

# `parsec setup desktop` rewrites this default to the resolved proxy URL when
# it writes the file, so a hand-run `mitmdump -s addon.py` behaves identically
# to the supervised one. The env var still wins when set.
_DEFAULT_TARGET = "http://127.0.0.1:8082"

_target_raw = os.environ.get("PARSEC_DESKTOP_TARGET", _DEFAULT_TARGET)
_target = _target_raw.rstrip("/")
_parsed = urlparse(_target)

if not _parsed.scheme or not _parsed.netloc:
    raise RuntimeError(f"PARSEC_DESKTOP_TARGET is not a valid URL: {_target_raw!r}")

_host = (_parsed.hostname or "127.0.0.1").lower()
_port = _parsed.port or (443 if _parsed.scheme == "https" else 80)

# The paths the parsec proxy speaks on the Anthropic wire. `/v1/messages`
# covers `/v1/messages/count_tokens` by prefix; `/v1/models` is a verbatim
# passthrough there. Both are routed in packages/proxy/src/server.rs `router`
# — keep this tuple and that route table in step.
_REDIRECT_PREFIXES = ("/v1/messages", "/v1/models")

# parsec's calling-tool attribution slug. Charset-gated server-side to
# [a-z0-9-]{1,32}; this value is deliberately inside that.
_TOOL_TAG = "claude-desktop"

# ── liveness: fail open when the proxy is down ───────────────────────────────
#
# The redirect is only safe while something answers at the target. Every other
# client parsec routes has a revival shim (Claude Code's SessionStart hook,
# the Codex hook, the opencode shim) — Claude Desktop has no hook surface at
# all, and after a reboot the boot service brings THIS interceptor back before
# anything starts the parsec proxy. Redirecting into that dead port turned the
# window into hung Desktop sessions. So: probe the target before rewriting,
# pass traffic through to api.anthropic.com UNTOUCHED while it is dead (fail
# open, the project rule), and nudge `parsec up` — throttled — so curation
# comes back on its own.

_PROBE_TTL_S = 3.0  # how long one probe verdict is trusted
_PROBE_TIMEOUT_S = 0.25
_REVIVE_EVERY_S = 30.0  # at most one `parsec up` attempt per window

_probe = {"at": 0.0, "alive": False, "ever": False}
_revive = {"at": 0.0, "proc": None}


def _target_alive() -> bool:
    """Cached TCP probe of the redirect target — only ever the target."""
    now = time.monotonic()
    if _probe["ever"] and now - _probe["at"] < _PROBE_TTL_S:
        return _probe["alive"]
    try:
        with socket.create_connection((_host, _port), timeout=_PROBE_TIMEOUT_S):
            alive = True
    except OSError:
        alive = False
    # Log transitions only: this runs per request and must not be a firehose.
    if alive != _probe["alive"] or not _probe["ever"]:
        logging.info(
            "parsec: proxy %s:%d is %s",
            _host,
            _port,
            "up — redirecting"
            if alive
            else "DOWN — passing Desktop traffic "
            "straight through untouched (fail open) until it answers",
        )
    _probe.update(at=now, alive=alive, ever=True)
    return alive


def _parsec_binary():
    """The stable alias the install scripts maintain, else PATH, else None."""
    name = "parsec.exe" if sys.platform == "win32" else "parsec"
    alias = os.path.join(os.path.expanduser("~"), ".parsec", "bin", name)
    if os.path.exists(alias):
        return alias
    return shutil.which(name)


def _maybe_revive() -> None:
    """Spawn `parsec up` detached, throttled. `up` is idempotent: it spawns
    the supervisor only when the routed port is dead, so racing a hook's own
    revival is harmless."""
    prev = _revive["proc"]
    if prev is not None and prev.poll() is not None:
        _revive["proc"] = None  # reap — bounds zombies to at most one
    now = time.monotonic()
    if now - _revive["at"] < _REVIVE_EVERY_S:
        return
    _revive["at"] = now
    binary = _parsec_binary()
    if binary is None:
        return
    kwargs = {}
    if sys.platform == "win32":
        # DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: outlive this process
        # and its console, same discipline as setup::spawn_detached.
        kwargs["creationflags"] = 0x0000_0008 | 0x0000_0200
    else:
        kwargs["start_new_session"] = True
    try:
        _revive["proc"] = subprocess.Popen(
            [binary, "up"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            **kwargs,
        )
        logging.info("parsec: proxy down — ran `%s up` to revive it", binary)
    except OSError as e:
        logging.info("parsec: could not run `parsec up` (%s)", e)


def responseheaders(flow: http.HTTPFlow) -> None:
    """Forward bodies incrementally instead of buffering them whole.

    mitmproxy ships `stream_large_bodies` UNSET, so by default it reads an
    entire body before forwarding one byte. Anthropic's wire is streaming:
    `/v1/messages` with stream:true is SSE, and the Cowork bridge's work-poll
    is a long-poll whose body does not complete until work exists. Buffering
    either makes the client wait for a response that by design does not end —
    the infinite Claude Desktop hang, and the reason the interceptor looked
    crashed when it was merely holding every stream open.

    Safe because this addon never reads or rewrites a response body: it
    redirects requests and stamps one request header. Streaming costs us
    nothing we use.
    """
    flow.response.stream = True


# Flow counters. Without these, "captured nothing" and "captured plenty,
# redirected none" are indistinguishable from outside the process — which is
# what made a hung Desktop take an evening to diagnose instead of one line.
_SEEN = {"flows": 0, "redirected": 0, "failed_open": 0}


def request(flow: http.HTTPFlow) -> None:
    host = (flow.request.pretty_host or "").lower()

    # Observability, deliberately narrow. Local mode decrypts EVERY host this
    # process talks to (docs §7); logging all of them would turn this file
    # into a browsing history. Only Anthropic/claude.ai, only method + host +
    # path prefix — never a query string, never a body.
    if host.endswith("anthropic.com") or host.endswith("claude.ai"):
        _SEEN["flows"] += 1
        logging.info(
            "parsec: saw %s %s%s",
            flow.request.method,
            host,
            flow.request.path.split("?")[0][:80],
        )

    # Case 2 (already aimed at the parsec proxy) falls out here: not
    # api.anthropic.com, so there is nothing to do.
    if host != "api.anthropic.com":
        return
    if not flow.request.path.startswith(_REDIRECT_PREFIXES):
        return

    if not _target_alive():
        # Fail open: the request proceeds to api.anthropic.com with the
        # user's own credentials, exactly as if parsec were not installed —
        # a session that loses curation, never a session that hangs.
        _SEEN["failed_open"] += 1
        _maybe_revive()
        return

    flow.request.scheme = _parsed.scheme
    flow.request.host = _host
    flow.request.port = _port
    flow.request.headers["host"] = _host + (f":{_port}" if _port not in (80, 443) else "")
    flow.request.headers["x-parsec-tool"] = _TOOL_TAG
    _SEEN["redirected"] += 1
    logging.info(
        "parsec: redirected -> %s:%d  (%d redirected / %d seen)",
        _host,
        _port,
        _SEEN["redirected"],
        _SEEN["flows"],
    )
