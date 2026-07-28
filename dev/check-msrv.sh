#!/bin/sh
set -eu

MSRV=1.87.0

if ! rustup run "$MSRV" rustc --version >/dev/null 2>&1; then
    echo "Rust $MSRV is required for the minimum-version check." >&2
    echo "Install it with: rustup toolchain install $MSRV" >&2
    exit 2
fi

rustup run "$MSRV" cargo check --workspace --all-targets --all-features --locked
