#!/usr/bin/env bash
#
# One-command installer for Sigil Memory (OpenClaw Plugin)
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/kckylechen1/tachi/main/scripts/install.sh | bash
#   # or with options:
#   bash install.sh --version 0.1.0
#
set -euo pipefail

VERSION="${SIGIL_VERSION:-latest}"
PLUGIN_DIR="${SIGIL_PLUGIN_DIR:-$HOME/.openclaw/extensions/memory-hybrid-bridge}"
REPO="kckylechen1/tachi"

# ── Parse args ────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --dir)     PLUGIN_DIR="$2"; shift 2 ;;
    -h|--help)
      echo "Usage: install.sh [--version <ver>] [--dir <path>]"
      echo "  --version  Version to install (default: latest)"
      echo "  --dir      Plugin install path (default: ~/.openclaw/extensions/memory-hybrid-bridge)"
      exit 0 ;;
    *) echo "Unknown option: $1"; exit 1 ;;
  esac
done

# ── Prereqs ───────────────────────────────────────────────────
echo "========================================================="
echo "🧠 Installing Sigil Memory (OpenClaw Plugin)"
echo "========================================================="
echo ""

for cmd in node npm curl; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "❌ Required command not found: $cmd"
    exit 1
  fi
done

NODE_MAJOR=$(node -e "console.log(process.versions.node.split('.')[0])")
if [ "$NODE_MAJOR" -lt 18 ]; then
  echo "❌ Node.js >= 18 required (found v$(node -v))"
  exit 1
fi

# ── Resolve version ──────────────────────────────────────────
if [ "$VERSION" = "latest" ]; then
  echo ">> Fetching latest release..."
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | node -e "
    let d=''; process.stdin.on('data',c=>d+=c); process.stdin.on('end',()=>{
      const tag = JSON.parse(d).tag_name || '';
      console.log(tag.replace(/^v/,''));
    })
  ")
  if [ -z "$VERSION" ]; then
    echo "❌ Could not determine latest version. Use --version to specify."
    exit 1
  fi
fi
echo "   Version: $VERSION"

# ── Download tarball ─────────────────────────────────────────
TARBALL_URL="https://github.com/$REPO/releases/download/v${VERSION}/memory-hybrid-bridge-v${VERSION}.tar.gz"
TMPDIR=$(mktemp -d)
TARBALL="$TMPDIR/plugin.tar.gz"

echo ">> Downloading from $TARBALL_URL..."
if ! curl -fSL -o "$TARBALL" "$TARBALL_URL"; then
  echo "❌ Download failed. Check that version v${VERSION} exists at:"
  echo "   https://github.com/$REPO/releases"
  rm -rf "$TMPDIR"
  exit 1
fi

# ── Install ──────────────────────────────────────────────────
echo ">> Installing to $PLUGIN_DIR..."
mkdir -p "$PLUGIN_DIR"

# Preserve user data (memory.db, audit-log, shadow-store)
if [ -d "$PLUGIN_DIR/data" ]; then
  echo "   Preserving existing data/ directory"
  mv "$PLUGIN_DIR/data" "$TMPDIR/data_backup"
fi

# Extract (overwrites old JS/config files)
tar -xzf "$TARBALL" -C "$PLUGIN_DIR"

# Restore data
if [ -d "$TMPDIR/data_backup" ]; then
  mv "$TMPDIR/data_backup" "$PLUGIN_DIR/data"
fi
mkdir -p "$PLUGIN_DIR/data"

# ── npm install (pulls @chaoxlabs/tachi-node optionally) ─────
echo ">> Installing dependencies..."
cd "$PLUGIN_DIR"
npm install --registry https://registry.npmjs.org 2>&1 | tail -3

# ── Smoke test ───────────────────────────────────────────────
echo ">> Verifying native module..."
if node --input-type=module -e "import('@chaoxlabs/tachi-node').then(m => { if(!m.JsMemoryStore) throw new Error('missing export'); console.log('   ✅ Native module loaded OK') })" 2>/dev/null; then
  :
else
  echo "   ⚠ Native module not available — running in MCP-only mode."
  echo "   Install 'tachi' via: brew tap kckylechen1/tachi && brew install tachi"
fi

# ── Auto-configure openclaw.json ─────────────────────────────
OPENCLAW_JSON="$HOME/.openclaw/openclaw.json"
if [ -f "$OPENCLAW_JSON" ]; then
  echo ""
  echo ">> Checking openclaw.json..."

  ALREADY_CONFIGURED=$(node -e "
    const c = require('$OPENCLAW_JSON');
    const allow = c.plugins?.allow || [];
    const paths = c.plugins?.load?.paths || [];
    const ok = allow.includes('memory-hybrid-bridge') && paths.includes('$PLUGIN_DIR');
    console.log(ok ? 'yes' : 'no');
  " 2>/dev/null || echo "no")

  if [ "$ALREADY_CONFIGURED" = "yes" ]; then
    echo "   Already configured ✓"
  else
    echo "   Adding plugin to openclaw.json..."
    node -e "
      const fs = require('fs');
      const c = JSON.parse(fs.readFileSync('$OPENCLAW_JSON', 'utf8'));

      if (!c.plugins) c.plugins = {};
      if (!c.plugins.allow) c.plugins.allow = [];
      if (!c.plugins.allow.includes('memory-hybrid-bridge')) c.plugins.allow.push('memory-hybrid-bridge');

      if (!c.plugins.load) c.plugins.load = {};
      if (!c.plugins.load.paths) c.plugins.load.paths = [];
      if (!c.plugins.load.paths.includes('$PLUGIN_DIR')) c.plugins.load.paths.push('$PLUGIN_DIR');

      if (!c.plugins.slots) c.plugins.slots = {};
      c.plugins.slots.memory = 'memory-hybrid-bridge';

      if (!c.plugins.entries) c.plugins.entries = {};
      if (!c.plugins.entries['memory-hybrid-bridge']) {
        c.plugins.entries['memory-hybrid-bridge'] = { enabled: true, config: {} };
      }

      fs.writeFileSync('$OPENCLAW_JSON', JSON.stringify(c, null, 2) + '
');
      console.log('   ✅ openclaw.json updated');
    " 2>/dev/null || echo "   ⚠ Could not auto-configure. Please add manually (see below)."
  fi
fi

# ── Cleanup ──────────────────────────────────────────────────
rm -rf "$TMPDIR"

# ── Done ─────────────────────────────────────────────────────
echo ""
echo "========================================================="
echo "🎉 Sigil Memory installed successfully!"
echo "========================================================="
echo ""
echo "Next steps:"
echo "  1. Configure API keys in .env (copy from .env.example):"
echo "     - VOYAGE_API_KEY     → Voyage AI  (https://dash.voyageai.com/)"
echo "     - SILICONFLOW_API_KEY → SiliconFlow (https://cloud.siliconflow.cn/)"
echo "     See integrations/openclaw/.env.example for all options."
echo "  2. Install the Tachi MCP server if not already present:"
echo "     brew tap kckylechen1/tachi && brew install tachi"
echo "  3. Restart OpenClaw gateway"
echo ""
echo "Plugin path: $PLUGIN_DIR"
echo "Data path:   $PLUGIN_DIR/data"
