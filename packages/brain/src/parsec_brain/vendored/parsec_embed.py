"""Parsec in-cluster embedding client.

Calls the embedding model we ALREADY run on GPU in the parsec cluster (the `dasein-embed` API pod,
which fronts the `bge-large-en-v1.5` SentenceTransformer encoder on the A100/L4 embed node pools and
owns GPU autoscale/warm). No Vertex, native batching (the pod micro-batches a whole list in one
request), the GPU we already pay for.

PARSEC-PATCH: transport swapped for the brain service. The reference client resolved the GPU pod's
IP via the k8s API (ADC token, TLS-unverified master dial) and hit the binary /encode_bin — viable
only from the bench VPC. The brain runs IN-cluster (or against a kubectl port-forward in dev), so it
posts JSON to the CPU proxy Service instead: POST PARSEC_EMBED_URL (default
http://dasein-embed.default.svc.cluster.local/embed) with {"model_id", "texts"} -> {"vectors":
[[...1024 f32...], ...]} (embed-protocol §2a). The proxy owns GPU scale-from-zero; the retry loop
below rides cold starts exactly as the reference did. Batching/retry semantics are unchanged.
"""

from __future__ import annotations

import json
import os
import time
import urllib.request
from collections.abc import Sequence

import numpy as np

# PARSEC-PATCH: JSON proxy endpoint replaces k8s pod discovery + binary /encode_bin.
EMBED_URL = os.environ.get("PARSEC_EMBED_URL",
                           "http://dasein-embed.default.svc.cluster.local/embed")
MODEL_ID = os.environ.get("PARSEC_EMBED_MODEL", "bge-large-en-v1.5")
# one batch per request, serially: the encoder is a single GPU that coalesces and runs one batch at
# a time, so concurrency buys ~1.2x and just adds JSON/transport pressure.
BATCH = int(os.environ.get("PARSEC_EMBED_BATCH", "512"))   # 512 amortizes the GPU better than 256 on
#                  long (~512-tok) chunks: ~134 vs ~92 texts/s. Throughput is sequence-length-bound
#                  (short texts hit ~1400/s); for full-length build chunks ~85-134 texts/s is GPU-bound.


class ParsecEmbedClient:
    """Thin client; same .embed(texts, as_query) surface as VertexClient."""

    def __init__(self, cfg: dict | None = None):
        self.cfg = cfg or {}
        emb = (self.cfg.get("models", {}) or {}).get("embedder", {}) or {}
        self.model_id = emb.get("model", MODEL_ID) if emb.get("backend") == "dasein" else MODEL_ID

    def _embed_batch(self, texts: list[str]) -> np.ndarray:
        # PARSEC-PATCH: JSON /embed on the CPU proxy (embed-protocol §2a) — response
        # {"vectors": [[f32 x dim] ...], "model_id", "count", "token_count"}.
        body = json.dumps({"model_id": self.model_id, "texts": texts}).encode("utf-8")
        req = urllib.request.Request(EMBED_URL, data=body, method="POST")
        req.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(req, timeout=300) as r:     # generous: rides GPU cold-start/warm
            d = json.loads(r.read())
        return np.asarray(d["vectors"], dtype=np.float32).reshape(len(texts), -1)

    def _batch_with_retry(self, chunk: list[str]) -> np.ndarray:
        for attempt in range(7):
            try:
                return self._embed_batch(chunk)
            except Exception:
                if attempt == 6:
                    raise
                time.sleep(min(30.0, 2.0 ** attempt))            # ride scale-from-zero / load spikes
        raise RuntimeError("unreachable")

    def embed(self, texts: Sequence[str], as_query: bool = True, workers: int | None = None
              ) -> list[list[float]]:
        # One batch per request, serially (see BATCH note): the single-GPU encoder coalesces —
        # concurrency would only add load.
        texts = list(texts)
        out: list[list[float]] = []
        for i in range(0, len(texts), BATCH):
            out.extend(self._batch_with_retry(texts[i:i + BATCH]))   # extend with the f32 rows
        return out
