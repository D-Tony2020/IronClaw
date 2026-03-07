#!/usr/bin/env bash
# Build the WeChat channel WASM component
#
# Prerequisites:
#   - Rust with wasm32-wasip2 target: rustup target add wasm32-wasip2
#   - wasm-tools for component creation: cargo install wasm-tools
#
# Output:
#   - wechat.wasm - WASM component ready for deployment
#   - wechat.capabilities.json - Capabilities file (copy alongside .wasm)

set -euo pipefail

cd "$(dirname "$0")"

echo "Building WeChat channel WASM component..."

# Build the WASM module
cargo build --release --target wasm32-wasip2

# Convert to component model
WASM_PATH="target/wasm32-wasip2/release/wechat_channel.wasm"

if [ -f "$WASM_PATH" ]; then
    # Create component if needed
    wasm-tools component new "$WASM_PATH" -o wechat.wasm 2>/dev/null || cp "$WASM_PATH" wechat.wasm

    # Strip debug info
    wasm-tools strip wechat.wasm -o wechat.wasm

    echo "Built: wechat.wasm ($(du -h wechat.wasm | cut -f1))"
    echo ""
    echo "To install:"
    echo "  mkdir -p ~/.ironclaw/channels"
    echo "  cp wechat.wasm wechat.capabilities.json ~/.ironclaw/channels/"
    echo ""
    echo "Then add your WeChat secrets:"
    echo "  ironclaw secret set wechat_app_id <your-app-id>"
    echo "  ironclaw secret set wechat_app_secret <your-app-secret>"
    echo "  ironclaw secret set wechat_verify_token <your-token>"
else
    echo "Error: WASM output not found at $WASM_PATH"
    exit 1
fi
