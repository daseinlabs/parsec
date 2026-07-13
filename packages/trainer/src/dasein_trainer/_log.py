"""Structured logging for the trainer (stdlib logging only — same pattern as
dasein_brain._log). Level from DASEIN_TRAINER_LOG (default INFO);
DASEIN_TRAINER_LOG_JSON=1 for JSON lines. UTC timestamps. Loggers live under
"dasein_trainer" and propagate (pytest caplog sees every record).

No print() on the pipeline stages: tests capture stage banners via caplog, and
the retrain orchestrator's per-stage lines (wall-ms, artifact paths, row
counts) are the operational record of a retrain run.
"""
from __future__ import annotations

import json
import logging
import os
import time

_ROOT = "dasein_trainer"
_FMT = "%(asctime)s %(levelname)s %(name)s %(message)s"


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
    if getattr(root, "_dasein_configured", False):
        return root
    level = os.environ.get("DASEIN_TRAINER_LOG", "INFO").upper()
    root.setLevel(getattr(logging, level, logging.INFO))
    handler = logging.StreamHandler()
    if os.environ.get("DASEIN_TRAINER_LOG_JSON") == "1":
        handler.setFormatter(_JsonFormatter(_FMT))
    else:
        fmt = logging.Formatter(_FMT)
        fmt.converter = time.gmtime
        handler.setFormatter(fmt)
    root.addHandler(handler)
    root.propagate = True
    root._dasein_configured = True                # type: ignore[attr-defined]
    return root


def get_logger(name: str) -> logging.Logger:
    _configure()
    return logging.getLogger(f"{_ROOT}.{name}")
