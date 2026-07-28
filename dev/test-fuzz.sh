#!/bin/sh
set -eu

if ! command -v cargo-fuzz >/dev/null 2>&1; then
    echo "cargo-fuzz is required." >&2
    echo "Install it with: cargo install --locked cargo-fuzz" >&2
    exit 2
fi

FUZZ_TMP=$(mktemp -d "${TMPDIR:-/tmp}/stackhour-fuzz.XXXXXX")
trap 'rm -rf "$FUZZ_TMP"' EXIT HUP INT TERM
mkdir -p "$FUZZ_TMP/corpus" "$FUZZ_TMP/artifacts"
cp fuzz/corpus/protocol_messages/seed-* "$FUZZ_TMP/corpus/"

rustup run nightly cargo fuzz run protocol_messages "$FUZZ_TMP/corpus" -- \
    "-artifact_prefix=$FUZZ_TMP/artifacts/" \
    -max_total_time=60
