#!/usr/bin/env bash
# Prints PLAN.md's "What to measure" numbers from the real app window, for
# an empty document and the generated 1, 5 and 10 MB books: startup,
# parse settled, RSS, split scrolling with both minimaps, keystroke →
# frame in both panes, save time, and idle CPU. Each run opens a window
# for about 20 seconds (`INKMARK_MEASURE`, crates/inkmark/src/measure.rs),
# with its own empty config and state directories.
#
#   scripts/measure.sh            # the generated fixtures
#   scripts/measure.sh book.md    # one file of your own (copied first)
#
# Set INKMARK_BENCH_OUT to collect JSON lines (scripts/bench.sh does).
# Edit latency and parse numbers per component come from scripts/bench.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q -p inkmark -p inkmark-bench
fixtures=${INKMARK_FIXTURES:-target/fixtures}
target/release/inkmark-bench fixtures --dir "$fixtures" > /dev/null
# Under target/, so saves hit the real disk (/tmp is often tmpfs).
mkdir -p target
tmp=$(mktemp -d -p target measure.XXXXXX)
trap 'rm -rf "$tmp"' EXIT
# A clean profile: the default layout (split, both minimaps, sidebar),
# no config.toml, the built-in theme, and nothing written to your recent
# files, layout or session log. Fonts are the system fontconfig defaults
# (a user fonts.conf lives under XDG_CONFIG_HOME too).
export XDG_CONFIG_HOME="$tmp/config" XDG_STATE_HOME="$tmp/state"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

run() { # label file
    local label=$1 file=${2:-}
    if [ -z "$file" ]; then
        echo "== empty"
        INKMARK_MEASURE=1 INKMARK_MEASURE_LABEL=$label target/release/inkmark 2>/dev/null
        return
    fi
    # Saving writes the file, so measure a scratch copy beside the others.
    cp "$file" "$tmp/$label.md"
    echo "== $label ($(stat -c %s "$file") bytes)"
    INKMARK_MEASURE=1 INKMARK_MEASURE_SAVE=1 INKMARK_MEASURE_LABEL=$label \
        target/release/inkmark "$tmp/$label.md" 2>/dev/null
}

if [ $# -gt 0 ]; then
    run custom "$1"
else
    run empty
    run 1mb "$fixtures/prose-1mb.md"
    run 5mb "$fixtures/prose-5mb.md"
    run 10mb "$fixtures/prose-10mb.md"
fi
