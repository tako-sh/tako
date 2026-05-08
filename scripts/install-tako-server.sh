#!/bin/sh
set -eu

# Tako installer (POSIX sh)
#
# Usage:
#   sudo sh -c "$(curl -fsSL https://tako.sh/install-server.sh)"
#
# What it does:
# - downloads and installs `tako-server`
# - creates OS user `tako`
# - configures a service manager (systemd or OpenRC) for `tako-server`
# - installs maintenance helpers and sudoers for the tako service user
#
# Optional env vars:
#   TAKO_USER               default: tako
#   TAKO_HOME               default: /opt/tako
#   TAKO_SOCKET             default: /var/run/tako/tako.sock
#   TAKO_MANAGEMENT_HOST    Tailscale IP to bind remote management on (optional)
#                           if unset, installer detects it with `tailscale ip -4`
#   TAKO_SSH_PUBKEY         public key line to authorize for TAKO_USER (optional)
#                           if unset, installer prompts in interactive terminals
#
#   TAKO_SERVER_URL         override archive URL (.tar.zst or .tar.gz; optional)
#   TAKO_DOWNLOAD_BASE_URL  override release download base URL (optional)
#   TAKO_ALLOW_INSECURE_DOWNLOAD_BASE
#                           default: unset
#                           set 1/true/yes/on to allow non-HTTPS download overrides for local testing
#   TAKO_REPO_OWNER         default: lilienblum
#   TAKO_REPO_NAME          default: tako
#   TAKO_RELEASE_TAG        default: latest
#   GH_TOKEN/GITHUB_TOKEN   optional GitHub token for release downloads
#   TAKO_SERVER_NAME        server identity for metrics labels (optional)
#                           if unset, installer prompts in interactive terminals
#                           defaults to machine hostname if non-interactive
#   TAKO_RESTART_SERVICE    default: 1 (set 0/false for install-only refresh; no service restart)

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
TAKO_MANAGEMENT_HOST="${TAKO_MANAGEMENT_HOST:-}"
TAKO_DOWNLOAD_BASE_URL="${TAKO_DOWNLOAD_BASE_URL:-}"
TAKO_ALLOW_INSECURE_DOWNLOAD_BASE="${TAKO_ALLOW_INSECURE_DOWNLOAD_BASE:-}"
TAKO_REPO_OWNER="${TAKO_REPO_OWNER:-lilienblum}"
TAKO_REPO_NAME="${TAKO_REPO_NAME:-tako}"
TAKO_RELEASE_TAG="${TAKO_RELEASE_TAG:-latest}"
TAKO_RESTART_SERVICE="${TAKO_RESTART_SERVICE:-1}"
TAKO_MANAGEMENT_REQUIRED_MESSAGE="Remote management requires Tailscale so Tako can keep server control traffic private by default."
TAKO_MANAGEMENT_ARGS=""
TAKO_SERVER_CAPABILITIES="cap_net_bind_service,cap_setuid,cap_setgid,cap_kill"
TAKO_SERVER_INSTALL_REFRESH_HELPER="/usr/local/bin/tako-server-install-refresh"
TAKO_SERVER_SERVICE_HELPER="/usr/local/bin/tako-server-service"
TAKO_MANAGEMENT_AUTH_KEYS="$TAKO_HOME/management-authorized-keys"

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

is_tailscale_ipv4() {
  printf '%s\n' "$1" | awk -F. '
    NF == 4 {
      for (i = 1; i <= 4; i++) {
        if ($i !~ /^[0-9]+$/ || $i < 0 || $i > 255) {
          exit 1
        }
      }
      if ($1 == 100 && $2 >= 64 && $2 <= 127) {
        exit 0
      }
    }
    { exit 1 }
  '
}

