#!/usr/bin/env bash
set -Eeuo pipefail

export DEBIAN_FRONTEND=noninteractive
export NEEDRESTART_MODE=a
export DISPLAY=:99
export BENCH_DISPLAY=:99
export BENCH_GPU_STRICT=1
export RUST_BACKTRACE=1
original_ld_library_path=${LD_LIBRARY_PATH-}

bundle=/tmp/stackhour-gpu-bench.bundle
run_root=/home/ubuntu/stackhour-gpu-benchmark
repo="$run_root/repo"
aggregate="$run_root/results"
remote_archive=/tmp/stackhour-gpu-results.tar.gz
status_file="$aggregate/status.tsv"
current_candidate=setup
only_candidate=${ONLY_CANDIDATE-}

mkdir -p "$run_root" "$aggregate"
exec > >(tee -a "$aggregate/remote-run.log") 2>&1

package_and_arm_shutdown() {
  status=$?
  trap - EXIT
  set +e
  printf '%s\t%s\t%s\n' "$(date -u +%FT%TZ)" "$current_candidate" "$status" >> "$aggregate/exit-status.tsv"
  sudo cp /var/log/Xorg.99.log "$aggregate/Xorg.99.log" 2>/dev/null
  cp /tmp/xorg-console.log "$aggregate/xorg-console.log" 2>/dev/null
  ps -ef > "$aggregate/processes-at-exit.txt"
  tar -C "$run_root" -czf "$remote_archive" results
  sudo systemctl stop stackhour-benchmark-watchdog.timer >/dev/null 2>&1
  sudo systemctl stop stackhour-benchmark-watchdog.service >/dev/null 2>&1
  sudo systemctl stop stackhour-benchmark-finish-stop.timer >/dev/null 2>&1
  sudo systemctl reset-failed stackhour-benchmark-finish-stop.service >/dev/null 2>&1
  sudo systemd-run \
    --unit=stackhour-benchmark-finish-stop \
    --on-active=10m \
    --timer-property=AccuracySec=1s \
    /usr/bin/systemctl poweroff >/dev/null 2>&1
  echo "Remote result archive: $remote_archive"
  exit "$status"
}
trap package_and_arm_shutdown EXIT

# This watchdog survives a lost SSH session. The EC2 instance is configured to
# stop, not terminate, when the guest shuts down.
sudo shutdown -c >/dev/null 2>&1 || true
sudo systemctl stop stackhour-benchmark-watchdog.timer >/dev/null 2>&1 || true
sudo systemctl stop stackhour-benchmark-watchdog.service >/dev/null 2>&1 || true
sudo systemctl stop stackhour-benchmark-finish-stop.timer >/dev/null 2>&1 || true
sudo systemctl stop stackhour-benchmark-finish-stop.service >/dev/null 2>&1 || true
sudo systemctl reset-failed stackhour-benchmark-watchdog.service >/dev/null 2>&1 || true
sudo systemd-run \
  --unit=stackhour-benchmark-watchdog \
  --on-active=4h \
  --timer-property=AccuracySec=1s \
  /usr/bin/systemctl poweroff >/dev/null

echo "== Host =="
date -u
uname -a
nvidia-smi
df -h /

