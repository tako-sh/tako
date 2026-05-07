#!/bin/sh
set -eu

# Tako CLI installer (POSIX sh)
#
# Usage:
#   curl -fsSL https://tako.sh/install.sh | sh
#
# What it does:
# - downloads and installs `tako`, `tako-dev-server`, and `tako-dev-proxy` for your OS/architecture
# - on macOS, installs `Tako.app` and symlinks `tako` to the signed CLI inside it
# - installs binaries to ~/.local/bin by default
#
# Optional env vars:
#   TAKO_INSTALL_DIR        default: $HOME/.local/bin
#   TAKO_MACOS_APP_DIR      default: $HOME/Applications
#   TAKO_URL                override archive URL (.tar.gz; optional)
#   TAKO_DOWNLOAD_BASE_URL  override release download base URL (optional)
#   TAKO_ALLOW_INSECURE_DOWNLOAD_BASE
#                           default: unset
#                           set 1/true/yes/on to allow non-HTTPS download overrides for local testing
#   TAKO_REPO_OWNER         default: lilienblum
#   TAKO_REPO_NAME          default: tako
#   TAKO_RELEASE_TAG        default: latest
#   GH_TOKEN/GITHUB_TOKEN   optional GitHub token for release downloads

need_cmd() { command -v "$1" >/dev/null 2>&1; }

github_auth_header() {
  url="$1"
  case "$url" in
    https://github.com/*|https://api.github.com/*|https://raw.githubusercontent.com/*)
      github_token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
      if [ -n "$github_token" ]; then
        printf 'Authorization: Bearer %s\n' "$github_token"
      fi
      ;;
  esac
}

download_file() {
  src="$1"
  dest="$2"
  auth_header="$(github_auth_header "$src")"
  if need_cmd curl; then
    if [ -n "$auth_header" ]; then
      curl -fsSL -H "$auth_header" "$src" -o "$dest"
    else
      curl -fsSL "$src" -o "$dest"
    fi
  else
    if [ -n "$auth_header" ]; then
      wget --header="$auth_header" -qO "$dest" "$src"
    else
      wget -qO "$dest" "$src"
    fi
  fi
}

download_stdout() {
  url="$1"
  auth_header="$(github_auth_header "$url")"
  if need_cmd curl; then
    if [ -n "$auth_header" ]; then
      curl -fsSL -H "$auth_header" "$url"
    else
      curl -fsSL "$url"
    fi
  else
    if [ -n "$auth_header" ]; then
      wget --header="$auth_header" -qO- "$url"
    else
      wget -qO- "$url"
    fi
  fi
}

is_enabled() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|on|ON)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

require_secure_download_override() {
  value="$1"
  case "$value" in
    https://*|file://*)
      return 0
      ;;
  esac
  if is_enabled "${TAKO_ALLOW_INSECURE_DOWNLOAD_BASE:-}"; then
    echo "warning: using insecure download override '$value' for local testing" >&2
    return 0
  fi
  echo "error: insecure download override '$value' is not allowed; use https:// or set TAKO_ALLOW_INSECURE_DOWNLOAD_BASE=1 for local testing" >&2
  exit 1
}

if [ -z "${HOME:-}" ]; then
  echo "error: HOME is not set" >&2
  exit 1
fi

if ! need_cmd install; then
  echo "error: missing required command: install" >&2
  exit 1
fi

if ! need_cmd curl && ! need_cmd wget; then
  echo "error: missing downloader (need curl or wget)" >&2
  exit 1
fi
if ! need_cmd tar; then
  echo "error: missing required command: tar" >&2
  exit 1
fi

TAKO_INSTALL_DIR="${TAKO_INSTALL_DIR:-$HOME/.local/bin}"
TAKO_MACOS_APP_DIR="${TAKO_MACOS_APP_DIR:-$HOME/Applications}"
TAKO_DOWNLOAD_BASE_URL="${TAKO_DOWNLOAD_BASE_URL:-}"
TAKO_ALLOW_INSECURE_DOWNLOAD_BASE="${TAKO_ALLOW_INSECURE_DOWNLOAD_BASE:-}"
TAKO_REPO_OWNER="${TAKO_REPO_OWNER:-lilienblum}"
TAKO_REPO_NAME="${TAKO_REPO_NAME:-tako}"
TAKO_RELEASE_TAG="${TAKO_RELEASE_TAG:-latest}"

os_raw="$(uname -s)"
case "$os_raw" in
  Linux) os="linux" ;;
  Darwin) os="darwin" ;;
  *)
    echo "error: unsupported OS: $os_raw (supported: Linux, Darwin)" >&2
    exit 1
    ;;
esac

arch_raw="$(uname -m)"
case "$arch_raw" in
  x86_64|amd64) arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)
    echo "error: unsupported architecture: $arch_raw (supported: x86_64, aarch64)" >&2
    exit 1
    ;;
esac

