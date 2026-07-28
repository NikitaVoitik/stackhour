#!/bin/sh
set -eu

cargo test -p stackhour --no-default-features --features bridge --locked
cargo test -p stackhour --no-default-features --features tracker --locked
cargo test -p stackhour --no-default-features --features agent --locked
cargo test -p stackhour --no-default-features --features control --locked
cargo test -p stackhour --no-default-features --features tracker,agent --locked
cargo build -p stackhour --no-default-features --locked

if cargo tree -p stackhour --no-default-features --features bridge -i libsqlite3-sys >/dev/null 2>&1; then
    echo "bridge unexpectedly depends on libsqlite3-sys" >&2
    exit 1
fi
if cargo tree -p stackhour --no-default-features --features bridge -i rusqlite >/dev/null 2>&1; then
    echo "bridge unexpectedly depends on rusqlite" >&2
    exit 1
fi
if cargo tree -p stackhour --no-default-features --features bridge -i axum >/dev/null 2>&1; then
    echo "bridge unexpectedly depends on axum" >&2
    exit 1
fi
cargo tree -p stackhour --no-default-features --features bridge -i tokio >/dev/null
