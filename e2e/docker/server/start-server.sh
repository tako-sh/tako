#!/bin/sh
# Fixed restart for the disposable systemd host. No runner-supplied arguments.
set -eu
test "$#" -eq 0
test "$(id -u)" = 0
test -f /.dockerenv
exec systemctl restart tako-server.service
