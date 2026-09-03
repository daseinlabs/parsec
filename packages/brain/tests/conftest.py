"""Hermetic test env: hash embed backend (no cluster), src/ on the path, skip without the ckpt.

The 36MB curator_v4_prod.pt stays OUT of the repo; tests load it from PARSEC_CKPT
(default ~/.parsec/brain/curator_v4_prod.pt) and skip loudly when it is absent.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path

os.environ.setdefault("PARSEC_EMBED_BACKEND", "hash")   # before any app/scorer construction
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

import pytest  # noqa: E402

CKPT = Path(os.environ.get("PARSEC_CKPT", "~/.parsec/brain/curator_v4_prod.pt")).expanduser()


def pytest_configure(config):
    config.addinivalue_line(
        "markers", "no_ckpt: hermetic test that runs without the curator ckpt"
    )


def pytest_collection_modifyitems(config, items):
    if not CKPT.is_file():
        skip = pytest.mark.skip(reason=f"curator ckpt not found at {CKPT} (set PARSEC_CKPT)")
        for item in items:
            if item.get_closest_marker("no_ckpt") is None:
                item.add_marker(skip)
