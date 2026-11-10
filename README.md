# KalaDoc

**A Rust-powered document management and processing platform.**

KalaDoc is a modular document platform designed to ingest, organize, store, and process documents through a unified application architecture.

Built with Rust, KalaDoc prioritizes reliability, performance, security, and extensibility. Its modular engine architecture provides a foundation for document management, storage, PDF processing, conversion, automation, and AI-assisted workflows.

`APP_SECRET_KEY` must be at least 32 characters (see Validation rules).

---

## Validation rules

Inputs are validated before they reach storage or the database:

- **User name** — 3–32 chars, ASCII letters/digits plus `_`, `-`, `.`
- **Email** — 1–254 chars, no whitespace, `local@domain.tld` shape
- **Password** — 8–1024 chars (hashed with Argon2id before storage)
- **`APP_SECRET_KEY`** — when set: 32–1024 chars, printable ASCII, no whitespace
- **Filenames/uploads** — `security_engine` sanitizes names, sniffs magic
  bytes (never trusts extensions), and enforces size/page/output limits
- **Job payloads** — handlers confine paths to the storage root and
  reject absolute paths / `..` components

## Overview

KalaDoc is designed around a simple principle:

> Documents should be managed through a unified system, while specialized engines handle the complexity of processing them.

The application separates business logic, document management, physical storage, and processing capabilities into well-defined components.

Its architecture is designed to support multiple interfaces, including command-line tools, APIs, and Model Context Protocol (MCP) integrations.

## Architecture

KalaDoc follows a modular architecture with clearly defined responsibilities.

| Component | Responsibility |
|---|---|
| Configuration | Layered application configuration and runtime settings |
| Core | Shared domain types, primitives, and engine contracts |
| Application | Business services, document facade, and CLI adapter |
| Storage Engine | Local and remote (S3) document storage, path confinement, quota |
| Document Engine | Document identity, metadata, versions, and lifecycle management |
| Job Engine | Durable background task queue, dispatch, and crash recovery |
| Scheduler | Recurring jobs driven through the job queue |
| Engine Registry | Unified registry of processing engines and health checks |
| Security Engine | Upload validation, filename sanitization, magic-byte sniffing |
| PDF Engine | PDF manipulation (100+ operations: merge, split, sign, OCR, ...) |
| Image Engine | Image processing operations |
| Multimedia Engine | Audio/video processing via managed `ffmpeg`/`ffprobe` processes |
| Conversion Engine | Document format conversion through pluggable backends |
| Pipeline Engine | Multi-step document processing pipelines |
| API | HTTP API (axum) exposing documents, jobs, engines, and pipelines |
| Observability | Logging, health checks, and request tracing |
| Shared Public | Shared platform detection and public utilities |
| Deployments | Platform-specific dependency and environment checks |

Specialized engines implement a common `ProcessingEngine` contract and are registered in `EngineRegistry::builtin()`; they integrate through shared application services rather than calling each other directly.

For a detailed explanation of architectural decisions, boundaries, and implementation status, see [`.doc/Architecture.md`](.doc/Architecture.md).

## Project Structure

```text
KalaDoc/
├── src/
│   ├── config/                 # Application configuration
│   ├── core/                   # Shared domain types and contracts
│   ├── crates/
│   │   ├── api/                 # HTTP API (axum)
│   │   ├── application/         # Application services, job handlers, CLI
│   │   ├── conversion-engine/   # Format conversion (pandoc/LibreOffice/poppler)
│   │   ├── core/                # Engine contracts (ProcessingEngine, limits)
│   │   ├── document_engine/     # Document management and persistence
│   │   ├── engine-registry/     # Processing-engine registry
│   │   ├── image-engine/        # Image processing
│   │   ├── job_engine/          # Durable background job queue
│   │   ├── multimedia-engine/   # Audio/video via ffmpeg/ffprobe
│   │   ├── observability/       # Logging, health, request IDs
│   │   ├── pdf-engine/          # PDF processing (100+ ops)
│   │   ├── pipeline-engine/     # Multi-step processing pipelines
│   │   ├── scheduler/           # Recurring-job schedules
│   │   ├── security-engine/     # Upload validation and sniffing
│   │   ├── shared_public/       # Shared platform utilities
│   │   └── storage/             # Local filesystem + remote S3 backends
│   ├── deployments/            # Deployment and dependency checks
│   └── main.rs                 # Application entry point
├── .doc/
│   └── Architecture.md         # Architecture documentation
├── Cargo.toml
├── Cargo.lock
└── README.md
```

