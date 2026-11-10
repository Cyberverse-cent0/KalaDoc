# Deploying KalaDoc on a server

This guide covers running KalaDoc as a plain binary on a Linux server —
for development, staging, or a self-hosted deployment. For containers see
[`deploy/README.md`](README.md) and `../docker-compose.yml`; for Kubernetes
see [`kubernetes/`](kubernetes/).

## TL;DR — run against a dev server for UI work

```bash
# on the server (one-time)
curl -fsSL https://.../kaladoc-<ver>-linux-x86_64.tar.gz | tar xz
sudo ./kaladoc-<ver>-linux-x86_64/install.sh     # installs to /usr/local/bin

export APP_SECRET_KEY="$(openssl rand -base64 48 | tr -d '\n' | tr '+/' '-_')"
export APP_DB_USER="kaladoc"
export APP_DB_PASSWORD="$(openssl rand -base64 24)"

kaladoc init          # create + migrate the database
kaladoc doctor        # verify config/db/storage/engines
kaladoc serve         # HTTP API on 127.0.0.1:8080
kaladoc jobs worker   # second terminal: drains the job queue
```

The UI talks to `http://<server>:8080`. Get a token with
`kaladoc auth login` and send it as `Authorization: Bearer <token>`.
`GET /health` and `GET /openapi.json` are public — `/openapi.json` is the
contract to generate API clients/types from.

> **CORS:** the API does not currently send CORS headers. Serve the UI
> from the same origin (reverse proxy), or add a CORS layer before
> hosting the UI on a different port/domain.

## Option A — prebuilt binary (fastest)

Release archives are published on GitHub Releases by the `release`
workflow (see [`../../build.md`](../../build.md)). Each archive contains
the binary plus `install.sh` (unix) / `install.ps1` (Windows).

```bash
# pick the slug for your server:
#   linux-x86_64  linux-aarch64  macos-x86_64  macos-aarch64  windows-x86_64
curl -fLO https://github.com/Cyberverse-cent0/KalaDoc/releases/latest/download/kaladoc-<ver>-linux-x86_64.tar.gz
tar -xzf kaladoc-<ver>-linux-x86_64.tar.gz
cd kaladoc-<ver>-linux-x86_64
sha256sum -c ../SHA256SUMS        # optional integrity check
sudo ./install.sh                 # -> /usr/local/bin/kaladoc
```

If no release exists yet, build one locally — see "Cutting a release"
in `build.md` (`kaladoc-release build/package/aggregate/publish`).

### Runtime dependencies on the server

The binary needs nothing beyond libc, but engine features shell out to
tools that must be on `PATH`:

| Tool | Needed for |
|---|---|
| `ffmpeg`, `ffprobe` | video/multimedia engine |
| `pdftoppm` (poppler-utils) | PDF render/OCR prep |
| `tesseract` | PDF OCR |
| `openssl` | PDF signing |
| `pandoc`, `libreoffice` | conversion backends |

Missing tools are detected at runtime — the engine reports `unavailable`
in `kaladoc doctor` / `GET /health` instead of crashing. Install only what
you need:

```bash
# Debian/Ubuntu
sudo apt install ffmpeg poppler-utils tesseract-ocr pandoc libreoffice
# Fedora/RHEL
sudo dnf install ffmpeg poppler-utils tesseract pandoc libreoffice
```

## Option B — build from source on the server

```bash
# 1. Rust toolchain (if missing)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# 2. Source
git clone https://github.com/Cyberverse-cent0/KalaDoc.git
cd KalaDoc

# 3. Build
cargo build --release            # -> target/release/kaladoc
cargo build --release --bin kaladoc-release   # optional packaging tool

# quick sanity build check (faster, no codegen)
cargo check --all-targets
```

### Build commands

```bash
cargo build --release                          # release binary
cargo build                                   # debug binary
cargo check --all-targets                     # compile check incl. tests
cargo test                                    # full test suite (~1,900 tests)
cargo test --lib                              # library tests only
cargo clippy                                  # lints
cargo fmt --all                               # format
cargo run --release -- --help                 # run without installing
```

