#!/bin/sh
set -eu

# Install the libvips runtime used by Tako image transforms on CI runners.
# Build jobs still install the development package where headers are needed.

need_cmd() { command -v "$1" >/dev/null 2>&1; }

run_as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif need_cmd sudo; then
    sudo "$@"
  else
    echo "error: root privileges are required to install libvips" >&2
    return 1
  fi
}

install_apt_libvips_runtime() {
  run_as_root apt-get update -y

  apt_avif_pkgs=""
  for apt_avif_pkg in libheif-plugin-aomenc libheif-plugin-aomdec libheif-plugin-dav1d; do
    if apt-cache show "$apt_avif_pkg" >/dev/null 2>&1; then
      apt_avif_pkgs="$apt_avif_pkgs $apt_avif_pkg"
    fi
  done

  for apt_vips_pkg in libvips42t64 libvips42 libvips; do
    if run_as_root apt-get install -y --no-install-recommends "$apt_vips_pkg" $apt_avif_pkgs; then
      return 0
    fi
  done

  echo "error: no supported libvips runtime package found" >&2
  return 1
}

if need_cmd vips; then
  exit 0
fi

case "$(uname -s)" in
  Darwin)
    if ! need_cmd brew; then
      echo "error: Homebrew is required to install libvips on macOS" >&2
      exit 1
    fi
    if brew list --formula vips >/dev/null 2>&1; then
      exit 0
    fi
    brew install vips
    ;;
  Linux)
    if need_cmd apt-get; then
      install_apt_libvips_runtime
    else
      echo "error: this CI helper only supports apt-based Linux runners" >&2
      exit 1
    fi
    ;;
  *)
    echo "error: unsupported OS for libvips runtime install: $(uname -s)" >&2
    exit 1
    ;;
esac
