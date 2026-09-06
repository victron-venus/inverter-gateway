#!/usr/bin/env bash
#
# Deploy inverter-gateway to a Synology (or any SSH Docker host).
#
# Usage:
#   ./deploy.sh              # default SSH host: synology; pull IMAGE_TAG from GHCR
#   ./deploy.sh other-host
#   IMAGE_TAG=0.2.1 ./deploy.sh
#   BUILD_LOCAL=1 ./deploy.sh   # escape hatch: docker compose build on NAS (slow)
#
# Prefers prebuilt images from GitHub Container Registry (CI publishes on tag v*).
# For private GHCR packages set GHCR_TOKEN (PAT with read:packages) in .env or env.
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
IMAGE_REPO="${IMAGE_REPO:-ghcr.io/victron-venus/inverter-gateway}"
BUILD_LOCAL="${BUILD_LOCAL:-0}"
SEPARATOR="=============================================="

# Default tag: Cargo.toml version, or latest
if [[ -z "${IMAGE_TAG:-}" ]]; then
  IMAGE_TAG="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$SCRIPT_DIR/Cargo.toml" | head -1)"
  IMAGE_TAG="${IMAGE_TAG:-latest}"
fi

echo "$SEPARATOR"
echo "  Deploying inverter-gateway"
echo "$SEPARATOR"
echo "SSH host:   $SSH_HOST"
echo "Remote dir: $REMOTE_DIR"
echo "Env file:   $ENV_FILE"
echo "Image:      $IMAGE_REPO:$IMAGE_TAG"
echo "Mode:       $([[ "$BUILD_LOCAL" == "1" ]] && echo 'local build on NAS' || echo 'pull from GHCR')"
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

# Optional GHCR token from env or .env
if [[ -z "${GHCR_TOKEN:-}" ]] && grep -qE '^GHCR_TOKEN=.+' "$ENV_FILE"; then
  # shellcheck disable=SC1090
  GHCR_TOKEN="$(grep -E '^GHCR_TOKEN=' "$ENV_FILE" | head -1 | cut -d= -f2- | tr -d '"' | tr -d "'")"
  export GHCR_TOKEN
fi

echo ">>> Checking SSH to $SSH_HOST..."
ssh -o BatchMode=yes -o ConnectTimeout=10 "$SSH_HOST" 'echo ok' >/dev/null

echo ">>> Ensuring remote directory..."
ssh "$SSH_HOST" "mkdir -p '$REMOTE_DIR'"

echo ">>> Syncing files to remote..."
RSYNC_EXCLUDES=(
  --exclude '.git/'
  --exclude 'target/'
  --exclude 'logs/'
  --exclude '.env'
  --exclude '.env.local'
  --exclude '.env.secrets'
  --exclude '.DS_Store'
  --exclude '**/*.rs.bk'
)
# Pull mode does not need Rust sources on the NAS.
if [[ "$BUILD_LOCAL" != "1" ]]; then
  RSYNC_EXCLUDES+=(--exclude 'src/')
fi
rsync -az --delete   -e "ssh -o BatchMode=yes"   "${RSYNC_EXCLUDES[@]}"   "$SCRIPT_DIR/" "$SSH_HOST:$REMOTE_DIR/"

# Prefer committed compose; fall back to example renamed on the host.
ssh "$SSH_HOST" "cd '$REMOTE_DIR' && if [ ! -f docker-compose.yml ] && [ -f docker-compose.example.yml ]; then cp docker-compose.example.yml docker-compose.yml; fi"

echo ">>> Installing secrets as remote .env (mode 600)..."
rsync -az -e "ssh -o BatchMode=yes" "$ENV_FILE" "$SSH_HOST:$REMOTE_DIR/.env"
# Ensure IMAGE_TAG is available to compose on the remote
ssh "$SSH_HOST" "grep -q '^IMAGE_TAG=' '$REMOTE_DIR/.env' && sed -i.bak 's/^IMAGE_TAG=.*/IMAGE_TAG=$IMAGE_TAG/' '$REMOTE_DIR/.env' && rm -f '$REMOTE_DIR/.env.bak' || echo \"IMAGE_TAG=$IMAGE_TAG\" >> '$REMOTE_DIR/.env'"
ssh "$SSH_HOST" "chmod 600 '$REMOTE_DIR/.env'"

echo ">>> Pulling/starting containers..."
# Pass token via env only for the remote session (not written to disk beyond .env if user put it there).
ssh "$SSH_HOST" \
  "env IMAGE_TAG='$IMAGE_TAG' IMAGE_REPO='$IMAGE_REPO' BUILD_LOCAL='$BUILD_LOCAL' GHCR_TOKEN='${GHCR_TOKEN:-}' REMOTE_DIR='$REMOTE_DIR' bash -s" <<'REMOTE'
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

if [[ "${BUILD_LOCAL}" == "1" ]]; then
  echo ">>> BUILD_LOCAL=1 — composing build on NAS (slow)..."
  # Temporarily use local image name for build
  "${COMPOSE[@]}" build
else
  if [[ -n "${GHCR_TOKEN:-}" ]]; then
    echo ">>> docker login ghcr.io..."
    echo "$GHCR_TOKEN" | "${DOCKER[@]}" login ghcr.io -u TOKEN --password-stdin
  fi
  echo ">>> docker compose pull ${IMAGE_TAG}..."
  if ! "${COMPOSE[@]}" pull; then
    echo "ERROR: pull failed for ghcr.io/victron-venus/inverter-gateway:${IMAGE_TAG}" >&2
    echo "Publish the image (git tag v${IMAGE_TAG} / workflow_dispatch) or use BUILD_LOCAL=1." >&2
    exit 1
  fi
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

echo ""
echo "$SEPARATOR"
echo "  Deployment complete → $SSH_HOST:$REMOTE_DIR ($IMAGE_REPO:$IMAGE_TAG)"
echo "$SEPARATOR"
echo "Secrets stay only in local .env (gitignored) and remote $REMOTE_DIR/.env."
