#!/bin/bash
# Records the outputs the report quotes, from a clean export of the pinned commit, into
# docs/output/<crate>/. Usage: docs/record.sh [crate ...] (default: every docs/record.d/*.sh)
# REPORT_WORK is a private directory for the export and the build (default: a new temp dir).
set -euo pipefail
PIN=673716c
DOCS=$(cd "$(dirname "$0")" && pwd)
WORK=${REPORT_WORK:-$(mktemp -d)}
export SRC=$WORK/src
export CARGO_TARGET_DIR=$WORK/target CARGO_BUILD_BUILD_DIR=$WORK/build
if [ ! -f "$SRC/Cargo.toml" ]; then
    mkdir -p "$SRC"
    git -C "$DOCS/.." archive $PIN | tar -x -C "$SRC"
fi
cd "$SRC"
# Runs a kernel headless and keeps its serial log: kernel <name> <arch> <file> [release]
kernel() {
    cargo xtask kernel --kernel "$1" --target "$2" --headless ${4:+--release} >/dev/null 2>&1
    cp "$CARGO_TARGET_DIR/kernel/$1-$2.log" "$OUT/$3"
}
if [ $# -eq 0 ]; then set -- $(basename -a "$DOCS"/record.d/*.sh | sed 's/\.sh$//'); fi
for crate in "$@"; do
    export OUT=$DOCS/output/$crate
    mkdir -p "$OUT"
    echo "=== $crate"
    source "$DOCS/record.d/$crate.sh"
done
