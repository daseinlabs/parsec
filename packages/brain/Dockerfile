# parsec-brain — one image for local self-hosting AND container platforms (Cloud Run, k8s).
# The 36MB R-GCN runs on CPU; the in-process bge-large embedder
# (PARSEC_EMBED_BACKEND=local) runs on GPU when the torch wheels are CUDA and a
# GPU is present. Default build is CPU (works on arm64 + x86_64); Cloud Run GPU
# builds pass --build-arg TORCH_INDEX=https://download.pytorch.org/whl/cu121.
#
# Two things are baked from the build context when present, so the image digest
# PINS them (determinism-honest deploy); both are empty placeholders locally,
# where the runtime volume / HF cache supplies them instead:
#   bundle/       -> /bundle       (the checkpoint; docker-compose mounts ~/.parsec/brain,
#                                   or set PARSEC_CKPT=hf://<org>/<repo>/<file> at run time)
#   embed-model/  -> /embed-model  (the bge weights; local uses the HF hub/cache)
FROM python:3.12-slim

# torch index: CPU by default; cu121 for the GPU deploy. The cu wheels bundle
# the CUDA runtime as pip deps, so python:slim + a GPU host driver is enough.
ARG TORCH_INDEX=https://download.pytorch.org/whl/cpu
# Install the embed extras (transformers + safetensors) only when building the
# 'local' backend image; empty skips them (smaller image for parsec/hash).
ARG EMBED_EXTRAS="transformers>=4.40,<5 safetensors"
RUN pip install --no-cache-dir \
      --index-url ${TORCH_INDEX} \
      --extra-index-url https://pypi.org/simple \
      "torch>=2.2,<2.3" \
 && pip install --no-cache-dir \
      "torch-geometric>=2.5" "numpy>=1.26,<2" \
      fastapi hypercorn "pydantic>=2" httpx "huggingface_hub>=0.23" \
      tree-sitter-language-pack "tree-sitter-python==0.23.6" \
 && if [ -n "${EMBED_EXTRAS}" ]; then pip install --no-cache-dir ${EMBED_EXTRAS}; fi

WORKDIR /app
COPY pyproject.toml README.md ./
COPY src ./src
COPY models ./models
RUN pip install --no-cache-dir --no-deps -e .

# Bake the bundle + embed model when the build context carries them (Cloud
# builds); harmless empty dirs otherwise (compose mounts ~/.parsec/brain over
# /bundle; the local backend falls back to the HF hub/cache for the weights).
COPY bundle/ /bundle/
COPY embed-model/ /embed-model/

ENV PARSEC_CKPT=/bundle/curator_v4_prod.pt \
    PARSEC_RULES_JSON=/app/models/rules.json \
    AC_CHANGEPRONE_PKL=/app/models/changeprone.pkl
# PARSEC_EMBED_MODEL_DIR is set at DEPLOY time (to /embed-model) only when the
# model was baked — not here, so a local/hash build never points at an empty dir.

EXPOSE 8080
# The bundle self-validates at startup — a bad/missing ckpt fails the
# container fast (and the healthcheck below never goes healthy). Docker-only;
# Cloud Run and k8s use their own startup probes (a few seconds for the bundle
# load; up to a minute or two if the bge weights are pulled at boot).
HEALTHCHECK --interval=10s --timeout=3s --start-period=90s --retries=5 \
  CMD python -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:8080/health', timeout=2).status==200 else 1)"
# Shell form so ${PORT} expands: container platforms inject PORT (8080 by
# default); compose leaves it unset -> 8080. `exec` keeps hypercorn as PID 1 so
# the platform's SIGTERM reaches it for a graceful drain.
# hypercorn, not uvicorn: Cloud Run is deployed with --use-http2 (cloudbuild.yaml)
# so the Google Frontend forwards HTTP/2 end-to-end, which lifts its 32 MiB
# HTTP/1 request-body cap (large score/neighbors bodies were 413'd at the edge,
# before the container — invisible in the service logs). That requires the
# container to speak cleartext HTTP/2 (h2c, prior knowledge); uvicorn cannot,
# hypercorn does, and still serves HTTP/1.1 for docker/compose and the healthcheck.
CMD exec hypercorn --bind 0.0.0.0:${PORT:-8080} 'parsec_brain.app:create_app()'
