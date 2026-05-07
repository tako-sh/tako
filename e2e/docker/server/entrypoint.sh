#!/bin/sh
set -eu

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
chmod 600 /home/tako/.ssh/authorized_keys
chown -R tako:tako /home/tako/.ssh /var/run/tako /opt/tako || true

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
