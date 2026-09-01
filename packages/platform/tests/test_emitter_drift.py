"""The proxy's write_ledger is the third party the schema<->mirror drift
guard cannot see: 2026-09-02 it shipped `tools_served_sha8` while schema and
LedgerRow were both (consistently) missing it, so every curated row 422'd on
extra="forbid" and savings silently stopped reaching the dashboard. Guard the
emitter itself: every top-level key write_ledger can insert must be declared
in the ledger schema (and therefore, via the existing field-set test, in the
mirror). Regex over the Rust source is crude but exactly as strict as the
failure mode requires — a key emitted only conditionally still 422s a row."""

import json
import re
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]


def test_write_ledger_keys_are_all_declared_in_schema() -> None:
    src = (REPO / "packages/proxy/src/server.rs").read_text()
    m = re.search(r"fn write_ledger\(.*?\n\}\n", src, re.S)
    assert m, "write_ledger not found — update this test's anchor"
    body = m.group(0)
    emitted = set(re.findall(r'o\.insert\("([a-z0-9_]+)"', body))
    emitted |= set(re.findall(r'"([a-z0-9_]+)":', body))
    schema = json.loads(
        (REPO / "packages/contracts/schemas/savings-ledger.schema.json").read_text()
    )
    undeclared = emitted - set(schema["properties"])
    assert not undeclared, (
        f"write_ledger emits {sorted(undeclared)} but the ledger schema does not "
        "declare them — the platform mirror rejects such rows with 422 "
        '(extra="forbid"). Add the fields to the schema AND models.py::LedgerRow.'
    )
