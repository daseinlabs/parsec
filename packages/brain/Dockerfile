# dasein-brain — one image for BOTH local testing and Cloud Run.
# The 36MB R-GCN runs on CPU; the in-process bge-large embedder
# (DASEIN_EMBED_BACKEND=local) runs on GPU when the torch wheels are CUDA and a
# GPU is present. Default build is CPU (works on arm64 + x86_64); Cloud Run GPU
# builds pass --build-arg TORCH_INDEX=https://download.pytorch.org/whl/cu121.
#
# Two things are baked from the build context when present, so the image digest
# PINS them (determinism-honest deploy); both are empty placeholders locally,
# where the runtime volume / HF cache supplies them instead:
#   bundle/       -> /bundle       (the checkpoint; compose mounts ~/.dasein/brain)
#   embed-model/  -> /embed-model  (the bge weights; local uses the HF hub/cache)
FROM python:3.12-slim

# torch index: CPU by default; cu121 for the GPU deploy. The cu wheels bundle
# the CUDA runtime as pip deps, so python:slim + a GPU host driver is enough.
ARG TORCH_INDEX=https://download.pytorch.org/whl/cpu
# Install the embed extras (transformers + safetensors) only when building the
# 'local' backend image; empty skips them (smaller image for dasein/hash).
ARG EMBED_EXTRAS="transformers>=4.40,<5 safetensors"
RUN pip install --no-cache-dir \
      --index-url ${TORCH_INDEX} \
      --extra-index-url https://pypi.org/simple \
      "torch>=2.2,<2.3" \
 && pip install --no-cache-dir \
      "torch-geometric>=2.5" "numpy>=1.26,<2" \
      fastapi "uvicorn[standard]" "pydantic>=2" httpx \
      tree-sitter-language-pack "tree-sitter-python==0.23.6" \
 && if [ -n "${EMBED_EXTRAS}" ]; then pip install --no-cache-dir ${EMBED_EXTRAS}; fi

WORKDIR /app
COPY pyproject.toml README.md ./
COPY src ./src
COPY models ./models
RUN pip install --no-cache-dir --no-deps -e .

# Bake the bundle + embed model when the build context carries them (Cloud
# Run); harmless empty dirs otherwise (compose mounts ~/.dasein/brain over
# /bundle; the local backend falls back to the HF hub/cache for the weights).
COPY bundle/ /bundle/
COPY embed-model/ /embed-model/

ENV DASEIN_CKPT=/bundle/curator_v4_prod.pt \
    DASEIN_RULES_JSON=/app/models/rules.json \
    AC_CHANGEPRONE_PKL=/app/models/changeprone.pkl
# DASEIN_EMBED_MODEL_DIR is set at DEPLOY time (to /embed-model) only when the
# model was baked — not here, so a local/hash build never points at an empty dir.

EXPOSE 8080
# The bundle self-validates at startup — a bad/missing ckpt fails the
# container fast (and the healthcheck below never goes healthy). Docker-only;
# Cloud Run ignores this and uses its own TCP startup probe (default 240s
# window, ample for the few-second bundle load).
HEALTHCHECK --interval=10s --timeout=3s --start-period=90s --retries=5 \
  CMD python -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:8080/health', timeout=2).status==200 else 1)"
# Shell form so ${PORT} expands: Cloud Run injects PORT (8080 by default);
# compose leaves it unset -> 8080. `exec` keeps uvicorn as PID 1 so Cloud
# Run's SIGTERM reaches it for a graceful drain.
CMD exec uvicorn --factory dasein_brain.app:create_app --host 0.0.0.0 --port ${PORT:-8080}