is_tailscale_ipv6() {
  lower="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  case "$lower" in
    fd7a:115c:a1e0:*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_tailscale_ip() {
  is_tailscale_ipv4 "$1" || is_tailscale_ipv6 "$1"
}

detect_tailscale_ip() {
  if ! need_cmd tailscale; then
    return 1
  fi

  for ip in $(tailscale ip -4 2>/dev/null || true); do
    if is_tailscale_ip "$ip"; then
      printf '%s\n' "$ip"
      return 0
    fi
  done

  return 1
}

configure_management_http() {
  if [ -z "$TAKO_MANAGEMENT_HOST" ]; then
    TAKO_MANAGEMENT_HOST="$(detect_tailscale_ip || true)"
  fi

  if [ -n "$TAKO_MANAGEMENT_HOST" ]; then
    if ! is_tailscale_ip "$TAKO_MANAGEMENT_HOST"; then
      echo "error: TAKO_MANAGEMENT_HOST must be this server's Tailscale IP (100.64.0.0/10 or fd7a:115c:a1e0::/48)." >&2
      exit 1
    fi
    TAKO_MANAGEMENT_ARGS="--management-host $TAKO_MANAGEMENT_HOST"
    echo "OK remote management bound to Tailscale address $TAKO_MANAGEMENT_HOST"
    return
  fi

  if is_enabled "$TAKO_RESTART_SERVICE"; then
    echo "error: $TAKO_MANAGEMENT_REQUIRED_MESSAGE" >&2
    echo "Install and connect Tailscale on this server, then rerun the installer." >&2
    echo "Or set TAKO_MANAGEMENT_HOST to this server's Tailscale IP." >&2
    exit 1
  fi

  if [ "$SERVICE_MANAGER" != "none" ]; then
    echo "warning: remote management was not configured; connect Tailscale before starting tako-server." >&2
  fi
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

systemd_main_pid() {
  systemctl show -p MainPID --value tako-server 2>/dev/null || true
}

openrc_main_pid() {
  if [ -r /run/tako-server.pid ]; then
    sed -n '1p' /run/tako-server.pid 2>/dev/null || true
  fi
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

openrc_is_usable() {
  if ! need_cmd rc-service; then
    return 1
  fi

  if ! need_cmd rc-update; then
    return 1
  fi

  # OpenRC creates this runtime directory when it is the active init system.
  if [ ! -d /run/openrc ]; then
    return 1
  fi

  return 0
}

detect_service_manager() {
  if systemd_is_usable; then
    echo "systemd"
    return
  fi

  if openrc_is_usable; then
    echo "openrc"
    return
  fi

  echo "none"
}

SERVICE_MANAGER="$(detect_service_manager)"

install_upgrade_helpers() {
  cat > "$TAKO_SERVER_INSTALL_REFRESH_HELPER" <<'EOF'
#!/bin/sh
set -eu

installer_url="https://tako.sh/install-server.sh"
installer="$(mktemp)"
trap 'rm -f "$installer"' EXIT

if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$installer_url" -o "$installer"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$installer" "$installer_url"
else
  echo "error: missing required downloader (curl or wget)" >&2
  exit 1
fi

TAKO_RESTART_SERVICE=0 sh "$installer"
EOF
  chmod 0755 "$TAKO_SERVER_INSTALL_REFRESH_HELPER"

  cat > "$TAKO_SERVER_SERVICE_HELPER" <<'EOF'
#!/bin/sh
set -eu

action="${1:-}"
case "$action" in
  reload|restart)
    ;;
  *)
    echo "error: expected action 'reload' or 'restart'" >&2
    exit 1
    ;;
esac

if command -v systemctl >/dev/null 2>&1; then
  systemctl "$action" tako-server
elif command -v rc-service >/dev/null 2>&1; then
  rc-service tako-server "$action"
else
  echo "error: no supported service manager found (systemctl or rc-service)" >&2
  exit 1
fi
EOF
  chmod 0755 "$TAKO_SERVER_SERVICE_HELPER"

  cat > /etc/sudoers.d/tako <<EOF
# Managed by Tako install-server.
# The tako user is a no-login service account (only accessible via SSH key).
# It needs root for upgrades (binary install + service reload) and server
# administration tasks (DNS setup, systemd drop-ins). Commands are invoked
# via sudo sh -c '...' so the rule must not be restricted to specific binaries.
$TAKO_USER ALL=(root) NOPASSWD: ALL
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
  if ! need_cmd setcap && [ "$SERVICE_MANAGER" != "systemd" ]; then
    install_setcap_tool
  fi

  if ! need_cmd setcap; then
    echo "warning: setcap not found; systemd service still sets required capabilities via AmbientCapabilities." >&2
    return
  fi

  if [ "$SERVICE_MANAGER" = "systemd" ]; then
    if setcap "$TAKO_SERVER_CAPABILITIES=+ep" /usr/local/bin/tako-server; then
      echo "OK granted required capabilities to /usr/local/bin/tako-server"
      return
    fi
    echo "warning: failed to grant capabilities via setcap; systemd service still uses AmbientCapabilities." >&2
    return
  fi

  if setcap "$TAKO_SERVER_CAPABILITIES=+ep" /usr/local/bin/tako-server; then
    echo "OK granted required capabilities to /usr/local/bin/tako-server"
    return
  fi

  echo "error: failed to grant required capabilities to /usr/local/bin/tako-server" >&2
  echo "Install setcap/libcap support, then rerun the installer." >&2
  exit 1
}

if is_enabled "$TAKO_RESTART_SERVICE" && [ "$SERVICE_MANAGER" = "none" ]; then
  echo "error: a usable service manager is required for tako-server (systemd or OpenRC)" >&2
  exit 1
fi

configure_management_http

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
  elif need_cmd apk; then
    apk add --no-cache "$@"
  elif need_cmd zypper; then
    zypper --non-interactive install "$@"
  else
    echo "error: unsupported package manager; install curl + ca-certificates + tar manually" >&2
    exit 1
  fi
}

