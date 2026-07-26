#!/usr/bin/env python3
"""Generate hash-embedder parity fixtures from the vendored Python reference.

Runs parsec_brain.vendored.embedding._hash_embed over a battery of texts and
dims, storing each output element as the u32 bit pattern of its float32
narrowing. The Rust suite (tests/parity_embed.rs) replays the battery through
engine::embed::hash_embed and asserts bit equality — the §7b parity contract
for the deterministic test backend.

Usage (any python that can import the vendored module, e.g. the brain venv):
    packages/brain/.venv/bin/python parity/gen_embed_fixtures.py [out.json]
"""
import json
import os
import struct
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    os.path.dirname(__file__), "fixtures", "hash_embed.json")

sys.path.insert(0, os.path.abspath(os.path.join(
    os.path.dirname(__file__), "..", "..", "brain", "src")))

from parsec_brain.vendored.embedding import _hash_embed  # noqa: E402

CASES = [
    # (text, dim) — dims chosen to hit the digest boundaries: one sha256
    # digest yields 8 u32s, so 8 = exactly one digest, 9 = counter extension,
    # 1024 = 128 digests (the prod dim), 768 = the vendored default.
    ("", 1024),
    ("hello world", 1024),
    ("def foo(): return 1", 1024),
    ("café ☕ 日本語 — résumé", 1024),
    ("x" * 3000, 1024),
    ("hello\nworld\ttabs and  spaces ", 768),
    ("hello world", 8),
    ("hello world", 9),
    ("x", 4),
]


def f32_bits(x: float) -> int:
    return struct.unpack("<I", struct.pack("<f", x))[0]


def main() -> None:
    cases = []
    for text, dim in CASES:
        vec = _hash_embed(text, dim)
        cases.append({"text": text, "dim": dim,
                      "bits": [f32_bits(v) for v in vec]})
    with open(OUT, "w") as f:
        json.dump({"cases": cases}, f, indent=1)
        f.write("\n")
    print(f"wrote {len(cases)} cases -> {OUT}")


if __name__ == "__main__":
    main()
