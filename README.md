# KalaDoc

Rust application for ingesting, storing, and processing documents.

## Layout

- `src/config` — layered configuration (defaults → `kaladoc.toml`/`KALADOC_CONFIG` → env)
- `src/core` — shared domain types
- `src/crates/application` — business services (Organization: users/sessions, CLI/MCP adapters)
- `src/crates/storage` — filesystem engine (path confinement, quota, fault injection)
- `src/crates/shared_public` — platform detection
- `src/deployments` — environment/dependency checks

See `.doc/Architecture.md` for details.

## Build & test

```sh
cargo check
cargo test
```

## Database init

```sh
APP_SECRET_KEY=... APP_DB_USER=... APP_DB_PASSWORD=... cargo run -- --init-db
```