*The structure above describes the main architectural areas; additional modules may be introduced as the platform evolves.*

## Technology

KalaDoc uses the Rust ecosystem to build a reliable and efficient application.

Key technologies include:

- **Rust** — Primary implementation language.
- **Tokio** — Asynchronous runtime.
- **SQLx** — Database access and persistence.
- **SQLite** — Initial application database.
- **Serde** — Data serialization and deserialization.
- **Tracing** — Structured application diagnostics.

The technology stack is intentionally modular so that infrastructure and processing capabilities can evolve independently.

## Development

### Prerequisites

- Rust toolchain (stable).
- Cargo.
- Git.
- Platform-specific dependencies required by the enabled engines.

### Clone the repository

```bash
git clone https://github.com/Cyberverse-cent0/KalaDoc.git
cd KalaDoc
```

### Build

```bash
cargo check
```

### Run tests

```bash
cargo test
```

### Format the code

```bash
cargo fmt --all
```

### Run Clippy

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

## Database Initialization

KalaDoc provides a database initialization mode.

Configure the required environment variables:

```bash
export APP_SECRET_KEY="your-application-secret"
export APP_DB_USER="your-database-user"
export APP_DB_PASSWORD="your-database-password"
```

Initialize the database:

```bash
cargo run -- init
# or, once built:
cargo run --release -- init
```

(`--init-db` remains as a hidden legacy alias for `kaladoc init`.)

Use appropriate secrets for your environment. Do not commit credentials, production secrets, or populated environment files to version control.

## Configuration

Configuration resolves in layers: built-in defaults → `kaladoc.toml`
(or the file named by `KALADOC_CONFIG` / `--config`) → environment
variables. Inspect the merged result with `kaladoc config show` /
`kaladoc config get <dotted.key>`; secrets are always redacted.

```toml
[database]
path = "db.sqlite"
max_connections = 8

[storage]
root_dir = "./storage"
max_size_bytes = 10737418240   # optional quota

[api]
addr = "127.0.0.1:8080"        # kaladoc serve bind address

[jobs]
poll_interval_secs = 2
default_max_attempts = 3
backoff_base_secs = 30
stale_after_secs = 600         # crash-recovery timeout for running jobs
```

Notable env vars: `DB_PATH`, `KALADOC_STORAGE_DIR`, `KALADOC_API_ADDR`,
`KALADOC_LOG_LEVEL`, `JOB_POLL_INTERVAL_SECS`, `JOB_STALE_AFTER_SECS`
(alias `JOB_RECOVERY_TIMEOUT_SECS`), and the required secrets
`APP_SECRET_KEY` (≥ 32 chars), `APP_DB_USER`, `APP_DB_PASSWORD`.
The full mapping is documented on `AppConfig::apply_env` in
`src/config/mod.rs`. Remote S3 storage and multimedia tool discovery are
env-only / PATH-based — see the sections below.

## Command line

The `kaladoc` binary provides the full local surface (all commands support
`--output json|yaml`, `--config <path>`, and `--quiet`):

```text
kaladoc version                     # version/build info
kaladoc doctor                      # config, database, storage health
kaladoc init                        # create/migrate the database
kaladoc config show|get|validate    # inspect resolved configuration
kaladoc auth login|logout|status|whoami|session ...
kaladoc user create|list|delete
kaladoc jobs worker|list|enqueue|cancel|retry|recover
kaladoc engine list|health          # registered processing engines
kaladoc doc list|get|create|rename|move|delete|restore|purge|folders ...
kaladoc pdf <op>                    # local-file PDF ops (merge, split,
                                    # rotate, sign, encrypt, ocr, ...)
kaladoc serve [--addr host:port]    # start the HTTP API
```

Processing engines (`pdf`, `image`, `multimedia`, `convert`, `pipeline`,
`security`) are also reachable in-process through `EngineRegistry::builtin()`
and as background jobs (`document.pdf_op`, `pipeline.run`, ...).

