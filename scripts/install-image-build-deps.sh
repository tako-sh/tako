#!/bin/sh
set -eu

# Install the build tools for the image codecs that tako-images links
# statically: libavif (CMake >= 3.22), dav1d (Meson >= 0.54, Ninja, and NASM
# on x86), and libwebp (C compiler only). Binaries built with them need no
# image libraries at runtime.

need_cmd() { command -v "$1" >/dev/null 2>&1; }

run_as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif need_cmd sudo; then
    sudo "$@"
  else
    echo "error: root privileges are required to install image build tools" >&2
    return 1
  fi
}

# version_at_least <actual> <minimum>, both dotted numbers.
version_at_least() {
  [ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -n 1)" = "$2" ]
}

tool_version() {
  "$1" --version 2>/dev/null | head -n 1 | grep -Eo '[0-9]+(\.[0-9]+)+' | head -n 1
}

has_tool_version() {
  need_cmd "$1" && version_at_least "$(tool_version "$1")" "$2"
}

install_apt_build_deps() {
  run_as_root apt-get -o Acquire::Retries=5 update
  run_as_root apt-get -o Acquire::Retries=5 install -y --no-install-recommends \
    cmake meson nasm ninja-build pkg-config

  # Older distributions (Debian 11) ship CMake or Meson below the minimum.
  if ! has_tool_version cmake 3.22 || ! has_tool_version meson 0.54; then
    run_as_root apt-get -o Acquire::Retries=5 install -y --no-install-recommends python3-pip
    run_as_root python3 -m pip install --disable-pip-version-check 'cmake>=3.22' 'meson>=0.54' ninja
  fi
}

install_brew_build_deps() {
  for formula in cmake meson nasm; do
    if ! brew list --formula "$formula" >/dev/null 2>&1; then
      brew install "$formula"
    fi
  done
}

case "$(uname -s)" in
  Darwin)
    if ! need_cmd brew; then
      echo "error: Homebrew is required to install image build tools on macOS" >&2
      exit 1
    fi
    install_brew_build_deps
    ;;
  Linux)
    if need_cmd apt-get; then
      install_apt_build_deps
    else
      echo "error: this CI helper only supports apt-based Linux runners" >&2
      exit 1
    fi
    ;;
  *)
    echo "error: unsupported OS for image build tools: $(uname -s)" >&2
    exit 1
    ;;
esac

for tool in cmake meson ninja; do
  if ! need_cmd "$tool"; then
    echo "error: $tool not found after installing image build tools" >&2
    exit 1
  fi
done
if ! has_tool_version cmake 3.22; then
  echo "error: CMake 3.22 or newer is required to build libavif" >&2
  exit 1
fi
