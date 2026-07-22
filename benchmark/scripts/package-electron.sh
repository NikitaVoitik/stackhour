#!/usr/bin/env bash
set -euo pipefail
base="$(cd "$(dirname "$0")/.." && pwd)"
source="$base/node_modules/electron/dist"
target="$base/artifacts/StackhourBenchSvelteElectron-linux-x64"
test -x "$source/electron"
rm -rf "$target"
mkdir -p "$target"
cp -a "$source/." "$target/"
mv "$target/electron" "$target/StackhourBenchSvelteElectron"
mkdir -p "$target/resources/app/electron" "$target/resources/app/dist"
cp "$base/electron/app-package.json" "$target/resources/app/package.json"
cp "$base/electron/main.mjs" "$base/electron/preload.cjs" "$base/electron/backend.mjs" "$target/resources/app/electron/"
cp -a "$base/dist/." "$target/resources/app/dist/"