install_setcap_tool() {
  if need_cmd apt-get; then
    install_pkgs libcap2-bin
  elif need_cmd dnf; then
    install_pkgs libcap
  elif need_cmd yum; then
    install_pkgs libcap
  elif need_cmd pacman; then
    install_pkgs libcap
  elif need_cmd apk; then
    install_pkgs libcap
  elif need_cmd zypper; then
    zypper --non-interactive install libcap-progs || zypper --non-interactive install libcap2
  else
    echo "error: unsupported package manager; install the setcap/libcap tools manually" >&2
    exit 1
  fi

  if ! need_cmd setcap; then
    echo "error: setcap not found after installing capability tools" >&2
    exit 1
  fi
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
  elif need_cmd apk; then
    apk add --no-cache sqlite-libs
  elif need_cmd zypper; then
    zypper --non-interactive install sqlite3
  else
    echo "warning: unsupported package manager; install libsqlite3 runtime manually if needed." >&2
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
  elif need_cmd apk; then
    apk add --no-cache netcat-openbsd
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

arch="$(uname -m)"
case "$arch" in
  x86_64|amd64) arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)
    echo "error: unsupported architecture: $arch (supported: x86_64, aarch64)" >&2
    exit 1
    ;;
esac

libc="$(detect_libc)"
case "$libc" in
  glibc|musl) ;;
  *)
    echo "error: unsupported libc: $libc (supported: glibc, musl)" >&2
    exit 1
    ;;
esac

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

# Create `tako-app` user for app and worker processes.
if ! id -u "tako-app" >/dev/null 2>&1; then
  if need_cmd useradd; then
    useradd --system --no-create-home --shell /usr/sbin/nologin --gid "$TAKO_USER" "tako-app" 2>/dev/null || \
      useradd --system --no-create-home --shell /sbin/nologin --gid "$TAKO_USER" "tako-app"
  elif need_cmd adduser; then
    adduser -S -D -H -s /sbin/nologin -G "$TAKO_USER" "tako-app"
  fi
fi

install_upgrade_helpers

mkdir -p "$TAKO_HOME" "$(dirname "$TAKO_SOCKET")"
chown -R "$TAKO_USER":"$TAKO_USER" "$TAKO_HOME" "$(dirname "$TAKO_SOCKET")" 2>/dev/null || true
# 0o710: owner (tako) full; group (tako, contains tako-app) traverse-only so
# sandboxed app processes can descend into runtimes/ and releases/ to exec
# binaries; world none. Must not be 0o700 — that returns ENOENT on execve.
chmod 0710 "$TAKO_HOME"
chmod 0700 "$(dirname "$TAKO_SOCKET")"

maybe_prompt_ssh_pubkey

# Install authorized_keys for SSH (optional).
home_dir="$(tako_home_dir)"
auth_keys="$home_dir/.ssh/authorized_keys"

