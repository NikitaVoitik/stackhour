#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"

pattern='(^|[^[:alnum:]_])unsafe([[:space:]]+(fn|impl|trait|extern)|[[:space:]]*\{|[[:space:]]*\()'

scan_rust() {
    scan_pattern=$1
    if command -v rg >/dev/null 2>&1; then
        rg -n "$scan_pattern" crates fuzz --glob '*.rs'
        scan_status=$?
    else
        grep -R -n -E --include='*.rs' --exclude-dir=target \
            "$scan_pattern" crates fuzz
        scan_status=$?
    fi
    if [ "$scan_status" -gt 1 ]; then
        echo "unsafe Rust scan tool failed with exit code $scan_status" >&2
        exit "$scan_status"
    fi
    return "$scan_status"
}

scan_lint_exceptions() {
    scan_pattern=$1
    if command -v rg >/dev/null 2>&1; then
        rg -n "$scan_pattern" crates fuzz Cargo.toml --glob '*.rs' --glob 'Cargo.toml'
        scan_status=$?
    else
        grep -R -n -E --include='*.rs' --include='Cargo.toml' --exclude-dir=target \
            "$scan_pattern" crates fuzz Cargo.toml
        scan_status=$?
    fi
    if [ "$scan_status" -gt 1 ]; then
        echo "unsafe Rust scan tool failed with exit code $scan_status" >&2
        exit "$scan_status"
    fi
    return "$scan_status"
}

if scan_rust "$pattern"; then
    echo "direct unsafe Rust is forbidden" >&2
    exit 1
fi

if scan_lint_exceptions \
    'allow[[:space:]]*\([[:space:]]*unsafe_code|allow[[:space:]]*=[[:space:]]*"unsafe_code"'; then
    echo "unsafe-code lint exceptions are forbidden" >&2
    exit 1
fi

echo "unsafe Rust scan passed"
