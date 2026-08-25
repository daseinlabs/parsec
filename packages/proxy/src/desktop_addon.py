# parsec-managed-interceptor — mitmproxy addon for Claude Desktop.
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
#      *inside* Desktop (Cowork), which reads ~/.claude/settings.json and so
#      already goes to ANTHROPIC_BASE_URL. This addon passes those through
#      UNTOUCHED, on purpose: there is nothing to rewrite, and tagging them
#      as Desktop traffic would mis-attribute Claude Code's own savings.
#      (CC-Router's addon does act on this case, but only to inject its proxy
#      secret — a step parsec has no equivalent of.)
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

import os
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


def request(flow: http.HTTPFlow) -> None:
    # Case 2 (already aimed at the parsec proxy) falls out here: not
    # api.anthropic.com, so there is nothing to do.
    if (flow.request.pretty_host or "").lower() != "api.anthropic.com":
        return
    if not flow.request.path.startswith(_REDIRECT_PREFIXES):
        return

    flow.request.scheme = _parsed.scheme
    flow.request.host = _host
    flow.request.port = _port
    flow.request.headers["host"] = _host + (
        f":{_port}" if _port not in (80, 443) else ""
    )
    flow.request.headers["x-parsec-tool"] = _TOOL_TAG
