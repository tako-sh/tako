

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
