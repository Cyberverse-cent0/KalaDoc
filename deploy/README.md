# Deploying KalaDoc

This directory contains container deployment artifacts:

- `../Dockerfile` — multi-stage image (Rust builder → `debian:bookworm-slim`
  runtime, non-root user `kaladoc` uid 10001).
- `../docker-compose.yml` — single-node API + optional job worker / MinIO.
- `kubernetes/` — Deployment, Service, ConfigMap, PVC, optional worker.

## Image

```bash
docker build -t kaladoc:local .
```

The image bundles `ffmpeg`/`ffprobe` (multimedia engine) and
`poppler-utils` (`pdftoppm`, `pdfinfo` — PDF rasterization/OCR prep),
plus `ca-certificates`. These power PDF page-render and audio/video ops;
the core document API works without them but those engines report
unavailable. To slim the image, drop them from the `apt-get install`
line in the Dockerfile.

`ENTRYPOINT` is `/usr/local/bin/kaladoc`; `CMD` defaults to `serve`.
Other subcommands:

```bash
docker run --rm kaladoc init
docker run --rm kaladoc doctor
docker run --rm kaladoc jobs worker
```

## Configuration (environment variables)

Resolved in order: built-in defaults → optional TOML file
(`KALADOC_CONFIG`, or `kaladoc.toml` in the working directory) → env.
Full list in `src/config/mod.rs`; the ones that matter for containers:

| Variable | Purpose | Default |
|---|---|---|
| `APP_SECRET_KEY` | Session-signing key. **Required.** ≥32 printable ASCII chars, no whitespace. | — |
| `APP_DB_USER` / `APP_DB_PASSWORD` | DB credentials. **Required** (informational under SQLite). | — |
| `KALADOC_API_ADDR` | HTTP bind address | `127.0.0.1:8080` (image sets `0.0.0.0:8080`) |
| `DB_PATH` | SQLite database file | `db.sqlite` (image: `/data/kaladoc.db`) |
| `KALADOC_STORAGE_DIR` | Filesystem storage root | `./storage` (image: `/data/storage`) |
| `KALADOC_STORAGE_MAX_BYTES` | Optional storage quota | unset |
| `KALADOC_LOG_LEVEL` / `RUST_LOG` | Tracing filter | `info` |
| `KALADOC_CONFIG` | Path to TOML config file | unset |
| `JOB_*` (`JOB_POLL_INTERVAL_SECS`, `JOB_STALE_AFTER_SECS`, …) | Job engine tuning | see `src/config/mod.rs` |

### S3 / remote storage

`KALADOC_S3_*` variables (`KALADOC_S3_ENDPOINT`, `KALADOC_S3_BUCKET`,
`KALADOC_S3_REGION`, `KALADOC_S3_PREFIX`, `KALADOC_S3_ACCESS_KEY`,
`KALADOC_S3_SECRET_KEY`, `KALADOC_S3_TIMEOUT_SECS`,
`KALADOC_S3_MAX_ATTEMPTS`) configure the S3-compatible backend — see
`src/crates/storage/remote/config.rs`. Credentials are env-only.
`docker-compose.yml` includes a commented MinIO service for local S3
testing.

## State and volumes

Everything persistent lives under `/data` in the container:

- `/data/kaladoc.db` — SQLite database (`DB_PATH`)
- `/data/storage/` — uploaded/derived documents (`KALADOC_STORAGE_DIR`)

Mount a volume/PVC at `/data` or state is lost on container restart.
The container runs as uid/gid 10001; ensure mounted volumes are writable
by that user (the k8s manifests set `fsGroup: 10001`).

## Health endpoint

`GET /health` is public (no auth). Returns `200` with per-engine detail,
or `503` when any engine reports `Unavailable`. Use it for readiness;
use a TCP check for liveness so a degraded engine doesn't cause restart
loops.

## Docker Compose

```bash
docker compose up --build
```

Edit the `APP_*` secrets in `docker-compose.yml` first (or use a `.env`
file / `env_file:`). Optional, commented services:

- `worker` — `kaladoc jobs worker`, drains the durable job queue.
  `serve` does **not** process background jobs on its own.
- `minio` — S3 backend; pair with the `KALADOC_S3_*` env vars.

## Kubernetes

```bash
kubectl apply -f deploy/kubernetes/secret.example.yaml   # then edit values
# or create the secret directly (preferred):
kubectl -n kaladoc create secret generic kaladoc-secrets \
  --from-literal=APP_SECRET_KEY="..." \
  --from-literal=APP_DB_USER="kaladoc" \
  --from-literal=APP_DB_PASSWORD="..."

kubectl apply -k deploy/kubernetes
# optional job worker:
kubectl apply -f deploy/kubernetes/worker.yaml
```

Notes:

- `replicas: 1` — SQLite on a `ReadWriteOnce` PVC is single-writer.
  Horizontal scaling needs an external database first.
- Probes: readiness = `GET /health` (503-aware), liveness = TCP :8080.
- Replace `kaladoc:local` with your registry image (`kustomize edit set
  image …` or edit `deployment.yaml`/`worker.yaml`).
- The worker's liveness probe execs `kaladoc --version`; it proves the
  binary runs but not that the claim loop is healthy — adequate for a
  minimal manifest, tighten if needed.
