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
vpn_functions() {
  cat <<'VPN_EOF'
valid_management_ip() {
  case "$1" in
    *:*)
      printf '%s\n' "$1" | awk -F: '
        {
          first = tolower($1)
          if (length(first) != 4 || (substr(first, 1, 2) != "fc" && substr(first, 1, 2) != "fd")) exit 1
          value = $0
          compressed = sub(/::/, ":", value)
          if (value ~ /::/ || (!compressed && NF != 8)) exit 1
          groups = 0
          for (i = 1; i <= NF; i++) {
            if ($i == "") continue
            if ($i !~ /^[a-fA-F0-9]+$/ || length($i) > 4) exit 1
            groups++
          }
          if (compressed && groups >= 8) exit 1
          if (!compressed && groups != 8) exit 1
        }
      '
      ;;
    *)
      printf '%s\n' "$1" | awk -F. '
        NF == 4 {
          for (i = 1; i <= 4; i++) {
            if ($i !~ /^[0-9]+$/ || $i < 0 || $i > 255) exit 1
          }
          if ($1 == 10 || ($1 == 172 && $2 >= 16 && $2 <= 31) ||
              ($1 == 192 && $2 == 168) || ($1 == 100 && $2 >= 64 && $2 <= 127)) exit 0
        }
        { exit 1 }
      '
      ;;
  esac
}

detect_management_ip() {
  provider="$1"
  preferred="${2:-}"
  case "$provider" in
    native)
      addresses="$(/usr/local/libexec/tako/native-address "$preferred" 2>/dev/null)" || return 1
      ;;
    *) echo "error: only native Tako networking is supported; enroll with tako servers add admin@host --network <network>" >&2; return 1 ;;
  esac
  for address in $addresses; do
    if valid_management_ip "$address" && { [ -z "$preferred" ] || [ "$address" = "$preferred" ]; }; then
      printf '%s\n' "$address"
      return 0
    fi
  done
  return 1
}
VPN_EOF
}
eval "$(vpn_functions)"

configure_management_http() {
  saved_vpn=native
  if [ -r /usr/local/libexec/tako/vpn-provider ]; then
    IFS= read -r saved_vpn < /usr/local/libexec/tako/vpn-provider
  fi
  TAKO_VPN="${TAKO_VPN:-$saved_vpn}"
  case "$TAKO_VPN" in
    native) ;;
    *) echo "error: only native Tako networking is supported; re-enroll with tako servers add admin@host --network <network>" >&2; exit 1 ;;
  esac

  requested_management_host="$TAKO_MANAGEMENT_HOST"
  preferred_management_host="$requested_management_host"
  if [ -z "$preferred_management_host" ] && [ "$TAKO_VPN" = "$saved_vpn" ]; then
    preferred_management_host="$(existing_service_arg_value --management-host || true)"
  fi
  TAKO_MANAGEMENT_HOST="$(detect_management_ip "$TAKO_VPN" "$preferred_management_host" || true)"
  if [ -z "$TAKO_MANAGEMENT_HOST" ] && [ -z "$requested_management_host" ]; then
    TAKO_MANAGEMENT_HOST="$(detect_management_ip "$TAKO_VPN" || true)"
  fi
  if [ -n "$requested_management_host" ] && [ -z "$TAKO_MANAGEMENT_HOST" ]; then
    echo "error: TAKO_MANAGEMENT_HOST must be an address assigned to this server by $TAKO_VPN." >&2
    exit 1
  fi
  if [ -n "$TAKO_MANAGEMENT_HOST" ]; then
    TAKO_MANAGEMENT_ARGS="--management-host $TAKO_MANAGEMENT_HOST"
    echo "OK remote management bound to $TAKO_VPN address $TAKO_MANAGEMENT_HOST"
    return
  fi
  if is_enabled "$TAKO_RESTART_SERVICE"; then
    echo "error: $TAKO_MANAGEMENT_REQUIRED_MESSAGE Enroll this server with tako servers add admin@host --network <network>." >&2
    exit 1
  fi
  echo "warning: remote management was not configured; enroll this server in a Tako network before starting tako-server." >&2
}

process_has_management_host() {
  pid="$1"
  host="$2"
  case "$pid" in
    ''|0|*[!0-9]*)
      return 1
      ;;
  esac
  if [ ! -r "/proc/$pid/cmdline" ]; then
    return 1
  fi

  cmdline="$(tr '\000' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)"
  case "$cmdline" in
    *"--management-host $host"*|*"--management-host=$host"*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

cmdline_arg_value() {
  _cmdline="$1"
  _arg="$2"
  printf '%s\n' "$_cmdline" | awk -v arg="$_arg" '
    {
      for (i = 1; i <= NF; i++) {
        if ($i == arg && (i + 1) <= NF) {
          print $(i + 1)
          exit
        }
        prefix = arg "="
        if (index($i, prefix) == 1) {
          print substr($i, length(prefix) + 1)
          exit
        }
      }
    }
  '
}

process_has_public_ports() {
  pid="$1"
  http_port="$2"
  https_port="$3"
  case "$pid" in
    ''|0|*[!0-9]*)
      return 1
      ;;
  esac
  if [ ! -r "/proc/$pid/cmdline" ]; then
    return 1
  fi

  cmdline="$(tr '\000' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)"
  running_http="$(cmdline_arg_value "$cmdline" "--http-port")"
  if [ -z "$running_http" ]; then
    running_http="$(cmdline_arg_value "$cmdline" "--port")"
  fi
  running_https="$(cmdline_arg_value "$cmdline" "--https-port")"
  if [ -z "$running_https" ]; then
    running_https="$(cmdline_arg_value "$cmdline" "--tls-port")"
  fi

  [ "${running_http:-80}" = "$http_port" ] && [ "${running_https:-443}" = "$https_port" ]
}

systemd_main_pid() {
  systemctl show -p MainPID --value tako-server 2>/dev/null || true
}

require_secure_download_override() {
  value="$1"
  case "$value" in
    https://*|file://*)
      return 0
      ;;
  esac
  if is_enabled "$TAKO_ALLOW_INSECURE_DOWNLOAD_BASE"; then
    echo "warning: using insecure download override '$value' for local testing" >&2
    return 0
  fi
  echo "error: insecure download override '$value' is not allowed; use https:// or set TAKO_ALLOW_INSECURE_DOWNLOAD_BASE=1 for local testing" >&2
  exit 1
}

systemd_is_usable() {
  if ! need_cmd systemctl; then
    return 1
  fi

  # Containers can have systemctl installed without systemd as PID 1.
  if [ ! -d /run/systemd/system ]; then
    return 1
  fi

  if ! systemctl show-environment >/dev/null 2>&1; then
    return 1
  fi

  return 0
}

detect_service_manager() {
  if systemd_is_usable; then
    echo "systemd"
    return
  fi

  echo "none"
}

SERVICE_MANAGER="$(detect_service_manager)"

existing_service_arg_value() {
  _arg="$1"
  for _file in /etc/systemd/system/tako-server.service; do
    if [ -r "$_file" ]; then
      _value="$(cmdline_arg_value "$(sed -n 's/.*ExecStart=//p' "$_file" 2>/dev/null)" "$_arg")"
      if [ -n "$_value" ]; then
        printf '%s\n' "$_value"
        return 0
      fi
    fi
  done
  return 1
}

existing_http_port() {
  existing_service_arg_value "--http-port" || existing_service_arg_value "--port" || true
}

existing_https_port() {
  existing_service_arg_value "--https-port" || existing_service_arg_value "--tls-port" || true
}

prompt_with_default() {
  label="$1"
  default="$2"
  answer=""
  if [ -t 0 ] && [ -t 1 ]; then
    printf '%s [%s]: ' "$label" "$default"
    IFS= read -r answer || answer=""
  elif [ -r /dev/tty ] && [ -w /dev/tty ] && (printf '' > /dev/tty) 2>/dev/null; then
    printf '%s [%s]: ' "$label" "$default" > /dev/tty
    IFS= read -r answer < /dev/tty || answer=""
  fi
  if [ -z "$answer" ]; then
    answer="$default"
  fi
  printf '%s\n' "$answer"
}

validate_port_value() {
  label="$1"
  value="$2"
  case "$value" in
    ''|*[!0-9]*)
      echo "error: $label must be between 1 and 65535" >&2
      exit 1
      ;;
  esac
  if [ "$value" -lt 1 ] || [ "$value" -gt 65535 ]; then
    echo "error: $label must be between 1 and 65535" >&2
    exit 1
  fi
}

