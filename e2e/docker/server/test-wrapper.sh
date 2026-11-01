#!/bin/sh
# Only for installed-server integration tests inside a disposable Linux host.
set -eu
test "${TAKO_E2E_DISPOSABLE_HOST:-}" = 1
test "$(id -u)" = 0
test -f /.dockerenv

binary=$1
shift
data_dir=""
previous=""
for argument in "$@"; do
  if [ "$previous" = --data-dir ]; then data_dir=$argument; fi
  previous=$argument
done
test -n "$data_dir"
test -d "$data_dir"
test -f /etc/tako/isolation.conf

install -o root -g root -m 0755 "$binary" /usr/local/bin/tako-server
setcap cap_net_bind_service,cap_setuid,cap_setgid,cap_kill=ep /usr/local/bin/tako-server
printf '%s\ntako\n' "$data_dir" > /etc/tako/isolation.conf
chmod 0644 /etc/tako/isolation.conf
chown tako:tako-app "$data_dir"
chmod 0710 "$data_dir"
exec setpriv --reuid="$(id -u tako)" --regid="$(id -g tako)" --init-groups /usr/local/bin/tako-server "$@"
