#!/bin/sh
# The fixture image contains a dummy proxy. Replace it only in this disposable
# container to exercise PAM with the production proxy unit's capabilities,
# without booting systemd or enrolling a network for this focused check.
set -eu
test "$(id -u)" = 0
test -f /.dockerenv

cat > /usr/local/bin/tako-server <<'SH'
#!/bin/sh
set -eu
# Match a systemd User=tako service instead of retaining sudo's root environment.
test "$USER" = tako
test "$LOGNAME" = tako
test "$HOME" = "$(getent passwd tako | cut -d: -f6)"
# The proxy must not receive either DAC capability; only its sudo helpers may.
effective=$(sed -n 's/^CapEff:[[:space:]]*//p' /proc/self/status)
test "$((0x$effective & 6))" -eq 0
docker compose --ansi never version
exec env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin sudo -n -l
SH
chmod 0755 /usr/local/bin/tako-server
exec setpriv \
  --bounding-set=-all,+net_bind_service,+setuid,+setgid,+kill,+chown,+fowner,+dac_override,+dac_read_search \
  --inh-caps=-all,+net_bind_service,+setuid,+setgid,+kill \
  --ambient-caps=-all,+net_bind_service,+setuid,+setgid,+kill \
  --reuid=tako --regid=tako --init-groups --reset-env \
  /usr/local/bin/tako-server
