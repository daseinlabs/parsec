# dasein-brain — local/manual-testing image (docker-compose.yml at repo root).
# CPU inference only (36MB R-GCN); works on arm64 and x86_64. The checkpoint
# is NOT baked — mount it (compose mounts ~/.dasein/brain). This same file is
# deploy-ready for the GKE pattern later (bake the ckpt there so the image
# digest pins the bundle version).
FROM python:3.12-slim

# torch CPU wheels from the pytorch index (falls back to PyPI where the cpu
# index lacks an arch — PyPI aarch64 builds are CPU-only anyway).
RUN pip install --no-cache-dir \
      --index-url https://download.pytorch.org/whl/cpu \
      --extra-index-url https://pypi.org/simple \
      "torch>=2.2,<2.3" \
 && pip install --no-cache-dir \
      "torch-geometric>=2.5" "numpy>=1.26,<2" \
      fastapi "uvicorn[standard]" "pydantic>=2" httpx \
      tree-sitter-language-pack "tree-sitter-python==0.23.6"

WORKDIR /app
COPY pyproject.toml README.md ./
COPY src ./src
COPY models ./models
RUN pip install --no-cache-dir --no-deps -e .

ENV DASEIN_CKPT=/bundle/curator_v4_prod.pt \
    DASEIN_RULES_JSON=/app/models/rules.json \
    AC_CHANGEPRONE_PKL=/app/models/changeprone.pkl

EXPOSE 8080
# The bundle self-validates at startup — a bad/missing ckpt fails the
# container fast (and the healthcheck below never goes healthy).
HEALTHCHECK --interval=10s --timeout=3s --start-period=90s --retries=5 \
  CMD python -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:8080/health', timeout=2).status==200 else 1)"
CMD ["uvicorn", "--factory", "dasein_brain.app:create_app", "--host", "0.0.0.0", "--port", "8080"]
