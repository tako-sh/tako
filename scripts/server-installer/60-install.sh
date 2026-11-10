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

