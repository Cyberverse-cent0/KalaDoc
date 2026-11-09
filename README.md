# KalaDoc

**A Rust-powered document management and processing platform.**

KalaDoc is a modular document platform designed to ingest, organize, store, and process documents through a unified application architecture.

Built with Rust, KalaDoc prioritizes reliability, performance, security, and extensibility. Its modular engine architecture provides a foundation for document management, storage, PDF processing, video processing, conversion, automation, and AI-assisted workflows.

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
| Core | Shared domain types, primitives, and application contracts |
| Application | Business services and application orchestration |
| Storage Engine | Physical document storage, path confinement, and quota management |
| Document Engine | Document identity, metadata, versions, and lifecycle management |
| Job Engine | Background task execution and job persistence |
| Shared Public | Shared platform detection and public utilities |
| Deployments | Platform-specific dependency and environment checks |

Specialized engines are designed to provide independent capabilities while integrating through shared application services.

For a detailed explanation of architectural decisions, boundaries, and implementation status, see [`.doc/Architecture.md`](.doc/Architecture.md).

## Project Structure

```text
KalaDoc/
├── src/
│   ├── config/                 # Application configuration
│   ├── core/                   # Shared domain types and contracts
│   ├── crates/
│   │   ├── application/        # Application services and adapters
│   │   ├── storage/            # Storage engine
│   │   ├── document_engine/    # Document management
│   │   ├── job_engine/         # Background job execution
│   │   └── shared_public/      # Shared platform utilities
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
- `ffmpeg` and `ffprobe` binaries on `PATH` (required by the video engine — detected, never bundled).

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

`APP_SECRET_KEY` must be at least 32 characters (see Validation rules).
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
cargo run -- --init-db
```

Use appropriate secrets for your environment. Do not commit credentials, production secrets, or populated environment files to version control.

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

The current focus is establishing reliable application infrastructure and completing the foundational engines before expanding into advanced document-processing capabilities.

Planned development areas include:

- Document ingestion and lifecycle management.
- Persistent document metadata and versioning.
- Storage provider abstractions.
- Reliable asynchronous job execution.
- PDF manipulation and rendering.
- Image processing and OCR.
- Document format conversion.
- Video processing via the ffmpeg-backed video engine.
- Pipeline-based document automation.
- CLI and MCP integrations.

Features should be considered available only when implemented and verified in the codebase.

## Documentation

Project documentation is maintained in the `.doc/` directory.

Start with:

- [Architecture](.doc/Architecture.md) — System architecture and design decisions.

## Contributing

Contributions, technical discussions, bug reports, and architectural feedback are welcome.

Before introducing significant architectural changes, review the existing design documentation and preserve established module boundaries.

## License

See the repository's license file for licensing terms.

---

**KalaDoc — A unified foundation for document management and processing.**
