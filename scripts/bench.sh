#!/usr/bin/env bash
# Runs inkmark's benchmarks and writes one report per commit:
#
#   scripts/bench.sh [quick|full] [--compare BASE.json] [--strict]
#
#   quick  micro benches (criterion, short) and frame benches with fewer
#          iterations. CI runs this as a smoke test.
#   full   everything at full length, the old #[ignore]d bench tests, and
#          the app-level numbers from a real window (scripts/measure.sh,
#          needs a Wayland or X display). Authoritative numbers come from
#          this mode on the Omarchy host.
#
# Output: target/bench/<sha>.json and .md (plus the raw .jsonl). Fixtures
# are generated into target/fixtures from a fixed seed; nothing outside
# the repo is read. With --compare, the run is compared against a
# baseline report (docs/perf/baseline-*.json): a median 15% slower, or a
# p95 over a bench's 16 ms budget, is flagged, and --strict makes that a
# failing exit status.
set -euo pipefail
cd "$(dirname "$0")/.."

mode=quick
compare=""
strict=""
while [ $# -gt 0 ]; do
    case $1 in
        quick | full) mode=$1 ;;
        --compare) compare=$2; shift ;;
        --strict) strict=1 ;;
        *) echo "usage: scripts/bench.sh [quick|full] [--compare BASE.json] [--strict]" >&2; exit 2 ;;
    esac
    shift
done

sha=$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
if [ -n "$(git status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    sha="$sha-dirty"
fi
out=target/bench
mkdir -p "$out"
lines="$out/$sha.jsonl"
rm -f "$lines"
rm -rf target/criterion

# The same fonts and no personal settings, wherever this runs: a user
# fontconfig and inkmark's config both live under XDG_CONFIG_HOME.
profile=$(mktemp -d)
trap 'rm -rf "$profile"' EXIT
export XDG_CONFIG_HOME="$profile/config" XDG_STATE_HOME="$profile/state"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

export INKMARK_BENCH_MODE=$mode
export INKMARK_BENCH_OUT="$PWD/$lines"
export INKMARK_FIXTURES="$PWD/target/fixtures"

echo "== build"
cargo build --release -q -p inkmark-bench
if [ "$mode" = quick ]; then notes=2000; else notes=10000; fi
echo "== fixtures"
target/release/inkmark-bench fixtures --dir "$INKMARK_FIXTURES" --notes "$notes" | head -1

# Runs "$@" with its output in a log, shows the lines matching the
# filter, and fails the script if the command failed.
filtered() { # grep-args -- command...
    local args=() log
    while [ "$1" != -- ]; do args+=("$1"); shift; done
    shift
    log=$(mktemp)
    local status=0
    "$@" > "$log" 2>&1 || status=$?
    grep "${args[@]}" "$log" || true
    if [ $status -ne 0 ]; then
        echo "failed ($status): $*" >&2
        tail -30 "$log" >&2
        rm -f "$log"
        exit $status
    fi
    rm -f "$log"
}

echo "== micro and frame benches ($mode)"
filtered -vE '^(Benchmarking|Found [0-9]+ outliers|  +[0-9]+ \(|running 0 tests|test result|Gnuplot|\s*$)|change:|thrpt:|No change|Change within|Performance has' \
    -- cargo bench --workspace --locked -q

echo "== bench tests"
if [ "$mode" = full ]; then
    filtered -vE '^(running|test result|\s*$)|\.\.\. ok$' \
        -- cargo test --release --workspace --locked -q -- --ignored --nocapture bench_
else
    filtered -E '^search/' \
        -- cargo test --release -p inkmark --locked -q -- --ignored --nocapture bench_folder_search
fi

if [ "$mode" = full ]; then
    if [ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]; then
        echo "== app (real window)"
        scripts/measure.sh | grep -v 'idle repaint cause'
    else
        echo "== app: skipped (no display)"
    fi
fi

# Host details the report can't read from /proc.
host=()
if command -v lspci > /dev/null; then
    gpu=$(lspci 2>/dev/null | grep -iE 'vga|3d' | sed 's/.*: //' | paste -sd ';' -)
    host+=(--host "gpu=$gpu")
fi
if command -v pacman > /dev/null; then
    host+=(--host "mesa=$(pacman -Q mesa 2>/dev/null | cut -d' ' -f2)")
fi
if command -v hyprctl > /dev/null && command -v jq > /dev/null; then
    monitors=$(hyprctl monitors -j 2>/dev/null \
        | jq -r '.[] | "\(.name) \(.width)x\(.height)@\(.refreshRate | floor)Hz scale \(.scale)"' \
        | paste -sd ';' - || true)
    host+=(--host "monitors=$monitors")
fi
if command -v fc-match > /dev/null; then
    host+=(--host "fonts=$(fc-match -f '%{family[0]}' monospace) / $(fc-match -f '%{family[0]}' sans-serif)")
fi
host+=(--host "rustc=$(rustc --version | cut -d' ' -f2)")

echo "== report"
target/release/inkmark-bench report --sha "$sha" --mode "$mode" --lines "$lines" \
    --criterion target/criterion --out "$out" "${host[@]}"

if [ -n "$compare" ]; then
    echo "== compare with $compare"
    if [ -n "$strict" ]; then
        target/release/inkmark-bench compare "$compare" "$out/$sha.json"
    else
        target/release/inkmark-bench compare "$compare" "$out/$sha.json" --warn-only
    fi
fi