configure_public_ports() {
  default_http="$(existing_http_port)"
  default_https="$(existing_https_port)"
  default_http="${default_http:-80}"
  default_https="${default_https:-443}"

  if [ -z "$TAKO_HTTP_PORT" ] && is_enabled "$TAKO_RESTART_SERVICE"; then
    TAKO_HTTP_PORT="$(prompt_with_default "HTTP port" "$default_http")"
  fi
  if [ -z "$TAKO_HTTPS_PORT" ] && is_enabled "$TAKO_RESTART_SERVICE"; then
    TAKO_HTTPS_PORT="$(prompt_with_default "HTTPS port" "$default_https")"
  fi

  TAKO_HTTP_PORT="${TAKO_HTTP_PORT:-$default_http}"
  TAKO_HTTPS_PORT="${TAKO_HTTPS_PORT:-$default_https}"

  validate_port_value "HTTP port" "$TAKO_HTTP_PORT"
  validate_port_value "HTTPS port" "$TAKO_HTTPS_PORT"
  if [ "$TAKO_HTTP_PORT" = "$TAKO_HTTPS_PORT" ]; then
    echo "error: HTTP and HTTPS ports must differ" >&2
    exit 1
  fi

  if [ "$TAKO_HTTP_PORT" != "80" ]; then
    echo "warning: ACME HTTP-01 needs public port 80. Forward port 80 to Tako, or use DNS-01/manual certificates." >&2
  fi

  TAKO_PUBLIC_PORT_ARGS="--http-port $TAKO_HTTP_PORT --https-port $TAKO_HTTPS_PORT"
  echo "OK public ports: HTTP $TAKO_HTTP_PORT, HTTPS $TAKO_HTTPS_PORT"
}

install_upgrade_helpers() {
  mkdir -p /etc/tako
  # The privileged helper reads only this root-owned configuration.
  case "$TAKO_HOME:$TAKO_USER" in
    *[!a-zA-Z0-9_./:-]*) echo "error: unsupported isolation path or user" >&2; exit 1 ;;
  esac
  printf '%s\n%s\n' "$TAKO_HOME" "$TAKO_USER" > /etc/tako/isolation.conf
  chown root:root /etc/tako /etc/tako/isolation.conf
  chmod 0755 /etc/tako
  chmod 0644 /etc/tako/isolation.conf
  cat > /usr/local/bin/tako-provision-app <<'EOF'
#!/bin/sh
set -eu
exec /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin /usr/local/bin/tako-server --provision-app "$@"
EOF
  chown root:root /usr/local/bin/tako-provision-app
  chmod 0755 /usr/local/bin/tako-provision-app
  helper_dir=/usr/local/libexec/tako
  mkdir -p "$helper_dir"
  chown root:root "$helper_dir"
  chmod 0755 "$helper_dir"
  case "$TAKO_SOCKET" in
    *[!a-zA-Z0-9_./:-]*) echo "error: unsupported socket path" >&2; exit 1 ;;
  esac
  printf '%s\n%s\n%s\n' "$TAKO_USER" "$TAKO_HOME" "$TAKO_SOCKET" > "$helper_dir/install-settings"
  chown root:root "$helper_dir/install-settings"
  chmod 0644 "$helper_dir/install-settings"
  printf '%s\n' "$TAKO_VPN" > "$helper_dir/vpn-provider"
  chown root:root "$helper_dir/vpn-provider"
  chmod 0644 "$helper_dir/vpn-provider"
  {
    printf '#!/bin/sh\nset -eu\n'
    vpn_functions
    printf 'detect_management_ip "$@"\n'
  } > "$helper_dir/vpn-address"
  chown root:root "$helper_dir/vpn-address"
  chmod 0755 "$helper_dir/vpn-address"
  cat > "$TAKO_SERVER_INSTALL_REFRESH_HELPER" <<'EOF'
#!/bin/sh
set -eu
exec /usr/bin/env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin /bin/sh /usr/local/libexec/tako/refresh "$@"
EOF
  cat > "$helper_dir/refresh" <<'EOF'
#!/bin/sh
set -eu
[ "$#" -eq 2 ] || { echo "error: refresh requires an archive name and SHA-256" >&2; exit 1; }
archive_name="$1"
expected_sha="$2"
case "$archive_name" in
  tako-server-linux-x86_64-glibc.tar.zst|tako-server-linux-aarch64-glibc.tar.zst) ;;
  *) echo "error: unsupported release archive" >&2; exit 1 ;;
esac
case "$expected_sha" in
  ''|*[!a-fA-F0-9]*) echo "error: invalid release SHA-256" >&2; exit 1 ;;
esac
[ "${#expected_sha}" -eq 64 ] || { echo "error: invalid release SHA-256" >&2; exit 1; }

{
  IFS= read -r TAKO_USER
  IFS= read -r TAKO_HOME
  IFS= read -r TAKO_SOCKET
} < /usr/local/libexec/tako/install-settings
TAKO_VPN=native
if [ -r /usr/local/libexec/tako/vpn-provider ]; then
  IFS= read -r TAKO_VPN < /usr/local/libexec/tako/vpn-provider
fi
export TAKO_USER TAKO_HOME TAKO_SOCKET TAKO_VPN

installer_url="https://tako.sh/install-server.sh"
temporary_dir="$(mktemp -d)"
installer="$temporary_dir/install-server.sh"
archive="$temporary_dir/$archive_name"
trap 'rm -rf "$temporary_dir"' EXIT

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    echo "error: missing required downloader (curl or wget)" >&2
    exit 1
  fi
}
download "$installer_url" "$installer"
download "https://github.com/tako-sh/tako/releases/download/latest/$archive_name" "$archive"
if command -v sha256sum >/dev/null 2>&1; then
  actual_sha="$(sha256sum "$archive" | awk '{print $1}')"
else
  actual_sha="$(shasum -a 256 "$archive" | awk '{print $1}')"
fi
expected_sha="$(printf '%s' "$expected_sha" | tr 'A-F' 'a-f')"
[ "$actual_sha" = "$expected_sha" ] || { echo "error: release SHA-256 mismatch" >&2; exit 1; }
printf '%s\n' "$expected_sha" > "$archive.sha256"

install -m 0755 /usr/local/bin/tako-server /usr/local/bin/tako-server.prev
if ! TAKO_SERVER_URL="file://$archive" TAKO_ALLOW_INSECURE_DOWNLOAD_BASE=1 TAKO_RESTART_SERVICE=0 sh "$installer" </dev/null; then
  /usr/local/bin/tako-server-service rollback
  exit 1
fi
EOF
  chmod 0755 "$TAKO_SERVER_INSTALL_REFRESH_HELPER"

  cat > "$TAKO_SERVER_SERVICE_HELPER" <<'EOF'
#!/bin/sh
set -eu
exec /usr/bin/env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin /bin/sh /usr/local/libexec/tako/service "$@"
EOF
  cat > "$helper_dir/service" <<'EOF'
#!/bin/sh
set -eu

action="${1:-}"
case "$action" in
  reload|restart|rollback|cleanup-upgrade|implode)
    [ "$#" -eq 1 ] || { echo "error: expected one maintenance action" >&2; exit 1; }
    ;;
  configure)
    [ "$#" -eq 4 ] || [ "$#" -eq 5 ] || { echo "error: configure requires HTTP and HTTPS ports, a VPN provider, and an optional VPN IP" >&2; exit 1; }
    case "$4" in
      native) ;;
      *) echo "error: only native Tako networking is supported" >&2; exit 1 ;;
    esac
    for port in "$2" "$3"; do
      case "$port" in
        ''|*[!0-9]*) echo "error: ports must be between 1 and 65535" >&2; exit 1 ;;
      esac
      [ "${#port}" -le 5 ] && [ "$port" -ge 1 ] && [ "$port" -le 65535 ] || {
        echo "error: ports must be between 1 and 65535" >&2; exit 1;
      }
    done
    [ "$2" -ne "$3" ] || { echo "error: HTTP and HTTPS ports must differ" >&2; exit 1; }
    ;;
  *)
    echo "error: unsupported maintenance action" >&2
    exit 1
    ;;
esac

saved_vpn=native
if [ -r /usr/local/libexec/tako/vpn-provider ]; then
  IFS= read -r saved_vpn < /usr/local/libexec/tako/vpn-provider
fi

