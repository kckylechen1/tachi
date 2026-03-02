#!/usr/bin/env bash

set -e

echo "========================================================"
echo "🦞 Installing Sigil Memory as OpenClaw Extension"
echo "========================================================"
echo ""

# 1. Determine target directory
TARGET_DIR="${SIGIL_INSTALL_DIR:-$HOME/sigil}"

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
echo ">> Building Rust NAPI bindings for Node.js..."
cd "$TARGET_DIR/integrations/openclaw"

# Check for npm
if ! command -v npm &> /dev/null; then
    echo "❌ Error: npm is not installed. Please install Node.js/npm."
    exit 1
fi

npm install

# Compile TypeScript sources to JS (no build script in package.json)
npx tsc --target ES2022 --module NodeNext --moduleResolution NodeNext \
  index.ts config.ts scorer.ts extractor.ts store.ts

echo ""
# 4. Symlink to OpenClaw
OPENCLAW_EXT_DIR="$HOME/.openclaw/local-plugins/extensions"
EXT_NAME="sigil-memory"
SYMLINK_PATH="$OPENCLAW_EXT_DIR/$EXT_NAME"

echo ">> Setting up OpenClaw extension..."
mkdir -p "$OPENCLAW_EXT_DIR"

if [ -L "$SYMLINK_PATH" ]; then
    echo "   Removing old symlink..."
    rm "$SYMLINK_PATH"
elif [ -d "$SYMLINK_PATH" ]; then
    echo "   Removing old directory..."
    rm -rf "$SYMLINK_PATH"
fi

ln -s "$TARGET_DIR/integrations/openclaw" "$SYMLINK_PATH"
echo "✅ Successfully linked to $SYMLINK_PATH"

echo ""
# 5. Final Instructions
echo "========================================================"
echo "🎉 Sigil installation complete!"
echo "========================================================"
echo ""
echo "Next Steps:"
echo "1. Enable 'sigil-memory' in your openclaw.json 'plugins.allow' list (or 'memory-hybrid-bridge' if keeping the old ID)."
echo "2. Edit $TARGET_DIR/.env with your API keys if you haven't globally exported them:"
echo "   - VOYAGE_API_KEY (for embedding & reranking)"
echo "   - SILICONFLOW_API_KEY (for fact extraction)"
echo "3. Restart the OpenClaw gateway for changes to take effect."
echo ""
echo "Need API keys?"
echo "👉 Voyage API: https://dash.voyageai.com/ (200M free tokens)"
echo "👉 SiliconFlow: https://cloud.siliconflow.cn/ (Free tier available)"
echo ""