External tool dependencies are detected, never assumed: the multimedia
engine needs `ffmpeg` **and** `ffprobe` on `PATH`; PDF rendering/OCR uses
`pdftoppm`/`tesseract`; digital signing shells out to `openssl`; conversion
backends use `pandoc`/`libreoffice`/`pdftoppm` when present. Missing tools
surface as honest `unavailable` health, not crashes.

## HTTP API

`kaladoc serve` starts the axum API on `api.addr`
(`127.0.0.1:8080` by default; override with `--addr` or
`KALADOC_API_ADDR`). The same secrets as the CLI are required.

```bash
export APP_SECRET_KEY="..."   # >= 32 chars
export APP_DB_USER="..."
export APP_DB_PASSWORD="..."
kaladoc init                  # once
kaladoc serve
```

```bash
curl -s http://127.0.0.1:8080/health        # public: aggregated engine health
curl -s http://127.0.0.1:8080/openapi.json  # public: OpenAPI 3.x spec
```

All other endpoints (`/documents`, `/jobs`, `/engines`, `/pipelines/run`,
...) require `Authorization: Bearer <session-token>` — obtain one via
`kaladoc auth login`, which stores the token for reuse.

## Remote storage (S3)

The storage crate ships an S3-compatible backend (AWS S3, MinIO, R2, ...)
configured **exclusively through environment variables** — credentials are
never read from `kaladoc.toml`:

| Variable | Purpose | Default |
|---|---|---|
| `KALADOC_S3_ENDPOINT` | S3 endpoint URL (empty = derive from region) | `https://s3.{region}.amazonaws.com` |
| `KALADOC_S3_BUCKET` | Target bucket (required) | — |
| `KALADOC_S3_REGION` | AWS region for SigV4 signing | `us-east-1` |
| `KALADOC_S3_PREFIX` | Key prefix inside the bucket | `""` |
| `KALADOC_S3_ACCESS_KEY` | Access key id (required) | — |
| `KALADOC_S3_SECRET_KEY` | Secret access key (required) | — |
| `KALADOC_S3_TIMEOUT_SECS` | Per-request timeout | `30` |
| `KALADOC_S3_MAX_ATTEMPTS` | Retry attempts for transient failures | `4` |

See `src/crates/storage/remote/` for the backend contract.

## Design Principles

KalaDoc is developed around the following engineering principles:

**1. Modular architecture**

Each engine has a defined responsibility and communicates through explicit interfaces.

**2. Reuse proven technology**

Established libraries handle general-purpose infrastructure. KalaDoc focuses its custom implementation on document-specific functionality and application behavior.

**3. Resource-aware processing**

Document ingestion and processing should use bounded memory, controlled concurrency, and appropriate temporary storage.

**4. Persistence and reliability**

Important application state and background work should survive process restarts wherever durability is required.

**5. Security by design**

Storage access, document ownership, authentication, authorization, and resource limits are treated as fundamental system requirements.

**6. Multiple interfaces, shared logic**

CLI, HTTP API, and MCP interfaces should use the same application services rather than implementing separate business logic.

**7. Testable engineering**

Domain logic, persistence, storage integration, and processing engines should have independently testable boundaries.

## Project Status

KalaDoc is under active development.

The foundational platform is in place: layered configuration, SQLite
persistence, document storage with quotas, a durable job queue with crash
recovery, recurring schedules, and a registry of processing engines —
PDF (100+ operations), image, multimedia (ffmpeg), security validation,
format conversion, and multi-step pipelines — exposed through the CLI,
background jobs, and an HTTP API.

Active development areas include:

- Hardening engine coverage and additional document formats.
- Wiring the remote S3 storage backend into the application services.
- MCP server integration (the CLI adapter reserves the surface).
- Broader pipeline authoring and scheduling UX.

Features should be considered available only when implemented and verified in the codebase.

## Documentation

Project documentation is maintained in the `.doc/` directory.

Start with:

- [Architecture](.doc/Architecture.md) — System architecture and design decisions.
- [PDF engine](.doc/pdf_engine.md) — PDF operation catalog, contracts, and external tool requirements.

## Contributing

Contributions, technical discussions, bug reports, and architectural feedback are welcome.

Before introducing significant architectural changes, review the existing design documentation and preserve established module boundaries.

## License

See the repository's license file for licensing terms.

---

**KalaDoc — A unified foundation for document management and processing.**
