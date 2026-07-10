#!/usr/bin/env python3
"""Convert the changeprone sidecar pickle to committed JSON for the client.

The plugin ships JSON, never pickle: models/changeprone.pkl {cnt, total, held}
-> packages/engine/parity/fixtures/changeprone.json {cnt, total} (readout col
42 = cnt[basename]/total; `held` is eval bookkeeping, unused at serve). Keys
are sorted for a stable committed artifact; `total` stays raw — the loaders
(engine::readout::Changeprone::from_json, dasein_brain.bundle) clamp to >= 1.

  packages/brain/.venv/bin/python scripts/changeprone_to_json.py [pkl] [json]
"""
import json
import os
import pickle
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, ".."))
PKL = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    REPO, "packages", "brain", "models", "changeprone.pkl")
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(
    REPO, "packages", "engine", "parity", "fixtures", "changeprone.json")


def main():
    d = pickle.load(open(PKL, "rb"))
    # bundle.py parity: dict {cnt, total} or legacy (cnt, total) tuple.
    cnt = d["cnt"] if isinstance(d, dict) else d[0]
    total = d.get("total", 1) if isinstance(d, dict) else d[1]
    out = {"cnt": {str(k): int(v) for k, v in sorted(cnt.items())}, "total": int(total)}
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(out, f, ensure_ascii=False, indent=1, sort_keys=True)
        f.write("\n")
    print(f"wrote {OUT}: {len(out['cnt'])} files, total={out['total']}")


if __name__ == "__main__":
    main()
