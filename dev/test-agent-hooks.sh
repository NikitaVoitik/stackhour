#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
HOOK="$ROOT/dev/agent-verify-hook"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/stackhour-agent-hooks.XXXXXX")
trap 'rm -rf "$TMP"' EXIT HUP INT TERM

python3 -m json.tool "$ROOT/.claude/settings.json" >/dev/null
python3 -m json.tool "$ROOT/.codex/hooks.json" >/dev/null

grep -q '"SessionStart"' "$ROOT/.claude/settings.json"
grep -q '"PostToolUse"' "$ROOT/.claude/settings.json"
grep -q '"Stop"' "$ROOT/.claude/settings.json"
grep -q '"SessionStart"' "$ROOT/.codex/hooks.json"
grep -q '"PostToolUse"' "$ROOT/.codex/hooks.json"
grep -q '"Stop"' "$ROOT/.codex/hooks.json"

make_fixture() {
    fixture=$1
    profile=$2
    mkdir -p "$fixture/dev"
    git -C "$fixture" init -q
    git -C "$fixture" config user.email hooks@example.invalid
    git -C "$fixture" config user.name "Hook Test"
    printf '# fixture\n' >"$fixture/README.md"
    printf '[workspace.package]\nversion = "1.0.0"\n' >"$fixture/Cargo.toml"
    printf '#!/bin/sh\nexit 0\n' >"$fixture/dev/verify-fast"
    chmod +x "$fixture/dev/verify-fast"
    git -C "$fixture" add .
    git -C "$fixture" commit -qm baseline
    printf '# Working plan\n\nProfile: %s\n' "$profile" >"$fixture/PLAN.md"
}

assert_profile() {
    fixture=$1
    expected=$2
    actual=$(
        STACKHOUR_AGENT_HOOK_ROOT="$fixture" \
            "$HOOK" select-profile </dev/null
    )
    if [ "$actual" != "$expected" ]; then
        echo "expected profile $expected, got $actual for $fixture" >&2
        exit 1
    fi
}

make_fixture "$TMP/docs" Standard
printf '\nchange\n' >>"$TMP/docs/README.md"
assert_profile "$TMP/docs" standard

make_fixture "$TMP/full" Fast
mkdir -p "$TMP/full/.github/workflows"
printf 'name: test\n' >"$TMP/full/.github/workflows/ci.yml"
assert_profile "$TMP/full" full

make_fixture "$TMP/deep" Fast
mkdir -p "$TMP/deep/src"
printf 'pub fn parse() {}\n' >"$TMP/deep/src/protocol.rs"
assert_profile "$TMP/deep" deep

make_fixture "$TMP/frontend-source" Fast
mkdir -p "$TMP/frontend-source/frontend/src"
printf 'export const value = 1;\n' >"$TMP/frontend-source/frontend/src/App.tsx"
assert_profile "$TMP/frontend-source" standard

make_fixture "$TMP/frontend-tooling" Fast
mkdir -p "$TMP/frontend-tooling/frontend"
printf '{"private":true}\n' >"$TMP/frontend-tooling/frontend/package.json"
assert_profile "$TMP/frontend-tooling" full

make_fixture "$TMP/frontend-capability" Fast
mkdir -p "$TMP/frontend-capability/frontend/src-tauri/capabilities"
printf '{}\n' >"$TMP/frontend-capability/frontend/src-tauri/capabilities/default.json"
assert_profile "$TMP/frontend-capability" deep

make_fixture "$TMP/release" Fast
printf '[workspace.package]\nversion = "2.0.0"\n' >"$TMP/release/Cargo.toml"
assert_profile "$TMP/release" release

make_fixture "$TMP/planned" Deep
printf '\nchange\n' >>"$TMP/planned/README.md"
assert_profile "$TMP/planned" deep

make_fixture "$TMP/failure" Fast
printf '#!/bin/sh\nexit 7\n' >"$TMP/failure/dev/verify-fast"
chmod +x "$TMP/failure/dev/verify-fast"
printf '\nchange\n' >>"$TMP/failure/README.md"
if printf '{"session_id":"test","stop_hook_active":false}' |
    STACKHOUR_AGENT_HOOK_ROOT="$TMP/failure" "$HOOK" stop 2>/dev/null; then
    echo "a first Stop verification failure must block" >&2
    exit 1
else
    status=$?
    if [ "$status" -ne 2 ]; then
        echo "a first Stop failure must exit 2, got $status" >&2
        exit 1
    fi
fi

second=$(
    printf '{"session_id":"test","stop_hook_active":true}' |
        STACKHOUR_AGENT_HOOK_ROOT="$TMP/failure" "$HOOK" stop
)
printf '%s\n' "$second" | grep -q '"continue":false'

echo "agent hook tests passed"
