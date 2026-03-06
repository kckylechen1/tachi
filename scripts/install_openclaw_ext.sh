#!/usr/bin/env bash

set -euo pipefail

echo "========================================================"
echo "🦞 Installing Sigil Memory as OpenClaw Extension"
echo "========================================================"
echo ""

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "❌ Error: required command not found: $1"
        exit 1
    fi
}

require_cmd git
require_cmd node
require_cmd npm
require_cmd cargo

# 1. Determine target directory
TARGET_DIR="${SIGIL_INSTALL_DIR:-$HOME/sigil}"
OPENCLAW_PLUGIN_DIR="$TARGET_DIR/integrations/openclaw"
NATIVE_MODULE_DIR="$TARGET_DIR/crates/memory-node"

if [ -d "$TARGET_DIR" ]; then
    echo ">> Found existing Sigil repo at $TARGET_DIR. Pulling latest..."
    cd "$TARGET_DIR"
    git pull origin main
else
    echo ">> Cloning Sigil repository to $TARGET_DIR..."
    git clone https://github.com/kckylechen1/sigil.git "$TARGET_DIR"
    cd "$TARGET_DIR"
fi

echo ""
# 2. Setup .env
if [ ! -f "$TARGET_DIR/.env" ]; then
    echo ">> Setting up .env file..."
    cp .env.example .env
    echo "   Created $TARGET_DIR/.env (Please fill in API keys later if not using global env vars)"
fi

echo ""
# 3. Build NAPI-RS bindings
echo ">> Installing Node.js dependencies..."
npm --prefix "$NATIVE_MODULE_DIR" install
npm --prefix "$OPENCLAW_PLUGIN_DIR" install

echo ">> Building Rust NAPI bindings for Node.js..."
npm --prefix "$OPENCLAW_PLUGIN_DIR" run build

if ! ls "$NATIVE_MODULE_DIR"/memory-core.*.node >/dev/null 2>&1; then
    echo "❌ Error: native NAPI binary was not produced under $NATIVE_MODULE_DIR"
    exit 1
fi

if [ ! -f "$OPENCLAW_PLUGIN_DIR/index.js" ]; then
    echo "❌ Error: OpenClaw extension entrypoint was not compiled: $OPENCLAW_PLUGIN_DIR/index.js"
    exit 1
fi

echo ">> Running plugin load smoke test..."
node -e "import('node:url').then(({ pathToFileURL }) => import(pathToFileURL(process.argv[1]).href)).then(() => console.log('   Plugin load smoke test passed')).catch((err) => { console.error(err); process.exit(1); })" "$OPENCLAW_PLUGIN_DIR/index.js"

echo ""
# 4. Symlink to OpenClaw
OPENCLAW_EXT_DIR="$HOME/.openclaw/local-plugins/extensions"
EXT_NAME="memory-hybrid-bridge"
SYMLINK_PATH="$OPENCLAW_EXT_DIR/$EXT_NAME"
LEGACY_SYMLINK_PATH="$OPENCLAW_EXT_DIR/sigil-memory"

echo ">> Setting up OpenClaw extension..."
mkdir -p "$OPENCLAW_EXT_DIR"

if [ -L "$SYMLINK_PATH" ]; then
    echo "   Removing old symlink..."
    rm "$SYMLINK_PATH"
elif [ -e "$SYMLINK_PATH" ]; then
    echo "❌ Error: $SYMLINK_PATH already exists and is not a symlink. Move it away and rerun."
    exit 1
fi

if [ -L "$LEGACY_SYMLINK_PATH" ]; then
    echo "   Removing legacy symlink $LEGACY_SYMLINK_PATH..."
    rm "$LEGACY_SYMLINK_PATH"
elif [ -e "$LEGACY_SYMLINK_PATH" ]; then
    echo "   Leaving existing legacy path untouched: $LEGACY_SYMLINK_PATH"
fi

ln -s "$OPENCLAW_PLUGIN_DIR" "$SYMLINK_PATH"
echo "✅ Successfully linked to $SYMLINK_PATH"

echo ""
# 5. Final Instructions
echo "========================================================"
echo "🎉 Sigil installation complete!"
echo "========================================================"
echo ""
echo "Next Steps:"
echo "1. Enable 'memory-hybrid-bridge' in your openclaw.json 'plugins.allow' list."
echo "2. If your OpenClaw setup uses plugins.slots.memory, set it to 'memory-hybrid-bridge'."
echo "3. Edit $TARGET_DIR/.env with your API keys if you haven't globally exported them:"
echo "   - VOYAGE_API_KEY (for embedding & reranking)"
echo "   - SILICONFLOW_API_KEY (for fact extraction)"
echo "4. Restart the OpenClaw gateway for changes to take effect."
echo ""
echo "Need API keys?"
echo "👉 Voyage API: https://dash.voyageai.com/ (200M free tokens)"
echo "👉 SiliconFlow: https://cloud.siliconflow.cn/ (Free tier available)"
echo ""
