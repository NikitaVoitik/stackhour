#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"

pattern='(^|[^[:alnum:]_])unsafe([[:space:]]+(fn|impl|trait|extern)|[[:space:]]*\{|[[:space:]]*\()'

if rg -n "$pattern" crates fuzz --glob '*.rs'; then
    echo "direct unsafe Rust is forbidden" >&2
    exit 1
fi

if rg -n 'allow[[:space:]]*\([[:space:]]*unsafe_code|allow[[:space:]]*=[[:space:]]*"unsafe_code"' \
    crates fuzz Cargo.toml --glob '*.rs' --glob 'Cargo.toml'; then
    echo "unsafe-code lint exceptions are forbidden" >&2
    exit 1
fi

echo "unsafe Rust scan passed"
