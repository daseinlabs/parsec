# dasein-platform — Cloud Run image (DIRECTION.md §7c: thin FastAPI glue).
# Build from the REPO ROOT so contracts schemas ride along for the drift tests:
#   docker build -f packages/platform/Dockerfile -t dasein-platform .
FROM python:3.12-slim AS build
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
WORKDIR /app
COPY packages/platform/pyproject.toml packages/platform/
COPY packages/platform/src packages/platform/src
RUN uv venv /venv && \
    uv pip install --python /venv/bin/python './packages/platform[postgres]'

FROM python:3.12-slim
# Non-root: Cloud Run doesn't require it, but there is no reason to run as root.
RUN useradd --create-home app
USER app
COPY --from=build /venv /venv
ENV PATH="/venv/bin:$PATH"
# Cloud Run injects PORT; default matches local runs.
ENV PORT=8080
CMD ["sh", "-c", "uvicorn --factory parsec_platform:create_app --host 0.0.0.0 --port ${PORT}"]