packages=(
  build-essential
  clang
  cmake
  curl
  dbus-x11
  file
  fonts-noto-core
  fonts-noto-mono
  git
  imagemagick
  jq
  libasound2-dev
  libayatana-appindicator3-dev
  libegl1-mesa-dev
  libfontconfig1-dev
  libfreetype-dev
  libgl1-mesa-dev
  libgtk-3-dev
  libssl-dev
  libudev-dev
  libvulkan-dev
  libwayland-dev
  libwebkit2gtk-4.0-dev
  libwebkit2gtk-4.1-dev
  libxcb-cursor0
  libx11-dev
  libx11-xcb-dev
  libxcb-cursor-dev
  libxcb-glx0-dev
  libxcb-icccm4-dev
  libxcb-image0-dev
  libxcb-keysyms1-dev
  libxcb-randr0-dev
  libxcb-render-util0-dev
  libxcb-shape0-dev
  libxcb-shm0-dev
  libxcb-sync-dev
  libxcb-util-dev
  libxcb-xfixes0-dev
  libxcb-xkb-dev
  libxcb1-dev
  libxdo-dev
  libxext-dev
  libxfixes-dev
  libxi-dev
  libxkbcommon-dev
  libxkbcommon-x11-dev
  libxrender-dev
  matchbox-window-manager
  ninja-build
  openbox
  patchelf
  pkg-config
  python3
  python3-pip
  python3-venv
  p7zip-full
  unzip
  vulkan-tools
  weston
  wmctrl
  x11-utils
  x11-xserver-utils
  xauth
  xdotool
  xserver-xorg-core
  xz-utils
)

missing_packages=()
for package in "${packages[@]}"; do
  dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q 'install ok installed' ||
    missing_packages+=("$package")
