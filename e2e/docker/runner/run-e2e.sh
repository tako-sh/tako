#!/usr/bin/env bash
set -euo pipefail

WORKSPACE=${WORKSPACE:-/workspace}
FIXTURE_REL=${1:-${E2E_FIXTURE:-e2e/fixtures/javascript/tanstack-start}}
FIXTURE_DIR="$WORKSPACE/$FIXTURE_REL"

if [[ ! -d "$FIXTURE_DIR" ]]; then
  echo "Fixture directory not found: $FIXTURE_DIR" >&2
  exit 1
fi

# Pre-built binaries (mounted from host or CI artifacts)
BIN_DIR="${E2E_BIN_DIR:-/opt/e2e/bin}"
TAKO_BIN="$BIN_DIR/glibc/tako"
TAKO_SERVER_GLIBC="$BIN_DIR/glibc/tako-server"
TAKO_SERVER_MUSL="${BIN_DIR}/musl/tako-server"

if [[ ! -x "$TAKO_BIN" ]]; then
  echo "tako CLI not found at $TAKO_BIN" >&2
  exit 1
fi
if [[ ! -x "$TAKO_SERVER_GLIBC" ]]; then
  echo "tako-server (glibc) not found at $TAKO_SERVER_GLIBC" >&2
  exit 1
fi

