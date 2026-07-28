#!/bin/sh
set -eu

if ! command -v actionlint >/dev/null 2>&1; then
    echo "actionlint is required." >&2
    echo "Install it from: https://github.com/rhysd/actionlint" >&2
    exit 2
fi

actionlint
