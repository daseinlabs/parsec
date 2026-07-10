"""Embedding client with write-time caching (§6.5: compute once, cache, never re-embed).

Backends:
  - "dasein": the bge-large-en-v1.5 encoder we already run on GPU in the dasein cluster (1024-d,
              native batching, no Vertex). The production embedder.
  - "vertex": gemini-embedding (CODE_RETRIEVAL), 768-d — legacy / fallback.
  - "hash":   deterministic pseudo-embeddings (no network). Used by unit tests and offline
              dev so the suite never spends or requires ADC.

Past traces are immutable, so their embeddings are cached forever (content-addressed).
"""

from __future__ import annotations

import hashlib
import math
from collections.abc import Sequence

Vector = list[float]


def _hash_embed(text: str, dim: int) -> Vector:
    """Deterministic unit-norm pseudo-embedding from a content hash. Stable across runs."""
    out: list[float] = []
    counter = 0
    while len(out) < dim:
        h = hashlib.sha256(f"{counter}:{text}".encode()).digest()
        for i in range(0, len(h), 4):
            out.append(int.from_bytes(h[i:i + 4], "big") / 2**32 - 0.5)
            if len(out) >= dim:
                break
        counter += 1
    norm = math.sqrt(sum(x * x for x in out)) or 1.0
    return [x / norm for x in out]


class EmbeddingClient:
    def __init__(self, cfg: dict | None = None, backend: str = "hash", dim: int = 768):
        self.cfg = cfg or {}
        self.backend = backend
        self.dim = (cfg or {}).get("models", {}).get("embedder", {}).get("dim", dim) \
            if cfg else dim
        self._cache: dict[str, Vector] = {}
        self._vertex = None
        self._dasein = None
        self.calls = 0          # network calls actually made (overhead accounting)

    def _key(self, text: str, as_query: bool) -> str:
        return f"{'q' if as_query else 'd'}:{hashlib.sha1(text.encode()).hexdigest()}"

    def embed(self, texts: Sequence[str], as_query: bool = True) -> list[Vector]:
        missing = [t for t in texts if self._key(t, as_query) not in self._cache]
        if missing:
            if self.backend == "dasein":
                if self._dasein is None:
                    from .dasein_embed import DaseinEmbedClient
                    self._dasein = DaseinEmbedClient(self.cfg)
                vecs = self._dasein.embed(list(missing), as_query=as_query)
                self.calls += 1
            # (vertex branch deleted in the vendored copy: legacy 768-d backend, never used here)
            else:
                vecs = [_hash_embed(t, self.dim) for t in missing]
            for t, v in zip(missing, vecs):
                self._cache[self._key(t, as_query)] = v
        return [self._cache[self._key(t, as_query)] for t in texts]

    def embed_one(self, text: str, as_query: bool = True) -> Vector:
        return self.embed([text], as_query=as_query)[0]
