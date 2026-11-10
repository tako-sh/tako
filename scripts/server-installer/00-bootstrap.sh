#!/bin/sh
set -eu

# Tako installer (POSIX sh)
#
# Usage:
#   sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
#
# What it does:
# - downloads and installs `tako-server`
# - installs the libvips runtime used by the built-in image optimizer
# - installs Podman for container releases
# - creates OS user `tako`
# - installs systemd units for `tako-server`
# - installs maintenance helpers and sudoers for the tako service user
#
# Optional env vars:
#   TAKO_USER               default: tako
#   TAKO_HOME               default: /opt/tako
#   TAKO_SOCKET             default: /var/run/tako/tako.sock
#   TAKO_HTTP_PORT          public HTTP port for tako-server (default: 80)
#   TAKO_HTTPS_PORT         public HTTPS port for tako-server (default: 443)
#   TAKO_VPN_PROVIDER       native only (alias: TAKO_VPN; default: native)
#   TAKO_NETWORK_HELPER_SOURCE / TAKO_NETWORK_HELPER_SHA256
#                           optional local native helper and required digest
#   TAKO_MANAGEMENT_HOST    assigned native network IP to bind remote management on (optional)
#                           if unset, installer reads applied native network state
#   TAKO_SSH_PUBKEY         public key line to authorize for TAKO_USER (optional)
#                           if unset, installer prompts in interactive terminals
#
#   TAKO_SERVER_URL         override archive URL (.tar.zst or .tar.gz; optional)
#   TAKO_DOWNLOAD_BASE_URL  override release download base URL (optional)
#   TAKO_ALLOW_INSECURE_DOWNLOAD_BASE
#                           default: unset
#                           set 1/true/yes/on to allow non-HTTPS download overrides for local testing
#   TAKO_REPO_OWNER         default: tako-sh
#   TAKO_REPO_NAME          default: tako
#   TAKO_RELEASE_TAG        default: latest
#   GH_TOKEN/GITHUB_TOKEN   optional GitHub token for release downloads
#   TAKO_SERVER_NAME        server identity for metrics labels (optional)
#                           service-start mode prompts in interactive terminals
#                           defaults to machine hostname when non-interactive
#   TAKO_RESTART_SERVICE    default: 1 (set 0/false to install or refresh without starting the service)

if [ "$(id -u)" -ne 0 ]; then
  echo "error: run as root (use sudo)" >&2
  exit 1
fi

if [ "$(uname -s)" != "Linux" ]; then
  echo "error: this installer supports Linux only" >&2
  exit 1
fi

TAKO_USER="${TAKO_USER:-tako}"
TAKO_HOME="${TAKO_HOME:-/opt/tako}"
TAKO_SOCKET="${TAKO_SOCKET:-/var/run/tako/tako.sock}"
TAKO_HTTP_PORT="${TAKO_HTTP_PORT:-}"
TAKO_HTTPS_PORT="${TAKO_HTTPS_PORT:-}"
TAKO_MANAGEMENT_HOST="${TAKO_MANAGEMENT_HOST:-}"
TAKO_VPN="${TAKO_VPN_PROVIDER:-${TAKO_VPN:-}}"
TAKO_DOWNLOAD_BASE_URL="${TAKO_DOWNLOAD_BASE_URL:-}"
TAKO_ALLOW_INSECURE_DOWNLOAD_BASE="${TAKO_ALLOW_INSECURE_DOWNLOAD_BASE:-}"
TAKO_REPO_OWNER="${TAKO_REPO_OWNER:-tako-sh}"
TAKO_REPO_NAME="${TAKO_REPO_NAME:-tako}"
TAKO_RELEASE_TAG="${TAKO_RELEASE_TAG:-latest}"
TAKO_RESTART_SERVICE="${TAKO_RESTART_SERVICE:-1}"
TAKO_MANAGEMENT_REQUIRED_MESSAGE="Remote management requires an enrolled Tako network."
TAKO_MANAGEMENT_ARGS=""
TAKO_NETWORK_UNIT_ARGS=""
TAKO_PUBLIC_PORT_ARGS=""
TAKO_SERVER_CAPABILITIES="cap_net_bind_service,cap_setuid,cap_setgid,cap_kill"
TAKO_SERVER_INSTALL_REFRESH_HELPER="/usr/local/bin/tako-server-install-refresh"
TAKO_SERVER_SERVICE_HELPER="/usr/local/bin/tako-server-service"
TAKO_MANAGEMENT_AUTH_KEYS="$TAKO_HOME/management-authorized-keys"

need_cmd() { command -v "$1" >/dev/null 2>&1; }

usage() {
  cat <<EOF
Usage: install-tako-server.sh [--http-port PORT] [--https-port PORT]

Options:
  --http-port PORT    Public HTTP port for tako-server (default: 80)
  --https-port PORT   Public HTTPS port for tako-server (default: 443)
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --http-port)
      shift
      if [ "$#" -eq 0 ]; then
        echo "error: --http-port requires a value" >&2
        exit 1
      fi
      TAKO_HTTP_PORT="$1"
      ;;
    --http-port=*)
      TAKO_HTTP_PORT="${1#--http-port=}"
      ;;
    --https-port)
      shift
      if [ "$#" -eq 0 ]; then
        echo "error: --https-port requires a value" >&2
        exit 1
      fi
      TAKO_HTTPS_PORT="$1"
      ;;
    --https-port=*)
      TAKO_HTTPS_PORT="${1#--https-port=}"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

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
  case "$src" in
    file://*)
      cp "${src#file://}" "$dest"
      ;;
    *)
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
      ;;
  esac
}

download_stdout() {
  url="$1"
  case "$url" in
    file://*)
      cat "${url#file://}"
      ;;
    *)
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
      ;;
  esac
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

# Shared by installation and the root-owned recovery helper. Accept only an
# address reported by the selected local agent, never an arbitrary private IP.
