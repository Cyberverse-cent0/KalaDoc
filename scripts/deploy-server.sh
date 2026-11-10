#!/usr/bin/env bash
#
# deploy-server.sh — build kaladoc, ship it to a remote server, restart it.
#
# Usage:
#   ./scripts/deploy-server.sh [options]
#
# Options (or env vars):
#   -s, --server HOST        target server          (DEPLOY_SERVER, default 192.168.0.12)
#   -u, --user USER          ssh user               (DEPLOY_USER,   default nebular)
#   -k, --key PATH           ssh private key        (DEPLOY_KEY,    default ~/.ssh/kaladoc_deploy)
#   -r, --remote-path PATH   remote binary path    (DEPLOY_REMOTE_BIN, default ~/kaladoc)
#   -e, --env-file PATH      remote env file       (DEPLOY_ENV_FILE,  default ~/kaladoc.env)
#   -d, --data-dir PATH      remote data dir       (DEPLOY_DATA_DIR,  default ~/kaladoc-data)
#   -p, --port PORT          api port for healthcheck (DEPLOY_PORT,  default 8080)
#   --skip-build             reuse existing target/release/kaladoc
#   --no-restart             upload only, don't restart services
#   -h, --help               show this help
#
# What it does:
#   1. cargo build --release            (unless --skip-build)
#   2. scp binary to the server
#   3. remotely: backup old binary, install new one, restart `kaladoc serve`
#      and `kaladoc jobs worker` (same setsid/nohup layout the server already uses)
#   4. polls http://<server>:<port>/health until it responds
#
# First-time server setup is documented in deploy/server.md — this script
# assumes ~/kaladoc.env and ~/kaladoc-data already exist there.

set -euo pipefail

SERVER="${DEPLOY_SERVER:-192.168.0.12}"
USER="${DEPLOY_USER:-nebular}"
KEY="${DEPLOY_KEY:-$HOME/.ssh/kaladoc_deploy}"
REMOTE_BIN="${DEPLOY_REMOTE_BIN:-kaladoc}"
ENV_FILE="${DEPLOY_ENV_FILE:-kaladoc.env}"
DATA_DIR="${DEPLOY_DATA_DIR:-kaladoc-data}"
PORT="${DEPLOY_PORT:-8080}"
SKIP_BUILD=0
NO_RESTART=0
BINARY="target/release/kaladoc"

while [[ $# -gt 0 ]]; do
  case "$1" in
    -s|--server)        SERVER="$2"; shift 2 ;;
    -u|--user)          USER="$2"; shift 2 ;;
    -k|--key)           KEY="$2"; shift 2 ;;
    -r|--remote-path)   REMOTE_BIN="$2"; shift 2 ;;
    -e|--env-file)      ENV_FILE="$2"; shift 2 ;;
    -d|--data-dir)      DATA_DIR="$2"; shift 2 ;;
    -p|--port)          PORT="$2"; shift 2 ;;
    --skip-build)       SKIP_BUILD=1; shift ;;
    --no-restart)       NO_RESTART=1; shift ;;
    -h|--help)          sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

SSH="ssh -i $KEY -o BatchMode=yes -o ConnectTimeout=10"
SCP="scp -i $KEY -o BatchMode=yes"
TARGET="$USER@$SERVER"

log() { printf '\033[1;34m[deploy]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[deploy] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# --- 1. build ---------------------------------------------------------------
if [[ $SKIP_BUILD -eq 0 ]]; then
  log "building release binary (cargo build --release)"
  cargo build --release --bin kaladoc
else
  log "skipping build (--skip-build)"
fi
[[ -x $BINARY ]] || die "binary not found at $BINARY — run without --skip-build"
log "binary: $(ls -lh "$BINARY" | awk '{print $5}')  version: $("$BINARY" version | head -1)"

# --- 2. upload ---------------------------------------------------------------
log "uploading to $TARGET:~/ ($REMOTE_BIN.new)"
$SCP "$BINARY" "$TARGET:~/$REMOTE_BIN.new"

if [[ $NO_RESTART -eq 1 ]]; then
  log "uploaded as ~/$REMOTE_BIN.new (--no-restart); install manually on the server"
  exit 0
fi

# --- 3. remote install + restart --------------------------------------------
log "installing and restarting services on $TARGET"
$SSH "$TARGET" REMOTE_BIN="$REMOTE_BIN" ENV_FILE="$ENV_FILE" DATA_DIR="$DATA_DIR" bash -s <<'REMOTE'
set -euo pipefail
cd ~
# keep the previous binary for rollback
[[ -f ~/$REMOTE_BIN ]] && cp ~/$REMOTE_BIN ~/$REMOTE_BIN.prev || true
chmod +x ~/$REMOTE_BIN.new
mv ~/$REMOTE_BIN.new ~/$REMOTE_BIN
mkdir -p ~/$DATA_DIR/logs

# stop old processes (serve + worker); ignore if not running
pkill -f "kaladoc serve" 2>/dev/null || true
pkill -f "kaladoc jobs worker" 2>/dev/null || true
sleep 1

# start detached so they survive logout
setsid nohup bash -c "set -a; . ~/$ENV_FILE; set +a; exec ~/$REMOTE_BIN serve" \
  > ~/$DATA_DIR/logs/serve.log 2>&1 < /dev/null &
setsid nohup bash -c "set -a; . ~/$ENV_FILE; set +a; exec ~/$REMOTE_BIN jobs worker" \
  > ~/$DATA_DIR/logs/worker.log 2>&1 < /dev/null &
sleep 2
pgrep -af "kaladoc serve|kaladoc jobs" || { echo "RESTART FAILED — check ~/$DATA_DIR/logs/"; exit 1; }
REMOTE

# --- 4. health check ---------------------------------------------------------
log "waiting for http://$SERVER:$PORT/health"
for i in $(seq 1 30); do
  code=$(curl -s -o /dev/null -w '%{http_code}' -m 3 "http://$SERVER:$PORT/health" || true)
  if [[ "$code" == "200" || "$code" == "503" ]]; then
    log "API is up (HTTP $code)"
    curl -s -m 5 "http://$SERVER:$PORT/health" | python3 -m json.tool 2>/dev/null | head -30 || true
    exit 0
  fi
  sleep 1
done
die "health check failed — inspect logs on the server: ssh $TARGET 'tail ~/kaladoc-data/logs/serve.log'"
