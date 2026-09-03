"""Hermetic tests for the bounded embed caches (marked no_ckpt: no curator
checkpoint, no encoder weights — the hash backend and fakes only).

Two invariants (embed_cache.py):
- eviction must NEVER remove a key of the in-flight batch: callers read
  `cache[t]` directly right after `_embed(...)`, and in the assemble paths a
  missing entry silently becomes a zero vector — a wrong score, not a crash;
- duplicate texts inside one batch reach the encoder once, not once per copy.
"""

from __future__ import annotations

import pytest

from parsec_brain.embed_cache import EmbedCache, embed_cache_max
from parsec_brain.vendored import embedding as vend

pytestmark = pytest.mark.no_ckpt


def fake_embed(cache: EmbedCache, texts: list[str]) -> list:
    """The scorer._embed protocol against EmbedCache, with a stub encoder."""
    miss = cache.touch_batch(texts)
    cache.insert_batch(((t, [float(len(t))]) for t in miss), set(texts))
    return [cache[t] for t in texts]


def test_embed_cache_max_env(monkeypatch):
    monkeypatch.delenv("PARSEC_EMBED_CACHE_MAX", raising=False)
    assert embed_cache_max() == 100_000
    monkeypatch.setenv("PARSEC_EMBED_CACHE_MAX", "off")
    assert embed_cache_max() is None
    monkeypatch.setenv("PARSEC_EMBED_CACHE_MAX", "0")
    assert embed_cache_max() is None
    monkeypatch.setenv("PARSEC_EMBED_CACHE_MAX", "250")
    assert embed_cache_max() == 250
    monkeypatch.setenv("PARSEC_EMBED_CACHE_MAX", "junk")
    assert embed_cache_max() == 100_000


def test_lru_evicts_oldest_untouched():
    c = EmbedCache(max_entries=3)
    fake_embed(c, ["a", "b", "c"])
    fake_embed(c, ["a"])          # bump a: recency order is now b, c, a
    fake_embed(c, ["d"])          # over cap -> evict b (oldest, unpinned)
    assert "b" not in c
    assert set(c) == {"a", "c", "d"}
    assert c.evicted == 1


def test_in_flight_batch_is_never_evicted():
    c = EmbedCache(max_entries=2)
    got = fake_embed(c, ["a", "b", "c", "d", "e"])  # batch larger than cap
    # Every batch key must be readable right after — transiently over cap.
    assert len(got) == 5 and len(c) == 5
    fake_embed(c, ["x"])          # next batch shrinks it back toward the cap
    assert len(c) == 2 and "x" in c


def test_batch_dedupe_and_counters():
    c = EmbedCache(max_entries=None)
    miss = c.touch_batch(["a", "a", "b"])
    assert miss == ["a", "b"]     # one encoder row per unique text
    assert c.misses == 2          # not 3
    c.insert_batch(((t, [0.0]) for t in miss), {"a", "b"})
    c.touch_batch(["a", "b"])
    assert c.hits == 2


def counting_hash_embed(counter):
    real = vend._hash_embed

    def wrapped(t, dim):
        counter.append(t)
        return real(t, dim)

    return wrapped


def test_embedding_client_dedupes_batch(monkeypatch):
    calls: list[str] = []
    monkeypatch.setattr(vend, "_hash_embed", counting_hash_embed(calls))
    cl = vend.EmbeddingClient(backend="hash", dim=8)
    v = cl.embed(["x", "x", "y"], as_query=False)
    assert len(v) == 3 and calls == ["x", "y"]
    cl.embed(["x"], as_query=False)               # warm hit: no new encode
    assert calls == ["x", "y"]
    # as_query variants stay separately keyed (q:/d: prefixes).
    cl.embed(["x"], as_query=True)
    assert calls == ["x", "y", "x"]


def test_embedding_client_bounded_without_batch_loss(monkeypatch):
    calls: list[str] = []
    monkeypatch.setattr(vend, "_hash_embed", counting_hash_embed(calls))
    cl = vend.EmbeddingClient(backend="hash", dim=8, max_entries=2)
    for t in ["a", "b", "c"]:
        cl.embed([t], as_query=False)
    assert len(cl._cache) <= 2
    # A batch mixing an evicted key with fresh ones: everything comes back
    # (re-embed on miss), no KeyError, and the batch itself survives intact.
    v = cl.embed(["a", "p", "q", "r"], as_query=False)
    assert len(v) == 4
    assert cl.embed(["r"], as_query=False)[0] == v[3]


def test_scorer_embed_protocol_dedupes():
    torch = pytest.importorskip("torch")  # scorer.py imports it at module load
    del torch
    from types import SimpleNamespace

    from parsec_brain.scorer import TraceScorer

    batches: list[list[str]] = []

    class FakeEmbedder:
        def embed(self, texts, as_query=True):
            batches.append(list(texts))
            return [[float(len(t))] for t in texts]

    fake = SimpleNamespace(cache=EmbedCache(None), embedder=FakeEmbedder(), _embed_ms=0.0)
    out = TraceScorer._embed(fake, ["t", "t", "u"])
    assert len(out) == 3 and batches == [["t", "u"]]
    out2 = TraceScorer._embed(fake, ["t", "u", "v"])
    assert len(out2) == 3 and batches[-1] == ["v"]  # warm keys never re-encode
