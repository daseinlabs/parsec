#!/usr/bin/env python3
"""Export BAAI/bge-large-en-v1.5 to ONNX for the client-side embedder.

Produces ~/.parsec/models/bge-large-onnx/ containing model.onnx (fp32,
~1.3GB) + tokenizer.json, the layout engine::embed::OnnxEmbedder loads.
Optionally also model.int8.onnx (dynamic quantization, ~330MB) — a
download-UX candidate that must NOT ship until it passes the parity gate
on scores, not just cosine (see docs/brain-serving-v0.md).

Run inside a throwaway venv (optimum+transformers must not enter
packages/brain/.venv):

    uv venv /tmp/bge-export-venv --python 3.12
    uv pip install --python /tmp/bge-export-venv/bin/python \
        'optimum[onnxruntime]' 'transformers<5'
    /tmp/bge-export-venv/bin/python scripts/embed/export_bge_onnx.py [--int8]
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

MODEL_ID = "BAAI/bge-large-en-v1.5"
DEFAULT_OUT = Path.home() / ".parsec" / "models" / "bge-large-onnx"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--int8", action="store_true",
                    help="also emit model.int8.onnx via dynamic quantization")
    args = ap.parse_args()

    from optimum.onnxruntime import ORTModelForFeatureExtraction
    from transformers import AutoTokenizer

    args.out.mkdir(parents=True, exist_ok=True)

    print(f"exporting {MODEL_ID} -> {args.out} (fp32)", flush=True)
    model = ORTModelForFeatureExtraction.from_pretrained(MODEL_ID, export=True)
    model.save_pretrained(args.out)
    # tokenizer.json is the single-file HF tokenizer the Rust `tokenizers`
    # crate loads directly; save_pretrained writes it alongside the vocab.
    AutoTokenizer.from_pretrained(MODEL_ID).save_pretrained(args.out)

    if args.int8:
        from onnxruntime.quantization import QuantType, quantize_dynamic

        src = args.out / "model.onnx"
        dst = args.out / "model.int8.onnx"
        print(f"quantizing -> {dst} (int8 dynamic)", flush=True)
        quantize_dynamic(str(src), str(dst), weight_type=QuantType.QInt8)

    for p in sorted(args.out.iterdir()):
        print(f"  {p.name}\t{p.stat().st_size:,} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
