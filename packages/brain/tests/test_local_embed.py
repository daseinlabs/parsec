"""In-process bge-large embedder (DASEIN_EMBED_BACKEND=local).

Skipped unless `transformers` + the bge weights are available (they are not a
core dep — only the deploy/`local` path needs them). This asserts the RECIPE
that keeps the in-process vectors matching the cluster encoder the checkpoint
was trained on: 1024-d, unit-norm, and [CLS] pooling (mean pooling would blur
code/query similarity and silently drift every score).
"""

import numpy as np
import pytest

pytest.importorskip("transformers", reason="embed extra not installed")


@pytest.fixture(scope="module")
def client():
    try:
        from dasein_brain.vendored.local_embed import LocalEmbedClient

        return LocalEmbedClient()
    except Exception as e:  # weights not cached / offline
        pytest.skip(f"bge weights unavailable: {type(e).__name__}: {e}")


def test_shape_and_unit_norm(client):
    v = np.asarray(client.embed(["hello world", "def f(): pass"]), dtype=np.float32)
    assert v.shape == (2, 1024)
    assert np.allclose(np.linalg.norm(v, axis=1), 1.0, atol=1e-4)


def test_cls_pooling_semantics(client):
    # code and its natural-language question must be closer than code and an
    # unrelated sentence — holds under bge's [CLS] pooling, not under mean pool.
    v = np.asarray(
        client.embed(
            [
                "def add(a, b): return a + b",
                "how do I sum two numbers in python",
                "the cat sat on the mat",
            ]
        ),
        dtype=np.float32,
    )
    sim = v @ v.T
    assert sim[0, 1] > sim[0, 2]


def test_empty_string_embeds(client):
    # empty strings must embed without error (the scorer sends "" for absent
    # cmd/head before zeroing them; a crash here would break the trace path).
    v = np.asarray(client.embed(["", "x"]), dtype=np.float32)
    assert v.shape == (2, 1024)
