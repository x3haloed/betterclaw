#!/bin/sh
set -eu

if [ "$#" -lt 4 ]; then
  echo "Usage: $0 <domain_id> <handle_a> <handle_b> <task_text>" >&2
  exit 1
fi

DOMAIN_ID="$1"
HANDLE_A="$2"
HANDLE_B="$3"
TASK_TEXT="$4"

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
RUN_ID=$(date +"%Y%m%d-%H%M%S")
RUN_ROOT="$REPO_ROOT/.tidepool-runs/$RUN_ID"
AGENT_A_ROOT="$RUN_ROOT/agent-a"
AGENT_B_ROOT="$RUN_ROOT/agent-b"

BASE_URL="${TIDEPOOL_BASE_URL:-https://spacetimedb.com}"
DATABASE="${TIDEPOOL_DATABASE:-tidepool-dev}"
PORT_A="${BETTERCLAW_PORT_A:-3101}"
PORT_B="${BETTERCLAW_PORT_B:-3102}"

mkdir -p "$AGENT_A_ROOT/workspace" "$AGENT_B_ROOT/workspace"

cleanup() {
  if [ -n "${PID_A:-}" ]; then
    kill "$PID_A" 2>/dev/null || true
  fi
  if [ -n "${PID_B:-}" ]; then
    kill "$PID_B" 2>/dev/null || true
  fi
}

trap cleanup EXIT INT TERM

echo "Starting Tidepool pair run in $RUN_ROOT"

(
  cd "$REPO_ROOT"
  BETTERCLAW_DB_PATH="$AGENT_A_ROOT/betterclaw.db" \
  BETTERCLAW_PORT="$PORT_A" \
  BETTERCLAW_LOG=debug \
  TIDEPOOL_AGENT_ID="default" \
  TIDEPOOL_BASE_URL="$BASE_URL" \
  TIDEPOOL_DATABASE="$DATABASE" \
  TIDEPOOL_HANDLE="$HANDLE_A" \
  TIDEPOOL_TOKEN_PATH="$AGENT_A_ROOT/tidepool_token" \
  TIDEPOOL_SEED_DOMAIN_IDS="$DOMAIN_ID" \
  cargo run >"$AGENT_A_ROOT/runtime.log" 2>&1
) &
PID_A=$!

(
  cd "$REPO_ROOT"
  BETTERCLAW_DB_PATH="$AGENT_B_ROOT/betterclaw.db" \
  BETTERCLAW_PORT="$PORT_B" \
  BETTERCLAW_LOG=debug \
  TIDEPOOL_AGENT_ID="default" \
  TIDEPOOL_BASE_URL="$BASE_URL" \
  TIDEPOOL_DATABASE="$DATABASE" \
  TIDEPOOL_HANDLE="$HANDLE_B" \
  TIDEPOOL_TOKEN_PATH="$AGENT_B_ROOT/tidepool_token" \
  TIDEPOOL_SEED_DOMAIN_IDS="$DOMAIN_ID" \
  cargo run >"$AGENT_B_ROOT/runtime.log" 2>&1
) &
PID_B=$!

echo "Waiting for both agents to bootstrap..."
sleep "${PAIR_BOOTSTRAP_WAIT_SECS:-10}"

if [ ! -f "$AGENT_A_ROOT/tidepool_token" ]; then
  echo "Agent A token is missing: $AGENT_A_ROOT/tidepool_token" >&2
  echo "Register it first with ./scripts/tidepool_register.sh $HANDLE_A $AGENT_A_ROOT/tidepool_token $BASE_URL $DATABASE" >&2
  exit 1
fi

if [ ! -f "$AGENT_B_ROOT/tidepool_token" ]; then
  echo "Agent B token is missing: $AGENT_B_ROOT/tidepool_token" >&2
  echo "Register it first with ./scripts/tidepool_register.sh $HANDLE_B $AGENT_B_ROOT/tidepool_token $BASE_URL $DATABASE" >&2
  exit 1
fi

TOKEN_A=$(tr -d '\n' <"$AGENT_A_ROOT/tidepool_token")

curl -fsS "$BASE_URL/v1/database/$DATABASE/reducer/post_message" \
  -H "Authorization: Bearer $TOKEN_A" \
  -H "Content-Type: application/json" \
  -d "[${DOMAIN_ID}, $(printf '%s' "$TASK_TEXT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'), null]" \
  >/dev/null

echo "Seeded evaluation task into domain $DOMAIN_ID"
echo "Agent A log: $AGENT_A_ROOT/runtime.log"
echo "Agent B log: $AGENT_B_ROOT/runtime.log"
echo "Agent A DB:  $AGENT_A_ROOT/betterclaw.db"
echo "Agent B DB:  $AGENT_B_ROOT/betterclaw.db"
echo "Press Ctrl+C when you are done observing the run."

wait
