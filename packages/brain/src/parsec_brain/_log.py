"""Structured logging for the brain (stdlib logging only — no new deps).

Level comes from PARSEC_BRAIN_LOG (default INFO); PARSEC_BRAIN_LOG_JSON=1
switches the handler to JSON lines. Timestamps are UTC. Loggers live under the
"parsec_brain" hierarchy and PROPAGATE (pytest caplog sees every record).

DATA-PLANE RULE APPLIES TO LOGS: never log raw message text, task_text, tool
schemas, rule text or conversation ids in the clear — conv ids ride as sha8
prefixes only (`conv_sha8`), everything else as counts/ids/timings.

Fail-open events are COUNTED, not just printed (CLAUDE.md: "fail open, but
measured"): `count_fail_open()` increments COUNTERS["fail_opens"], which
/health exposes alongside COUNTERS["requests"].
"""
from __future__ import annotations

import hashlib
import json
import logging
import os
import time

_ROOT = "parsec_brain"
_FMT = "%(asctime)s %(levelname)s %(name)s %(message)s"

# process-wide observability counters, served on /health (fail-open events are
# alertable; this is measurement, not serving state — served bytes never read it)
COUNTERS = {"fail_opens": 0, "requests": 0}


class _JsonFormatter(logging.Formatter):
    converter = time.gmtime

    def format(self, record: logging.LogRecord) -> str:
        return json.dumps({
            "ts": self.formatTime(record, "%Y-%m-%dT%H:%M:%SZ"),
            "level": record.levelname,
            "logger": record.name,
            "msg": record.getMessage(),
        }, ensure_ascii=False)


def _configure() -> logging.Logger:
    root = logging.getLogger(_ROOT)
    if getattr(root, "_parsec_configured", False):
        return root
    level = os.environ.get("PARSEC_BRAIN_LOG", "INFO").upper()
    root.setLevel(getattr(logging, level, logging.INFO))
    handler = logging.StreamHandler()
    if os.environ.get("PARSEC_BRAIN_LOG_JSON") == "1":
        handler.setFormatter(_JsonFormatter(_FMT))
    else:
        fmt = logging.Formatter(_FMT)
        fmt.converter = time.gmtime
        handler.setFormatter(fmt)
    root.addHandler(handler)
    root.propagate = True                     # caplog/root handlers still see records
    root._parsec_configured = True            # type: ignore[attr-defined]
    return root


def get_logger(name: str) -> logging.Logger:
    _configure()
    return logging.getLogger(f"{_ROOT}.{name}")


def conv_sha8(conv_id: str) -> str:
    """The ONLY form a conversation id may take in a log line."""
    return hashlib.sha256((conv_id or "").encode("utf-8")).hexdigest()[:8]


def kv(**fields) -> str:
    """Stable key=value rendering for structured log lines (no raw text values)."""
    return " ".join(f"{k}={fields[k]}" for k in fields)


def count_fail_open(logger: logging.Logger, msg: str) -> None:
    """WARNING + counted: every fail-open is measured (COUNTERS on /health)."""
    COUNTERS["fail_opens"] += 1
    logger.warning("FAIL-OPEN %s %s", msg, kv(fail_opens_total=COUNTERS["fail_opens"]))