done
if ((${#missing_packages[@]})); then
  sudo apt-get update
  sudo apt-get install -y --no-install-recommends "${missing_packages[@]}"
fi

node_version=22.17.1
node_root="$HOME/.local/node-v$node_version-linux-x64"
if [[ ! -x "$node_root/bin/node" ]]; then
  curl -fsSL "https://nodejs.org/dist/v$node_version/node-v$node_version-linux-x64.tar.xz" -o /tmp/node.tar.xz
  mkdir -p "$node_root"
  tar -xJf /tmp/node.tar.xz -C "$node_root" --strip-components=1
fi
export PATH="$node_root/bin:$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.local/go/bin:$PATH"
if ! command -v pnpm >/dev/null || [[ "$(pnpm --version)" != 10.12.1 ]]; then
  npm install --global --prefix "$HOME/.local" pnpm@10.12.1
fi

if ! command -v rustc >/dev/null; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh
  sh /tmp/rustup-init.sh -y --profile minimal
fi
rustup toolchain install stable --profile minimal
rustup default stable

go_version=1.25.0
go_root="$HOME/.local/go"
if [[ ! -x "$go_root/bin/go" ]] || ! "$go_root/bin/go" version | grep -q "go$go_version"; then
  curl -fsSL "https://go.dev/dl/go$go_version.linux-amd64.tar.gz" -o /tmp/go.tar.gz
  rm -rf "$go_root"
  mkdir -p "$go_root"
  tar -xzf /tmp/go.tar.gz -C "$go_root" --strip-components=1
fi
export PATH="$go_root/bin:$PATH"

qt_version=6.8.3
qt_root="$HOME/Qt/$qt_version/gcc_64"
if [[ ! -x "$qt_root/bin/qtpaths" ]]; then
  if [[ ! -x "$HOME/.local/aqt/bin/aqt" ]]; then
    python3 -m venv "$HOME/.local/aqt"
    "$HOME/.local/aqt/bin/pip" install --disable-pip-version-check aqtinstall
  fi
  "$HOME/.local/aqt/bin/aqt" install-qt linux desktop "$qt_version" linux_gcc_64 \
    --archives qtbase qtdeclarative qtshadertools \
    -O "$HOME/Qt"
fi
export PATH="$qt_root/bin:$PATH"
icu_root="$HOME/.local/icu73"
if [[ ! -f "$icu_root/lib/libicui18n.so.73" ]]; then
  if [[ ! -x "$HOME/.local/bin/micromamba" ]]; then
    curl -fsSL https://micro.mamba.pm/api/micromamba/linux-64/latest -o /tmp/micromamba.tar.bz2
    mkdir -p /tmp/micromamba-extract "$HOME/.local/bin"
    tar -xjf /tmp/micromamba.tar.bz2 -C /tmp/micromamba-extract
    install -m 755 /tmp/micromamba-extract/bin/micromamba "$HOME/.local/bin/micromamba"
  fi
  MAMBA_ROOT_PREFIX="$HOME/.local/share/mamba" \
    "$HOME/.local/bin/micromamba" create -y -p "$icu_root" -c conda-forge 'icu=73.2'
fi
export CMAKE_PREFIX_PATH="$qt_root${CMAKE_PREFIX_PATH:+:$CMAKE_PREFIX_PATH}"
qt_ld_library_path="$icu_root/lib:$qt_root/lib${original_ld_library_path:+:$original_ld_library_path}"
export QT_PLUGIN_PATH="$qt_root/plugins"
export QML2_IMPORT_PATH="$qt_root/qml"

echo "== Toolchain =="
node --version
pnpm --version
rustc --version
cargo --version
go version
cmake --version | head -1
LD_LIBRARY_PATH="$qt_ld_library_path" "$qt_root/bin/qtpaths" --qt-version
pkg-config --modversion webkit2gtk-4.0
pkg-config --modversion webkit2gtk-4.1

if [[ -f /etc/X11/xorg.conf.nvidia-xconfig-original ]]; then
  sudo cp /etc/X11/xorg.conf.nvidia-xconfig-original /etc/X11/xorg.conf
fi
sudo systemctl stop display-manager.service >/dev/null 2>&1 || true
sudo systemctl stop xorg.service >/dev/null 2>&1 || true
sudo systemctl stop xorg@0.service >/dev/null 2>&1 || true
sudo pkill -TERM -x Xorg >/dev/null 2>&1 || true
for _ in $(seq 1 20); do
  pgrep -x Xorg >/dev/null || break
  sleep 0.25
done
sudo Xorg :99 \
  -noreset \
  -ac \
  -nolisten tcp \
  -dpi 96 \
  -config /etc/X11/xorg.conf \
  -logfile /var/log/Xorg.99.log \
  >/tmp/xorg-console.log 2>&1 &
for _ in $(seq 1 60); do
  xdpyinfo -display "$DISPLAY" >/dev/null 2>&1 && break
  sleep 1
done
if ! xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then
  sudo tail -n 160 /var/log/Xorg.99.log 2>/dev/null || true
  cat /tmp/xorg-console.log 2>/dev/null || true
  false
fi
DISPLAY="$DISPLAY" xrandr --fb 1280x800 --dpi 96
DISPLAY="$DISPLAY" nohup openbox >/tmp/openbox.log 2>&1 &
sleep 2

echo "== GPU display =="
DISPLAY="$DISPLAY" xdpyinfo | grep -E 'dimensions:|resolution:'
DISPLAY="$DISPLAY" glxinfo -B

if [[ ! -d "$repo/.git" ]]; then
  git clone -q -b benchmark/react-electron "$bundle" "$repo"
fi
git -C "$repo" fetch -q "$bundle" \
  'refs/heads/benchmark/*:refs/remotes/bench/benchmark/*'
git -C "$repo" config advice.detachedHead false
printf 'candidate\tbranch\tstatus\tstarted_at\tfinished_at\n' > "$status_file"

fixture_ready=0
failures=0

run_candidate() {
  branch=$1
  candidate=$2
  current_candidate=$candidate
  candidate_dir="$aggregate/$candidate"
  mkdir -p "$candidate_dir"
  started_at=$(date -u +%FT%TZ)
  echo
  echo "============================================================"
  echo "== $candidate ($branch) =="
  echo "============================================================"

  set +e
  (
    set -Eeuo pipefail
    git -C "$repo" checkout -q -f -B "$branch" "refs/remotes/bench/$branch"
    cd "$repo/benchmark"

    if [[ "$candidate" == gpui ]]; then
      # The headless NVIDIA RandR output advertises 1024x768 even though the
      # root framebuffer is 1280x800. Matchbox is a kiosk WM and maps GPUI to
      # the full root framebuffer; with-gpui-x11.sh verifies the exact size.
      pkill -TERM -x openbox >/dev/null 2>&1 || true
      for _ in $(seq 1 20); do
        pgrep -x openbox >/dev/null || break
        sleep 0.1
      done
      DISPLAY="$DISPLAY" nohup matchbox-window-manager -use_titlebar no \
        >/tmp/matchbox.log 2>&1 &
      sleep 1
      if ! pgrep -f '(^|/)matchbox-window-manager([[:space:]]|$)' >/dev/null; then
        cat /tmp/matchbox.log >&2 || true
        exit 13
      fi
    fi

    if [[ "$candidate" == qt-qml ]]; then
      export LD_LIBRARY_PATH="$qt_ld_library_path"
      ldd "$qt_root/plugins/platforms/libqxcb.so" | tee "$candidate_dir/libqxcb-ldd.txt"
      if grep -q 'not found' "$candidate_dir/libqxcb-ldd.txt"; then
        echo "Qt xcb plugin still has unresolved shared-library dependencies" >&2
        exit 12
      fi
    elif [[ -n "$original_ld_library_path" ]]; then
      export LD_LIBRARY_PATH="$original_ld_library_path"
    else
      unset LD_LIBRARY_PATH
    fi

    rm -rf results/raw results/gpu
    mkdir -p results/raw results/gpu screenshots

    pnpm install --frozen-lockfile --prefer-offline
    if ((fixture_ready == 0)) || [[ ! -d .fixture ]]; then
      pnpm fixture
    fi

    pnpm build
    pnpm manifest
    pnpm visual
    pnpm bench
    pnpm analyze
  )
  candidate_status=$?
  set -e

  fixture_ready=1
  finished_at=$(date -u +%FT%TZ)
  if ((candidate_status == 0)); then
    outcome=passed
  else
    outcome="failed:$candidate_status"
    failures=$((failures + 1))
  fi
  printf '%s\t%s\t%s\t%s\t%s\n' "$candidate" "$branch" "$outcome" "$started_at" "$finished_at" >> "$status_file"

  if [[ -d "$repo/benchmark/results/raw" ]]; then
    cp -a "$repo/benchmark/results/raw" "$candidate_dir/"
  fi
  if [[ -d "$repo/benchmark/results/gpu" ]]; then
    cp -a "$repo/benchmark/results/gpu" "$candidate_dir/"
  fi
  for artifact in machine.json summary.json; do
    if [[ -f "$repo/benchmark/results/$artifact" ]]; then
      cp "$repo/benchmark/results/$artifact" "$candidate_dir/"
    fi
  done
  if [[ -f "$repo/benchmark/screenshots/$candidate.png" ]]; then
    cp "$repo/benchmark/screenshots/$candidate.png" "$candidate_dir/"
  fi
  git -C "$repo" rev-parse HEAD > "$candidate_dir/commit.txt"
  echo "== $candidate: $outcome =="
}

run_if_selected() {
  branch=$1
  candidate=$2
  if [[ -z "$only_candidate" || "$only_candidate" == "$candidate" ]]; then
    run_candidate "$branch" "$candidate"
  fi
}

run_if_selected benchmark/react-electron react-electron
run_if_selected benchmark/react-tauri react-tauri
run_if_selected benchmark/solid-tauri solid-tauri
run_if_selected benchmark/vue-tauri vue-tauri
run_if_selected benchmark/egui egui
run_if_selected benchmark/iced iced
run_if_selected benchmark/qt-qml qt-qml
run_if_selected benchmark/dioxus-desktop dioxus-desktop
run_if_selected benchmark/wails wails
run_if_selected benchmark/gpui gpui

current_candidate=complete
cp /var/log/Xorg.99.log "$aggregate/Xorg.99.log" 2>/dev/null || true
cp /tmp/openbox.log "$aggregate/openbox.log" 2>/dev/null || true
nvidia-smi -q > "$aggregate/nvidia-smi-q.txt"
find "$aggregate" -maxdepth 3 -type f -print | sort

if ((failures > 0)); then
  echo "$failures candidate(s) failed"
  exit 1
fi
echo "All candidates passed"
