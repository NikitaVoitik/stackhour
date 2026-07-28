#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
BIN="$ROOT/target/verification-tools/bin"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/stackhour-tools.XXXXXX")
trap 'rm -rf "$TMP"' EXIT HUP INT TERM

mkdir -p "$BIN"

if ! command -v pnpm >/dev/null 2>&1; then
    echo "pnpm 11.15.0 is required. Enable Corepack before bootstrapping." >&2
    exit 2
fi
pnpm --dir "$ROOT/frontend" install --frozen-lockfile

rustup toolchain install 1.87.0 --component rustfmt --component clippy
rustup toolchain install nightly --component miri --component rust-src

cargo install --locked --root "$ROOT/target/verification-tools" cargo-deny
cargo install --locked --root "$ROOT/target/verification-tools" cargo-llvm-cov
cargo install --locked --root "$ROOT/target/verification-tools" cargo-mutants
cargo install --locked --root "$ROOT/target/verification-tools" cargo-fuzz

system=$(uname -s)
machine=$(uname -m)
case "$system:$machine" in
    Linux:x86_64)
        actionlint_os=linux
        actionlint_arch=amd64
        shellcheck_os=linux
        shellcheck_arch=x86_64
        ;;
    Linux:aarch64|Linux:arm64)
        actionlint_os=linux
        actionlint_arch=arm64
        shellcheck_os=linux
        shellcheck_arch=aarch64
        ;;
    Darwin:arm64)
        if command -v brew >/dev/null 2>&1; then
            brew install actionlint shellcheck
            exit 0
        fi
        echo "Homebrew is required to install actionlint and ShellCheck on macOS." >&2
        exit 2
        ;;
    *)
        echo "Unsupported verification-tool platform: $system $machine" >&2
        exit 2
        ;;
esac

curl --fail --location --silent --show-error \
    "https://github.com/rhysd/actionlint/releases/download/v1.7.7/actionlint_1.7.7_${actionlint_os}_${actionlint_arch}.tar.gz" \
    --output "$TMP/actionlint.tar.gz"
tar -C "$TMP" -xzf "$TMP/actionlint.tar.gz" actionlint
install -m 0755 "$TMP/actionlint" "$BIN/actionlint"

curl --fail --location --silent --show-error \
    "https://github.com/koalaman/shellcheck/releases/download/v0.10.0/shellcheck-v0.10.0.${shellcheck_os}.${shellcheck_arch}.tar.xz" \
    --output "$TMP/shellcheck.tar.xz"
tar -C "$TMP" -xJf "$TMP/shellcheck.tar.xz"
install -m 0755 \
    "$TMP/shellcheck-v0.10.0/shellcheck" \
    "$BIN/shellcheck"

echo "Verification tools installed in $BIN"
