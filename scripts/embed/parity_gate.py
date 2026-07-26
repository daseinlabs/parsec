#!/usr/bin/env python3
"""Vector-parity gate for the local bge-large ONNX export (v1 embedder).

Embeds a diverse text battery via:
  (a) local onnxruntime on ~/.parsec/models/bge-large-onnx (fp32 reference),
  (b) the real dasein-embed service, if reachable at PARSEC_EMBED_URL
      (kubectl port-forward svc/dasein-embed 18080:80),
  (c) optionally --rust-vectors: the OnnxEmbedder output from
      `cargo run -p parsec-engine --features onnx --example onnx_battery`,
  (d) optionally --int8: the model.int8.onnx dynamic-quantization variant.

Reports per-text cosine and max-abs-diff against (a). PASS requires
cosine >= 0.9999 for every text on the fp32 comparisons (b) and (c);
the int8 comparison is informational — quantization must additionally be
validated on SCORES, not cosine, before it may ship (brain-serving-v0.md).

Text prep mirrors serve time (embed-protocol.md §3): NO bge query prefix,
chunk texts clipped to 2000 chars / head strings to 240 BEFORE embedding;
tokenizer truncation 512; CLS pooling; L2 norm.

Run with the export venv (needs onnxruntime, tokenizers, numpy, requests):
    <venv>/bin/python scripts/embed/parity_gate.py [--int8] \
        [--battery-out battery.json] [--rust-vectors rust.json]
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

import numpy as np

MODEL_ID = "bge-large-en-v1.5"
DEFAULT_MODEL_DIR = Path.home() / ".parsec" / "models" / "bge-large-onnx"
PASS_COSINE = 0.9999

LONG_PROSE = (
    "The port boundary splits the decision engine from the scorer. " * 80
)  # > 512 tokens: exercises tokenizer truncation

CHUNK_3000 = (
    "def normalize(vectors):\n    # L2 normalize rows in place\n"
    "    for i, v in enumerate(vectors):\n        n = sum(x * x for x in v) ** 0.5\n"
    "        vectors[i] = [x / (n or 1.0) for x in v]\n    return vectors\n"
) * 12  # ~3.4k chars pre-clip

HEAD_LONG = "\n".join(f"src/module_{i}.py:{i * 7}: def handler_{i}(request):" for i in range(12))


def battery() -> list[str]:
    """Diverse texts, already serve-prepped (chunk[:2000], head[:240])."""
    return [
        "hello world",
        "def foo(): return 1",
        'fn serve(&mut self, messages: &[Value]) -> Result<Vec<Value>, FreezeError> { todo!() }',
        "café ☕ 日本語のテキスト — résumé naïve Ω≈ç√∫",
        "# Freezing\n\nServed bytes are a **pure function** of (prefix, checkpoint, config).\n- no wall clock\n- no RNG",
        '{"model_id": "bge-large-en-v1.5", "texts": ["a", "b"], "count": 2}',
        LONG_PROSE,
        CHUNK_3000[:2000],
        HEAD_LONG[:240],
        "   \t\n  spaced\t\tout\n\n\nwords   ",
        "$ cargo test -p parsec-engine\nrunning 13 tests\ntest embed::tests::hash_embedder_empty_input ... ok",
        "Traceback (most recent call last):\n  File \"app.py\", line 1, in <module>\nZeroDivisionError: division by zero",
        "x",
        "",
    ]


def load_tokenizer(model_dir: Path):
    # tokenizers.Tokenizer.from_file mirrors the Rust `tokenizers` crate
    # exactly (same library); truncation 512 + batch-longest padding.
    from tokenizers import Tokenizer

    tok = Tokenizer.from_file(str(model_dir / "tokenizer.json"))
    tok.enable_truncation(max_length=512)
    tok.enable_padding()
    return tok


def onnx_embed(model_dir: Path, model_file: str, texts: list[str]) -> np.ndarray:
    import onnxruntime as rt

    sess = rt.InferenceSession(str(model_dir / model_file),
                               providers=["CPUExecutionProvider"])
    tok = load_tokenizer(model_dir)
    encs = tok.encode_batch(texts)
    feed = {
        "input_ids": np.array([e.ids for e in encs], dtype=np.int64),
        "attention_mask": np.array([e.attention_mask for e in encs], dtype=np.int64),
        "token_type_ids": np.array([e.type_ids for e in encs], dtype=np.int64),
    }
    feed = {k: v for k, v in feed.items()
            if k in {i.name for i in sess.get_inputs()}}
    (last_hidden,) = sess.run(["last_hidden_state"], feed)
    cls = last_hidden[:, 0, :].astype(np.float32)  # CLS pooling
    norms = np.linalg.norm(cls, axis=1, keepdims=True)
    norms[norms == 0.0] = 1.0
    return cls / norms


def remote_embed(url: str, texts: list[str]) -> np.ndarray | None:
    import requests

    try:
        # 360s rides the GPU scale-from-zero cold start (proxy ready-wait 300s)
        resp = requests.post(url, json={"model_id": MODEL_ID, "texts": texts},
                             timeout=360)
        resp.raise_for_status()
    except Exception as e:  # noqa: BLE001 — gate reports, caller decides
        print(f"  remote UNREACHABLE ({url}): {e}")
        return None
    return np.asarray(resp.json()["vectors"], dtype=np.float32)


def compare(name: str, ref: np.ndarray, got: np.ndarray, texts: list[str],
            threshold: float | None) -> bool:
    print(f"\n== {name} (n={len(texts)}, dim={ref.shape[1]}) ==")
    print(f"{'cosine':>10}  {'max|diff|':>10}  text")
    cosines = []
    for i, text in enumerate(texts):
        denom = np.linalg.norm(ref[i]) * np.linalg.norm(got[i])
        cos = float(ref[i] @ got[i] / denom) if denom else 1.0
        mad = float(np.max(np.abs(ref[i] - got[i])))
        cosines.append(cos)
        label = repr(text[:48]) + ("…" if len(text) > 48 else "")
        print(f"{cos:10.7f}  {mad:10.2e}  {label}")
    worst = min(cosines)
    overall_mad = float(np.max(np.abs(ref - got)))
    print(f"  min cosine {worst:.7f}, overall max|diff| {overall_mad:.2e}")
    if threshold is None:
        print("  (informational — no threshold)")
        return True
    ok = worst >= threshold
    print(f"  {'PASS' if ok else 'FAIL'} (threshold cosine >= {threshold})")
    return ok


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--model-dir", type=Path, default=DEFAULT_MODEL_DIR)
    ap.add_argument("--remote-url",
                    default=os.environ.get("PARSEC_EMBED_URL",
                                           "http://127.0.0.1:18080/embed"))
    ap.add_argument("--skip-remote", action="store_true")
    ap.add_argument("--int8", action="store_true",
                    help="also compare model.int8.onnx (informational)")
    ap.add_argument("--battery-out", type=Path,
                    help="write the battery texts as JSON (for onnx_battery.rs)")
    ap.add_argument("--rust-vectors", type=Path,
                    help='{"vectors": ...} from the onnx_battery example')
    args = ap.parse_args()

    texts = battery()
    if args.battery_out:
        args.battery_out.write_text(json.dumps(texts))
        print(f"battery -> {args.battery_out}")

    print(f"(a) local onnxruntime fp32: {args.model_dir}/model.onnx")
    ref = onnx_embed(args.model_dir, "model.onnx", texts)

    ok = True
    if not args.skip_remote:
        print(f"(b) remote dasein-embed: {args.remote_url}")
        remote = remote_embed(args.remote_url, texts)
        if remote is None:
            ok = False
        else:
            ok &= compare("remote service vs local fp32 onnx", ref, remote,
                          texts, PASS_COSINE)

    if args.rust_vectors:
        rust = np.asarray(
            json.loads(args.rust_vectors.read_text())["vectors"],
            dtype=np.float32)
        ok &= compare("rust OnnxEmbedder vs python onnxruntime (same export)",
                      ref, rust, texts, PASS_COSINE)

    if args.int8:
        int8 = onnx_embed(args.model_dir, "model.int8.onnx", texts)
        compare("int8 dynamic-quant vs fp32 (informational)", ref, int8,
                texts, None)

    print(f"\nGATE: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
