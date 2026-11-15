#!/bin/sh
# Run only in a disposable privileged container with a private cgroup namespace.
set -eu
test "$(cat /proc/1/cgroup)" = "0::/"
mkdir -p /sys/fs/cgroup/control
printf '%s\n' "$$" > /sys/fs/cgroup/control/cgroup.procs
printf '%s\n' '+cpu +memory +pids' > /sys/fs/cgroup/cgroup.subtree_control

install -m 0755 /workspace/target/e2e-linux-glibc/release/tako-server /tmp/tako-server
tar -cf - -C /tmp tako-server | zstd -f -o /tmp/tako-isolation.tar.zst
sha256sum /tmp/tako-isolation.tar.zst | awk '{print $1}' > /tmp/tako-isolation.tar.zst.sha256
TAKO_SERVER_URL=file:///tmp/tako-isolation.tar.zst TAKO_RESTART_SERVICE=0 TAKO_SERVER_NAME=isolation-test sh /workspace/scripts/install-tako-server.sh

for app in first second; do
  mkdir -p "/opt/tako/apps/$app/production/releases/v1" "/opt/tako/apps/$app/production/data/app" "/opt/tako/apps/$app/production/data/tako" "/opt/tako/apps/$app/production/shared/logs"
  printf '%s\n' '{"protocol_version":0}' > "/opt/tako/apps/$app/production/releases/v1/app.json"
  mkdir -p "/opt/tako/apps/$app/production/data/app/legacy"
  printf 'old-data' > "/opt/tako/apps/$app/production/data/app/legacy/database"
  chmod 0700 "/opt/tako/apps/$app/production/data/app/legacy"
  chmod 0600 "/opt/tako/apps/$app/production/data/app/legacy/database"
  chown -R tako:tako "/opt/tako/apps/$app"
  # Reproduce the service bounding set without granting its extra caps ambiently.
  setpriv --bounding-set=-all,+net_bind_service,+setuid,+setgid,+kill,+chown,+fowner,+dac_override,+dac_read_search --reuid=tako --regid=tako --init-groups \
    sudo -n /usr/local/bin/tako-provision-app "$app/production" v1
done

first_group="$(stat -c %G /opt/tako/apps/first/production)"
second_group="$(stat -c %G /opt/tako/apps/second/production)"
test "$first_group" != "$second_group"
test "$(id -u "$first_group")" != "$(id -u tako)"
test "$(cat "/sys/fs/cgroup/tako-apps/$first_group/memory.max")" = 2147483648
test "$(cat "/sys/fs/cgroup/tako-apps/$first_group/pids.max")" = 512
test "$(stat -c %u "/sys/fs/cgroup/tako-apps/$first_group/cgroup.procs")" = 0
test "$(cat "/sys/fs/cgroup/tako-apps/$first_group/cpu.max")" = '200000 100000'

setpriv --reuid="$first_group" --regid="$first_group" --groups=tako-app --no-new-privs sh -c '
  set -eu
  cd /opt/tako/apps/first/production/releases/v1
  test "$(cat ../../data/app/legacy/database)" = old-data
  mkdir -m 0700 ../../data/app/nested
  printf nested-data > ../../data/app/nested/file
  mkdir node_modules
  printf "{}" > package-lock.json
  if printf bad > app.json 2>/dev/null; then exit 1; fi
  if rm app.json 2>/dev/null; then exit 2; fi
  if mv app.json replaced.json 2>/dev/null; then exit 3; fi
  if ls /opt/tako/apps/second/production >/dev/null 2>&1; then exit 4; fi
  if ls /opt/tako/apps/first/production/data/tako >/dev/null 2>&1; then exit 5; fi
  if sudo -n /usr/local/bin/tako-provision-app second/production 2>/dev/null; then exit 6; fi
'
su -s /bin/sh tako -c 'sudo -n /usr/local/bin/tako-provision-app first/production'
su -s /bin/sh tako -c 'test "$(cat /opt/tako/apps/first/production/data/app/nested/file)" = nested-data'
if su -s /bin/sh tako -c 'sudo -n sh -c true' 2>/dev/null; then
  echo 'unexpected arbitrary root shell' >&2
  exit 1
fi
# The capture identity retains the service uid and receives the private app
# group before capabilities are dropped. Exercise app-owned WAL and SHM files.
private_db=/opt/tako/apps/first/production/data/app/capture.sqlite
install -o tako -g "$first_group" -m 0660 /dev/null "$private_db"
python3 - "$private_db" "$first_group" <<'PYSQL'
import os, pwd, subprocess, sys
database, app = sys.argv[1:]
writer = subprocess.Popen([
    "setpriv", "--reuid=" + app, "--regid=" + app, "--groups=tako-app", "--no-new-privs",
    "python3", "-c", """
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.execute('PRAGMA journal_mode=WAL')
db.execute('CREATE TABLE captured (value INTEGER)')
db.execute('INSERT INTO captured VALUES (42)')
db.commit()
print('ready', flush=True)
input()
""", database,
], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
try:
    assert writer.stdout.readline().strip() == "ready"
    assert os.stat(database + "-wal").st_uid == pwd.getpwnam(app).pw_uid
    subprocess.run([
        "setpriv", "--reuid=tako", "--regid=tako", "--groups=" + app,
        "--inh-caps=-all", "--ambient-caps=-all", "--no-new-privs",
        "python3", "-c", "import sqlite3, sys; db = sqlite3.connect(sys.argv[1]); assert db.execute('SELECT value FROM captured').fetchone() == (42,)",
        database,
    ], check=True)
finally:
    writer.communicate("\n", timeout=10)
assert writer.returncode == 0
PYSQL

# Configured shared SQLite files retain separate app identities while allowing
# both processes to commit through WAL with the normal restrictive app umask.
shared_db=/opt/tako/shared/data/app/shared.sqlite
install -o tako -g tako-app -m 0660 /dev/null "$shared_db"
python3 - "$shared_db" <<'PYSQL'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.execute("PRAGMA journal_mode=WAL")
    db.execute("CREATE TABLE writes (app TEXT, value INTEGER, PRIMARY KEY(app, value))")
PYSQL
for app_user in "$first_group" "$second_group"; do
  setpriv --reuid="$app_user" --regid="$app_user" --groups=tako-app --no-new-privs \
    python3 - "$shared_db" "$app_user" <<'PYSQL' &
import os, sqlite3, sys
os.umask(0o027)
with sqlite3.connect(sys.argv[1], timeout=10) as db:
    for value in range(200):
        db.execute("INSERT INTO writes VALUES (?, ?)", (sys.argv[2], value))
        db.commit()
PYSQL
  if [ "$app_user" = "$first_group" ]; then first_writer=$!; else second_writer=$!; fi
done
wait "$first_writer"
wait "$second_writer"
python3 - "$shared_db" <<'PYSQL'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
    assert db.execute("SELECT COUNT(*) FROM writes").fetchone()[0] == 400
    assert db.execute("SELECT COUNT(DISTINCT app) FROM writes").fetchone()[0] == 2
PYSQL

# Re-exec the installed file-capability binary after dropping identity with NNP.
# EOF exits cleanly before parsing input, checking the kernel exec boundary.
setpriv --reuid=tako-images --regid=tako-images --clear-groups --inh-caps=-all --ambient-caps=-all --no-new-privs \
  /usr/bin/env -i PATH=/usr/bin:/bin VIPS_CONCURRENCY=2 /usr/local/bin/tako-server --image-worker </dev/null
echo 'PASS: installed helper, service bounding set, per-app identities, manifest protection, tenant directories, cgroup limits, restricted sudo'
