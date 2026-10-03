#!/usr/bin/env bash
# Packages a release build: inkmark-<version>-<target>.tar.gz with the
# binary, desktop entry, icon, licenses and README, plus a .sha256 file.
# Used by .github/workflows/release.yml; runs the same way locally:
#   scripts/package.sh [out-dir]       (default: dist)
set -euo pipefail
cd "$(dirname "$0")/.."

out=${1:-dist}
version=$(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "inkmark"))')
target=$(rustc -vV | sed -n 's/^host: //p')
name="inkmark-$version-$target"

# The release profile keeps line tables for profiling and backtraces; the
# shipped binary is stripped, as makepkg does for the Arch package.
CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --release --locked -p inkmark

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name"
install -m755 target/release/inkmark "$stage/$name/inkmark"
install -m644 pack/inkmark.desktop pack/inkmark.svg \
    LICENSE-MIT LICENSE-APACHE README.md "$stage/$name/"

mkdir -p "$out"
tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$out/$name.tar.gz"
