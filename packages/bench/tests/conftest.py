"""Shared test helpers for dasein-bench."""

from __future__ import annotations

from pathlib import Path

import pytest

FIXTURES = Path(__file__).parent / "fixtures"


@pytest.fixture
def replay_fixture_path() -> Path:
    return FIXTURES / "replay_2turn.json"
