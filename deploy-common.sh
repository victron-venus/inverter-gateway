#!/usr/bin/env bash
# Shared helpers for deploy.sh / deploy-from-release. Sourced, not executed.

deploy_common_init() {
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)"
  SSH_HOST="${1:-synology}"
  REMOTE_DIR="${REMOTE_DIR:-/volume1/docker/inverter-gateway}"
  ENV_FILE="${ENV_FILE:-$SCRIPT_DIR/.env}"
  IMAGE_REPO="${IMAGE_REPO:-ghcr.io/victron-venus/inverter-gateway}"
  export SEPARATOR="=============================================="
}

deploy_require_env() {
  if [[ ! -f "$ENV_FILE" ]]; then
    echo "ERROR: missing secrets file: $ENV_FILE" >&2
    echo "Copy the example and fill real values:" >&2
    echo "  cp .env.example .env" >&2
    exit 1
  fi
  if grep -qE 'your_mqtt_password|your_portal_id_here|replace_with_openssl_rand_hex_32' "$ENV_FILE"; then
    echo "ERROR: $ENV_FILE still contains example placeholders." >&2
    exit 1
  fi
  if ! grep -qE '^GATEWAY_API_TOKEN=.+' "$ENV_FILE"; then
    echo "ERROR: GATEWAY_API_TOKEN is empty in $ENV_FILE" >&2
    exit 1
  fi
  if [[ -z "${GHCR_TOKEN:-}" ]] && grep -qE '^GHCR_TOKEN=.+' "$ENV_FILE"; then
    GHCR_TOKEN="$(grep -E '^GHCR_TOKEN=' "$ENV_FILE" | head -1 | cut -d= -f2- | tr -d '"' | tr -d "'")"
    export GHCR_TOKEN
  fi
}

deploy_check_ssh() {
  echo ">>> Checking SSH to $SSH_HOST..."
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$SSH_HOST" 'echo ok' >/dev/null
  echo ">>> Ensuring remote directory..."
  ssh "$SSH_HOST" "mkdir -p '$REMOTE_DIR'"
}

deploy_rsync() {
  local exclude_src="${1:-0}"
  echo ">>> Syncing to $SSH_HOST:$REMOTE_DIR..."
  local excludes=(
    --exclude '.git/'
    --exclude 'target/'
    --exclude 'logs/'
    --exclude '.env'
    --exclude '.env.local'
    --exclude '.env.secrets'
    --exclude '.DS_Store'
    --exclude '**/*.rs.bk'
  )
  if [[ "$exclude_src" == "1" ]]; then
    excludes+=(--exclude 'src/')
  fi
  rsync -az --delete \
    -e "ssh -o BatchMode=yes" \
    "${excludes[@]}" \
    "$SCRIPT_DIR/" "$SSH_HOST:$REMOTE_DIR/"
  ssh "$SSH_HOST" "cd '$REMOTE_DIR' && if [ ! -f docker-compose.yml ] && [ -f docker-compose.example.yml ]; then cp docker-compose.example.yml docker-compose.yml; fi"
}

deploy_install_env() {
  local image_tag="$1"
  echo ">>> Installing secrets as remote .env (mode 600)..."
  rsync -az -e "ssh -o BatchMode=yes" "$ENV_FILE" "$SSH_HOST:$REMOTE_DIR/.env"
  ssh "$SSH_HOST" "grep -q '^IMAGE_TAG=' '$REMOTE_DIR/.env' && sed -i.bak 's/^IMAGE_TAG=.*/IMAGE_TAG=$image_tag/' '$REMOTE_DIR/.env' && rm -f '$REMOTE_DIR/.env.bak' || echo \"IMAGE_TAG=$image_tag\" >> '$REMOTE_DIR/.env'"
  ssh "$SSH_HOST" "chmod 600 '$REMOTE_DIR/.env'"
}

# mode: build | pull
deploy_remote_up() {
  local mode="$1"
  local image_tag="$2"
  echo ">>> Starting containers ($mode)..."
  ssh "$SSH_HOST" \
    "env IMAGE_TAG='$image_tag' IMAGE_REPO='$IMAGE_REPO' DEPLOY_MODE='$mode' GHCR_TOKEN='${GHCR_TOKEN:-}' REMOTE_DIR='$REMOTE_DIR' bash -s" <<'REMOTE'
set -euo pipefail
export PATH="/usr/local/bin:/var/packages/ContainerManager/target/usr/bin:$PATH"
cd "$REMOTE_DIR"

if ! command -v docker >/dev/null 2>&1; then
  echo "ERROR: docker not found on remote (expected Container Manager)." >&2
  exit 1
fi
if sudo -n docker info >/dev/null 2>&1; then
  DOCKER=(sudo -n docker)
elif docker info >/dev/null 2>&1; then
  DOCKER=(docker)
else
  echo "ERROR: cannot talk to docker daemon (try sudo or docker group)." >&2
  exit 1
fi
if "${DOCKER[@]}" compose version >/dev/null 2>&1; then
  COMPOSE=("${DOCKER[@]}" compose)
elif command -v docker-compose >/dev/null 2>&1 && sudo -n docker-compose version >/dev/null 2>&1; then
  COMPOSE=(sudo -n docker-compose)
else
  echo "ERROR: neither 'docker compose' nor docker-compose found." >&2
  exit 1
fi

export IMAGE_TAG

if [[ "$DEPLOY_MODE" == "build" ]]; then
  echo ">>> docker compose build (on NAS)..."
  "${COMPOSE[@]}" build
elif [[ "$DEPLOY_MODE" == "pull" ]]; then
  if [[ -n "${GHCR_TOKEN:-}" ]]; then
    echo ">>> docker login ghcr.io..."
    echo "$GHCR_TOKEN" | "${DOCKER[@]}" login ghcr.io -u TOKEN --password-stdin
  fi
  echo ">>> docker compose pull ${IMAGE_TAG}..."
  if ! "${COMPOSE[@]}" pull; then
    echo "ERROR: pull failed for ${IMAGE_REPO:-ghcr.io/victron-venus/inverter-gateway}:${IMAGE_TAG}" >&2
    echo "Tag a release / wait for GHCR, or use ./deploy.sh to build on the NAS." >&2
    exit 1
  fi
else
  echo "ERROR: unknown DEPLOY_MODE=$DEPLOY_MODE" >&2
  exit 1
fi

"${COMPOSE[@]}" up -d --remove-orphans
echo ""
echo ">>> Container status:"
"${COMPOSE[@]}" ps
echo ""
echo ">>> Health (loopback on host):"
sleep 3
curl -fsS http://127.0.0.1:9150/health || echo "(health not ready yet — check compose logs)"
REMOTE
}