case "$action" in
  rollback)
    [ -f /usr/local/bin/tako-server.prev ] || { echo "error: previous tako-server binary not found" >&2; exit 1; }
    install -m 0755 /usr/local/bin/tako-server.prev /usr/local/bin/tako-server
    exit 0
    ;;
  cleanup-upgrade)
    rm -f /usr/local/bin/tako-server.prev
    exit 0
    ;;
  configure)
    [ "$saved_vpn" = "$4" ] || { echo "error: server uses $saved_vpn; reinstall as administrator to change its VPN provider" >&2; exit 1; }
    management_host="$(/usr/local/libexec/tako/vpn-address "$saved_vpn" "${5:-}")" || {
      echo "error: connect this server to $saved_vpn before starting tako-server" >&2; exit 1;
    }
    for service_file in /etc/systemd/system/tako-server.service /etc/systemd/system/tako-server-standby.service; do
      [ -f "$service_file" ] || continue
      if [ "$service_file" != /etc/systemd/system/tako-server-standby.service ]; then
        if grep -q -- '--management-host ' "$service_file"; then
          sed -i "s/--management-host [^ ]*/--management-host $management_host/" "$service_file"
        else
          sed -i "s/--https-port [0-9][0-9]*/& --management-host $management_host/" "$service_file"
        fi
      fi
      sed -i "s/--http-port [0-9][0-9]*/--http-port $2/g; s/--https-port [0-9][0-9]*/--https-port $3/g" "$service_file"
    done
    if command -v systemctl >/dev/null 2>&1; then systemctl daemon-reload; fi
    action=restart
    ;;
  implode)
    {
      IFS= read -r tako_user
      IFS= read -r tako_home
      IFS= read -r tako_socket
    } < /usr/local/libexec/tako/install-settings
    # Refuse broad directories, including an administrator's invalid install setting.
    case "$tako_home" in
      /opt/?*|/srv/?*|/var/lib/?*) ;;
      *) echo "error: remove this custom data directory as administrator" >&2; exit 1 ;;
    esac
    case "$tako_socket" in
      /run/?*/?*|/var/run/?*/?*) ;;
      *) echo "error: remove this custom socket directory as administrator" >&2; exit 1 ;;
    esac
    case "$tako_home:$tako_socket" in
      *..*) echo "error: invalid install paths" >&2; exit 1 ;;
    esac
    if command -v systemctl >/dev/null 2>&1; then
      systemctl stop tako-server tako-server-standby
      systemctl disable tako-server tako-server-standby 2>/dev/null || true
      if [ -f /etc/systemd/system/tako-network-agent.service ]; then
        systemctl disable --now tako-network-agent.service
      fi
    fi
    rm -f /etc/systemd/system/tako-server.service /etc/systemd/system/tako-server-standby.service /etc/systemd/system/tako-network-agent.service
    rm -rf /etc/systemd/system/tako-server.service.d
    if command -v systemctl >/dev/null 2>&1; then systemctl daemon-reload; fi
    rm -rf -- "$tako_home"
    rm -f -- "$tako_socket"
    rmdir -- "$(dirname "$tako_socket")" 2>/dev/null || true
    rm -f /usr/local/bin/tako-server /usr/local/bin/tako-server.prev /usr/local/bin/tako-provision-app /usr/local/bin/tako-server-service /usr/local/bin/tako-server-install-refresh
    rm -f /etc/sudoers.d/tako /etc/tako/isolation.conf
    rm -rf /usr/local/libexec/tako
    exit 0
    ;;
esac

if command -v systemctl >/dev/null 2>&1; then
  # Enrollment and maintenance repair installs that were started without enabling boot startup.
  systemctl enable tako-server
  systemctl "$action" tako-server
else
  echo "error: systemd is required to manage tako-server" >&2
  exit 1
fi
EOF
  chmod 0755 "$TAKO_SERVER_SERVICE_HELPER"
  chown root:root "$TAKO_SERVER_SERVICE_HELPER" "$TAKO_SERVER_INSTALL_REFRESH_HELPER" "$helper_dir/refresh" "$helper_dir/service"
  chmod 0644 "$helper_dir/refresh" "$helper_dir/service"

  cat > /etc/sudoers.d/tako <<EOF
# Managed by Tako install-server.
# Fixed root-owned helpers validate their arguments; no arbitrary root shell.
$TAKO_USER ALL=(root) NOPASSWD: /usr/local/bin/tako-provision-app *, $TAKO_SERVER_INSTALL_REFRESH_HELPER *, $TAKO_SERVER_SERVICE_HELPER reload, $TAKO_SERVER_SERVICE_HELPER restart, $TAKO_SERVER_SERVICE_HELPER configure *, $TAKO_SERVER_SERVICE_HELPER rollback, $TAKO_SERVER_SERVICE_HELPER cleanup-upgrade, $TAKO_SERVER_SERVICE_HELPER implode
EOF
  chmod 0440 /etc/sudoers.d/tako

  if need_cmd visudo; then
    if ! visudo -cf /etc/sudoers.d/tako >/dev/null 2>&1; then
      echo "error: generated sudoers policy is invalid (/etc/sudoers.d/tako)" >&2
      exit 1
    fi
  fi
}

ensure_privileged_bind_capability() {
  : # Native units grant capabilities to the proxy; the agent binary has none.
}

if is_enabled "$TAKO_RESTART_SERVICE" && [ "$SERVICE_MANAGER" = "none" ]; then
  echo "error: systemd must be running to start tako-server" >&2
  exit 1
fi

if [ -z "$TAKO_VPN" ] && [ -r /usr/local/libexec/tako/vpn-provider ]; then
  IFS= read -r TAKO_VPN < /usr/local/libexec/tako/vpn-provider
fi
TAKO_VPN="${TAKO_VPN:-native}"
if [ "$TAKO_VPN" != native ]; then
  echo "error: only native Tako networking is supported; re-enroll with tako servers add admin@host --network <network>" >&2
  exit 1
fi

maybe_prompt_ssh_pubkey() {
  is_valid_ssh_public_key() {
    key_line="$1"
    key_type="$(printf '%s\n' "$key_line" | awk '{print $1}')"
    key_blob="$(printf '%s\n' "$key_line" | awk '{print $2}')"

    if [ -z "$key_type" ] || [ -z "$key_blob" ]; then
      return 1
    fi

    case "$key_type" in
      ssh-ed25519|ssh-rsa|ssh-dss|ecdsa-sha2-nistp256|ecdsa-sha2-nistp384|ecdsa-sha2-nistp521|sk-ssh-ed25519@openssh.com|sk-ecdsa-sha2-nistp256@openssh.com)
        ;;
      *)
        return 1
        ;;
    esac

    printf '%s\n' "$key_blob" | grep -Eq '^[A-Za-z0-9+/=]+$'
  }

  first_valid_authorized_key() {
    auth_file="$1"
    if [ ! -r "$auth_file" ]; then
      return 1
    fi
    awk '
      /^[[:space:]]*#/ { next }
      NF < 2 { next }
      $1 ~ /^(ssh-ed25519|ssh-rsa|ssh-dss|ecdsa-sha2-nistp256|ecdsa-sha2-nistp384|ecdsa-sha2-nistp521|sk-ssh-ed25519@openssh.com|sk-ecdsa-sha2-nistp256@openssh.com)$/ && $2 ~ /^[A-Za-z0-9+\/=]+$/ { print $1 " " $2; exit }
    ' "$auth_file"
  }

  maybe_use_invoking_user_key() {
    invoking_user="${SUDO_USER:-}"
    if [ -z "$invoking_user" ] || [ "$invoking_user" = "root" ]; then
      return 1
    fi

    invoking_home=""
    if need_cmd getent; then
      invoking_home="$(getent passwd "$invoking_user" 2>/dev/null | awk -F: '{print $6}' || true)"
    fi
    if [ -z "$invoking_home" ]; then
      invoking_home="$(awk -F: -v u="$invoking_user" '$1==u {print $6}' /etc/passwd 2>/dev/null || true)"
    fi
    if [ -z "$invoking_home" ]; then
      return 1
    fi

    fallback_key="$(first_valid_authorized_key "$invoking_home/.ssh/authorized_keys" || true)"
    if ! is_valid_ssh_public_key "$fallback_key"; then
      return 1
    fi

    TAKO_SSH_PUBKEY="$fallback_key"
    echo "OK using SSH key from '$invoking_user' authorized_keys for '$TAKO_USER'"
    return 0
  }

  if [ -n "${TAKO_SSH_PUBKEY:-}" ]; then
    if ! is_valid_ssh_public_key "$TAKO_SSH_PUBKEY"; then
      echo "error: TAKO_SSH_PUBKEY must be a single SSH public key line (for example: ssh-ed25519 AAAA...)." >&2
      exit 1
    fi
    return
  fi

  echo "SSH setup:"
  echo "  To allow SSH login as '$TAKO_USER', paste your public key."
  echo "  Get one from your local machine with: cat ~/.ssh/id_ed25519.pub"
  echo "  If needed, create one with: ssh-keygen -t ed25519"

  if [ -t 0 ] && [ -t 1 ]; then
    while :; do
      printf "Public key for '$TAKO_USER': "
      if ! IFS= read -r TAKO_SSH_PUBKEY; then
        if ! maybe_use_invoking_user_key; then
          echo "warning: could not read SSH key input; skipping SSH key setup." >&2
          echo "warning: re-run with TAKO_SSH_PUBKEY='ssh-ed25519 ...' to install a key." >&2
          TAKO_SSH_PUBKEY=""
        fi
        break
      fi
      if is_valid_ssh_public_key "$TAKO_SSH_PUBKEY"; then
        break
      fi
      echo "warning: invalid SSH public key format. Paste the full key line (for example: ssh-ed25519 AAAA...)." >&2
    done
  elif [ -r /dev/tty ] && [ -w /dev/tty ] && (printf '' > /dev/tty) 2>/dev/null; then
    # Support common piped installs (curl ... | sudo sh) by prompting on the controlling tty.
    while :; do
      printf "Public key for '$TAKO_USER': " > /dev/tty
      if ! IFS= read -r TAKO_SSH_PUBKEY < /dev/tty; then
        if ! maybe_use_invoking_user_key; then
          echo "warning: could not read SSH key input from terminal; skipping SSH key setup." > /dev/tty
          echo "warning: re-run with TAKO_SSH_PUBKEY='ssh-ed25519 ...' to install a key." > /dev/tty
          TAKO_SSH_PUBKEY=""
        fi
        break
      fi
      if is_valid_ssh_public_key "$TAKO_SSH_PUBKEY"; then
        break
      fi
      echo "warning: invalid SSH public key format. Paste the full key line (for example: ssh-ed25519 AAAA...)." > /dev/tty
    done
  else
    if ! maybe_use_invoking_user_key; then
      echo "warning: non-interactive install; skipping SSH key prompt." >&2
      echo "warning: re-run with TAKO_SSH_PUBKEY='ssh-ed25519 ...' to install a key." >&2
    fi
  fi
}

