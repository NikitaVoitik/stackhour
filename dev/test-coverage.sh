#!/bin/sh
set -eu

if ! cargo llvm-cov --version >/dev/null 2>&1; then
    echo "cargo-llvm-cov is required." >&2
    echo "Install it with: cargo install --locked cargo-llvm-cov" >&2
    exit 2
fi

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cleanup_profiles() {
    # Integration tests clear the child environment. Instrumented child
    # binaries then use LLVM's default path in their crate directory.
    find "$ROOT/crates" -type f -name '*.profraw' -delete
}
trap cleanup_profiles EXIT HUP INT TERM
cleanup_profiles

mkdir -p target/verification
cargo llvm-cov --workspace --all-features --locked \
    --fail-under-lines 70 \
    --lcov \
    --output-path target/verification/lcov.info
