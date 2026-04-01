#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo ">> install_openclaw_ext.sh is now a compatibility wrapper."
echo ">> For the full one-command flow, prefer scripts/install.sh."
echo ""

exec "$SCRIPT_DIR/install.sh" --skip-brew "$@"
