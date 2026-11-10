

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
