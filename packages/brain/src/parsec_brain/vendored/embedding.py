"""Embedding client with write-time caching (§6.5: compute once, cache, never re-embed).

Backends:
  - "local":  the bge-large-en-v1.5 encoder loaded IN-PROCESS (local_embed.py) — same model as
              "parsec", no HTTP hop. The Cloud Run deploy target (one GPU holds embedder + GNN).
  - "parsec": the same bge-large-en-v1.5 encoder, but called over HTTP at a GPU pod we run in the
              parsec cluster (1024-d, native batching, no Vertex). The current production path.
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
    def __init__(self, cfg: dict | None = None, backend: str = "hash", dim: int = 768,
                 max_entries: int | None = None):
        self.cfg = cfg or {}
        self.backend = backend
        self.dim = (cfg or {}).get("models", {}).get("embedder", {}).get("dim", dim) \
            if cfg else dim
        # Bounded since the 2026-09 latency work (None = the original
        # cache-forever behavior). Kept self-contained — same batch-pinning
        # policy as parsec_brain.embed_cache, inlined so the vendored copy
        # imports nothing above it.
        self._cache: dict[str, Vector] = {}
        self._max_entries = max_entries
        self._vertex = None
        self._parsec = None
        self._local = None
        self.calls = 0          # network calls actually made (overhead accounting)

    def _key(self, text: str, as_query: bool) -> str:
        return f"{'q' if as_query else 'd'}:{hashlib.sha1(text.encode()).hexdigest()}"

    def embed(self, texts: Sequence[str], as_query: bool = True) -> list[Vector]:
        # Bump hits to most-recent (dict preserves insertion order; pop+set
        # is the O(1) re-append) and DEDUPE the miss list — duplicates in one
        # batch used to hit the encoder once per copy.
        missing = []
        for t in dict.fromkeys(texts):
            k = self._key(t, as_query)
            if k in self._cache:
                self._cache[k] = self._cache.pop(k)
            else:
                missing.append(t)
        if missing:
            if self.backend == "local":
                if self._local is None:
                    from .local_embed import LocalEmbedClient
                    self._local = LocalEmbedClient(self.cfg)
                vecs = self._local.embed(list(missing), as_query=as_query)
                self.calls += 1
            elif self.backend == "dasein":
                if self._parsec is None:
                    from .parsec_embed import ParsecEmbedClient
                    self._parsec = ParsecEmbedClient(self.cfg)
                vecs = self._parsec.embed(list(missing), as_query=as_query)
                self.calls += 1
            # (vertex branch deleted in the vendored copy: legacy 768-d backend, never used here)
            else:
                vecs = [_hash_embed(t, self.dim) for t in missing]
            for t, v in zip(missing, vecs):
                self._cache[self._key(t, as_query)] = v
        # Evict oldest-first down to the cap, never touching this batch's
        # keys (all at the recent end after the bump/insert above) — the
        # return below reads every one of them.
        if self._max_entries is not None:
            batch = {self._key(t, as_query) for t in texts}
            while len(self._cache) > self._max_entries:
                oldest = next(iter(self._cache))
                if oldest in batch:
                    break
                del self._cache[oldest]
        return [self._cache[self._key(t, as_query)] for t in texts]

    def embed_one(self, text: str, as_query: bool = True) -> Vector:
        return self.embed([text], as_query=as_query)[0]