download_url="${TAKO_URL:-}"
if [ -z "$download_url" ]; then
  download_base="$TAKO_DOWNLOAD_BASE_URL"
  if [ -z "$download_base" ]; then
    download_base="https://github.com/$TAKO_REPO_OWNER/$TAKO_REPO_NAME/releases/download/$TAKO_RELEASE_TAG"
  else
    require_secure_download_override "$download_base"
  fi
  download_url="$download_base/tako-$os-$arch.tar.gz"
else
  require_secure_download_override "$download_url"
fi
case "$download_url" in
  *.tar.gz|file://*.tar.gz) ;;
  *)
    echo "error: TAKO_URL must point to a .tar.gz archive" >&2
    exit 1
    ;;
esac
sha_url="${download_url}.sha256"

tmp_payload="$(mktemp)"
tmp_extract="$(mktemp -d)"
trap 'rm -f "$tmp_payload"; rm -rf "$tmp_extract"' EXIT

echo "Downloading tako CLI: $download_url"
download_file "$download_url" "$tmp_payload"

expected_sha=""
expected_sha="$(download_stdout "$sha_url" 2>/dev/null | awk '{print $1}' || true)"

if [ -n "$expected_sha" ]; then
  if need_cmd sha256sum; then
    actual="$(sha256sum "$tmp_payload" | awk '{print $1}')"
  elif need_cmd shasum; then
    actual="$(shasum -a 256 "$tmp_payload" | awk '{print $1}')"
  else
    echo "warning: sha256 tool not found; skipping integrity check" >&2
    actual=""
  fi

  if [ -n "$actual" ] && [ "$actual" != "$expected_sha" ]; then
    echo "error: sha256 mismatch (expected=$expected_sha actual=$actual)" >&2
    exit 1
  fi
else
  echo "warning: could not fetch SHA256 ($sha_url); skipping integrity check" >&2
fi

tar -xzf "$tmp_payload" -C "$tmp_extract"
tmp_tako_bin=""
tmp_tako_app=""
if [ "$os" = "darwin" ]; then
  tmp_tako_app="$(find "$tmp_extract" -type d -name Tako.app | head -n 1 || true)"
  if [ -z "$tmp_tako_app" ]; then
    echo "error: archive did not contain Tako.app" >&2
    exit 1
  fi
  tmp_tako_bin="$tmp_tako_app/Contents/MacOS/tako"
else
  tmp_tako_bin="$(find "$tmp_extract" -type f -name tako | head -n 1 || true)"
  if [ -z "$tmp_tako_bin" ]; then
    echo "error: archive did not contain a tako binary" >&2
    exit 1
  fi
fi
tmp_dev_server_bin="$(find "$tmp_extract" -type f -name tako-dev-server | head -n 1 || true)"
if [ -z "$tmp_dev_server_bin" ]; then
  echo "error: archive did not contain a tako-dev-server binary" >&2
  exit 1
fi
tmp_dev_proxy_bin="$(find "$tmp_extract" -type f -name tako-dev-proxy | head -n 1 || true)"
if [ -z "$tmp_dev_proxy_bin" ]; then
  echo "error: archive did not contain a tako-dev-proxy binary" >&2
  exit 1
fi
mkdir -p "$TAKO_INSTALL_DIR"
target_tako="$TAKO_INSTALL_DIR/tako"
target_dev_server="$TAKO_INSTALL_DIR/tako-dev-server"
target_dev_proxy="$TAKO_INSTALL_DIR/tako-dev-proxy"
install -m 0755 "$tmp_dev_server_bin" "$target_dev_server"
install -m 0755 "$tmp_dev_proxy_bin" "$target_dev_proxy"

echo "OK installed tako-dev-server to $target_dev_server"
echo "OK installed tako-dev-proxy to $target_dev_proxy"

if [ "$os" = "darwin" ]; then
  target_tako_app="$TAKO_MACOS_APP_DIR/Tako.app"
  mkdir -p "$TAKO_MACOS_APP_DIR"
  rm -rf "$target_tako_app"
  if need_cmd ditto; then
    ditto "$tmp_tako_app" "$target_tako_app"
  else
    cp -R "$tmp_tako_app" "$target_tako_app"
  fi
  chmod 0755 "$target_tako_app/Contents/MacOS/tako"
  ln -sf "$target_tako_app/Contents/MacOS/tako" "$target_tako"
  echo "OK installed Tako.app to $target_tako_app"
  echo "OK linked tako to $target_tako"
else
  install -m 0755 "$tmp_tako_bin" "$target_tako"
  echo "OK installed tako to $target_tako"
fi

case ":$PATH:" in
  *":$TAKO_INSTALL_DIR:"*)
    echo "OK '$TAKO_INSTALL_DIR' is on PATH"
    ;;
  *)
    echo "warning: '$TAKO_INSTALL_DIR' is not on PATH." >&2
    echo "warning: add it to your shell profile and restart your shell." >&2
    ;;
esac

echo "Run: tako --version"
