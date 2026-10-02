#!/usr/bin/env bash
# Prints PLAN.md's "What to measure" numbers for an empty document and for
# 1 MB / 5 MB Markdown files. Pass a large .md file (default: the Tolstoy
# test book); the 1 MB case is its first megabyte.
# Edit latency and reparse numbers come from the ignored bench tests:
#   cargo test --release -p inkmark-view -- --ignored --nocapture
#   cargo test --release -p inkmark-parse -- --ignored --nocapture
set -euo pipefail
cd "$(dirname "$0")/.."
big=${1:-$HOME/Projects/inkmark/tolstoy.md}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
head -c 1000000 "$big" | sed '$d' > "$tmp/1mb.md"
cargo build --release -q
for f in "" "$tmp/1mb.md" "$big"; do
    if [ -z "$f" ]; then echo "== empty"; else echo "== $(basename "$f") ($(stat -c %s "$f") bytes)"; fi
    INKMARK_MEASURE=1 target/release/inkmark $f 2>/dev/null
done