install_pkgs() {
  # Avoid arrays for POSIX sh compatibility.
  if need_cmd apt-get; then
    apt-get update -y
    apt-get install -y "$@"
  elif need_cmd dnf; then
    dnf install -y "$@"
  elif need_cmd yum; then
    yum install -y "$@"
  elif need_cmd pacman; then
    pacman -Sy --noconfirm "$@"
  elif need_cmd zypper; then
    zypper --non-interactive install "$@"
  else
    echo "error: unsupported package manager; install curl + ca-certificates + tar manually" >&2
    exit 1
  fi
}

install_native_corrosion() {
  case "$arch" in
    x86_64) corrosion_archive_sha256=3504d7d1b4b53737457fc40f2353a400cf4df0c1217ec318924d7ee310876194 ;;
    aarch64) corrosion_archive_sha256=b00b5c6996dee89c36e51e1d4196602bb7e12bbb25abc9b68390e062bdd13be3 ;;
    *) echo "error: unsupported Corrosion architecture: $arch" >&2; exit 1 ;;
  esac
  corrosion_archive="$tmp_extract/corrosion.tar.gz"
  download_file "https://github.com/superfly/corrosion/releases/download/v1.0.0/corrosion-$arch-unknown-linux-gnu.tar.gz" "$corrosion_archive"
  corrosion_actual="$(sha256sum "$corrosion_archive" | awk '{print $1}')"
  [ "$corrosion_actual" = "$corrosion_archive_sha256" ] || {
    echo 'error: Corrosion archive checksum mismatch' >&2; exit 1;
  }
  mkdir "$tmp_extract/corrosion-release"
  tar -xzf "$corrosion_archive" -C "$tmp_extract/corrosion-release" -- corrosion
  corrosion_source="$tmp_extract/corrosion-release/corrosion"
  [ -f "$corrosion_source" ] && [ ! -L "$corrosion_source" ] && [ -s "$corrosion_source" ] || {
    echo 'error: Corrosion archive must contain a regular executable' >&2; exit 1;
  }
  sha256sum "$corrosion_source" | awk '{print $1}' > "$tmp_extract/corrosion.sha256"
  # Rename preserves a running agent's executable during install-refresh.
  install -o root -g root -m 0755 "$corrosion_source" /usr/local/libexec/tako/corrosion.new
  install -o root -g root -m 0644 "$tmp_extract/corrosion.sha256" /usr/local/libexec/tako/corrosion.sha256.new
  mv -f /usr/local/libexec/tako/corrosion.new /usr/local/libexec/tako/corrosion
  mv -f /usr/local/libexec/tako/corrosion.sha256.new /usr/local/libexec/tako/corrosion.sha256
  echo 'OK installed checksum-verified Corrosion v1.0.0 and executable digest'
}

install_native_network() {
  [ "$TAKO_VPN" = native ] || { echo "error: only native Tako networking is supported" >&2; return 1; }
  [ "$TAKO_USER:$TAKO_HOME" = tako:/opt/tako ] || {
    echo 'error: native networking requires TAKO_USER=tako and TAKO_HOME=/opt/tako' >&2
    exit 1
  }
  if need_cmd pacman; then
    install_pkgs wireguard-tools iproute2 nftables python openssh
  elif need_cmd apt-get || need_cmd zypper; then
    install_pkgs wireguard-tools iproute2 nftables python3 openssh-server
  else
    install_pkgs wireguard-tools iproute nftables python3 openssh-server
  fi
  for native_command in wg ip nft python3; do
    need_cmd "$native_command" || { echo "error: missing $native_command" >&2; exit 1; }
  done

  install_native_corrosion

  native_source="${TAKO_NETWORK_HELPER_SOURCE:-}"
  native_expected=""
  if [ -n "$native_source" ]; then
    [ -f "$native_source" ] && [ ! -L "$native_source" ] || {
      echo 'error: native helper source must be a regular local file' >&2; exit 1;
    }
    native_expected="${TAKO_NETWORK_HELPER_SHA256:-}"
    case "$native_expected" in
      ''|*[!0-9a-f]*) echo 'error: native helper requires an explicit SHA256' >&2; exit 1 ;;
    esac
    [ "${#native_expected}" -eq 64 ] || { echo 'error: invalid helper SHA256' >&2; exit 1; }
    cp "$native_source" "$tmp_extract/native-helper.py"
    native_source="$tmp_extract/native-helper.py"
  else
    native_source="$tmp_extract/scripts/tako-network-helper.py"
    if [ ! -f "$native_source" ] || [ -L "$native_source" ]; then
      native_source="$tmp_extract/native-helper.py"
      native_expected="97ccb6db11373d26a083bd46d0c3ee549a9ea2efce221d53e1973e617da4f7f9"
      download_file "https://raw.githubusercontent.com/$TAKO_REPO_OWNER/$TAKO_REPO_NAME/$TAKO_RELEASE_TAG/scripts/tako-network-helper.py" "$native_source"
    fi
  fi
  if [ -n "$native_expected" ]; then
    native_actual="$(sha256sum "$native_source" | awk '{print $1}')"
    [ "$native_actual" = "$native_expected" ] || { echo 'error: native helper checksum mismatch' >&2; exit 1; }
  fi

  # Validate the desired-state, root-state, and network SSH directories without following links.
  python3 -I - <<'PY'
import os
import pwd
uid = pwd.getpwnam('tako').pw_uid
gid = pwd.getpwnam('tako').pw_gid
for path in ('/opt/tako/networks', '/var/lib/tako-network-helper', '/var/lib/tako-network-helper/docker', '/etc/ssh/tako-network'):
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in path.split('/')[1:]:
            try:
                os.mkdir(part, 0o755, dir_fd=fd)
            except FileExistsError:
                pass
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
            info = os.fstat(fd)
            allowed = (0, uid) if path == '/opt/tako/networks' else (0,)
            if info.st_uid not in allowed or info.st_mode & 0o022:
                raise SystemExit('error: unsafe network directory')
        os.fchown(fd, uid if path == '/opt/tako/networks' else 0,
                  gid if path == '/opt/tako/networks' else 0)
        os.fchmod(fd, 0o755 if path == '/etc/ssh/tako-network' else 0o700)
    finally:
        os.close(fd)
PY
  install -m 0644 "$native_source" /usr/local/libexec/tako/tako-network-helper.py
  cat > /usr/local/libexec/tako/tako-network-helper <<'EOF'
#!/bin/sh
exec /usr/bin/python3 -I /usr/local/libexec/tako/tako-network-helper.py "$@"
EOF
  chmod 0755 /usr/local/libexec/tako/tako-network-helper
  chown root:root /usr/local/libexec/tako/tako-network-helper /usr/local/libexec/tako/tako-network-helper.py
  cat > /usr/local/libexec/tako/native-address <<'EOF'
#!/usr/bin/python3 -I
import importlib.util
import json
import sys
spec = importlib.util.spec_from_file_location('network', '/usr/local/libexec/tako/tako-network-helper.py')
network = importlib.util.module_from_spec(spec)
spec.loader.exec_module(network)
try:
    with network.directory(network.STATE) as fd:
        state = json.loads(network.read_file(fd, 'state.json'))
    host = network.Host(network.Runner(), state, None, None)
    candidates = []
    for ident, config in state['networks'].items():
        desired = network.read_desired(ident)
        link = host.owned_link(ident)
        if not link or 'UP' not in link.get('flags', []):
            continue
        value = desired['address'].split('/')[0]
        if config['address'] != desired['address']:
            continue
        addresses = json.loads(host.runner.run('ip', '-j', '-6', 'address', 'show', 'dev', network.interface(ident)))
        if any(a.get('local') == value and a.get('prefixlen') == 128
               for entry in addresses for a in entry.get('addr_info', [])):
            candidates.append(value)
    preferred = sys.argv[1] if len(sys.argv) == 2 else ''
    if preferred:
        candidates = [value for value in candidates if value == preferred]
    if len(candidates) != 1:
        raise ValueError('ambiguous or unavailable native management address')
    print(candidates[0])
except (OSError, ValueError, KeyError, network.Rejected):
    sys.exit(1)
EOF
  chown root:root /usr/local/libexec/tako/native-address
  chmod 0755 /usr/local/libexec/tako/native-address

  mkdir -p /etc/ssh/sshd_config.d /etc/systemd/system
  cat > /usr/local/libexec/tako/network-authorized-keys <<'EOF'
