#!/usr/bin/env bash
#
# Deploy inverter-gateway to a Synology (or any SSH Docker host).
#
# Usage:
#   ./deploy.sh              # default SSH host: synology
#   ./deploy.sh other-host   # deploy to that SSH host instead
#
# Local secrets:
#   .env          — real credentials (gitignored). Required.
#   .env.example  — fake template for users (committed).
#
# Remote layout (override with REMOTE_DIR):
#   /volume1/docker/inverter-gateway
#
set -euo pipefail

SSH_HOST="${1:-synology}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REMOTE_DIR="${REMOTE_DIR:-/volume1/docker/inverter-gateway}"
ENV_FILE="${ENV_FILE:-$SCRIPT_DIR/.env}"
SEPARATOR="=============================================="

echo "$SEPARATOR"
echo "  Deploying inverter-gateway"
echo "$SEPARATOR"
echo "SSH host:   $SSH_HOST"
echo "Remote dir: $REMOTE_DIR"
echo "Env file:   $ENV_FILE"
echo ""

if [[ ! -f "$ENV_FILE" ]]; then
  echo "ERROR: missing secrets file: $ENV_FILE" >&2
  echo "Copy the example and fill real values:" >&2
  echo "  cp .env.example .env" >&2
  echo "  \$EDITOR .env" >&2
  exit 1
fi

# Refuse to ship the example file as if it were secrets.
if grep -qE 'your_mqtt_password|your_portal_id_here|replace_with_openssl_rand_hex_32' "$ENV_FILE"; then
  echo "ERROR: $ENV_FILE still contains example placeholders." >&2
  echo "Fill MQTT_*, VICTRON_PORTAL_ID, and GATEWAY_API_TOKEN before deploying." >&2
  exit 1
fi

if ! grep -qE '^GATEWAY_API_TOKEN=.+' "$ENV_FILE"; then
  echo "ERROR: GATEWAY_API_TOKEN is empty in $ENV_FILE" >&2
  exit 1
fi

echo ">>> Checking SSH to $SSH_HOST..."
ssh -o BatchMode=yes -o ConnectTimeout=10 "$SSH_HOST" 'echo ok' >/dev/null

echo ">>> Ensuring remote directory..."
ssh "$SSH_HOST" "mkdir -p '$REMOTE_DIR'"

echo ">>> Syncing repository (excluding secrets, build artifacts, git)..."
# macOS tar: avoid AppleDouble / xattrs on Synology extract
rsync -az --delete \
  -e "ssh -o BatchMode=yes" \
  --exclude '.git/' \
  --exclude 'target/' \
  --exclude 'logs/' \
  --exclude '.env' \
  --exclude '.env.local' \
  --exclude '.env.secrets' \
  --exclude '.DS_Store' \
  --exclude '**/*.rs.bk' \
  "$SCRIPT_DIR/" "$SSH_HOST:$REMOTE_DIR/"

# Prefer committed compose; fall back to example renamed on the host.
ssh "$SSH_HOST" "cd '$REMOTE_DIR' && if [ ! -f docker-compose.yml ] && [ -f docker-compose.example.yml ]; then cp docker-compose.example.yml docker-compose.yml; fi"

echo ">>> Installing secrets as remote .env (mode 600)..."
# Prefer rsync: Synology SSH often rejects scp's SFTP subsystem ("subsystem request failed").
rsync -az -e "ssh -o BatchMode=yes" "$ENV_FILE" "$SSH_HOST:$REMOTE_DIR/.env"
ssh "$SSH_HOST" "chmod 600 '$REMOTE_DIR/.env'"

echo ">>> Building and starting containers..."
# Synology Container Manager puts docker in /usr/local/bin (not on default SSH PATH).
ssh "$SSH_HOST" "bash -s" <<REMOTE
set -euo pipefail
export PATH="/usr/local/bin:/var/packages/ContainerManager/target/usr/bin:\$PATH"
cd '$REMOTE_DIR'
if ! command -v docker >/dev/null 2>&1; then
  echo "ERROR: docker not found on remote (expected Container Manager)." >&2
  exit 1
fi
# Synology: docker.sock is root:root; administrators can use passwordless sudo.
if sudo -n docker info >/dev/null 2>&1; then
  DOCKER=(sudo -n docker)
elif docker info >/dev/null 2>&1; then
  DOCKER=(docker)
else
  echo "ERROR: cannot talk to docker daemon (try sudo or docker group)." >&2
  exit 1
fi
if "\${DOCKER[@]}" compose version >/dev/null 2>&1; then
  COMPOSE=("\${DOCKER[@]}" compose)
elif command -v docker-compose >/dev/null 2>&1 && sudo -n docker-compose version >/dev/null 2>&1; then
  COMPOSE=(sudo -n docker-compose)
else
  echo "ERROR: neither 'docker compose' nor docker-compose found." >&2
  exit 1
fi
"\${COMPOSE[@]}" build
"\${COMPOSE[@]}" up -d --remove-orphans
echo ""
echo ">>> Container status:"
"\${COMPOSE[@]}" ps
echo ""
echo ">>> Health (loopback on host):"
sleep 3
curl -fsS http://127.0.0.1:8080/health || echo "(health not ready yet — check: sudo docker compose -f $REMOTE_DIR/docker-compose.yml logs -f)"
REMOTE

echo ""
echo "$SEPARATOR"
echo "  Deployment complete → $SSH_HOST:$REMOTE_DIR"
echo "$SEPARATOR"
echo "Secrets stay only in local .env (gitignored) and remote $REMOTE_DIR/.env."
