"""Bounded LRU embed cache (2026-09 scorer-latency work).

Both embedding layers — TraceScorer.cache (exact text) and the vendored
EmbeddingClient._cache (sha1-keyed) — were unbounded dicts that grew for the
life of the replica. They alias the SAME vector objects, so bounding only one
frees no memory; both layers evict (the vendored layer inlines the same
policy to stay self-contained).

Safety invariant this class is built around: every caller that reads
`cache[t]` DIRECTLY (scorer trace/tools/rule/gate paths, app._v2_embed) does
so immediately after an `_embed(...)` call covering those exact keys, and a
missing entry there silently becomes a zero vector — a wrong score, not a
crash. So eviction must never remove a key of the in-flight batch:
`touch_batch` bumps every present key to most-recent and `insert_batch`
appends the misses, which puts the WHOLE batch at the recent end; eviction
pops from the old end and stops at the first pinned key. The cache may
transiently exceed the cap when a single batch is larger than it.

Purity: embeddings are content-addressed and deterministic, so eviction can
never change a served score — it only costs a re-embed. Replicas stay
stateless and round-robin-safe (DIRECTION.md).
"""

from __future__ import annotations

import os
from collections import OrderedDict


def embed_cache_max() -> int | None:
    """PARSEC_EMBED_CACHE_MAX: entries per layer; unset/junk -> 100_000
    (~0.5-1.5 GiB of 1024-d vectors incl. key text), 0/off -> unbounded
    (the pre-2026-09 behavior)."""
    raw = os.environ.get("PARSEC_EMBED_CACHE_MAX", "").strip().lower()
    if raw == "off" or raw == "0":
        return None
    try:
        n = int(raw)
        return n if n > 0 else None
    except ValueError:
        return 100_000


class EmbedCache(OrderedDict):
    """text -> vector, recency-ordered, with per-request hit/miss/evict
    telemetry (reset by score_trace next to _embed_ms)."""

    def __init__(self, max_entries: int | None = None):
        super().__init__()
        self.max_entries = max_entries
        self.hits = 0
        self.misses = 0
        self.evicted = 0

    def reset_stats(self) -> None:
        self.hits = 0
        self.misses = 0
        self.evicted = 0

    def touch_batch(self, texts) -> list:
        """Bump every present key to most-recent; return the misses,
        first-seen order, DEDUPED (duplicate texts in one batch used to hit
        the encoder once per copy)."""
        miss = []
        seen = set()
        for t in texts:
            if t in seen:
                continue
            seen.add(t)
            if t in self:
                self.move_to_end(t)
                self.hits += 1
            else:
                miss.append(t)
                self.misses += 1
        return miss

    def insert_batch(self, pairs, pinned) -> None:
        """Insert freshly embedded (text, vec) pairs, then evict oldest-first
        down to max_entries — stopping at the first key in `pinned` (the
        current batch, which after touch_batch is exactly the recent tail)."""
        for t, v in pairs:
            self[t] = v
            self.move_to_end(t)
        if self.max_entries is None:
            return
        while len(self) > self.max_entries:
            oldest = next(iter(self))
            if oldest in pinned:
                break
            del self[oldest]
            self.evicted += 1
