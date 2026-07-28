#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/stackhour-release-test.XXXXXX")
trap 'rm -rf "$TMP"' EXIT HUP INT TERM

mkdir -p "$TMP/package/stackhour-test"
cp "$ROOT/target/release/stackhour" "$TMP/package/stackhour-test/stackhour"
cp "$ROOT/deploy/install.sh" "$TMP/package/stackhour-test/install.sh"
cp "$ROOT/LICENSE" "$ROOT/README.md" "$TMP/package/stackhour-test/"
chmod 0755 "$TMP/package/stackhour-test/stackhour" "$TMP/package/stackhour-test/install.sh"
tar -C "$TMP/package" -czf "$TMP/stackhour-test.tar.gz" stackhour-test
tar -C "$TMP" -xzf "$TMP/stackhour-test.tar.gz"

test -x "$TMP/stackhour-test/stackhour"
test -x "$TMP/stackhour-test/install.sh"
"$TMP/stackhour-test/stackhour" --help >/dev/null
