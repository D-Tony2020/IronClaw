#!/bin/bash
# IronClaw Watchdog — runs every 5 minutes via LaunchAgent
# Checks health of gateway + tunnel, logs status, rotates logs

LOG_DIR="/Users/max/Desktop/IronClaw/logs"
TIMESTAMP=$(date '+%Y-%m-%d %H:%M:%S')
MAX_LOG_SIZE=10485760  # 10MB

echo "[$TIMESTAMP] Watchdog check starting"

# --- Health checks ---
GATEWAY_OK=false
TUNNEL_OK=false

# Check IronClaw gateway (port 8081 webhook server)
if curl -s --max-time 5 http://localhost:8081/wasm-channels/health > /dev/null 2>&1; then
    GATEWAY_OK=true
    echo "[$TIMESTAMP] ✓ IronClaw gateway healthy (port 8081)"
else
    echo "[$TIMESTAMP] ✗ IronClaw gateway NOT responding on port 8081"
fi

# Check Gateway Web UI (port 3001)
if curl -s --max-time 5 http://localhost:3001/ > /dev/null 2>&1; then
    echo "[$TIMESTAMP] ✓ Gateway Web UI healthy (port 3001)"
else
    echo "[$TIMESTAMP] ✗ Gateway Web UI NOT responding on port 3001"
fi

# Check Cloudflare tunnel
if curl -s --max-time 10 -o /dev/null -w "%{http_code}" https://wechat.moore-ai.org/wasm-channels/health 2>/dev/null | grep -q "200"; then
    TUNNEL_OK=true
    echo "[$TIMESTAMP] ✓ Cloudflare tunnel healthy"
else
    echo "[$TIMESTAMP] ✗ Cloudflare tunnel NOT responding"
fi

# --- Log rotation ---
for logfile in "$LOG_DIR"/*.log; do
    if [ -f "$logfile" ] && [ $(stat -f%z "$logfile" 2>/dev/null || echo 0) -gt $MAX_LOG_SIZE ]; then
        mv "$logfile" "${logfile}.old"
        echo "[$TIMESTAMP] Rotated $logfile (exceeded 10MB)"
    fi
done

# --- Summary ---
if $GATEWAY_OK && $TUNNEL_OK; then
    echo "[$TIMESTAMP] All systems operational"
else
    echo "[$TIMESTAMP] ⚠ Some checks failed — LaunchAgent KeepAlive should auto-recover"
fi