TMP_ROOT=$(mktemp -d)
cleanup() {
  rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

HOME_DIR="$TMP_ROOT/home"
TAKO_HOME="$TMP_ROOT/tako-home"
JS_WORKSPACE_DIR="$TMP_ROOT/js-workspace"
PROJECT_DIR="$JS_WORKSPACE_DIR/$FIXTURE_REL"
mkdir -p "$HOME_DIR/.ssh" "$TAKO_HOME" "$JS_WORKSPACE_DIR"

cp /opt/e2e/keys/id_ed25519 "$HOME_DIR/.ssh/id_ed25519"
cp /opt/e2e/keys/id_ed25519.pub "$HOME_DIR/.ssh/id_ed25519.pub"
cat > "$HOME_DIR/.ssh/config" <<'CFG'
Host server-ubuntu server-alma server-alpine
  User tako
  IdentityFile ~/.ssh/id_ed25519
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
CFG
chmod 700 "$HOME_DIR/.ssh"
chmod 600 "$HOME_DIR/.ssh/id_ed25519"
chmod 644 "$HOME_DIR/.ssh/id_ed25519.pub"
chmod 600 "$HOME_DIR/.ssh/config"
SSH_KEY="$HOME_DIR/.ssh/id_ed25519"
SSH_OPTS=(
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o BatchMode=yes
  -i "$SSH_KEY"
)

ssh_exec() {
  local host=$1
  shift
  HOME="$HOME_DIR" ssh "${SSH_OPTS[@]}" "tako@$host" "$@"
}

scp_to() {
  local source=$1
  local host=$2
  local destination=$3
  HOME="$HOME_DIR" scp "${SSH_OPTS[@]}" "$source" "tako@$host:$destination"
}

ssh_wait() {
  local host=$1
  for _ in $(seq 1 80); do
    if ssh_exec "$host" "echo ok" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  echo "SSH not ready: $host" >&2
  return 1
}

wait_tako_socket() {
  local host=$1
  for _ in $(seq 1 120); do
    if ssh_exec "$host" \
      "printf '%s\n' '{\"command\":\"list\"}' | nc -U /var/run/tako/tako.sock | head -n 1 | grep -q ." \
      >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  echo "tako-server socket not ready: $host" >&2
  ssh_exec "$host" "tail -n 120 /tmp/tako-server.log || true" >&2 || true
  return 1
}

wait_tako_management_http() {
  local host=$1
  for _ in $(seq 1 120); do
    if curl -fsS -m 1 \
      -H 'content-type: application/json' \
      --data '{"command":"hello","protocol_version":0}' \
      "http://${host}:9844/rpc" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  echo "tako-server management HTTP not ready: $host" >&2
  ssh_exec "$host" "tail -n 120 /tmp/tako-server.log || true" >&2 || true
  return 1
}

resolve_current_release_link() {
  local host=$1
  for _ in $(seq 1 20); do
    local result
    result=$(ssh_exec "$host" '
for link in /opt/tako/apps/*/*/current; do
  [ -L "$link" ] || continue
  readlink -f "$link"
  exit 0
done
exit 1
' 2>/dev/null) && [ -n "$result" ] && echo "$result" && return 0
    sleep 0.5
  done
  return 1
}

detect_route_host() {
  local toml_path=$1
  local env_name=${2:-production}
  awk -v env_name="$env_name" '
    $0 ~ "^\\[envs\\." env_name "\\]" {
      in_env = 1
      next
    }
    in_env && $0 ~ "^\\[" {
      in_env = 0
    }
    in_env && $1 == "route" {
      line = $0
      sub(/^[^=]*=[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit
    }
  ' "$toml_path"
}

detect_app_name() {
  local toml_path=$1
  awk '
    $1 == "name" {
      line = $0
      sub(/^[^=]*=[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit
    }
  ' "$toml_path"
}

require_file_contains() {
  local file=$1
  local needle=$2
  local description=$3

  if ! grep -Fq "$needle" "$file"; then
    echo "$description did not include expected text: $needle" >&2
    cat "$file" >&2 || true
    exit 1
  fi
}

fetch_route_path() {
  local server_host=$1
  local route_host=$2
  local route_path=$3
  local headers_file=$4
  local body_file=$5

  curl -sS \
    --http1.1 \
    --connect-timeout 3 \
    --max-time 10 \
    -H "Host: ${route_host}" \
    -H "Connection: close" \
    -D "$headers_file" \
    -o "$body_file" \
    -w "%{http_code}" \
    "http://${server_host}:8080${route_path}"
}

fetch_route_path_forwarded_https() {
  local server_host=$1
  local route_host=$2
  local route_path=$3
  local headers_file=$4
  local body_file=$5

  curl -sS \
    --http1.1 \
    --connect-timeout 3 \
    --max-time 10 \
    -H "Host: ${route_host}" \
    -H "X-Forwarded-Proto: https" \
    -H "Forwarded: proto=https" \
    -H "Connection: close" \
    -D "$headers_file" \
    -o "$body_file" \
    -w "%{http_code}" \
    "http://${server_host}:8080${route_path}"
}

require_http_ok() {
  local server_host=$1
  local route_host=$2
  local route_path=$3
  local description=$4
  local require_non_empty=${5:-1}
  local headers_file="$TMP_ROOT/http_headers.tmp"
  local body_file="$TMP_ROOT/http_body.tmp"
  local status

  status=$(fetch_route_path "$server_host" "$route_host" "$route_path" "$headers_file" "$body_file" || true)

  if [[ ! "$status" =~ ^[0-9]+$ ]] || (( status < 200 || status >= 400 )); then
    echo "$description check failed for path '$route_path' (status=$status)" >&2
    [[ -f "$headers_file" ]] && cat "$headers_file" >&2 || true
    [[ -f "$body_file" ]] && cat "$body_file" >&2 || true
    exit 1
  fi

  if (( require_non_empty )) && [[ ! -s "$body_file" ]]; then
    echo "$description check failed for path '$route_path': empty response body" >&2
    exit 1
  fi
}

post_route_json() {
  local server_host=$1
  local route_host=$2
  local route_path=$3
  local json_body=$4
  local description=$5
  local headers_file="$TMP_ROOT/post_headers.tmp"
  local body_file="$TMP_ROOT/post_body.tmp"
  local status

  status=$(curl -sS \
    --http1.1 \
    --connect-timeout 3 \
    --max-time 10 \
    -H "Host: ${route_host}" \
    -H "X-Forwarded-Proto: https" \
    -H "Forwarded: proto=https" \
    -H "Connection: close" \
    -H "Content-Type: application/json" \
    -D "$headers_file" \
    -o "$body_file" \
    -w "%{http_code}" \
    -d "$json_body" \
    "http://${server_host}:8080${route_path}" || true)

  if [[ ! "$status" =~ ^[0-9]+$ ]] || (( status < 200 || status >= 300 )); then
    echo "$description failed for path '$route_path' (status=$status)" >&2
    [[ -f "$headers_file" ]] && cat "$headers_file" >&2 || true
    [[ -f "$body_file" ]] && cat "$body_file" >&2 || true
    exit 1
  fi

  if ! jq -e '.ok == true' "$body_file" >/dev/null 2>&1; then
    echo "$description did not return { ok: true }" >&2
    cat "$body_file" >&2 || true
    exit 1
  fi
}

fixture_has_secrets_for_env() {
  local env_name=$1
  [[ -f "$PROJECT_DIR/.tako/secrets.json" ]] && \
    jq -e --arg env_name "$env_name" '
      ((.[$env_name].app // {}) | length > 0) or
      ((.[$env_name].storages // {}) | length > 0)
    ' \
      "$PROJECT_DIR/.tako/secrets.json" >/dev/null 2>&1
}

import_fixture_secret_key() {
  local env_name=$1
  local import_log="$TMP_ROOT/secrets-import-${env_name}.log"

  if ! fixture_has_secrets_for_env "$env_name"; then
    return 0
  fi

  if ! printf '%s\n' "${TAKO_EXAMPLE_SECRET_PASSPHRASE:-tako-example}" | \
    HOME="$HOME_DIR" TAKO_HOME="$TAKO_HOME" "$TAKO_BIN" \
      --config "$PROJECT_DIR/tako.toml" secrets key import --passphrase --env "$env_name" \
      >"$import_log" 2>&1; then
    echo "Failed to import $env_name fixture secret key" >&2
    cat "$import_log" >&2 || true
    exit 1
  fi
}

run_secret_checks() {
  local server_host=$1
  local route_host=$2
  local headers_file="$TMP_ROOT/secret_headers.tmp"
  local body_file="$TMP_ROOT/secret_body.tmp"
  local status
  local path

  if ! fixture_has_secrets_for_env production; then
    return 0
  fi

  echo "Running secret checks for route: $route_host on $server_host"

  for path in /api/secret /secret /; do
    status=$(
      fetch_route_path_forwarded_https \
        "$server_host" "$route_host" "$path" "$headers_file" "$body_file" || true
    )
    if [[ "$status" =~ ^[0-9]+$ ]] && (( status >= 200 && status < 400 )); then
      if grep -Eq '"has_secret"[[:space:]]*:[[:space:]]*true|Secret check:[[:space:]]*present' "$body_file"; then
        return 0
      fi
    fi
  done

  echo "Secret check did not find an injected secret response." >&2
  [[ -f "$headers_file" ]] && cat "$headers_file" >&2 || true
  [[ -f "$body_file" ]] && cat "$body_file" >&2 || true
  exit 1
}

check_http_ok_optional() {
  local server_host=$1
  local route_host=$2
  local route_path=$3
  local description=$4
  local headers_file="$TMP_ROOT/http_headers_optional.tmp"
  local body_file="$TMP_ROOT/http_body_optional.tmp"
  local status

  status=$(fetch_route_path "$server_host" "$route_host" "$route_path" "$headers_file" "$body_file" || true)

  if [[ ! "$status" =~ ^[0-9]+$ ]] || (( status < 200 || status >= 400 )); then
    echo "$description check skipped for path '$route_path' (status=$status)" >&2
    [[ -f "$headers_file" ]] && cat "$headers_file" >&2 || true
    [[ -f "$body_file" ]] && cat "$body_file" >&2 || true
  fi
}

wait_for_file_text() {
  local file=$1
  local needle=$2
  local description=$3

  for _ in $(seq 1 80); do
    if [[ -f "$file" ]] && grep -Fq "$needle" "$file"; then
      return 0
    fi
    sleep 0.25
  done

  echo "Timed out waiting for $description: $needle" >&2
  [[ -f "$file" ]] && cat "$file" >&2 || true
  return 1
}

run_channels_workflows_checks() {
  local server_host=$1
  local route_host=$2
  local events_file="$TMP_ROOT/channels-events-${server_host}.txt"
  local headers_file="$TMP_ROOT/channels-headers-${server_host}.txt"
  local stderr_file="$TMP_ROOT/channels-curl-${server_host}.err"
  local direct_message="direct-${server_host}-$(date +%s%N)"
  local workflow_message="workflow-${server_host}-$(date +%s%N)"
  local sse_pid

  echo "Running channel/workflow SSE checks for route: $route_host on $server_host"

  rm -f "$events_file" "$headers_file" "$stderr_file"
  curl -sS -N \
    --http1.1 \
    --connect-timeout 3 \
    --max-time 30 \
    -H "Host: ${route_host}" \
    -H "X-Forwarded-Proto: https" \
    -H "Forwarded: proto=https" \
    -H "Authorization: Bearer e2e" \
    -H "Accept: text/event-stream" \
    -D "$headers_file" \
    -o "$events_file" \
    "http://${server_host}:8080/_tako/channels/demo" \
    2>"$stderr_file" &
  sse_pid=$!

  cleanup_sse() {
    kill "$sse_pid" >/dev/null 2>&1 || true
    wait "$sse_pid" >/dev/null 2>&1 || true
  }

  for _ in $(seq 1 40); do
    if [[ -f "$headers_file" ]] && grep -qi "content-type: text/event-stream" "$headers_file"; then
      break
    fi
    if ! kill -0 "$sse_pid" >/dev/null 2>&1; then
      echo "SSE connection exited before opening" >&2
      [[ -f "$headers_file" ]] && cat "$headers_file" >&2 || true
      [[ -f "$stderr_file" ]] && cat "$stderr_file" >&2 || true
      cleanup_sse
      exit 1
    fi
    sleep 0.25
  done

  if ! grep -qi "content-type: text/event-stream" "$headers_file"; then
    echo "SSE connection did not return text/event-stream headers" >&2
    [[ -f "$headers_file" ]] && cat "$headers_file" >&2 || true
    [[ -f "$stderr_file" ]] && cat "$stderr_file" >&2 || true
    cleanup_sse
    exit 1
  fi

  post_route_json "$server_host" "$route_host" "/publish" "{\"message\":\"$direct_message\"}" "Direct channel publish"
  post_route_json "$server_host" "$route_host" "/enqueue" "{\"message\":\"$workflow_message\"}" "Workflow enqueue"

  if ! wait_for_file_text "$events_file" "$direct_message" "direct channel publish SSE event"; then
    cleanup_sse
    exit 1
  fi
  if ! wait_for_file_text "$events_file" "$workflow_message" "workflow channel publish SSE event"; then
    cleanup_sse
    exit 1
  fi

  cleanup_sse
}

run_cli_post_deploy_checks() {
  local server_host=$1
  local app_name=$2
  local route_host=$3
  local release_version=$4
  local remote_app_name="${app_name}/production"
  local status_log="$TMP_ROOT/status-${server_host}.log"
  local releases_log="$TMP_ROOT/releases-${server_host}.log"
  local logs_log="$TMP_ROOT/logs-${server_host}.log"
  local log_marker="e2e-log-marker-${server_host}-$(date +%s%N)"
  local remote_log_dir="/opt/tako/apps/${remote_app_name}/logs"
  local remote_log_file="${remote_log_dir}/current.log"

  echo "Running CLI post-deploy checks for $app_name on $server_host"

  if ! HOME="$HOME_DIR" TAKO_HOME="$TAKO_HOME" "$TAKO_BIN" --ci status >"$status_log" 2>&1; then
    echo "tako status failed on $server_host" >&2
    cat "$status_log" >&2 || true
    exit 1
  fi
  require_file_contains "$status_log" "Server ssh" "tako status"
  require_file_contains "$status_log" "$app_name" "tako status"
  require_file_contains "$status_log" "production" "tako status"
  require_file_contains "$status_log" "$route_host" "tako status"
  require_file_contains "$status_log" "healthy" "tako status"

  if ! HOME="$HOME_DIR" TAKO_HOME="$TAKO_HOME" "$TAKO_BIN" --config "$PROJECT_DIR/tako.toml" releases list --env production >"$releases_log" 2>&1; then
    echo "tako releases list failed on $server_host" >&2
    cat "$releases_log" >&2 || true
    exit 1
  fi
  require_file_contains "$releases_log" "$release_version" "tako releases list"
  require_file_contains "$releases_log" "[current]" "tako releases list"

  ssh_exec "$server_host" "mkdir -p '$remote_log_dir' && printf '%s [out] [e2e] ${log_marker}\n' \"\$(date -u '+%Y-%m-%dT%H:%M:%S.000Z')\" >> '$remote_log_file' && grep -F '$log_marker' '$remote_log_file' >/dev/null"

  if ! HOME="$HOME_DIR" TAKO_HOME="$TAKO_HOME" "$TAKO_BIN" --ci --config "$PROJECT_DIR/tako.toml" logs --env production --json --days 1 >"$logs_log" 2>&1; then
    echo "tako logs failed on $server_host" >&2
    cat "$logs_log" >&2 || true
    exit 1
  fi
  require_file_contains "$logs_log" "$log_marker" "tako logs --json"
  require_file_contains "$logs_log" '"source":"e2e"' "tako logs --json"
}

run_universal_http_checks() {
  local server_host=$1
  local route_host=$2
  local release_app_dir=$3
  local root_headers="$TMP_ROOT/root_headers.tmp"
  local root_body="$TMP_ROOT/root_body.tmp"
  local root_status
  local root_content_type
  local root_ready=0
  local response_kind="text"
  local static_path=""
  local public_path=""
  local compiled_release_path=""
  local compiled_checked=0

  echo "Running universal HTTP checks for route: $route_host on $server_host"

  for _ in $(seq 1 80); do
    root_status=$(fetch_route_path "$server_host" "$route_host" "/" "$root_headers" "$root_body" || true)
    if [[ "$root_status" =~ ^[0-9]+$ ]] && (( root_status >= 200 && root_status < 400 )) && [[ -s "$root_body" ]]; then
      root_ready=1
      break
    fi
    sleep 0.5
  done
  if (( root_ready == 0 )); then
    echo "App root check failed for '/' (status=$root_status)" >&2
    [[ -f "$root_headers" ]] && cat "$root_headers" >&2 || true
    [[ -f "$root_body" ]] && cat "$root_body" >&2 || true
    exit 1
  fi

  root_content_type=$(tr -d '\r' < "$root_headers" | awk 'tolower($1) == "content-type:" {print tolower($2)}' | tail -n 1)
  if [[ "$root_content_type" == *"text/html"* ]] || grep -Eqi '<!doctype html|<html[[:space:]>]' "$root_body"; then
    response_kind="html"
  elif [[ "$root_content_type" == *"application/json"* ]] || jq -e . "$root_body" >/dev/null 2>&1; then
    response_kind="json"
  fi

  if [[ "$response_kind" == "html" ]] && ! grep -Eqi '<!doctype html|<html[[:space:]>]' "$root_body"; then
    echo "App root was classified as HTML but did not contain HTML markup." >&2
    exit 1
  fi
  if [[ "$response_kind" == "json" ]] && ! jq -e . "$root_body" >/dev/null 2>&1; then
    echo "App root was classified as JSON but body is not valid JSON." >&2
    exit 1
  fi

  echo "Root response kind: $response_kind"

  static_path=$(ssh_exec "$server_host" "cd '$release_app_dir' && find static -type f 2>/dev/null | head -n 1 | sed 's#^#/#'" || true)
  static_path=$(echo "$static_path" | tr -d '\r' | head -n 1)
  if [[ -n "$static_path" ]]; then
    check_http_ok_optional "$server_host" "$route_host" "$static_path" "Static file"
  fi

  public_path=$(ssh_exec "$server_host" "cd '$release_app_dir' && find public -type f 2>/dev/null | head -n 1 | sed 's#^public/#/#'" || true)
  public_path=$(echo "$public_path" | tr -d '\r' | head -n 1)
  if [[ -n "$public_path" ]]; then
    check_http_ok_optional "$server_host" "$route_host" "$public_path" "Public file"
  fi

  if [[ "$response_kind" == "html" ]]; then
    mapfile -t html_asset_paths < <(grep -Eo "/[^\"'[:space:]>]+\\.(js|mjs|css)(\\?[^\"'[:space:]>]+)?" "$root_body" | sort -u)
    if (( ${#html_asset_paths[@]} > 0 )); then
      for asset_path in "${html_asset_paths[@]}"; do
        check_http_ok_optional "$server_host" "$route_host" "$asset_path" "Compiled asset"
        compiled_checked=1
      done
    fi
  fi

  if (( compiled_checked == 0 )); then
    compiled_release_path=$(ssh_exec "$server_host" "cd '$release_app_dir' && { find static -type f \\( -name '*.js' -o -name '*.mjs' -o -name '*.css' \\) 2>/dev/null; find assets -type f \\( -name '*.js' -o -name '*.mjs' -o -name '*.css' \\) 2>/dev/null; } | head -n 1 | sed 's#^#/#'" || true)
    compiled_release_path=$(echo "$compiled_release_path" | tr -d '\r' | head -n 1)
    if [[ -n "$compiled_release_path" ]]; then
      check_http_ok_optional "$server_host" "$route_host" "$compiled_release_path" "Compiled asset"
      compiled_checked=1
    fi
  fi

  if (( compiled_checked == 0 )); then
    echo "No compiled static asset candidates found; skipping compiled asset check."
  fi
}

start_tako_server() {
  local host=$1
  local server_bin=$2
  local server_config='{"server_name":"e2e","trusted_proxy":{"trusted_cidrs":["172.16.0.0/12"]}}'
  scp_to "$server_bin" "$host" "/home/tako/tako-server"
  scp_to "$WORKSPACE/scripts/install-tako-server.sh" "$host" "/home/tako/install-tako-server.sh"
  ssh_exec "$host" "set -eu; chmod 0755 /home/tako/tako-server /home/tako/install-tako-server.sh; tar -cf - -C /home/tako tako-server | zstd -o /home/tako/tako-server.tar.zst; sha256sum /home/tako/tako-server.tar.zst | awk '{print \$1}' > /home/tako/tako-server.tar.zst.sha256"
  if ! ssh_exec "$host" "sudo sh -c 'TAKO_SERVER_URL=file:///home/tako/tako-server.tar.zst TAKO_RESTART_SERVICE=0 TAKO_SERVER_NAME=e2e sh /home/tako/install-tako-server.sh'"; then
    ssh_exec "$host" "rm -f /home/tako/tako-server /home/tako/tako-server.tar.zst /home/tako/tako-server.tar.zst.sha256 /home/tako/install-tako-server.sh" >/dev/null 2>&1 || true
    return 2
  fi
  # E2E marks requests as HTTPS through forwarded headers from the Docker bridge.
  # Keep production defaults strict while making the test proxy relationship explicit.
  ssh_exec "$host" "printf '%s\n' '$server_config' | sudo tee /opt/tako/config.json >/dev/null && sudo chown tako:tako /opt/tako/config.json && sudo chmod 0644 /opt/tako/config.json"
  ssh_exec "$host" "rm -f /home/tako/tako-server /home/tako/tako-server.tar.zst /home/tako/tako-server.tar.zst.sha256 /home/tako/install-tako-server.sh"
  ssh_exec "$host" "sudo pkill -x tako-server >/dev/null 2>&1 || true"
  ssh_exec "$host" "sudo rm -f /var/run/tako/tako.sock"
  ssh_exec "$host" "RUST_LOG=info nohup /usr/local/bin/tako-server --no-acme --http-port 8080 --https-port 8443 --data-dir /opt/tako --management-host 0.0.0.0 >/tmp/tako-server.log 2>&1 &"
  wait_tako_socket "$host"
  wait_tako_management_http "$host"
}

# Wait for SSH on all servers
ssh_wait server-ubuntu
ssh_wait server-alma
ssh_wait server-alpine

# Start tako-server on each (glibc for Ubuntu/Alma, musl for Alpine)
ACTIVE_SERVERS=()
start_tako_server server-ubuntu "$TAKO_SERVER_GLIBC"
ACTIVE_SERVERS+=("server-ubuntu:gnu")
if start_tako_server server-alma "$TAKO_SERVER_GLIBC"; then
  ACTIVE_SERVERS+=("server-alma:gnu")
else
  rc=$?
  if [[ $rc -ne 2 ]]; then
    exit "$rc"
  fi
  echo "=== server-alma skipped (tako-server runtime dependencies unavailable) ==="
fi
if [[ -x "$TAKO_SERVER_MUSL" ]]; then
  if start_tako_server server-alpine "$TAKO_SERVER_MUSL"; then
    ACTIVE_SERVERS+=("server-alpine:musl")
  else
    rc=$?
    if [[ $rc -ne 2 ]]; then
      exit "$rc"
    fi
    echo "=== server-alpine skipped (tako-server runtime dependencies unavailable) ==="
  fi
fi
if (( ${#ACTIVE_SERVERS[@]} == 0 )); then
  echo "No E2E servers could start." >&2
  exit 1
fi

# Stage a workspace copy for the fixture. JS fixtures need a monorepo-style
# workspace; Go fixtures just need the SDK source for the replace directive.
mkdir -p "$(dirname "$PROJECT_DIR")"
cp -R "$FIXTURE_DIR" "$PROJECT_DIR"

if [[ "$FIXTURE_REL" == examples/go/* ]]; then
  # The committed examples point at a real demo server. E2E runs against the
  # disposable server named "ssh" that is written below.
  sed -i 's/^servers = .*/servers = ["ssh"]/' "$PROJECT_DIR/tako.toml"
fi

if [[ -f "$PROJECT_DIR/go.mod" ]]; then
  # ── Go fixture setup ─────────────────────────────────────────────
  # Copy the Go SDK source (go.mod + *.go + internal/) so the fixture's replace directive resolves.
  mkdir -p "$JS_WORKSPACE_DIR/internal"
  cp "$WORKSPACE/go.mod" "$WORKSPACE/tako.go" "$JS_WORKSPACE_DIR/"
  cp -R "$WORKSPACE/internal/." "$JS_WORKSPACE_DIR/internal/"
  (cd "$JS_WORKSPACE_DIR" && git init -q)
  # Rewrite the replace directive to point to the staged SDK location
  sed -i "s|=> .*|=> $JS_WORKSPACE_DIR|" "$PROJECT_DIR/go.mod"
  (cd "$PROJECT_DIR" && go mod tidy 2>&1 || true)
else
  # ── JS fixture setup ─────────────────────────────────────────────
  # Stage a minimal JS monorepo copy so Bun workspace/catalog references resolve
  # like local dev, without rewriting dependency declarations.
  jq --arg fixture_rel "$FIXTURE_REL" '
    .workspaces.packages = ["sdk/javascript", $fixture_rel]
  ' "$WORKSPACE/package.json" > "$JS_WORKSPACE_DIR/package.json"
  mkdir -p "$JS_WORKSPACE_DIR/sdk"
  cp -R "$WORKSPACE/sdk/javascript" "$JS_WORKSPACE_DIR/sdk/javascript"
  rm -rf "$JS_WORKSPACE_DIR/sdk/javascript/node_modules" "$PROJECT_DIR/node_modules"
  # Ensure deploy uses this staged JS workspace as source root so Bun workspace
  # dependencies (for example tako.sh = workspace:*) resolve on remote installs.
  (cd "$JS_WORKSPACE_DIR" && git init -q)
fi

if [[ -f "$PROJECT_DIR/package.json" ]]; then
  if ! command -v bun >/dev/null 2>&1; then
    echo "bun is required in the e2e runner image to build JS fixtures" >&2
    exit 1
  fi

  (cd "$JS_WORKSPACE_DIR" && bun install)
  (cd "$JS_WORKSPACE_DIR/sdk/javascript" && bun run build)

  # Pack the built SDK as a tarball so bun copies (not symlinks) it into
  # node_modules/tako.sh/. This ensures the SDK entrypoint resolves sibling
  # deps from the project's node_modules/, not from the SDK source dir.
  SDK_SRC="$JS_WORKSPACE_DIR/sdk/javascript"
  if [[ -d "$SDK_SRC/dist" ]]; then
    # npm/bun tarballs expect a `package/` prefix inside the archive
    (cd "$SDK_SRC" && tar czf "$PROJECT_DIR/tako-sdk.tgz" --transform='s,^,package/,' package.json dist/)
    jq '.dependencies["tako.sh"] = "file:tako-sdk.tgz"' "$PROJECT_DIR/package.json" > "$PROJECT_DIR/package.json.tmp"
    mv "$PROJECT_DIR/package.json.tmp" "$PROJECT_DIR/package.json"
  fi

  # Install deps in the fixture dir. This creates a bun.lock that matches
  # the rewritten package.json (tarball instead of workspace:*).
  (cd "$PROJECT_DIR" && bun install)
fi

ARCH_RAW=$(uname -m)
TARGET_ARCH="x86_64"
if [[ "$ARCH_RAW" == "aarch64" || "$ARCH_RAW" == "arm64" ]]; then
  TARGET_ARCH="aarch64"
fi

# Populate known_hosts for the tako CLI (uses $HOME/.ssh/known_hosts)
ssh-keyscan -H server-ubuntu >> "$HOME_DIR/.ssh/known_hosts" 2>/dev/null
ssh-keyscan -H server-alma >> "$HOME_DIR/.ssh/known_hosts" 2>/dev/null
ssh-keyscan -H server-alpine >> "$HOME_DIR/.ssh/known_hosts" 2>/dev/null

# Deploy test targets
SERVERS=("${ACTIVE_SERVERS[@]}")

ROUTE_HOST=$(detect_route_host "$PROJECT_DIR/tako.toml" "production")
if [[ -z "$ROUTE_HOST" ]]; then
  echo "Could not resolve production route host from $PROJECT_DIR/tako.toml" >&2
  exit 1
fi
APP_NAME=$(detect_app_name "$PROJECT_DIR/tako.toml")
if [[ -z "$APP_NAME" ]]; then
  echo "Could not resolve app name from $PROJECT_DIR/tako.toml" >&2
  exit 1
fi

import_fixture_secret_key production

for entry in "${SERVERS[@]}"; do
  server="${entry%%:*}"
  libc="${entry##*:}"

  echo "=== Testing deploy on $server ($libc) ==="

  cat > "$TAKO_HOME/config.toml" <<CFG
[[servers]]
name = "ssh"
host = "$server"
port = 22
arch = "$TARGET_ARCH"
libc = "$libc"
CFG

  DEPLOY_LOG="$TMP_ROOT/deploy-${server}.log"

  if ! HOME="$HOME_DIR" TAKO_HOME="$TAKO_HOME" "$TAKO_BIN" --config "$PROJECT_DIR/tako.toml" deploy --env production --yes --verbose >"$DEPLOY_LOG" 2>&1; then
    if [[ "$libc" == "musl" ]]; then
      echo "=== $server skipped (deploy failed on musl — runtime may not support musl) ==="
      continue
    fi
    cat "$DEPLOY_LOG" >&2 || true
    echo "--- tako-server log from $server ---" >&2
    ssh_exec "$server" "cat /tmp/tako-server.log 2>/dev/null | tail -50" >&2 || true
    exit 1
  fi
  cat "$DEPLOY_LOG"

  CURRENT_LINK=$(resolve_current_release_link "$server" || true)

  if [[ -z "$CURRENT_LINK" ]]; then
    echo "Failed to resolve deployed release symlink on $server" >&2
    exit 1
  fi
  CURRENT_VERSION=$(basename "$CURRENT_LINK")

  if ! ssh_exec "$server" "test -f '$CURRENT_LINK/app.json'" >/dev/null 2>&1; then
    echo "Missing app.json under $CURRENT_LINK on $server" >&2
    exit 1
  fi

  # Read app_dir from the manifest to find the actual app directory
  MANIFEST_APP_DIR=$(ssh_exec "$server" "cat '$CURRENT_LINK/app.json'" | jq -r '.app_dir // empty')
  if [[ -n "$MANIFEST_APP_DIR" ]]; then
    APP_RELEASE_DIR="$CURRENT_LINK/$MANIFEST_APP_DIR"
  else
    APP_RELEASE_DIR="$CURRENT_LINK"
  fi

  run_universal_http_checks "$server" "$ROUTE_HOST" "$APP_RELEASE_DIR"
  run_secret_checks "$server" "$ROUTE_HOST"
  if [[ "$FIXTURE_REL" == "e2e/fixtures/javascript/channels-workflows" ]]; then
    run_channels_workflows_checks "$server" "$ROUTE_HOST"
  fi
  run_cli_post_deploy_checks "$server" "$APP_NAME" "$ROUTE_HOST" "$CURRENT_VERSION"

  echo "=== $server passed ==="
done

TESTED_SERVERS=$(printf '%s\n' "${SERVERS[@]}" | cut -d: -f1 | tr '\n' ' ')
echo "E2E deploy test passed for $FIXTURE_REL on${TESTED_SERVERS:+ $TESTED_SERVERS}"
