#!/usr/bin/env bash
set -euo pipefail
export RUST_BACKTRACE=0

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
echo "=== 2. Apply the social graph schema ==="
"$CHIFFON" schema apply --db "$DB" --schema "$SCRIPT_DIR/schema.graph"

echo ""
echo "=== 3. Show the schema ==="
"$CHIFFON" schema show --db "$DB"

echo ""
echo "=== 4. Query: non-archived Projects owned by Alice ==="
echo "   (returns NodeNotFound for now, since no nodes are inserted yet)"
"$CHIFFON" query --db "$DB" --json "$SCRIPT_DIR/query_user_projects.json" || true

echo ""
echo "=== 5. Query: active Users (owners) of project_alpha ==="
"$CHIFFON" query --db "$DB" --json "$SCRIPT_DIR/query_project_authors.json" || true

echo ""
echo "=== 6. Database info ==="
"$CHIFFON" info --db "$DB"

# Clean up
rm -f "$DB"