#!/usr/bin/python3 -I
import importlib.util
import sys
if sys.argv[1:] != ['tako']:
    sys.exit(1)
spec = importlib.util.spec_from_file_location('network', '/usr/local/libexec/tako/tako-network-helper.py')
network = importlib.util.module_from_spec(spec)
spec.loader.exec_module(network)
try:
    with network.directory(network.SSH) as fd:
        sys.stdout.write(network.read_file(fd, 'authorized_keys', private=False))
except (OSError, ValueError, network.Rejected):
    sys.exit(1)
EOF
  chown root:root /usr/local/libexec/tako/network-authorized-keys
  chmod 0755 /usr/local/libexec/tako/network-authorized-keys
  python3 -I - <<'SSH_POLICY_PY'
import glob
import os
from pathlib import Path
import re
import secrets
import shlex
import stat
import subprocess

ssh_dir = Path('/etc/ssh')
config_path = ssh_dir / 'sshd_config'
policy_path = ssh_dir / 'sshd_config.d/00-tako-network.conf'
policy = '''Match User tako
    AuthorizedKeysCommand /usr/local/libexec/tako/network-authorized-keys %u
    AuthorizedKeysCommandUser tako
Match all
'''


def read_config(path, *, included=False):
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
    if included:
        # Distributions such as AlmaLinux symlink their SSH crypto policy.
        # Only follow administrator-owned links while inspecting includes;
        # files we rewrite must remain regular files, checked without following.
        link = path.lstat()
        if stat.S_ISLNK(link.st_mode):
            if link.st_uid != os.geteuid():
                raise SystemExit('error: unsafe SSH configuration symlink')
            flags &= ~os.O_NOFOLLOW
    fd = os.open(path, flags)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or (not included and info.st_nlink != 1)
                or info.st_uid != os.geteuid() or info.st_mode & 0o022):
            raise SystemExit(f'error: unsafe SSH configuration file: {path}')
        content = stream.read(1024 * 1024 + 1)
        if len(content) > 1024 * 1024:
            raise SystemExit('error: SSH configuration too large')
        return content.decode(), stat.S_IMODE(info.st_mode)


try:
    old_policy, policy_mode = read_config(policy_path)
except FileNotFoundError:
    old_policy, policy_mode = None, 0o644
if old_policy is not None and old_policy != policy and not re.fullmatch(
        r'Match User tako\n    AuthorizedKeysFile [^\n]* /etc/ssh/tako-network/authorized_keys\nMatch all\n', old_policy):
    raise SystemExit('error: refusing to replace an administrator-owned network SSH policy')

seen = set()

def inspect(path):
    if path == policy_path or path in seen:
        return
    if len(seen) >= 256:
        raise SystemExit('error: too many SSH configuration includes')
    seen.add(path)
    text, _ = read_config(path, included=path != config_path)
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        match = re.fullmatch(r'\s*([A-Za-z][A-Za-z0-9]*)\s*(?:=\s*)?(.*)', line)
        if not match:
            raise SystemExit('error: unsupported SSH configuration syntax')
        keyword, raw = match.groups()
        arguments = shlex.split(raw, comments=True)
        if keyword.lower() == 'authorizedkeyscommand' and arguments != ['none']:
            raise SystemExit('error: existing AuthorizedKeysCommand requires administrator integration')
        if keyword.lower() == 'include':
            for pattern in arguments:
                if not os.path.isabs(pattern):
                    pattern = str(ssh_dir / pattern)
                for included in sorted(glob.glob(pattern)):
                    inspect(Path(included))


inspect(config_path)
original, config_mode = read_config(config_path)
include = f'Include {policy_path}\n'
updated = original if original.startswith(include) else include + original


def replace(path, text, mode):
    parent = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    temporary = '.tako-ssh-' + secrets.token_hex(16)
    try:
        info = os.fstat(parent)
        if info.st_uid != os.geteuid() or info.st_mode & 0o022:
            raise SystemExit('error: unsafe SSH configuration directory')
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode, dir_fd=parent)
        with os.fdopen(fd, 'w') as stream:
            os.fchmod(stream.fileno(), mode)
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
        os.rename(temporary, path.name, src_dir_fd=parent, dst_dir_fd=parent)
        os.fsync(parent)
    finally:
        try:
            os.unlink(temporary, dir_fd=parent)
        except FileNotFoundError:
            pass
        os.close(parent)


try:
    replace(policy_path, policy, policy_mode)
    replace(config_path, updated, config_mode)
    result = subprocess.run(['/usr/sbin/sshd', '-t'], capture_output=True)
    if result.returncode:
        raise RuntimeError('sshd validation failed')
except Exception:
    replace(config_path, original, config_mode)
    if old_policy is None:
        policy_path.unlink(missing_ok=True)
    else:
        replace(policy_path, old_policy, policy_mode)
    raise SystemExit('error: SSH configuration validation failed; original policy restored') from None
SSH_POLICY_PY
  if [ "$SERVICE_MANAGER" = systemd ]; then
    if systemctl is-active --quiet ssh; then systemctl reload ssh; fi
    if systemctl is-active --quiet sshd; then systemctl reload sshd; fi
  fi

  cat > /etc/systemd/system/tako-network-helper.socket <<'EOF'
[Unit]
Description=Tako privileged network request socket
[Socket]
ListenStream=/run/tako-network-helper.sock
SocketUser=root
SocketGroup=tako
SocketMode=0660
RemoveOnStop=yes
[Install]
WantedBy=sockets.target
EOF
  cat > /etc/systemd/system/tako-network-helper.service <<'EOF'
[Unit]
Description=Tako privileged network reconciler
Requires=tako-network-helper.socket
After=tako-network-helper.socket
[Service]
ExecStart=/usr/bin/python3 -I /usr/local/libexec/tako/tako-network-helper.py --serve
User=root
Group=root
NoNewPrivileges=true
CapabilityBoundingSet=CAP_NET_ADMIN CAP_DAC_OVERRIDE CAP_CHOWN CAP_FOWNER
RestrictAddressFamilies=AF_UNIX AF_NETLINK AF_INET AF_INET6
Restart=on-failure
EOF
  cat > /etc/systemd/system/tako-network.target <<'EOF'
[Unit]
Description=Tako native networks
Before=tako-server.service tako-server-standby.service
[Install]
WantedBy=multi-user.target
EOF
  cat > /etc/systemd/system/tako-network@.service <<'EOF'
[Unit]
Description=Tako native network %i
Requires=tako-network-helper.socket
After=network-online.target tako-network-helper.socket
Wants=network-online.target
Before=tako-network.target
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/libexec/tako/tako-network-helper %i apply
[Install]
RequiredBy=tako-network.target
EOF

  if [ -f /etc/systemd/system/tako-corrosion.service ]; then
    if [ "$SERVICE_MANAGER" = systemd ]; then
      systemctl disable --now tako-corrosion.service
    fi
    rm -f /etc/systemd/system/tako-corrosion.service
  fi
  cat > /etc/systemd/system/tako-network-agent.service <<'EOF'
[Unit]
Description=Tako native network agent
Requires=tako-network-helper.socket
After=network-online.target tako-network.target tako-network-helper.socket
Wants=network-online.target tako-network.target
Before=tako-server.service tako-server-standby.service
ConditionPathIsExecutable=/usr/local/libexec/tako/corrosion
ConditionPathExists=/opt/tako/native-network.json
[Service]
Type=exec
User=tako
Group=tako
WorkingDirectory=/opt/tako
ExecStart=/usr/local/bin/tako-server --network-agent --data-dir /opt/tako
Restart=on-failure
RestartSec=1
KillMode=control-group
TimeoutStopSec=120
UMask=0077
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/opt/tako
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
[Install]
WantedBy=multi-user.target
EOF
  chown root:root /etc/systemd/system/tako-network-agent.service
  chmod 0644 /etc/systemd/system/tako-network-agent.service
  TAKO_NETWORK_UNIT_ARGS='Requires=tako-network.target
After=tako-network.target'
  if [ "$SERVICE_MANAGER" = systemd ]; then
    systemctl daemon-reload
    systemctl enable --now tako-network-helper.socket
    if systemctl is-active --quiet tako-network-helper.service; then
      systemctl restart tako-network-helper.service
    fi
    systemctl enable tako-network-agent.service
  fi
  for desired_file in /opt/tako/networks/*/desired.json; do
    [ -e "$desired_file" ] || continue
    native_id="${desired_file%/desired.json}"
    native_id="${native_id##*/}"
    /usr/local/libexec/tako/tako-network-helper "$native_id" apply
  done
  if [ "$SERVICE_MANAGER" = systemd ] && is_enabled "$TAKO_RESTART_SERVICE" &&
     [ -f /opt/tako/native-network.json ]; then
    systemctl restart tako-network-agent.service
  fi
  echo 'OK native WireGuard installed. Allow its UDP listen port in your cloud security group if required.'
  echo 'OK native network agent installed; activation requires native-network.json and service-start mode.'
}


