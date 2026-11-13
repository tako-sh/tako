#!/bin/sh
set -eu

# Only the disposable GitHub runner needs this host-policy correction.
if [ "${GITHUB_ACTIONS:-}" != true ]; then
  echo "This script is only for disposable GitHub Actions runners." >&2
  exit 1
fi

# Native enrollment needs the host kernel's WireGuard module. Load it on the
# disposable runner before starting services with restricted capabilities.
sudo modprobe wireguard
sudo unshare --net sh -ec '
  ip link add dev tako-e2e-probe type wireguard
  ip -6 address add fd12:7461:6b6f::1/128 dev tako-e2e-probe nodad noprefixroute
'

# Ubuntu's host profile also attaches to AlmaLinux's PAM helper in privileged
# containers. AlmaLinux's mode-000 shadow file requires DAC read permission.
# Keep PAM, shadow permissions, and all other AppArmor rules intact.
if [ -f /etc/apparmor.d/unix-chkpwd ]; then
  printf '%s\n' 'capability dac_read_search,' | sudo tee -a /etc/apparmor.d/local/unix-chkpwd >/dev/null
  sudo apparmor_parser -r /etc/apparmor.d/unix-chkpwd
fi
