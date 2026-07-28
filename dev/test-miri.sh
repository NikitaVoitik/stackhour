#!/bin/sh
set -eu

if ! rustup run nightly cargo miri --version >/dev/null 2>&1; then
    echo "Miri and the nightly toolchain are required." >&2
    echo "Install them with: rustup toolchain install nightly --component miri" >&2
    exit 2
fi

# The domain and store crates link SQLite through FFI, which Miri cannot run.
# Run the pure numeric and time parsers. Disable isolation because the shared
# test binary also initializes filesystem-backed test support.
MIRIFLAGS="-Zmiri-disable-isolation" \
    rustup run nightly cargo miri test -p stackhour-core jsnum::tests::
MIRIFLAGS="-Zmiri-disable-isolation" \
    rustup run nightly cargo miri test -p stackhour-core timeparse::tests::
