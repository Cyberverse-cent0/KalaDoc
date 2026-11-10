# syntax=docker/dockerfile:1
#
# KalaDoc container image.
#
# Multi-stage build:
#   1. `builder`  — rust:1-bookworm compiles the `kaladoc` binary.
#   2. `runtime`  — debian:bookworm-slim runs it as an unprivileged user.
#
# Runtime dependencies installed in the final image:
#   * ca-certificates — TLS for outbound calls (S3, updates, etc.)
#   * ffmpeg         — multimedia engine discovers `ffmpeg`/`ffprobe` on
#                      PATH for audio/video transcode, extract, probe ops.
#                      Without it those ops report "engine unavailable".
#   * poppler-utils  — `pdftoppm`/`pdfinfo` for PDF rasterization/OCR prep.
#                      Without it PDF page-render ops are unavailable.
# Both are optional for the core document API but included here so the
# image has full functionality out of the box.
#
# Configuration is entirely env-driven (see src/config/mod.rs and
# deploy/README.md). The image sets container-friendly defaults:
#   DB_PATH=/data/kaladoc.db          — SQLite database
#   KALADOC_STORAGE_DIR=/data/storage — filesystem storage root
#   KALADOC_API_ADDR=0.0.0.0:8080     — bind all interfaces (required in
#                                       containers; the built-in default
#                                       is 127.0.0.1:8080)
# All state lives under /data, which is exposed as a volume mount point.

# ---------------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------------
FROM rust:1-bookworm AS builder

WORKDIR /src

# Copy manifests first so dependency layers can be cached independently
# of source changes (single crate — no workspace members).
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --bin kaladoc

# ---------------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        ffmpeg \
        poppler-utils \
    && rm -rf /var/lib/apt/lists/* \
    # Unprivileged runtime user with a writable state directory.
    && groupadd --system kaladoc \
    && useradd --system --gid kaladoc --home-dir /data --create-home kaladoc \
    && mkdir -p /data \
    && chown -R kaladoc:kaladoc /data

COPY --from=builder /src/target/release/kaladoc /usr/local/bin/kaladoc

ENV DB_PATH=/data/kaladoc.db \
    KALADOC_STORAGE_DIR=/data/storage \
    KALADOC_API_ADDR=0.0.0.0:8080

WORKDIR /data
USER kaladoc

VOLUME ["/data"]
EXPOSE 8080

# `kaladoc` is the entrypoint; the default command starts the HTTP API.
# Override CMD for other subcommands, e.g.:
#   docker run kaladoc jobs worker
#   docker run kaladoc init
#   docker run kaladoc doctor
ENTRYPOINT ["/usr/local/bin/kaladoc"]
CMD ["serve"]
