#!/usr/bin/env bash
# Record demo.gif from the made-up sessions in scripts/demo-sessions.py. Your own sessions and
# index are never read or written: everything lives in target/demo.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v vhs >/dev/null || { echo "record-demo: needs vhs (brew install vhs)" >&2; exit 1; }
cargo build --release --locked
rm -rf target/demo
python3 scripts/demo-sessions.py target/demo/.claude/projects
CLAUDE_CONFIG_DIR="$PWD/target/demo/.claude" target/release/claude-resume sync --quiet
vhs scripts/record-demo.tape
echo "record-demo: wrote demo.gif"
