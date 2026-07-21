# Deploying the brain to Cloud Run

The brain is stateless and round-robin-safe by construction (the determinism
boundary: the proxy chunks/freezes/renders/splices client-side; the brain only
returns scores + tau, never bytes). That is exactly the property Cloud Run's
request router needs — so **Cloud Run is the dynamic routing layer** we would
otherwise stand up with nginx. No sticky sessions, no shared state.

## What ships

One image (`packages/brain/Dockerfile`) for both `make dev` and Cloud Run:

- **Local**: `docker compose` mounts `~/.dasein/brain` over `/bundle`.
- **Cloud Run**: Cloud Build fetches the checkpoint from GCS into the build
  context and the image **bakes** it, so the image digest pins the exact bundle
  version (determinism-honest — a redeploy can't silently swap the checkpoint).

The container honors Cloud Run's injected `$PORT` and runs uvicorn as PID 1 for
a graceful SIGTERM drain. `create_app()` loads + self-validates the bundle
before the port opens, so a bad checkpoint fails the deploy fast; Cloud Run's
default TCP startup probe (240s window) covers the few-second load.

## One-time setup

```sh
gcloud config set project <PROJECT_ID>
gcloud services enable run.googleapis.com cloudbuild.googleapis.com artifactregistry.googleapis.com
gcloud artifacts repositories create dasein --repository-format=docker --location=us-central1
# Cloud Build's SA needs to read the checkpoint bucket:
gcloud storage buckets add-iam-policy-binding gs://dasein-473321-ac-learning \
  --member="serviceAccount:$(gcloud projects describe <PROJECT_ID> --format='value(projectNumber)')-compute@developer.gserviceaccount.com" \
  --role=roles/storage.objectViewer
```

## Deploy

```sh
# recommended: in-process bge on an L4 GPU — single hop, no external embedder
scripts/deploy_brain.sh

# external embed pod instead (CPU service; needs a reachable endpoint)
EMBED_BACKEND=dasein EMBED_URL=https://<embed-endpoint>/embed scripts/deploy_brain.sh

# wiring-only smoke test (deterministic TEST vectors, not real scores; CPU)
EMBED_BACKEND=hash scripts/deploy_brain.sh
```

Everything is overridable by env (`PROJECT`, `REGION`, `EMBED_MODEL`,
`MIN_INSTANCES`, `CONCURRENCY`, `CPU`, `MEMORY`, `BRAIN_KEY_SECRET`, …). See the
script header.

The default `local` backend build: fetches the bge weights into the build
context, installs CUDA (cu121) torch + transformers, bakes both the checkpoint
and the weights into the image, and deploys with `--gpu 1 --gpu-type nvidia-l4
--no-cpu-throttling --cpu 4 --memory 16Gi`. One L4 (24 GB) holds bge-large
(~1.3 GB) + the R-GCN comfortably, and the request path is a single
`proxy -> brain -> proxy` round trip.

## Serving shape (why these numbers)

| Flag | Value | Reason |
|------|-------|--------|
| `--concurrency` | `1` | The scorer is single-threaded (GIL + one CPU/GPU forward under a lock). Scale **horizontally** on request count, not in-process — this is the "add multithreading later" seam. |
| `--min-instances` | `1` | The proxy path is latency-sensitive; keep one instance warm so no request eats a cold start. Scale-to-zero is fine for dev/staging. |
| `DASEIN_SERVE_TAU` | *unset* | Serve at the checkpoint's **calibrated** tau, not a forced demo tau. |

## The embedder: in-process bge (`local`)

`DASEIN_EMBED_BACKEND=local` (`vendored/local_embed.py`) loads bge-large-en-v1.5
into the brain process, so there is no external embed hop — this is what makes
the Cloud Run GPU deploy a true single round trip. Recipe pinned to bge's
reference usage so the vectors match the cluster encoder the checkpoint was
trained on: **[CLS]-token pooling** (not mean), L2 normalize, 512-token cap, no
query instruction. It is verified in-process, through the API, and inside the
container; see `tests/test_local_embed.py`.

Two things to keep true:

- **Parity vs the cluster.** `local` and `dasein` load the *same* model, so
  they match by construction on CPU. Before trusting `local` for production
  scores, still run a one-off comparison against the `dasein` endpoint on a
  batch of real chunks (cosine ≈ 1.0 per row) to catch any tokenizer/precision
  drift.
- **GPU determinism vs round-robin-safety.** CUDA kernels aren't strictly
  bit-deterministic across instances, and a chunk scoring exactly at
  `scores_q >= tau_q` (`freeze.rs`) could flip keep/cut between replicas. The
  fixed-point 1e-6 grid absorbs most jitter; add a two-instance parity test
  (same input → identical served bytes) before relying on the round-robin
  claim, and fall back to fp32 or `torch.use_deterministic_algorithms(True)` if
  the edge bites.

## Throttling / API keys — do it at the edge

Rate-limit and per-key quota state is per-user and mutable; putting it in a
replica would break statelessness/round-robin-safety. Put API-key auth + quotas
in front of Cloud Run (API Gateway / Cloud Armor / Apigee) and keep the brain a
pure scorer. The in-app `DASEIN_BRAIN_KEY` bearer gate (optional
`BRAIN_KEY_SECRET`) is a coarse backstop, not the throttling layer.
