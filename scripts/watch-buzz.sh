#!/usr/bin/env bash
# watch-buzz.sh — Supervise BUZZ (BetterClaw) process. Auto-restarts on crash.
#
# Usage:
#   ./scripts/watch-buzz.sh              # run in foreground (Ctrl+C to stop)
#   ./scripts/watch-buzz.sh --daemon     # run in background
#   ./scripts/watch-buzz.sh --stop       # stop the daemon
#   ./scripts/watch-buzz.sh --status     # check if BUZZ is running
#
# Environment:
#   BETTERCLAW_ENV_PATH  — path to .env file (default: ~/.betterclaw/.env)
#   BETTERCLAW_PORT      — health check port (default: 3000)
#   WATCHDOG_INTERVAL    — seconds between health checks (default: 10)
#   WATCHDOG_MAX_RESTARTS — max restarts before giving up (default: 5)
#   WATCHDOG_LOG         — watchdog log file (default: ~/.betterclaw/watchdog.log)

set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PID_FILE="${HOME}/.betterclaw/buzz.pid"
WATCHDOG_PID_FILE="${HOME}/.betterclaw/watchdog.pid"
HEALTH_PORT="${BETTERCLAW_PORT:-3000}"
HEALTH_URL="http://127.0.0.1:${HEALTH_PORT}/health"
INTERVAL="${WATCHDOG_INTERVAL:-10}"
MAX_RESTARTS="${WATCHDOG_MAX_RESTARTS:-5}"
LOG="${WATCHDOG_LOG:-${HOME}/.betterclaw/watchdog.log}"
ENV_PATH="${BETTERCLAW_ENV_PATH:-${HOME}/.betterclaw/.env}"

log() {
    local msg="[$(date '+%Y-%m-%d %H:%M:%S')] $*"
    echo "$msg" | tee -a "$LOG"
}

buzz_pid() {
    if [[ -f "$PID_FILE" ]]; then
        local pid
        pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            echo "$pid"
            return 0
        fi
    fi
    # Fallback: find by process name
    pgrep -f "target/debug/betterclaw" 2>/dev/null | head -1
}

is_healthy() {
    curl -sf --max-time 3 "$HEALTH_URL" > /dev/null 2>&1
}

start_buzz() {
    log "Starting BUZZ..."
    cd "$REPO_DIR"

    local env_args=()
    if [[ -f "$ENV_PATH" ]]; then
        env_args+=("BETTERCLAW_ENV_PATH=$ENV_PATH")
    fi

    env "${env_args[@]}" nohup target/debug/betterclaw >> "${HOME}/.betterclaw/buzz.log" 2>&1 &
    local pid=$!
    echo "$pid" > "$PID_FILE"
    log "BUZZ started (pid=$pid), waiting for health check..."

    # Wait up to 30s for health endpoint
    local waited=0
    while [[ $waited -lt 30 ]]; do
        sleep 2
        waited=$((waited + 2))
        if is_healthy; then
            log "BUZZ healthy after ${waited}s (pid=$pid)"
            return 0
        fi
        # Check if process died during startup
        if ! kill -0 "$pid" 2>/dev/null; then
            log "BUZZ died during startup (pid=$pid)"
            return 1
        fi
    done

    log "BUZZ health check timed out after 30s (pid=$pid)"
    return 1
}

stop_buzz() {
    local pid
    pid=$(buzz_pid) || true
    if [[ -n "${pid:-}" ]]; then
        log "Stopping BUZZ (pid=$pid)..."
        kill "$pid" 2>/dev/null || true
        sleep 2
        kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f "$PID_FILE"
    log "BUZZ stopped"
}

stop_watchdog() {
    if [[ -f "$WATCHDOG_PID_FILE" ]]; then
        local wpid
        wpid=$(cat "$WATCHDOG_PID_FILE")
        if kill -0 "$wpid" 2>/dev/null; then
            log "Stopping watchdog (pid=$wpid)..."
            kill "$wpid" 2>/dev/null || true
        fi
        rm -f "$WATCHDOG_PID_FILE"
    fi
}

do_status() {
    local pid
    pid=$(buzz_pid) || true
    if [[ -n "${pid:-}" ]]; then
        echo "BUZZ running (pid=$pid)"
        if is_healthy; then
            echo "Health check: OK"
        else
            echo "Health check: FAILING (process alive but /health unreachable)"
        fi
    else
        echo "BUZZ not running"
    fi

    if [[ -f "$WATCHDOG_PID_FILE" ]]; then
        local wpid
        wpid=$(cat "$WATCHDOG_PID_FILE")
        if kill -0 "$wpid" 2>/dev/null; then
            echo "Watchdog running (pid=$wpid)"
        else
            echo "Watchdog stale (pid file exists but process dead)"
        fi
    else
        echo "Watchdog not running"
    fi
}

watchdog_loop() {
    echo $$ > "$WATCHDOG_PID_FILE"
    log "Watchdog started (pid=$$, interval=${INTERVAL}s, max_restarts=${MAX_RESTARTS})"

    local restart_count=0
    local last_restart_time=0

    while true; do
        sleep "$INTERVAL"

        local pid
        pid=$(buzz_pid) || true

        if [[ -n "${pid:-}" ]]; then
            # Process exists, check health
            if is_healthy; then
                # All good — reset restart counter if enough time has passed
                local now
                now=$(date +%s)
                if [[ $((now - last_restart_time)) -gt 300 ]]; then
                    restart_count=0
                fi
                continue
            fi
            # Process alive but unhealthy — give it a chance
            log "BUZZ alive (pid=$pid) but /health unreachable, waiting..."
            sleep 5
            if is_healthy; then
                continue
            fi
            log "BUZZ still unhealthy, restarting..."
            stop_buzz
        else
            log "BUZZ not running"
        fi

        # Restart
        restart_count=$((restart_count + 1))
        if [[ $restart_count -gt $MAX_RESTARTS ]]; then
            log "MAX RESTARTS ($MAX_RESTARTS) exceeded. Giving up."
            log "Manual intervention required. Check ~/.betterclaw/buzz.log"
            rm -f "$WATCHDOG_PID_FILE"
            exit 1
        fi

        log "Restart attempt $restart_count/$MAX_RESTARTS"
        last_restart_time=$(date +%s)

        if start_buzz; then
            log "Restart successful"
        else
            log "Restart failed, will retry in ${INTERVAL}s"
        fi
    done
}

# Main
case "${1:-}" in
    --daemon)
        log "Starting watchdog in background..."
        nohup "$0" >> "$LOG" 2>&1 &
        echo "Watchdog started (background). Logs: $LOG"
        ;;
    --stop)
        stop_watchdog
        stop_buzz
        ;;
    --status)
        do_status
        ;;
    --help|-h)
        echo "Usage: $0 [--daemon|--stop|--status|--help]"
        echo ""
        echo "Supervises BUZZ (BetterClaw) process with auto-restart on crash."
        echo ""
        echo "  --daemon   Run watchdog in background"
        echo "  --stop     Stop watchdog and BUZZ"
        echo "  --status   Show current status"
        echo "  --help     Show this help"
        ;;
    *)
        watchdog_loop
        ;;
esac
