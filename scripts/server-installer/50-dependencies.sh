

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

# TRANSITIONAL (with install_legacy_libvips_runtime below): RHEL-family hosts
# (AlmaLinux/Rocky/RHEL/CentOS Stream) carry no vips package in any default or
# EPEL repository, so older binaries get libvips from Remi's RPM repository.
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


# TRANSITIONAL — remove once the `latest` release ships a tako-server built
# with the static Rust image pipeline (tako-sh/tako#223).
# /install-server.sh serves this installer from master, but the binary comes
# from the `latest` release. Between merging that change and the next release,
# a fresh install pairs this installer with an older binary that still links
# libvips, so install the distro runtime only when the binary asks for it.
install_legacy_libvips_runtime() {
  if ! missing_runtime_libraries /usr/local/bin/tako-server | grep -Eq '^libvips(-cpp)?\.so'; then
    return 0
  fi

  echo "Installing libvips runtime for an older tako-server release"
  if need_cmd apt-get; then
    apt-get update -y
    for apt_vips_pkg in libvips42t64 libvips42; do
      if apt-get install -y "$apt_vips_pkg"; then
        return 0
      fi
    done
  elif need_cmd dnf; then
    dnf install -y vips && return 0
    install_libvips_from_remi && return 0
  elif need_cmd yum; then
    yum install -y vips && return 0
  elif need_cmd pacman; then
    pacman -Sy --noconfirm libvips && return 0
  elif need_cmd zypper; then
    zypper --non-interactive install libvips42 && return 0
  fi
  echo "warning: could not install libvips for this tako-server release; rerun the installer after the next release" >&2
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
