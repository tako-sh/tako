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

