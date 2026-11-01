#!/bin/sh
# Exercise the native app identity against a real production preparation.
set -eu
test "$(id -u)" = 0
test "$#" = 0
app=bun-redis/production
root="/opt/tako/apps/$app"
release=$(readlink -f "$root/current")
identity=$(stat -c %G "$root")
test "$(id -u "$identity")" != "$(id -u tako)"
private="$root/data/compose"
test "$(stat -c %U:%a "$private")" = tako:700
record=$(find "$private" -name current.json -print -quit)
test -n "$record"
test "$(stat -c %U:%a "$record")" = tako:600
source=$(python3 - "$release/app.json" <<'PY'
import json, pathlib, sys
manifest = pathlib.Path(sys.argv[1])
print(manifest.parent / json.loads(manifest.read_text()).get('app_dir', ''))
PY
)
setpriv --reuid="$identity" --regid="$identity" --groups=tako-app --no-new-privs sh -s -- "$source" "$private" "$record" <<'SH'
set -eu
if ls "$2" >/dev/null 2>&1; then exit 1; fi
if cat "$3" >/dev/null 2>&1; then exit 2; fi
if (printf '{}' > "$3") 2>/dev/null; then exit 3; fi
# These are deliberately writable release files. Adoption, secret rotation,
# and rollback must continue to use the private prepared input instead.
printf '%s\n' 'services: {web: {image: busybox, privileged: true, pid: host}}' > "$1/compose.yaml"
printf '%s\n' '{"web":"untrusted-image"}' > "$1/.tako-compose-images.json"
printf '%s\n' 'FROM invalid-build-input' > "$1/Dockerfile"
SH
