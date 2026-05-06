#!/usr/bin/env bash
set -euo pipefail

FIXTURE=${1:-e2e/fixtures/javascript/tanstack-start}
REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
COMPOSE_FILE="$REPO_ROOT/e2e/docker/compose.yml"
PROJECT_NAME="tako-e2e"
E2E_BIN_DIR="${E2E_BIN_DIR:-$REPO_ROOT/.e2e-bin}"
E2E_BIN_STAMP_FILE="$E2E_BIN_DIR/.build-stamp"
RUSTUP_BIN_DIR="${HOME}/.cargo/bin"
CARGO_BIN="${RUSTUP_BIN_DIR}/cargo"
RUSTC_BIN="${RUSTUP_BIN_DIR}/rustc"

if [[ ! -x "$CARGO_BIN" ]]; then
  CARGO_BIN="$(command -v cargo)"
fi
if [[ -x "$RUSTUP_BIN_DIR/cargo" ]] && [[ -x "$RUSTUP_BIN_DIR/rustc" ]]; then
  export PATH="$RUSTUP_BIN_DIR:$PATH"
  export RUSTC="$RUSTC_BIN"
fi

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
    docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" logs --no-color --tail=200 server-ubuntu server-alma server-alpine runner || true
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
  mkdir -p "$E2E_BIN_DIR/glibc" "$E2E_BIN_DIR/musl"

  # Detect host arch → pick matching Linux target
  ARCH_RAW=$(uname -m)
  if [[ "$ARCH_RAW" == "arm64" || "$ARCH_RAW" == "aarch64" ]]; then
    GLIBC_TARGET="aarch64-unknown-linux-gnu"
    MUSL_TARGET="aarch64-unknown-linux-musl"
  else
    GLIBC_TARGET="x86_64-unknown-linux-gnu"
    MUSL_TARGET="x86_64-unknown-linux-musl"
  fi

  "$CARGO_BIN" zigbuild -p tako-server -p tako \
    --bin tako --bin tako-dev-server --bin tako-server \
    --release --target "$GLIBC_TARGET"
  cp target/"$GLIBC_TARGET"/release/tako \
     target/"$GLIBC_TARGET"/release/tako-dev-server \
     target/"$GLIBC_TARGET"/release/tako-server \
     "$E2E_BIN_DIR/glibc/"

  # musl build (used for Alpine)
  if "$CARGO_BIN" zigbuild -p tako-server --release --target "$MUSL_TARGET" 2>"$E2E_BIN_DIR/musl-build.log"; then
    cp target/"$MUSL_TARGET"/release/tako-server "$E2E_BIN_DIR/musl/"
    rm -f "$E2E_BIN_DIR/musl-build.log"
  else
    echo "musl build skipped (see .e2e-bin/musl-build.log for details)"
  fi

  chmod +x "$E2E_BIN_DIR/glibc/"* "$E2E_BIN_DIR/musl/"* 2>/dev/null || true
  printf '%s\n' "$EXPECTED_E2E_BIN_STAMP" > "$E2E_BIN_STAMP_FILE"
fi

docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" down --volumes --remove-orphans >/dev/null 2>&1 || true
docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" build server-ubuntu server-alma server-alpine runner
docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" run --rm --no-deps --entrypoint sh runner \
  -c "rm -f /opt/e2e/keys/id_ed25519 /opt/e2e/keys/id_ed25519.pub && ssh-keygen -t ed25519 -N '' -f /opt/e2e/keys/id_ed25519 -q"
docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" up -d --force-recreate server-ubuntu server-alma server-alpine
docker compose -p "$PROJECT_NAME" -f "$COMPOSE_FILE" run --rm runner "$FIXTURE"