### Test commands

```bash
cargo test                          # everything (lib + integration tests/)
cargo test --lib                    # unit tests inside src/
cargo test --test jobs_e2e          # one integration test file
cargo test pdf                      # filter by name substring
cargo test -- --nocapture           # show println output
```

Note: media/PDF tests that shell out to `ffmpeg`, `tesseract`, etc. skip
or fail honestly when the tool is absent — install the deps above for
full coverage.

## First-run configuration

Secrets come from the environment (never commit them):

```bash
export APP_SECRET_KEY="$(openssl rand -base64 48 | tr -d '\n' | tr '+/' '-_')"  # >=32 chars
export APP_DB_USER="kaladoc"
export APP_DB_PASSWORD="$(openssl rand -base64 24)"
```

| Variable | Purpose | Default |
|---|---|---|
| `APP_SECRET_KEY` | Session-signing key (**required**) | — |
| `APP_DB_USER` / `APP_DB_PASSWORD` | DB credentials (**required**) | — |
| `KALADOC_API_ADDR` | HTTP bind address | `127.0.0.1:8080` |
| `DB_PATH` | SQLite file | `db.sqlite` |
| `KALADOC_STORAGE_DIR` | Storage root | `./storage` |
| `KALADOC_STORAGE_MAX_BYTES` | Storage quota | unset |
| `KALADOC_LOG_LEVEL` / `RUST_LOG` | Log filter | `info` |
| `JOB_POLL_INTERVAL_SECS`, `JOB_STALE_AFTER_SECS` | Job tuning | see `src/config/mod.rs` |
| `KALADOC_S3_*` | S3 backend (env-only) | — |

A `kaladoc.toml` (or `KALADOC_CONFIG`) layers under env vars; inspect the
merged result with `kaladoc config show` (secrets redacted).

Initialize and verify:

```bash
kaladoc init       # create/migrate DB
kaladoc doctor     # config + db + storage + engine health
kaladoc serve      # API
kaladoc jobs worker  # job queue consumer (separate process)
```

## Running as a service (systemd)

`/etc/systemd/system/kaladoc.service`:

```ini
[Unit]
Description=KalaDoc API server
After=network.target

[Service]
Type=simple
User=kaladoc
EnvironmentFile=/etc/kaladoc/env
ExecStart=/usr/local/bin/kaladoc serve --addr 0.0.0.0:8080
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

`/etc/systemd/system/kaladoc-worker.service` — same unit but
`ExecStart=/usr/local/bin/kaladoc jobs worker`.

`/etc/kaladoc/env`:

```bash
APP_SECRET_KEY=...
APP_DB_USER=kaladoc
APP_DB_PASSWORD=...
DB_PATH=/var/lib/kaladoc/kaladoc.db
KALADOC_STORAGE_DIR=/var/lib/kaladoc/storage
KALADOC_API_ADDR=0.0.0.0:8080
```

```bash
sudo systemctl enable --now kaladoc kaladoc-worker
curl http://localhost:8080/health
```

Put a reverse proxy (nginx/Caddy) in front for TLS — terminate HTTPS there
and forward to `127.0.0.1:8080`.

## UI development notes

- API base: `http://<server>:8080` (or your proxy origin)
- Auth: `POST` login via `kaladoc auth login` once to mint a token, then
  `Authorization: Bearer <token>` on every non-public route
- Public routes: `GET /health`, `GET /openapi.json` — generate TS/Python
  clients from the OpenAPI spec
- Long-running work (PDF ops, video jobs) is async: `POST /documents/{id}/process`
  or `/jobs` returns a job id; poll `GET /jobs/{id}` or stream progress
- Media files upload/download via the `/documents` and `/media/*` routes;
  check `storage_status` for quota headroom

## Upgrading

```bash
# binary install: re-run install.sh over the old binary, restart service
# source install:
cd KalaDoc && git pull && cargo build --release
sudo install -m755 target/release/kaladoc /usr/local/bin/kaladoc
sudo systemctl restart kaladoc kaladoc-worker
kaladoc init    # apply any pending DB migrations
```
