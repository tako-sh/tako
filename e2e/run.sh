#!/usr/bin/env bash
set -euo pipefail

FIXTURE=${1:-e2e/fixtures/javascript/tanstack-start}
REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
COMPOSE_FILE="$REPO_ROOT/e2e/docker/compose.yml"
PROJECT_NAME="tako-e2e"
E2E_BIN_DIR="${E2E_BIN_DIR:-$REPO_ROOT/.e2e-bin}"
E2E_BIN_STAMP_FILE="$E2E_BIN_DIR/.build-stamp"
GLIBC_BUILDER_IMAGE="tako-e2e-builder-glibc"

current_e2e_build_stamp() {
  local head arch dirty_suffix
  local -a binary_inputs=(
    Cargo.lock
    Cargo.toml
    e2e/run.sh
    tako
    tako-channels
    tako-core
    tako-runtime
    tako-server
    tako-socket
    tako-spawn
    tako-workflows
  )
  arch=$(uname -m)
  head=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo "nogit")
  dirty_suffix=""
  if ! git -C "$REPO_ROOT" diff --quiet --ignore-submodules HEAD -- "${binary_inputs[@]}"; then
    dirty_suffix="-dirty-$(git -C "$REPO_ROOT" diff --binary --ignore-submodules HEAD -- "${binary_inputs[@]}" | git -C "$REPO_ROOT" hash-object --stdin)"
  fi
  printf '%s-%s%s\n' "$head" "$arch" "$dirty_suffix"
}

cleanup() {
  local exit_code=$?
  if [[ $exit_code -ne 0 ]]; then
    docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" logs --no-color --tail=200 server-ubuntu server-alma runner || true
  fi
  docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" down --volumes --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

export E2E_BIN_DIR

cd "$REPO_ROOT"

EXPECTED_E2E_BIN_STAMP=$(current_e2e_build_stamp)

# Build Linux binaries when missing or stale for the current checkout.
if [[ ! -f "$E2E_BIN_DIR/glibc/tako" ]] || [[ ! -f "$E2E_BIN_STAMP_FILE" ]] || [[ "$(cat "$E2E_BIN_STAMP_FILE" 2>/dev/null)" != "$EXPECTED_E2E_BIN_STAMP" ]]; then
  echo "Building fresh E2E binaries at $E2E_BIN_DIR..."
  mkdir -p "$E2E_BIN_DIR/glibc"

  docker build \
    --file e2e/docker/builder/glibc.Dockerfile \
    --tag "$GLIBC_BUILDER_IMAGE" \
    .
  docker run --rm \
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

docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" down --volumes --remove-orphans >/dev/null 2>&1 || true
docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" build server-ubuntu server-alma runner
docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" run --rm --no-deps --entrypoint sh runner \
  -c "rm -f /opt/e2e/keys/id_ed25519 /opt/e2e/keys/id_ed25519.pub && ssh-keygen -t ed25519 -N '' -f /opt/e2e/keys/id_ed25519 -q"
docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" up -d --force-recreate server-ubuntu server-alma
docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" run --rm runner "$FIXTURE"
