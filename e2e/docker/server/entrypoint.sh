#!/bin/sh
set -eu

# Compose gives each server its own cgroup namespace. Never mount the host's
# cgroup tree: this only delegates the disposable container's subtree.
test "$(cat /proc/1/cgroup)" = "0::/"
mkdir -p /sys/fs/cgroup/control
printf '%s\n' "$$" > /sys/fs/cgroup/control/cgroup.procs
printf '%s\n' '+cpu +memory +pids' > /sys/fs/cgroup/cgroup.subtree_control

if ! getent group tako >/dev/null 2>&1; then
  groupadd --system tako
fi

if ! id -u tako >/dev/null 2>&1; then
  useradd --system --create-home --home-dir /home/tako --shell /bin/bash --gid tako tako
fi

if command -v chpasswd >/dev/null 2>&1; then
  echo "tako:tako-e2e" | chpasswd
fi

mkdir -p /home/tako/.ssh /var/run/tako /opt/tako /run/sshd
chmod 700 /home/tako/.ssh
cp /opt/e2e/keys/id_ed25519.pub /home/tako/.ssh/authorized_keys
cp /opt/e2e/keys/id_ed25519.pub /opt/tako/management-authorized-keys
chmod 600 /home/tako/.ssh/authorized_keys
chmod 600 /opt/tako/management-authorized-keys
chown -R tako:tako /home/tako/.ssh /var/run/tako /opt/tako || true

# Install the mounted build while still root, then exercise only the production
# sudo policy through SSH. The runner has no unrestricted root shell.
libc_kind=glibc
if [ -x "/opt/e2e/bin/$libc_kind/tako-server" ]; then
  archive_dir="$(mktemp -d)"
  archive="$archive_dir/tako-server.tar.zst"
  tar -cf - -C "/opt/e2e/bin/$libc_kind" tako-server | zstd -f -o "$archive"
  sha256sum "$archive" | awk '{print $1}' > "$archive.sha256"
  TAKO_SERVER_URL="file://$archive" TAKO_RESTART_SERVICE=0 TAKO_SERVER_NAME=e2e sh /opt/e2e/install-server.sh
  rm -f "$archive" "$archive.sha256"
  rmdir "$archive_dir"
fi

cat > /etc/ssh/sshd_config <<'CFG'
Port 22
Protocol 2
HostKey /etc/ssh/ssh_host_ed25519_key
HostKey /etc/ssh/ssh_host_rsa_key
PermitRootLogin no
PasswordAuthentication no
ChallengeResponseAuthentication no
PubkeyAuthentication yes
AuthorizedKeysFile .ssh/authorized_keys
AllowUsers tako
Subsystem sftp internal-sftp
PidFile /run/sshd.pid
CFG

exec /usr/sbin/sshd -D -e -f /etc/ssh/sshd_config
