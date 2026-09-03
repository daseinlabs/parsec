"""In-process bge-large-en-v1.5 embedder (PARSEC_EMBED_BACKEND=local).

Collapses the dasein-embed HTTP hop: loads the SAME encoder the cluster fronts
(BAAI/bge-large-en-v1.5, 1024-d) INTO this process, so scoring is a single
proxy -> brain -> proxy round trip with no external embed service. This is the
Cloud Run deploy target — one L4 instance holds bge-large + the R-GCN.

Parity is the point: the checkpoint was trained on the cluster encoder's
vectors, so this must reproduce them. The recipe is pinned to bge's reference
usage (model card + the cluster's SentenceTransformer config):

  * [CLS]-token pooling — bge uses the first token's hidden state, NOT mean
    pooling (the SentenceTransformer default). Getting this wrong silently
    drifts every score.
  * L2 normalize to unit vectors (dupcos cosine sims assume it).
  * max 512 tokens, truncated (the scorer already caps text to [:2000] chars
    upstream; 512 tokens is the encoder's own limit).
  * NO query instruction. The cluster JSON endpoint (parsec_embed.py) posts
    raw texts with no as_query flag, so it embeds everything as documents;
    `as_query` is accepted for interface symmetry and deliberately ignored.

Determinism note (matters for Cloud Run's round-robin-safe claim): CPU is
bit-deterministic; GPU kernels are not strictly so across instances. The
scorer quantizes to the 1e-6 grid which absorbs sub-ulp jitter, but a chunk
scoring exactly at tau could flip keep/cut between replicas — validate with a
two-instance parity test before trusting the round-robin claim on GPU.
"""

from __future__ import annotations

import os
from collections.abc import Sequence

# HF id or a baked local dir (Cloud Run sets PARSEC_EMBED_MODEL_DIR=/embed-model).
_MODEL = os.environ.get("PARSEC_EMBED_MODEL_DIR", "BAAI/bge-large-en-v1.5")
_MAXTOK = int(os.environ.get("PARSEC_EMBED_MAXTOK", "512"))
_BATCH = int(os.environ.get("PARSEC_EMBED_BATCH", "512"))


class LocalEmbedClient:
    """In-process encoder; same .embed(texts, as_query) surface as ParsecEmbedClient."""

    def __init__(self, cfg: dict | None = None):
        import torch
        from transformers import AutoModel, AutoTokenizer

        self.device = os.environ.get("PARSEC_EMBED_DEVICE") or (
            "cuda" if torch.cuda.is_available() else "cpu"
        )
        # local_files_only when a dir is baked: never reach the network at serve.
        local_only = os.path.isdir(_MODEL)
        self.tok = AutoTokenizer.from_pretrained(_MODEL, local_files_only=local_only)
        self.model = (
            AutoModel.from_pretrained(_MODEL, local_files_only=local_only)
            .to(self.device)
            .eval()
        )

    def embed(
        self, texts: Sequence[str], as_query: bool = False, workers: int | None = None
    ) -> list[list[float]]:
        import torch

        texts = list(texts)
        out: list[list[float]] = []
        for i in range(0, len(texts), _BATCH):
            batch = texts[i : i + _BATCH]
            enc = self.tok(
                batch,
                padding=True,
                truncation=True,
                max_length=_MAXTOK,
                return_tensors="pt",
            ).to(self.device)
            with torch.inference_mode():
                cls = self.model(**enc).last_hidden_state[:, 0]  # [CLS] pooling (bge recipe)
                cls = torch.nn.functional.normalize(cls, p=2, dim=1)  # L2 -> unit vectors
            out.extend(cls.cpu().to(torch.float32).numpy().tolist())
        return out
