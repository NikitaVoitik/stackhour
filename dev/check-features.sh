#!/bin/sh
set -eu

cargo test -p stackhour --no-default-features --features tracker --locked
cargo test -p stackhour --no-default-features --features agent --locked
cargo test -p stackhour --no-default-features --features control --locked
cargo test -p stackhour --no-default-features --features tracker,agent --locked
cargo test -p stackhour --no-default-features --features tracker,agent,control --locked
cargo build -p stackhour --no-default-features --locked