if [ -n "${TAKO_SSH_PUBKEY:-}" ]; then
  mkdir -p "$home_dir/.ssh"
  chmod 700 "$home_dir/.ssh"

  # Check if key already exists in authorized_keys to avoid duplicates
  if [ -f "$auth_keys" ] && grep -qF "$TAKO_SSH_PUBKEY" "$auth_keys" 2>/dev/null; then
    echo "OK SSH key already present in authorized_keys"
  elif [ -f "$auth_keys" ] && [ -s "$auth_keys" ]; then
    # File exists and is non-empty — append instead of overwriting
    printf '%s\n' "$TAKO_SSH_PUBKEY" >> "$auth_keys"
    echo "OK appended SSH key to existing authorized_keys"
  else
    printf '%s\n' "$TAKO_SSH_PUBKEY" > "$auth_keys"
    echo "OK wrote SSH key to authorized_keys"
  fi

  chmod 600 "$auth_keys"
  chown -R "$TAKO_USER":"$TAKO_USER" "$home_dir/.ssh" 2>/dev/null || true

  printf '%s\n' "$TAKO_SSH_PUBKEY" > "$TAKO_MANAGEMENT_AUTH_KEYS"
  chmod 600 "$TAKO_MANAGEMENT_AUTH_KEYS"
  chown "$TAKO_USER":"$TAKO_USER" "$TAKO_MANAGEMENT_AUTH_KEYS" 2>/dev/null || true
  echo "OK enrolled SSH key for remote management"
elif [ -f "$auth_keys" ] && [ -s "$auth_keys" ]; then
  echo "OK existing SSH key retained for '$TAKO_USER'"
else
  echo "warning: no SSH key installed for '$TAKO_USER'." >&2
  echo "warning: configure ~/.ssh/authorized_keys manually or rerun installer with TAKO_SSH_PUBKEY." >&2
fi

# ── Server config (config.json) ──────────────────────────────────────
# Stores server_name (metrics label) and dns.provider in a single file.

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

# Write config.json from CONFIG_SERVER_NAME and CONFIG_DNS_PROVIDER variables.
write_config() {
  local json="{"
  local need_comma=false
  if [ -n "$CONFIG_SERVER_NAME" ]; then
    json="${json}\"server_name\":\"$CONFIG_SERVER_NAME\""
    need_comma=true
  fi
  if [ -n "$CONFIG_DNS_PROVIDER" ]; then
    $need_comma && json="${json},"
    json="${json}\"dns\":{\"provider\":\"$CONFIG_DNS_PROVIDER\"}"
  fi
  json="${json}}"
  printf '%s\n' "$json" > "$TAKO_CONFIG"
  chown "$TAKO_USER":"$TAKO_USER" "$TAKO_CONFIG" 2>/dev/null || true
  chmod 0644 "$TAKO_CONFIG"
}

CONFIG_SERVER_NAME=""
CONFIG_DNS_PROVIDER=""
if [ -f "$TAKO_CONFIG" ]; then
  CONFIG_SERVER_NAME="$(config_get server_name)"
  CONFIG_DNS_PROVIDER="$(config_get dns.provider)"
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

  # Interactive prompt
  if [ -t 0 ] 2>/dev/null; then
    printf 'Server name (used in metrics) [%s]: ' "$default_name"
    IFS= read -r TAKO_SERVER_NAME
    if [ -z "$TAKO_SERVER_NAME" ]; then
      TAKO_SERVER_NAME="$default_name"
    fi
  elif [ -e /dev/tty ]; then
    printf 'Server name (used in metrics) [%s]: ' "$default_name" > /dev/tty
    IFS= read -r TAKO_SERVER_NAME < /dev/tty
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

install_systemd_service_unit() {
  mkdir -p /etc/systemd/system
  cat > /etc/systemd/system/tako-server.service <<EOF
[Unit]
Description=Tako Server
After=network.target

[Service]
Type=notify
NotifyAccess=all
User=$TAKO_USER
Group=$TAKO_USER
NoNewPrivileges=true
AmbientCapabilities=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/usr/local/bin/tako-server --socket $TAKO_SOCKET --data-dir $TAKO_HOME $TAKO_MANAGEMENT_ARGS
ExecReload=/bin/kill -HUP \$MAINPID
Restart=always
RestartSec=1
KillMode=mixed
TimeoutStopSec=30min
RuntimeDirectory=tako
RuntimeDirectoryMode=0700

[Install]
WantedBy=multi-user.target
EOF
}

install_openrc_service_script() {
  cat > /etc/init.d/tako-server <<EOF
#!/sbin/openrc-run
description="Tako Server"

command="/usr/local/bin/tako-server"
command_args="--socket $TAKO_SOCKET --data-dir $TAKO_HOME $TAKO_MANAGEMENT_ARGS"
command_user="$TAKO_USER:$TAKO_USER"
pidfile="/run/\${RC_SVCNAME}.pid"
command_background="yes"
retry="TERM/1800/KILL/5"

depend() {
  need net
}

extra_started_commands="reload"

reload() {
  ebegin "Reloading \${RC_SVCNAME}"
  if [ ! -f "\$pidfile" ]; then
    eend 1
    return 1
  fi
  start-stop-daemon --signal HUP --pidfile "\$pidfile"
  eend \$?
}
EOF
  chmod 0755 /etc/init.d/tako-server
}

