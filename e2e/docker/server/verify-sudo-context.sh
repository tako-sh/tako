#!/bin/sh
# The fixture image contains a dummy proxy. Replace it only in this disposable
# container to exercise PAM under the exact E2E launcher capability settings.
set -eu
test "$(id -u)" = 0
test -f /.dockerenv

cat > /usr/local/bin/tako-server <<'SH'
#!/bin/sh
set -eu
# The proxy must not receive either DAC capability; only its sudo helpers may.
effective=$(sed -n 's/^CapEff:[[:space:]]*//p' /proc/self/status)
test "$((0x$effective & 6))" -eq 0
exec env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin sudo -n -l
SH
chmod 0755 /usr/local/bin/tako-server
: > /run/tako-e2e-env
exec sh /opt/e2e/start-server.sh
