#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/stackhour-installer-test.XXXXXX")

cleanup() {
  rm -rf -- "$test_dir"
}
trap cleanup EXIT HUP INT TERM

fail() {
  echo "installer test failed: $1" >&2
  exit 1
}

run_download_case() {
  system_name=$1
  machine_name=$2
  expected_target=$3
  case_dir="$test_dir/$expected_target"
  fake_bin="$case_dir/bin"
  install_dir="$case_dir/install"
  mkdir -p "$fake_bin" "$install_dir"

  cp "$root/deploy/install.sh" "$case_dir/install.sh"

  printf '%s\n' \
    '#!/bin/sh' \
    'case "$1" in' \
    '  -s) printf "%s\n" "$TEST_UNAME_S" ;;' \
    '  -m) printf "%s\n" "$TEST_UNAME_M" ;;' \
    '  *) exit 1 ;;' \
    'esac' >"$fake_bin/uname"

  printf '%s\n' \
    '#!/bin/sh' \
    'url=' \
    'output=' \
    'while [ "$#" -gt 0 ]; do' \
    '  case "$1" in' \
    '    http*) url=$1 ;;' \
    '    --output) shift; output=$1 ;;' \
    '  esac' \
    '  shift' \
    'done' \
    'printf "%s\n" "$url" >>"$TEST_CURL_LOG"' \
    'case "$url" in' \
    '  */SHA256SUMS)' \
    '    printf "test-checksum  stackhour-%s.tar.gz\n" "$TEST_EXPECTED_TARGET" >"$output"' \
    '    ;;' \
    '  */stackhour-*.tar.gz)' \
    '    printf "archive\n" >"$output"' \
    '    ;;' \
    '  *) exit 1 ;;' \
    'esac' >"$fake_bin/curl"

  printf '%s\n' \
    '#!/bin/sh' \
    'printf "test-checksum  %s\n" "$1"' >"$fake_bin/sha256sum"

  printf '%s\n' \
    '#!/bin/sh' \
    'archive=$2' \
    'destination=$4' \
    'name=$(basename "$archive" .tar.gz)' \
    'mkdir -p "$destination/$name"' \
    'printf "%s\n" "#!/bin/sh" "exit 0" >"$destination/$name/stackhour"' \
    'chmod 0755 "$destination/$name/stackhour"' >"$fake_bin/tar"

  chmod 0755 "$fake_bin/uname" "$fake_bin/curl" "$fake_bin/sha256sum" "$fake_bin/tar"

  TEST_UNAME_S=$system_name \
  TEST_UNAME_M=$machine_name \
  TEST_EXPECTED_TARGET=$expected_target \
  TEST_CURL_LOG=$case_dir/curl-log \
  STACKHOUR_INSTALL_DIR=$install_dir \
  PATH="$fake_bin:/usr/bin:/bin" \
    sh "$case_dir/install.sh" >"$case_dir/output"

  test -x "$install_dir/stackhour" || fail "$expected_target was not installed"
  grep -q "/stackhour-$expected_target.tar.gz$" "$case_dir/curl-log" ||
    fail "$expected_target release was not requested"
  grep -q "/SHA256SUMS$" "$case_dir/curl-log" ||
    fail "release checksums were not requested"
  grep -q "Installed Stackhour at $install_dir/stackhour" "$case_dir/output" ||
    fail "$expected_target did not report its install path"
}

run_download_case Linux x86_64 x86_64-unknown-linux-gnu
run_download_case Linux aarch64 aarch64-unknown-linux-gnu
run_download_case Darwin arm64 aarch64-apple-darwin

unsupported_dir="$test_dir/unsupported-intel-macos"
mkdir -p "$unsupported_dir/bin"
cp "$root/deploy/install.sh" "$unsupported_dir/install.sh"
printf '%s\n' \
  '#!/bin/sh' \
  'case "$1" in' \
  '  -s) printf "Darwin\n" ;;' \
  '  -m) printf "x86_64\n" ;;' \
  '  *) exit 1 ;;' \
  'esac' >"$unsupported_dir/bin/uname"
chmod 0755 "$unsupported_dir/bin/uname"
if STACKHOUR_INSTALL_DIR="$unsupported_dir/install" \
  PATH="$unsupported_dir/bin:/usr/bin:/bin" \
  sh "$unsupported_dir/install.sh" >"$unsupported_dir/output" 2>"$unsupported_dir/error"
then
  fail "Intel macOS was accepted"
fi
grep -q "does not support Intel macOS" "$unsupported_dir/error" ||
  fail "Intel macOS did not report the support limit"

package_dir="$test_dir/package"
package_install_dir="$test_dir/package-install"
mkdir -p "$package_dir" "$package_install_dir"
cp "$root/deploy/install.sh" "$package_dir/install.sh"
printf '%s\n' '#!/bin/sh' 'exit 0' >"$package_dir/stackhour"
chmod 0755 "$package_dir/stackhour"
STACKHOUR_INSTALL_DIR=$package_install_dir \
  sh "$package_dir/install.sh" >"$test_dir/package-output"
cmp "$package_dir/stackhour" "$package_install_dir/stackhour" ||
  fail "the packaged binary changed during installation"
test -x "$package_install_dir/stackhour" ||
  fail "the packaged binary is not executable"

echo "release installer tests passed"
