#!/bin/sh
set -eu

if ! command -v shellcheck >/dev/null 2>&1; then
    echo "ShellCheck is required. Install it with your system package manager." >&2
    exit 2
fi

shellcheck deploy/install.sh deploy/install-control.sh deploy/setup-mac.sh deploy/test-install.sh dev/*.sh
