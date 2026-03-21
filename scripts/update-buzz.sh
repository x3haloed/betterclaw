#!/usr/bin/env bash
# update-buzz.sh — Trigger BUZZ (BetterClaw) to self-update via HTTP API.
#
# Usage:
#   ./scripts/update-buzz.sh           # trigger update (pull + build + exec)
#   ./scripts/update-buzz.sh --check   # only check for updates, don't apply
#   ./scripts/update-buzz.sh --port N  # use different port (default: 3000)

set -euo pipefail

PORT="${BETTERCLAW_PORT:-3000}"
BASE_URL="http://127.0.0.1:${PORT}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)
            echo "Checking for updates..."
            curl -sf "${BASE_URL}/api/runtime/check-update" | python3 -m json.tool
            exit 0
            ;;
        --port)
            PORT="$2"
            BASE_URL="http://127.0.0.1:${PORT}"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--check] [--port N]"
            exit 1
            ;;
    esac
done

echo "Triggering self-update on ${BASE_URL}..."
echo "This will: git pull → cargo build → exec into new binary"
echo ""

curl -sf -X POST "${BASE_URL}/api/runtime/self-update" | python3 -m json.tool

echo ""
echo "If will_exec is true, BUZZ is restarting with the new binary."
echo "Wait ~30s for rebuild + startup, then check: curl -sf ${BASE_URL}/api/threads"
