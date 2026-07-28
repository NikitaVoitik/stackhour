#!/bin/sh
set -eu

if ! command -v cargo-mutants >/dev/null 2>&1; then
    echo "cargo-mutants is required." >&2
    echo "Install it with: cargo install --locked cargo-mutants" >&2
    exit 2
fi

cargo mutants \
    --package stackhour-domain \
    --file 'crates/stackhour-domain/src/protocol.rs' \
    --timeout 120
cargo mutants \
    --package stackhour-store \
    --file 'crates/stackhour-store/src/db.rs' \
    --re 'migrate_[1-4]|applied_migrations|run_migrations|schema_version|open_db' \
    --timeout 120
