#!/usr/bin/env bash
#
# Build inverter-gateway on the Synology (or SSH Docker host) and start it.
#
# Usage:
#   ./deploy.sh              # default SSH host: synology
#   ./deploy.sh other-host
#
# For GHCR pull of the latest release, use ./deploy-from-release instead.
#
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=deploy-common.sh
source "$SCRIPT_DIR/deploy-common.sh"

deploy_common_init "${1:-synology}"

IMAGE_TAG="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$SCRIPT_DIR/Cargo.toml" | head -1)"
IMAGE_TAG="${IMAGE_TAG:-latest}"

echo "$SEPARATOR"
echo "  Deploying inverter-gateway (local Docker build on NAS)"
echo "$SEPARATOR"
echo "SSH host:   $SSH_HOST"
echo "Remote dir: $REMOTE_DIR"
echo "Env file:   $ENV_FILE"
echo "Image tag:  $IMAGE_TAG (compose image name after build)"
echo ""

deploy_require_env
deploy_check_ssh
deploy_rsync 0
deploy_install_env "$IMAGE_TAG"
deploy_remote_up build "$IMAGE_TAG"

echo ""
echo "$SEPARATOR"
echo "  Deployment complete → $SSH_HOST:$REMOTE_DIR (built on NAS)"
echo "$SEPARATOR"
echo "Secrets stay only in local .env (gitignored) and remote $REMOTE_DIR/.env."
