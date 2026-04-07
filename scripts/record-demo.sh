#!/usr/bin/env bash
# Record demo GIF with mock data — no real sessions exposed
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
DB_PATH="$HOME/.claude/recall.db"
DB_BAK="$HOME/.claude/recall.db.demo-backup"
WAL="$HOME/.claude/recall.db-wal"
SHM="$HOME/.claude/recall.db-shm"

echo "==> Backing up real database..."
[ -f "$DB_PATH" ] && cp "$DB_PATH" "$DB_BAK"

echo "==> Creating mock database..."
rm -f "$DB_PATH" "$WAL" "$SHM"
sqlite3 "$DB_PATH" < "$SCRIPT_DIR/mock-data.sql"

echo "==> Recording demo..."
cd "$REPO_DIR"
vhs "$SCRIPT_DIR/record-demo.tape"

echo "==> Restoring real database..."
rm -f "$DB_PATH" "$WAL" "$SHM"
[ -f "$DB_BAK" ] && mv "$DB_BAK" "$DB_PATH"

echo "==> Done! Output: demo.gif"
