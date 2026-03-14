#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "Usage: $0 /path/to/tidepool-repo" >&2
  exit 1
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
TIDEPOOL_REPO=$(CDPATH= cd -- "$1" && pwd)
MODULE_PATH="$TIDEPOOL_REPO/spacetimedb"
OUT_DIR="$REPO_ROOT/src/generated/tidepool"

if [ ! -d "$TIDEPOOL_REPO" ]; then
  echo "Tidepool repo not found: $TIDEPOOL_REPO" >&2
  exit 1
fi

if [ ! -d "$MODULE_PATH" ]; then
  echo "SpacetimeDB module path not found: $MODULE_PATH" >&2
  exit 1
fi

if [ -n "${SPACETIMEDB_CLI:-}" ]; then
  CLI="$SPACETIMEDB_CLI"
elif command -v spacetimedb-cli >/dev/null 2>&1; then
  CLI=$(command -v spacetimedb-cli)
elif command -v spacetime >/dev/null 2>&1; then
  CLI=$(command -v spacetime)
elif [ -x "$HOME/.local/share/spacetime/bin/current/spacetimedb-cli" ]; then
  CLI="$HOME/.local/share/spacetime/bin/current/spacetimedb-cli"
else
  echo "Unable to find a SpacetimeDB CLI. Set SPACETIMEDB_CLI or add it to PATH." >&2
  exit 1
fi

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

"$CLI" generate --lang rust --module-path "$MODULE_PATH" --out-dir "$OUT_DIR" --yes

MOD_FILE="$OUT_DIR/mod.rs"
if [ -f "$MOD_FILE" ]; then
  python3 - "$MOD_FILE" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
text = text.replace("#[derive(Default)]\n#[allow(non_snake_case)]\n#[doc(hidden)]\npub struct DbUpdate {", "#[derive(Debug, Default)]\n#[allow(non_snake_case)]\n#[doc(hidden)]\npub struct DbUpdate {")
text = text.replace("pub struct RemoteModule;", "#[derive(Debug)]\npub struct RemoteModule;")
path.write_text(text)
PY
fi

echo "Generated Tidepool Rust bindings into $OUT_DIR"
