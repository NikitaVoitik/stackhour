#!/bin/sh
set -eu

repository=${STACKHOUR_GITHUB_REPOSITORY:-NikitaVoitik/stackhour}
install_dir=${STACKHOUR_INSTALL_DIR:-"$HOME/.local/bin"}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" 2>/dev/null && pwd || pwd)
source_binary="$script_dir/stackhour"
temporary_dir=

cleanup() {
  if [ -n "$temporary_dir" ] && [ -d "$temporary_dir" ]; then
    rm -rf -- "$temporary_dir"
  fi
}
trap cleanup EXIT HUP INT TERM

if [ ! -x "$source_binary" ]; then
  command -v curl >/dev/null 2>&1 || {
    echo "curl is required." >&2
    exit 1
  }
  command -v tar >/dev/null 2>&1 || {
    echo "tar is required." >&2
    exit 1
  }

  system_name=$(uname -s)
  machine_name=$(uname -m)
  case "$system_name/$machine_name" in
    Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-gnu ;;
    Linux/arm64|Linux/aarch64) target=aarch64-unknown-linux-gnu ;;
    Darwin/arm64|Darwin/aarch64) target=aarch64-apple-darwin ;;
    Darwin/x86_64|Darwin/amd64)
      echo "Stackhour does not support Intel macOS." >&2
      exit 1
      ;;
    *)
      echo "Stackhour does not support this operating system and CPU type." >&2
      exit 1
      ;;
  esac

  asset="stackhour-$target.tar.gz"
  release_root="https://github.com/$repository/releases/latest/download"
  temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/stackhour-install.XXXXXX")

  curl --fail --location --silent --show-error \
    "$release_root/$asset" \
    --output "$temporary_dir/$asset"
  curl --fail --location --silent --show-error \
    "$release_root/SHA256SUMS" \
    --output "$temporary_dir/SHA256SUMS"

  expected=$(awk -v asset="$asset" '$2 == asset { print $1 }' "$temporary_dir/SHA256SUMS")
  if [ -z "$expected" ]; then
    echo "The release checksum does not list $asset." >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temporary_dir/$asset" | awk '{ print $1 }')
  else
    actual=$(shasum -a 256 "$temporary_dir/$asset" | awk '{ print $1 }')
  fi
  if [ "$actual" != "$expected" ]; then
    echo "The Stackhour release checksum is invalid." >&2
    exit 1
  fi

  tar -xzf "$temporary_dir/$asset" -C "$temporary_dir"
  source_binary="$temporary_dir/stackhour-$target/stackhour"
fi

mkdir -p "$install_dir"
install -m 0755 "$source_binary" "$install_dir/stackhour"
echo "Installed Stackhour at $install_dir/stackhour"

if [ "$#" -gt 0 ]; then
  exec "$install_dir/stackhour" "$@"
fi
