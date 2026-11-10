
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
    native_expected="a31b17d972037f245912cf13d664907651d51797545ebc000d6cde9cca77305e"
    native_source="$tmp_extract/scripts/tako-network-helper.py"
    if [ ! -f "$native_source" ] || [ -L "$native_source" ]; then
      native_source="$tmp_extract/native-helper.py"
      download_file "https://raw.githubusercontent.com/$TAKO_REPO_OWNER/$TAKO_REPO_NAME/$TAKO_RELEASE_TAG/scripts/tako-network-helper.py" "$native_source"
    fi
  fi
  native_actual="$(sha256sum "$native_source" | awk '{print $1}')"
  [ "$native_actual" = "$native_expected" ] || { echo 'error: native helper checksum mismatch' >&2; exit 1; }

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
ExecStart=/usr/local/libexec/tako/tako-network-helper %i isolate
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
ConditionFileIsExecutable=/usr/local/libexec/tako/corrosion
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
  # The running agent owns peer grants. With the agent stopped, restore only the link.
  if [ "$SERVICE_MANAGER" = systemd ] && ! systemctl is-active --quiet tako-network-agent.service; then
    for desired_file in /opt/tako/networks/*/desired.json; do
      [ -e "$desired_file" ] || continue
      native_id="${desired_file%/desired.json}"
      native_id="${native_id##*/}"
      /usr/local/libexec/tako/tako-network-helper "$native_id" isolate
    done
  fi
  if [ "$SERVICE_MANAGER" = systemd ] && is_enabled "$TAKO_RESTART_SERVICE" &&
     [ -f /opt/tako/native-network.json ]; then
    systemctl restart tako-network-agent.service
  fi
  echo 'OK native WireGuard installed. Allow its UDP listen port in your cloud security group if required.'
  echo 'OK native network agent installed; activation requires native-network.json and service-start mode.'
}
