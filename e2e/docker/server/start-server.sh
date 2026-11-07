#!/bin/sh
# Fixed launcher for the disposable host, which has no systemd. Keep the shared
# binary capability-free and reproduce the production proxy unit's privileges.
set -eu
test "$#" -eq 0
test "$(id -u)" = 0
test -f /.dockerenv

exec setpriv \
  --bounding-set=-all,+net_bind_service,+setuid,+setgid,+kill,+chown,+fowner,+dac_override,+dac_read_search \
  --inh-caps=-all,+net_bind_service,+setuid,+setgid,+kill \
  --ambient-caps=-all,+net_bind_service,+setuid,+setgid,+kill \
  --reuid=tako --regid=tako --init-groups \
  sh -ec '. /run/tako-e2e-env
    export RUST_LOG=info
    exec /usr/local/bin/tako-server --no-acme --http-port 8080 --https-port 8443 --data-dir /opt/tako --management-host 0.0.0.0'
