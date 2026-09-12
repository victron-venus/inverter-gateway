#!/usr/bin/env bash
# Build local artifacts without publishing or deploying. Dependencies must be installed.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION="${1:?Usage: release-build.sh X.Y.Z [nightly|beta|rc]}"
CHANNEL="${2:-rc}"
python3 scripts/check-release-version.py "$VERSION" "$CHANNEL"
mkdir -p release-output
cargo build --locked --release
tar -czf "release-output/inverter-gateway-$(uname -s)-$(uname -m).tar.gz" -C target/release inverter-gateway
