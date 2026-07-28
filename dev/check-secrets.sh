#!/bin/sh
set -eu

patterns='-----BEGIN (RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----
AKIA[0-9A-Z]{16}
gh[pousr]_[A-Za-z0-9]{30,}
xox[baprs]-[A-Za-z0-9-]{20,}'

found=0
old_ifs=$IFS
IFS='
'
for pattern in $patterns; do
    files=$(git grep -l -I -E -e "$pattern" -- . ':(exclude)dev/check-secrets.sh' 2>/dev/null || true)
    if [ -n "$files" ]; then
        found=1
        echo "possible secret pattern found in:" >&2
        printf '%s\n' "$files" >&2
    fi
done
IFS=$old_ifs

if [ "$found" -ne 0 ]; then
    echo "Remove the secret or add a narrowly scoped documented exception." >&2
    exit 1
fi
