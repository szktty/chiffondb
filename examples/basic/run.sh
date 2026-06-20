#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
CHIFFON="$ROOT_DIR/target/debug/chiffon"
DB="$SCRIPT_DIR/demo.chiffon"

# Build
cargo build --manifest-path "$ROOT_DIR/Cargo.toml" -q

# Clean up
rm -f "$DB"

echo "=== 1. Create the database ==="
"$CHIFFON" init --db "$DB"

echo ""
echo "=== 2. Apply the schema ==="
"$CHIFFON" schema apply --db "$DB" --schema "$SCRIPT_DIR/schema.graph"

echo ""
echo "=== 3. Show the schema ==="
"$CHIFFON" schema show --db "$DB"

echo ""
echo "=== 4. Show database file info ==="
"$CHIFFON" info --db "$DB"

# Clean up
rm -f "$DB"
