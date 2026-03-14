#!/bin/sh
set -eu

if [ "$#" -lt 2 ] || [ "$#" -gt 4 ]; then
  echo "Usage: $0 <handle> <token_path> [base_url] [database]" >&2
  exit 1
fi

HANDLE="$1"
TOKEN_PATH="$2"
BASE_URL="${3:-${TIDEPOOL_BASE_URL:-https://spacetimedb.com}}"
DATABASE="${4:-${TIDEPOOL_DATABASE:-}}"

if [ -z "$DATABASE" ]; then
  echo "Database is required as arg 4 or TIDEPOOL_DATABASE" >&2
  exit 1
fi

mkdir -p "$(dirname "$TOKEN_PATH")"

HEADER_FILE=$(mktemp)
BODY_FILE=$(mktemp)
cleanup() {
  rm -f "$HEADER_FILE" "$BODY_FILE"
}
trap cleanup EXIT INT TERM

curl -fsS -D "$HEADER_FILE" \
  "$BASE_URL/v1/database/$DATABASE/reducer/create_account" \
  -H "Content-Type: application/json" \
  -d "$(printf '[%s]' "$(printf '%s' "$HANDLE" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')")" \
  >"$BODY_FILE"

TOKEN=$(awk 'BEGIN{IGNORECASE=1} /^spacetime-identity-token:/ {sub(/\r$/,"",$2); print $2}' "$HEADER_FILE" | tail -n 1)

if [ -z "$TOKEN" ]; then
  echo "Signup succeeded but no spacetime-identity-token header was returned." >&2
  cat "$BODY_FILE" >&2
  exit 1
fi

printf '%s\n' "$TOKEN" >"$TOKEN_PATH"
echo "Stored Tidepool token for handle '$HANDLE' at $TOKEN_PATH"
