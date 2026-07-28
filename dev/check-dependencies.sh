#!/bin/sh
set -eu

if ! command -v cargo-deny >/dev/null 2>&1; then
    echo "cargo-deny is required." >&2
    echo "Install it with: cargo install --locked cargo-deny" >&2
    exit 2
fi

cargo deny check
cargo deny \
    --manifest-path fuzz/Cargo.toml \
    --config fuzz/deny.toml \
    --offline \
    check
