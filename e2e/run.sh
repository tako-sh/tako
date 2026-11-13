#!/usr/bin/env bash
set -euo pipefail

FIXTURE=${1:-e2e/fixtures/javascript/tanstack-start}
REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
COMPOSE_FILE="$REPO_ROOT/e2e/docker/compose.yml"
PROJECT_NAME="tako-e2e"
E2E_BIN_DIR="${E2E_BIN_DIR:-$REPO_ROOT/.e2e-bin}"
E2E_BIN_STAMP_FILE="$E2E_BIN_DIR/.build-stamp"
GLIBC_BUILDER_IMAGE="tako-e2e-builder-glibc"
# CI or constrained local engines can reuse already-built test artifacts.
E2E_USE_PREBUILT="${E2E_USE_PREBUILT:-0}"
COMPOSE=(docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE")
BUILD_TRUST=()
RUN_TRUST=()
TRUST_OVERRIDE=""
CACHE_OVERRIDE=""
export E2E_PROXY_CA="${E2E_PROXY_CA:-${CODEX_PROXY_CERT:-}}"
if [[ -n "$E2E_PROXY_CA" ]]; then
  test -s "$E2E_PROXY_CA"
  BUILD_TRUST=(--secret "id=proxy_ca,src=$E2E_PROXY_CA")
  RUN_TRUST=(--mount "type=bind,src=$E2E_PROXY_CA,dst=/run/e2e-proxy-ca.pem,readonly" --env CARGO_HTTP_CAINFO=/run/e2e-proxy-ca.pem)
  TRUST_OVERRIDE=$(mktemp)
  cat > "$TRUST_OVERRIDE" <<'YAML'
services:
  server-ubuntu: &trusted
    build:
      secrets: [proxy_ca]
    volumes:
      - ${E2E_PROXY_CA}:/etc/tako-e2e-proxy-ca.pem:ro
    environment:
      SSL_CERT_FILE: /etc/tako-e2e-proxy-ca.pem
      CURL_CA_BUNDLE: /etc/tako-e2e-proxy-ca.pem
      NODE_EXTRA_CA_CERTS: /etc/tako-e2e-proxy-ca.pem
  server-alma: *trusted
  runner: *trusted
secrets:
  proxy_ca:
    file: ${E2E_PROXY_CA}
YAML
  COMPOSE+=(-f "$TRUST_OVERRIDE")
fi
if [[ -n "${E2E_BUN_CACHE_DIR:-}" ]]; then
  export E2E_BUN_CACHE_DIR
  test -d "$E2E_BUN_CACHE_DIR"
  CACHE_OVERRIDE=$(mktemp)
  cat > "$CACHE_OVERRIDE" <<'YAML'
services:
  runner:
    environment:
      BUN_INSTALL_CACHE_DIR: /opt/e2e/bun-cache
    volumes:
      - ${E2E_BUN_CACHE_DIR}:/opt/e2e/bun-cache
YAML
  COMPOSE+=(-f "$CACHE_OVERRIDE")
fi

current_e2e_build_stamp() {
  local arch inputs_hash
  local -a binary_inputs=(
    Cargo.lock Cargo.toml .cargo/config.toml e2e/run.sh e2e/docker/builder
    tako ':(glob)tako-*/**' vendor
    scripts/install-tako-server.sh presets website/docs sdk/javascript/skills
  )
  arch=$(uname -m)
  inputs_hash=$(
    git -C "$REPO_ROOT" ls-files -coz --exclude-standard -- "${binary_inputs[@]}" |
      sort -zu |
      while IFS= read -r -d '' path; do
        printf '%s\0' "$path"
        if [[ -f "$REPO_ROOT/$path" ]]; then
          git -C "$REPO_ROOT" hash-object "$path"
        else
          printf 'deleted\n'
        fi
      done | git -C "$REPO_ROOT" hash-object --stdin
  )
  printf '%s-%s\n' "$arch" "$inputs_hash"

}

cleanup() {
  local exit_code=$?
  if [[ $exit_code -ne 0 ]]; then
    "${COMPOSE[@]}" logs --no-color --tail=200 server-ubuntu server-alma runner || true
  fi
  if [[ $exit_code -ne 0 && "${E2E_KEEP_FAILED:-0}" == 1 ]]; then
    echo "Keeping failed E2E servers for inspection (E2E_KEEP_FAILED=1)." >&2
  else
    "${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  if [[ -n "$TRUST_OVERRIDE" ]]; then rm -f "$TRUST_OVERRIDE"; fi
  if [[ -n "$CACHE_OVERRIDE" ]]; then rm -f "$CACHE_OVERRIDE"; fi
}
trap cleanup EXIT

export E2E_BIN_DIR

cd "$REPO_ROOT"

EXPECTED_E2E_BIN_STAMP=$(current_e2e_build_stamp)

# Build Linux binaries when missing or stale for the current checkout.
if [[ "$E2E_USE_PREBUILT" != 1 ]] && { [[ ! -f "$E2E_BIN_DIR/glibc/tako" ]] || [[ ! -f "$E2E_BIN_STAMP_FILE" ]] || [[ "$(cat "$E2E_BIN_STAMP_FILE" 2>/dev/null)" != "$EXPECTED_E2E_BIN_STAMP" ]]; }; then
  echo "Building fresh E2E binaries at $E2E_BIN_DIR..."
  mkdir -p "$E2E_BIN_DIR/glibc"

  docker build \
    --build-arg HTTP_PROXY --build-arg HTTPS_PROXY --build-arg NO_PROXY \
    "${BUILD_TRUST[@]}" \
    --file e2e/docker/builder/glibc.Dockerfile \
    --tag "$GLIBC_BUILDER_IMAGE" \
    .
  docker run --rm \
    "${RUN_TRUST[@]}" \
    --env HTTP_PROXY --env HTTPS_PROXY --env NO_PROXY \
    --env CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
    --env CARGO_TARGET_DIR=/workspace/target/e2e-linux-glibc \
    --env TAKO_BUILD_SHA="$(git rev-parse HEAD 2>/dev/null || true)" \
    --volume "$REPO_ROOT:/workspace" \
    --volume tako-e2e-cargo-git:/usr/local/cargo/git \
    --volume tako-e2e-cargo-registry:/usr/local/cargo/registry \
    --workdir /workspace \
    "$GLIBC_BUILDER_IMAGE" \
    cargo build -p tako-server -p tako-cli \
      --bin tako --bin tako-dev-server --bin tako-server \
      --locked --release
  cp target/e2e-linux-glibc/release/tako \
     target/e2e-linux-glibc/release/tako-dev-server \
     target/e2e-linux-glibc/release/tako-server \
     "$E2E_BIN_DIR/glibc/"

  chmod +x "$E2E_BIN_DIR/glibc/"* 2>/dev/null || true
  printf '%s\n' "$EXPECTED_E2E_BIN_STAMP" > "$E2E_BIN_STAMP_FILE"
fi

"${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
if [[ "$E2E_USE_PREBUILT" == 1 ]]; then
  for binary in tako tako-dev-server tako-server; do
    test -x "$E2E_BIN_DIR/glibc/$binary"
  done
  for image in tako-e2e-server-ubuntu tako-e2e-server-alma tako-e2e-runner; do
    docker image inspect "$image" >/dev/null
  done
else
  "${COMPOSE[@]}" build server-ubuntu server-alma runner
fi
"${COMPOSE[@]}" run --rm --no-deps --entrypoint sh runner \
  -c "rm -f /opt/e2e/keys/id_ed25519 /opt/e2e/keys/id_ed25519.pub && ssh-keygen -t ed25519 -N '' -f /opt/e2e/keys/id_ed25519 -q"
"${COMPOSE[@]}" up -d --force-recreate server-ubuntu server-alma
"${COMPOSE[@]}" run --rm runner "$FIXTURE"
