
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
