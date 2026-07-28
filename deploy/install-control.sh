#!/bin/sh
set -eu

case "${1:-}" in
  hub|node|ssh) ;;
  *)
    echo "usage: deploy/install-control.sh <hub|node|ssh> [options]" >&2
    exit 2
    ;;
esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(dirname -- "$script_dir")
install_dir=${STACKHOUR_INSTALL_DIR:-"$HOME/.local/bin"}

command -v cargo >/dev/null 2>&1 || {
  echo "cargo is required. Install Rust from https://rustup.rs." >&2
  exit 1
}

cd "$repo_dir"
cargo build --locked --release -p stackhour
mkdir -p "$install_dir"
install -m 0755 "$repo_dir/target/release/stackhour" "$install_dir/stackhour"

exec "$install_dir/stackhour" control install "$@"
