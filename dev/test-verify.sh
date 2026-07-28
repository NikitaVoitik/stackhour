#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)

output() {
    STACKHOUR_VERIFY_DRY_RUN=1 "$ROOT/dev/verify-run" "$1"
}

require_step() {
    level=$1
    name=$2
    text=$(output "$level")
    printf '%s\n' "$text" | grep -q "level=$level step=$name status=start"
}

reject_step() {
    level=$1
    name=$2
    text=$(output "$level")
    if printf '%s\n' "$text" | grep -q "level=$level step=$name status=start"; then
        echo "$level must not include $name" >&2
        exit 1
    fi
}

require_step fast rust-format
require_step fast unsafe-rust-scan
require_step fast rust-check
require_step fast frontend-fast
reject_step fast rust-clippy

require_step standard rust-clippy
require_step standard workspace-tests
require_step standard frontend-standard
reject_step standard feature-matrix

require_step full feature-matrix
require_step full agent-hook-contract
require_step full dependency-policy
require_step full frontend-contract
require_step full frontend-full
require_step full fuzz-clippy
require_step full installer-tests
reject_step full mutations

require_step deep coverage
require_step deep deep-space
require_step deep miri
require_step deep protocol-fuzz
require_step deep mutations
reject_step deep release-build

require_step release mutations
require_step release release-build
require_step release release-tests
require_step release release-package

if STACKHOUR_VERIFY_DRY_RUN=1 "$ROOT/dev/verify-run" unknown >/dev/null 2>&1; then
    echo "unknown verification level must fail" >&2
    exit 1
fi