install_sqlite_runtime() {
  if need_cmd apt-get; then
    apt-get update -y
    apt-get install -y libsqlite3-0
  elif need_cmd dnf; then
    dnf install -y sqlite-libs
  elif need_cmd yum; then
    yum install -y sqlite-libs
  elif need_cmd pacman; then
    pacman -Sy --noconfirm sqlite
  elif need_cmd zypper; then
    zypper --non-interactive install sqlite3
  else
    echo "warning: unsupported package manager; install libsqlite3 runtime manually if needed." >&2
  fi
}

install_podman_runtime() {
  if need_cmd podman; then
    return
  fi

  echo "Installing Podman runtime"
  install_pkgs podman

  if ! need_cmd podman; then
    echo "error: podman not found after installing package" >&2
    exit 1
  fi
}

apt_available_avif_packages() {
  for apt_avif_pkg in libheif-plugin-aomenc libheif-plugin-aomdec libheif-plugin-dav1d; do
    if apt-cache show "$apt_avif_pkg" >/dev/null 2>&1; then
      printf '%s\n' "$apt_avif_pkg"
    fi
  done
}

install_apt_avif_plugins() {
  apt_avif_pkgs="$(apt_available_avif_packages | tr '\n' ' ')"
  if [ -n "$apt_avif_pkgs" ]; then
    apt-get install -y $apt_avif_pkgs
  fi
}

# dnf hosts fall into two groups: real Fedora, where "vips" installs
# directly, and RHEL-family clones (AlmaLinux/Rocky/RHEL/CentOS Stream),
# where no repo — including EPEL — ever carries it: Fedora's own package
# page (https://packages.fedoraproject.org/pkgs/vips/vips/) only builds
# Fedora branches, and that build requires glibc symbols RHEL9's glibc
# 2.34 doesn't export. Retrying the plain install after enabling EPEL/CRB
# can't succeed on the second group, so RHEL-family hosts go straight to
# Remi's RPM repository (https://rpms.remirepo.net/), which builds vips
# natively against each RHEL major version. This is also what libvips'
# own current maintainer recommends for RHEL9:
# https://github.com/libvips/pyvips/issues/356#issuecomment-1287872830
dnf_install_with_retry() {
  for dnf_attempt in 1 2 3; do
    if dnf install -y --refresh --disablerepo='remi-modular*' "$@"; then
      return 0
    fi
    if [ "$dnf_attempt" -eq 3 ]; then
      return 1
    fi
    # Mirror metadata can change between resolving a mirror and downloading it.
    dnf clean metadata
    sleep 5
  done
}

install_libvips_from_remi() {
  el_version="$(rpm -E %rhel 2>/dev/null || true)"
  case "$el_version" in
    '' | *[!0-9]*)
      echo "error: could not detect the RHEL major version (rpm -E %rhel) for Remi's repository" >&2
      return 1
      ;;
  esac

  dnf_install_with_retry epel-release || return 1
  if need_cmd dnf config-manager; then
    dnf config-manager --set-enabled crb || true
  fi
  # EPEL is already installed; its metadata is not needed to install this RPM.
  if ! dnf_install_with_retry --disablerepo='epel*' "https://rpms.remirepo.net/enterprise/remi-release-${el_version}.rpm"; then
    echo "error: failed to install Remi's repository configuration" >&2
    return 1
  fi
  dnf config-manager --set-enabled remi || true

  # install_weak_deps=False: vips' optional codec modules (heif, jxl,
  # ImageMagick/GraphicsMagick, OpenSlide, Poppler) pull in a desktop-sized
  # dependency chain (GTK, WebKit, PulseAudio, X11 fonts...) as recommends.
  # tako-server only needs the base runtime library.
  # libvips comes from the non-modular repository, not the PHP module streams.
  dnf_install_with_retry --setopt=install_weak_deps=False vips
}

install_libvips_runtime() {
  echo "Installing libvips runtime"
  if need_cmd apt-get; then
    apt-get update -y
    apt_avif_pkgs=
    apt_avif_pkgs="$(apt_available_avif_packages | tr '\n' ' ')"
    for apt_vips_pkg in libvips42t64 libvips42 libvips; do
      if apt-get install -y "$apt_vips_pkg" $apt_avif_pkgs; then
        return
      fi
    done
    return 1
  elif need_cmd dnf; then
    if dnf install -y vips; then
      return
    fi
    install_libvips_from_remi
  elif need_cmd yum; then
    yum install -y libvips || yum install -y vips || {
      yum install -y epel-release
      yum install -y libvips || yum install -y vips
    }
  elif need_cmd pacman; then
    pacman -Sy --noconfirm vips
  elif need_cmd zypper; then
    zypper --non-interactive install libvips42 || zypper --non-interactive install vips
  else
    echo "error: unsupported package manager; install libvips manually before starting tako-server" >&2
    exit 1
  fi
}

install_libvips_codec_runtime() {
  if need_cmd apt-get; then
    apt-get update -y
    install_apt_avif_plugins || true
  fi
}

missing_runtime_libraries() {
  _bin="$1"
  if ! need_cmd ldd; then
    return
  fi

  ldd "$_bin" 2>&1 | awk '
    /not found/ { print $1 }
    /Error loading shared library/ {
      lib = $5
      sub(/:$/, "", lib)
      print lib
    }
  ' || true
}

install_missing_tako_server_runtime_deps() {
  _bin="$1"
  if ! need_cmd ldd; then
    return
  fi

  missing="$(missing_runtime_libraries "$_bin")"
  if [ -z "$missing" ]; then
    install_libvips_codec_runtime
    return
  fi

  if printf '%s\n' "$missing" | grep -Eq '^libvips(\.|$)'; then
    install_libvips_runtime
    return
  fi

  install_libvips_codec_runtime
}

verify_tako_server_runtime_deps() {
  if ! need_cmd ldd; then
    return
  fi

  missing="$(missing_runtime_libraries /usr/local/bin/tako-server)"
  if [ -n "$missing" ]; then
    echo "error: tako-server is missing runtime libraries:" >&2
    printf '%s\n' "$missing" >&2
    echo "Install the missing packages, then rerun the installer." >&2
    exit 1
  fi
}

tako_home_dir() {
  _home=""
  if need_cmd getent; then
    _home="$(getent passwd "$TAKO_USER" 2>/dev/null | awk -F: '{print $6}' || true)"
  fi
  if [ -z "$_home" ]; then
    _home="$(awk -F: -v u="$TAKO_USER" '$1==u {print $6}' /etc/passwd 2>/dev/null || true)"
  fi
  if [ -z "$_home" ]; then
    _home="/home/$TAKO_USER"
  fi
  printf '%s' "$_home"
}