install_systemd_standby_unit() {
  cat > /etc/systemd/system/tako-server-standby.service <<EOF
[Unit]
Description=Tako Server Standby
After=network.target

[Service]
Type=notify
NotifyAccess=all
User=$TAKO_USER
Group=$TAKO_USER
NoNewPrivileges=true
AmbientCapabilities=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_SETUID CAP_SETGID CAP_KILL
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/usr/local/bin/tako-server --standby --socket $TAKO_SOCKET --data-dir $TAKO_HOME --instance-port-offset 1000
Restart=always
RestartSec=1
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
elif [ "$SERVICE_MANAGER" = "openrc" ]; then
  install_openrc_service_script
fi

if [ "$SERVICE_MANAGER" = "systemd" ]; then
  systemctl daemon-reload
  if is_enabled "$TAKO_RESTART_SERVICE"; then
    systemctl enable tako-server >/dev/null 2>&1 || true
    if systemctl is-active --quiet tako-server; then
      main_pid="$(systemd_main_pid)"
      if [ -n "$TAKO_MANAGEMENT_ARGS" ] && ! process_has_management_host "$main_pid" "$TAKO_MANAGEMENT_HOST"; then
        systemctl restart tako-server
        echo "OK tako-server restarted with remote management"
      else
        # Service already running — graceful reload (SIGHUP) to pick up new binary.
        systemctl reload tako-server
        echo "OK tako-server reloaded (SIGHUP)"
      fi
    else
      systemctl start tako-server
    fi
    systemctl --no-pager status tako-server || true
    if ! systemctl is-active --quiet tako-server; then
      echo "error: tako-server failed to start; see service status above" >&2
      exit 1
    fi
  else
    systemctl enable tako-server >/dev/null 2>&1 || true
    echo "OK install refreshed without restarting tako-server (TAKO_RESTART_SERVICE=0)"
  fi
elif [ "$SERVICE_MANAGER" = "openrc" ]; then
  rc-update add tako-server default >/dev/null 2>&1 || true
  if is_enabled "$TAKO_RESTART_SERVICE"; then
    if rc-service tako-server status >/dev/null 2>&1; then
      main_pid="$(openrc_main_pid)"
      if [ -n "$TAKO_MANAGEMENT_ARGS" ] && ! process_has_management_host "$main_pid" "$TAKO_MANAGEMENT_HOST"; then
        rc-service tako-server restart
        echo "OK tako-server restarted with remote management"
      else
        rc-service tako-server reload || rc-service tako-server restart
      fi
    else
      rc-service tako-server start
    fi
    rc-service tako-server status || true
    if ! rc-service tako-server status >/dev/null 2>&1; then
      echo "error: tako-server failed to start via OpenRC." >&2
      exit 1
    fi
  else
    echo "OK install refreshed without restarting tako-server (TAKO_RESTART_SERVICE=0)"
  fi
else
  # Install-refresh mode can run before init is active (for example in image builds).
  # In this mode we install binaries/users only and skip service definition install.
  echo "OK install refreshed without active service manager (TAKO_RESTART_SERVICE=0); skipped service definition install"
fi

# Ensure DNS credentials systemd drop-in is in place (idempotent)
TAKO_DNS_CREDENTIALS_ENV="$TAKO_HOME/dns-credentials.env"

if [ -n "$CONFIG_DNS_PROVIDER" ]; then
  echo "OK DNS provider already configured: $CONFIG_DNS_PROVIDER"
  if [ "$SERVICE_MANAGER" = "systemd" ] && [ -f "$TAKO_DNS_CREDENTIALS_ENV" ]; then
    dropin_dir="/etc/systemd/system/tako-server.service.d"
    if [ ! -f "$dropin_dir/dns.conf" ]; then
      mkdir -p "$dropin_dir"
      cat > "$dropin_dir/dns.conf" <<DNSEOF
[Service]
EnvironmentFile=$TAKO_DNS_CREDENTIALS_ENV
DNSEOF
      systemctl daemon-reload
      echo "OK restored DNS systemd drop-in"
    fi
  fi
fi

echo "OK installed tako-server"
echo "OK configured user: $TAKO_USER"
