# ── Server config (config.json) ──────────────────────────────────────
# Stores server_name (metrics label).

TAKO_CONFIG="$TAKO_HOME/config.json"

# Read a field from config.json. Uses jq > python3 > sed fallback.
config_get() {
  local key="$1"
  if [ ! -f "$TAKO_CONFIG" ]; then return; fi
  if command -v jq >/dev/null 2>&1; then
    jq -r ".$key // empty" "$TAKO_CONFIG" 2>/dev/null
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c "
import json, sys, functools, operator
d = json.load(open(sys.argv[1]))
keys = sys.argv[2].split('.')
try: v = functools.reduce(operator.getitem, keys, d)
except (KeyError, TypeError): v = ''
print(v if isinstance(v, str) and v else '')
" "$TAKO_CONFIG" "$key" 2>/dev/null
  else
    # Flat keys only (e.g. server_name), not nested
    sed -n 's/.*"'"$key"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$TAKO_CONFIG" 2>/dev/null | head -1
  fi
}

# Write config.json from CONFIG_SERVER_NAME.
# Preserve non-installer keys such as trusted_proxy when updating installer-owned fields.
write_config() {
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$TAKO_CONFIG" "$CONFIG_SERVER_NAME" <<'PY'
import json
import os
import sys

path, server_name = sys.argv[1:3]
try:
    with open(path) as fh:
        data = json.load(fh)
    if not isinstance(data, dict):
        data = {}
except (FileNotFoundError, json.JSONDecodeError):
    data = {}

if server_name:
    data["server_name"] = server_name

tmp = f"{path}.tmp"
with open(tmp, "w") as fh:
    json.dump(data, fh)
    fh.write("\n")
os.replace(tmp, path)
PY
  elif command -v jq >/dev/null 2>&1; then
    local existing
    existing="$(cat "$TAKO_CONFIG" 2>/dev/null || echo '{}')"
    printf '%s\n' "$existing" | jq \
      --arg server_name "$CONFIG_SERVER_NAME" \
      'if $server_name != "" then .server_name = $server_name else . end' \
      > "$TAKO_CONFIG.tmp"
    mv "$TAKO_CONFIG.tmp" "$TAKO_CONFIG"
  else
    local json="{"
    if [ -n "$CONFIG_SERVER_NAME" ]; then
      json="${json}\"server_name\":\"$CONFIG_SERVER_NAME\""
    fi
    json="${json}}"
    printf '%s\n' "$json" > "$TAKO_CONFIG"
  fi
  chown "$TAKO_USER":"$TAKO_USER" "$TAKO_CONFIG" 2>/dev/null || true
  chmod 0644 "$TAKO_CONFIG"
}

CONFIG_SERVER_NAME=""
if [ -f "$TAKO_CONFIG" ]; then
  CONFIG_SERVER_NAME="$(config_get server_name)"
fi

maybe_prompt_server_name() {
  default_name="$(hostname -s 2>/dev/null || hostname 2>/dev/null || echo "")"

  # Env var takes precedence
  if [ -n "${TAKO_SERVER_NAME:-}" ]; then
    CONFIG_SERVER_NAME="$TAKO_SERVER_NAME"
    write_config
    echo "OK server name set to '$CONFIG_SERVER_NAME'"
    return
  fi

  # Preserve existing name on re-installs
  if [ -n "$CONFIG_SERVER_NAME" ]; then
    echo "OK server name already configured: $CONFIG_SERVER_NAME"
    return
  fi

  if ! is_enabled "$TAKO_RESTART_SERVICE"; then
    return
  fi

  # Interactive prompt
  if [ -t 0 ] 2>/dev/null; then
    printf 'Server name (used in metrics) [%s]: ' "$default_name"
    IFS= read -r TAKO_SERVER_NAME || TAKO_SERVER_NAME=""
    if [ -z "$TAKO_SERVER_NAME" ]; then
      TAKO_SERVER_NAME="$default_name"
    fi
  elif [ -e /dev/tty ]; then
    printf 'Server name (used in metrics) [%s]: ' "$default_name" > /dev/tty
    IFS= read -r TAKO_SERVER_NAME < /dev/tty || TAKO_SERVER_NAME=""
    if [ -z "$TAKO_SERVER_NAME" ]; then
      TAKO_SERVER_NAME="$default_name"
    fi
  else
    TAKO_SERVER_NAME="$default_name"
  fi

  if [ -n "$TAKO_SERVER_NAME" ]; then
    CONFIG_SERVER_NAME="$TAKO_SERVER_NAME"
    write_config
    echo "OK server name set to '$CONFIG_SERVER_NAME'"
  fi
}

maybe_prompt_server_name

configure_public_ports