detect_libc() {
  if need_cmd ldd; then
    ldd_out="$(ldd --version 2>&1 || true)"
    ldd_lower="$(printf "%s" "$ldd_out" | tr '[:upper:]' '[:lower:]')"
    if printf "%s" "$ldd_lower" | grep -q "musl"; then
      echo "musl"
      return
    fi
    if printf "%s" "$ldd_lower" | grep -Eq "glibc|gnu libc|gnu c library"; then
      echo "glibc"
      return
    fi
  fi

  if need_cmd getconf && getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
    echo "glibc"
    return
  fi

  if ls /lib/ld-musl-*.so.1 /usr/lib/ld-musl-*.so.1 >/dev/null 2>&1; then
    echo "musl"
    return
  fi

  if ls /lib/*-linux-gnu/libc.so.6 /usr/lib/*-linux-gnu/libc.so.6 >/dev/null 2>&1; then
    echo "glibc"
    return
  fi

  echo "unknown"
}

ensure_nc() {
  nc_supports_unix_socket() {
    if ! need_cmd nc; then
      return 1
    fi

    # Preferred check: implementation advertises -U in help output.
    if nc -h 2>&1 | grep -Eq '(^|[[:space:][:punct:]])-U([[:space:][:punct:]]|$)'; then
      return 0
    fi

    # Fallback probe: detect option-parser errors for -U.
    nc_err="$(nc -U /var/run/tako/nonexistent.sock 2>&1 >/dev/null || true)"
    if printf "%s" "$nc_err" | grep -Eqi 'unrecognized option|illegal option|invalid option'; then
      return 1
    fi

    # If parser accepted -U, treat as supported even if connect failed.
    return 0
  }

  if nc_supports_unix_socket; then
    return
  fi

  if need_cmd nc; then
    echo "warning: installed netcat ('nc') does not support Unix sockets (-U); installing a compatible netcat implementation." >&2
  fi

  if need_cmd apt-get; then
    apt-get update -y
    apt-get install -y netcat-openbsd || apt-get install -y netcat-traditional
  elif need_cmd dnf; then
    dnf install -y nmap-ncat || dnf install -y nc
  elif need_cmd yum; then
    yum install -y nmap-ncat || yum install -y nc
  elif need_cmd pacman; then
    pacman -Sy --noconfirm openbsd-netcat || pacman -Sy --noconfirm gnu-netcat
  elif need_cmd zypper; then
    zypper --non-interactive install netcat-openbsd || zypper --non-interactive install netcat
  else
    echo "error: unsupported package manager; install netcat ('nc') manually" >&2
    exit 1
  fi

  if ! need_cmd nc; then
    echo "error: netcat ('nc') not found after install. Install it manually and retry." >&2
    exit 1
  fi

  if ! nc_supports_unix_socket; then
    echo "error: netcat ('nc') does not support Unix sockets (-U)." >&2
    echo "Install a compatible implementation (for example: netcat-openbsd or nmap-ncat), then retry." >&2
    exit 1
  fi
}

libc="$(detect_libc)"
case "$libc" in
  glibc) ;;
  *)
    echo "error: unsupported libc: $libc; tako-server requires glibc" >&2
    exit 1
    ;;
esac

arch="$(uname -m)"
case "$arch" in
  x86_64|amd64) arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)
    echo "error: unsupported architecture: $arch (supported: x86_64, aarch64)" >&2
    exit 1
    ;;
esac

if ! need_cmd curl && ! need_cmd wget; then
  install_pkgs curl
fi
if ! need_cmd tar; then
  install_pkgs tar
fi
if ! need_cmd sha256sum && ! need_cmd shasum; then
  install_pkgs coreutils
fi
if ! need_cmd sudo; then
  install_pkgs sudo
fi
if ! need_cmd zstd; then
  install_pkgs zstd
fi
if ! need_cmd git; then
  install_pkgs git
fi
if ! need_cmd which; then
  install_pkgs which
fi
ensure_nc
install_sqlite_runtime
install_podman_runtime

download_url="${TAKO_SERVER_URL:-}"
if [ -z "$download_url" ]; then
  download_base="$TAKO_DOWNLOAD_BASE_URL"
  if [ -z "$download_base" ]; then
    download_base="https://github.com/$TAKO_REPO_OWNER/$TAKO_REPO_NAME/releases/download/$TAKO_RELEASE_TAG"
  else
    require_secure_download_override "$download_base"
  fi
  download_url="$download_base/tako-server-linux-$arch-$libc.tar.zst"
else
  require_secure_download_override "$download_url"
fi
case "$download_url" in
  *.tar.zst|file://*.tar.zst) ;;
  *.tar.gz|file://*.tar.gz) ;;
  *)
    echo "error: TAKO_SERVER_URL must point to a .tar.zst or .tar.gz archive" >&2
    exit 1
    ;;
esac
sha_url="${download_url}.sha256"

tmp_payload="$(mktemp)"
tmp_extract="$(mktemp -d)"
trap 'rm -f "$tmp_payload"; rm -rf "$tmp_extract"' EXIT

echo "Downloading tako-server: $download_url"
download_file "$download_url" "$tmp_payload"
echo "OK downloaded tako-server"

echo "Verifying tako-server checksum"
expected_sha=""
expected_sha="$(download_stdout "$sha_url" 2>/dev/null | awk '{print $1}' || true)"

if [ -n "$expected_sha" ]; then
  if need_cmd sha256sum; then
    actual="$(sha256sum "$tmp_payload" | awk '{print $1}')"
  else
    actual="$(shasum -a 256 "$tmp_payload" | awk '{print $1}')"
  fi
  if [ "$actual" != "$expected_sha" ]; then
    echo "error: sha256 mismatch (expected=$expected_sha actual=$actual)" >&2
    exit 1
  fi
else
  echo "error: could not fetch SHA256 ($sha_url); aborting install" >&2
  exit 1
fi
echo "OK verified tako-server checksum"

echo "Extracting tako-server"
case "$download_url" in
  *.tar.zst|file://*.tar.zst)
    zstd -d "$tmp_payload" --stdout | tar -x -C "$tmp_extract"
    ;;
  *)
    tar -xzf "$tmp_payload" -C "$tmp_extract"
    ;;
esac
tmp_bin="$(find "$tmp_extract" -type f -name tako-server | head -n 1 || true)"
if [ -z "$tmp_bin" ]; then
  echo "error: archive did not contain a tako-server binary" >&2
  exit 1
fi

install -m 0755 "$tmp_bin" /usr/local/bin/tako-server
echo "OK installed tako-server binary"
install_missing_tako_server_runtime_deps /usr/local/bin/tako-server
verify_tako_server_runtime_deps
ensure_privileged_bind_capability

# Create `tako` user.
if ! id -u "$TAKO_USER" >/dev/null 2>&1; then
  if need_cmd useradd; then
    groupadd --system "$TAKO_USER" 2>/dev/null || true
    useradd --system --create-home --home-dir "/home/$TAKO_USER" --shell /bin/bash --gid "$TAKO_USER" "$TAKO_USER" 2>/dev/null || \
      useradd --system --create-home --home-dir "/home/$TAKO_USER" --shell /bin/sh --gid "$TAKO_USER" "$TAKO_USER"
  elif need_cmd adduser; then
    addgroup -S "$TAKO_USER" 2>/dev/null || true
    adduser -S -D -h "/home/$TAKO_USER" -s /bin/sh -G "$TAKO_USER" "$TAKO_USER"
  else
    echo "error: missing useradd/adduser" >&2
    exit 1
  fi
fi

# Shadow's account tools are also used by the narrow provisioning helper.
if ! need_cmd useradd || ! need_cmd groupadd; then
  install_pkgs shadow
fi
getent group tako-app >/dev/null 2>&1 || groupadd --system tako-app
usermod -a -G tako-app "$TAKO_USER"
if ! id -u tako-images >/dev/null 2>&1; then
  useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin tako-images
fi

install_upgrade_helpers

socket_dir="$(dirname "$TAKO_SOCKET")"
mkdir -p "$TAKO_HOME" "$socket_dir"
chown "$TAKO_USER":tako-app "$TAKO_HOME"
chown "$TAKO_USER":"$TAKO_USER" "$socket_dir"
# App identities share traverse permission, but each app subtree has its own group.
chmod 0710 "$TAKO_HOME"
chmod 0700 "$socket_dir"
echo "OK prepared data and socket directories"

install_native_network
configure_management_http

maybe_prompt_ssh_pubkey

# Install authorized_keys for SSH and remote management (optional).
if [ -n "${TAKO_SSH_PUBKEY:-}" ]; then
  if ! need_cmd python3; then
    if need_cmd pacman; then install_pkgs python; else install_pkgs python3; fi
  fi
  home_dir="$(tako_home_dir)"
  TAKO_ENROLL_KEY="$TAKO_SSH_PUBKEY" python3 -I - "$TAKO_USER" "$TAKO_HOME" "$home_dir" <<'ENROLL_PY'
import contextlib
import fcntl
import os
from pathlib import Path
import pwd
import stat
import sys

user = pwd.getpwnam(sys.argv[1])
key = os.environ['TAKO_ENROLL_KEY']
if not key or '\n' in key or '\r' in key or len(key) > 16384:
    raise SystemExit('error: enrollment requires one public key line')


def check_directory(fd):
    info = os.fstat(fd)
    if info.st_uid not in (0, user.pw_uid) or info.st_mode & 0o022:
        raise ValueError('unsafe directory')


@contextlib.contextmanager
def anchor(path):
    if not Path(path).is_absolute() or '..' in Path(path).parts:
        raise ValueError('invalid enrollment directory')
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in Path(path).parts[1:]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
            check_directory(fd)
        yield fd
    finally:
        os.close(fd)


@contextlib.contextmanager
def service_identity():
    uid, gid = os.geteuid(), os.getegid()
    try:
        os.setegid(user.pw_gid)
        os.seteuid(user.pw_uid)
        yield
    finally:
        os.seteuid(uid)
        os.setegid(gid)


@contextlib.contextmanager
def enrollment_file(parent, name):
    flags = os.O_RDWR | os.O_APPEND | os.O_NOFOLLOW | os.O_NONBLOCK
    with service_identity():
        try:
            fd = os.open(name, flags | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=parent)
        except FileExistsError:
            fd = os.open(name, flags, dir_fd=parent)
    with os.fdopen(fd, 'r+b') as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != user.pw_uid or
                info.st_mode & 0o022 or info.st_size > 1024 * 1024):
            raise ValueError('unsafe enrollment file')
        fcntl.flock(stream, fcntl.LOCK_EX)
        content = stream.read(1024 * 1024 + 1)
        if len(content) > 1024 * 1024:
            raise ValueError('enrollment file too large')
        yield stream, content


try:
    with anchor(sys.argv[2]) as data, anchor(sys.argv[3]) as home:
        with service_identity():
            try:
                os.mkdir('.ssh', 0o700, dir_fd=home)
            except FileExistsError:
                pass
            ssh_fd = os.open('.ssh', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=home)
        try:
            check_directory(ssh_fd)
            if os.fstat(ssh_fd).st_uid != user.pw_uid:
                raise ValueError('unexpected SSH directory owner')
            with enrollment_file(data, 'management-authorized-keys') as management, enrollment_file(ssh_fd, 'authorized_keys') as ssh:
                for stream, content in (management, ssh):
                    os.fchmod(stream.fileno(), 0o600)
                    os.fchown(stream.fileno(), user.pw_uid, user.pw_gid)
                    encoded = key.encode()
                    if encoded not in content.splitlines():
                        if content and not content.endswith(b'\n'):
                            stream.write(b'\n')
                        stream.write(encoded + b'\n')
                    stream.flush()
                    os.fsync(stream.fileno())
                os.fchmod(ssh_fd, 0o700)
                os.fchown(ssh_fd, user.pw_uid, user.pw_gid)
        finally:
            os.close(ssh_fd)
except (OSError, ValueError):
    raise SystemExit('error: unsafe or unavailable SSH enrollment files; existing files must belong to the service user') from None
ENROLL_PY
  echo 'OK enrolled SSH and management key; existing keys retained'
else
  echo 'No new SSH key supplied; existing SSH and management keys retained'
fi

# ── Server config (config.json) ──────────────────────────────────────
# Stores server_name (metrics label).

TAKO_CONFIG="$TAKO_HOME/config.json"

# Read a field from config.json. Uses jq > python3 > sed fallback.
config_get() {
  local key="$1"
  if [ ! -f "$TAKO_CONFIG" ]; then return; fi
  if command -v jq >/dev/null 2>&1; then
    jq -r ".$key // empty" "$TAKO_CONFIG" 2>/dev/null
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c "
import json, sys, functools, operator
d = json.load(open(sys.argv[1]))
keys = sys.argv[2].split('.')
try: v = functools.reduce(operator.getitem, keys, d)
except (KeyError, TypeError): v = ''
print(v if isinstance(v, str) and v else '')
" "$TAKO_CONFIG" "$key" 2>/dev/null
  else
    # Flat keys only (e.g. server_name), not nested
    sed -n 's/.*"'"$key"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$TAKO_CONFIG" 2>/dev/null | head -1
  fi
}

# Write config.json from CONFIG_SERVER_NAME.
# Preserve non-installer keys such as trusted_proxy when updating installer-owned fields.
write_config() {
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$TAKO_CONFIG" "$CONFIG_SERVER_NAME" <<'PY'
import json
import os
import sys

path, server_name = sys.argv[1:3]
try:
    with open(path) as fh:
        data = json.load(fh)
    if not isinstance(data, dict):
        data = {}
except (FileNotFoundError, json.JSONDecodeError):
    data = {}

if server_name:
    data["server_name"] = server_name

tmp = f"{path}.tmp"
with open(tmp, "w") as fh:
    json.dump(data, fh)
    fh.write("\n")
os.replace(tmp, path)
PY
  elif command -v jq >/dev/null 2>&1; then
    local existing
    existing="$(cat "$TAKO_CONFIG" 2>/dev/null || echo '{}')"
    printf '%s\n' "$existing" | jq \
      --arg server_name "$CONFIG_SERVER_NAME" \
      'if $server_name != "" then .server_name = $server_name else . end' \
      > "$TAKO_CONFIG.tmp"
    mv "$TAKO_CONFIG.tmp" "$TAKO_CONFIG"
  else
    local json="{"
    if [ -n "$CONFIG_SERVER_NAME" ]; then
      json="${json}\"server_name\":\"$CONFIG_SERVER_NAME\""
    fi
    json="${json}}"
    printf '%s\n' "$json" > "$TAKO_CONFIG"
  fi
  chown "$TAKO_USER":"$TAKO_USER" "$TAKO_CONFIG" 2>/dev/null || true
  chmod 0644 "$TAKO_CONFIG"
}

CONFIG_SERVER_NAME=""
if [ -f "$TAKO_CONFIG" ]; then
  CONFIG_SERVER_NAME="$(config_get server_name)"
fi

maybe_prompt_server_name() {
  default_name="$(hostname -s 2>/dev/null || hostname 2>/dev/null || echo "")"

  # Env var takes precedence
  if [ -n "${TAKO_SERVER_NAME:-}" ]; then
    CONFIG_SERVER_NAME="$TAKO_SERVER_NAME"
    write_config
    echo "OK server name set to '$CONFIG_SERVER_NAME'"
    return
  fi

  # Preserve existing name on re-installs
  if [ -n "$CONFIG_SERVER_NAME" ]; then
    echo "OK server name already configured: $CONFIG_SERVER_NAME"
    return
  fi

  if ! is_enabled "$TAKO_RESTART_SERVICE"; then
    return
  fi

  # Interactive prompt
  if [ -t 0 ] 2>/dev/null; then
    printf 'Server name (used in metrics) [%s]: ' "$default_name"
    IFS= read -r TAKO_SERVER_NAME || TAKO_SERVER_NAME=""
    if [ -z "$TAKO_SERVER_NAME" ]; then
      TAKO_SERVER_NAME="$default_name"
    fi
  elif [ -e /dev/tty ]; then
    printf 'Server name (used in metrics) [%s]: ' "$default_name" > /dev/tty
    IFS= read -r TAKO_SERVER_NAME < /dev/tty || TAKO_SERVER_NAME=""
    if [ -z "$TAKO_SERVER_NAME" ]; then
      TAKO_SERVER_NAME="$default_name"
    fi
  else
    TAKO_SERVER_NAME="$default_name"
  fi

  if [ -n "$TAKO_SERVER_NAME" ]; then
    CONFIG_SERVER_NAME="$TAKO_SERVER_NAME"
    write_config
    echo "OK server name set to '$CONFIG_SERVER_NAME'"
  fi
}

maybe_prompt_server_name

configure_public_ports

install_systemd_service_unit() {
  mkdir -p /etc/systemd/system
  cat > /etc/systemd/system/tako-server.service <<EOF
[Unit]
Description=Tako Server
After=network.target
$TAKO_NETWORK_UNIT_ARGS

[Service]
Type=notify
NotifyAccess=all
User=$TAKO_USER
Group=$TAKO_USER
# sudo is restricted to the root-owned provisioning and maintenance helpers.
# DAC_READ_SEARCH stays bounding-only for PAM shadow reads by those helpers.
NoNewPrivileges=false
SupplementaryGroups=tako-app
AmbientCapabilities=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/usr/local/bin/tako-server --socket $TAKO_SOCKET --data-dir $TAKO_HOME $TAKO_PUBLIC_PORT_ARGS $TAKO_MANAGEMENT_ARGS
ExecReload=/bin/kill -HUP \$MAINPID
Restart=always
RestartSec=1
LimitNOFILE=1048576
KillMode=mixed
TimeoutStopSec=30min
RuntimeDirectory=tako
RuntimeDirectoryMode=0700

[Install]
WantedBy=multi-user.target
EOF
}

install_systemd_standby_unit() {
  cat > /etc/systemd/system/tako-server-standby.service <<EOF
[Unit]
Description=Tako Server Standby
After=network.target
$TAKO_NETWORK_UNIT_ARGS

[Service]
Type=notify
NotifyAccess=all
User=$TAKO_USER
Group=$TAKO_USER
NoNewPrivileges=false
SupplementaryGroups=tako-app
AmbientCapabilities=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/usr/local/bin/tako-server --standby --socket $TAKO_SOCKET --data-dir $TAKO_HOME $TAKO_PUBLIC_PORT_ARGS --instance-port-offset 1000
Restart=always
RestartSec=1
LimitNOFILE=1048576
KillMode=mixed
TimeoutStopSec=30min
RuntimeDirectory=tako
RuntimeDirectoryMode=0700

[Install]
WantedBy=multi-user.target
EOF
}

if [ "$SERVICE_MANAGER" = "systemd" ]; then
  install_systemd_service_unit
  install_systemd_standby_unit
fi

if [ "$SERVICE_MANAGER" = "systemd" ]; then
  systemctl daemon-reload
  if is_enabled "$TAKO_RESTART_SERVICE"; then
    systemctl enable tako-server
    if systemctl is-active --quiet tako-server; then
      # Restart also replaces children started under the previous security policy.
      echo "Restarting tako-server service"
      systemctl restart tako-server
      echo "OK tako-server restarted with updated service settings"
    else
      echo "Starting tako-server service"
      systemctl start tako-server
    fi
    systemctl --no-pager status tako-server || true
    if ! systemctl is-active --quiet tako-server; then
      echo "error: tako-server failed to start; see service status above" >&2
      exit 1
    fi
  else
    if systemctl is-active --quiet tako-server; then
      # Upgrades refresh a running service without restarting it. Repair boot startup too.
      systemctl enable tako-server
      echo "OK refreshed tako-server service without restarting"
    else
      echo "OK installed tako-server service (not started)"
      echo 'Run `tako servers add <host>` from your workstation to start and verify it.'
    fi
  fi
else
  # Install-refresh mode can run before init is active (for example in image builds).
  # In this mode we install binaries/users only and skip service definition install.
  echo "OK install refreshed without active service manager (TAKO_RESTART_SERVICE=0); skipped service definition install"
fi

echo "OK installed tako-server"
echo "OK configured user: $TAKO_USER"
