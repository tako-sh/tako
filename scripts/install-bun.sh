#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/install-bun.sh <version>" >&2
  exit 2
fi

version="$1"
if [[ "$version" != bun-v* ]]; then
  version="bun-v${version}"
fi

attempts=4
for attempt in $(seq 1 "$attempts"); do
  installer="$(mktemp)"
  if curl -fsSL --retry 5 --retry-delay 2 --retry-all-errors https://bun.sh/install -o "$installer" && bash "$installer" "$version"; then
    rm -f "$installer"
    if [ -n "${GITHUB_PATH:-}" ]; then
      echo "$HOME/.bun/bin" >> "$GITHUB_PATH"
    fi
    exit 0
  fi

  rm -f "$installer"
  if [ "$attempt" -lt "$attempts" ]; then
    sleep "$((attempt * 5))"
  fi
done

echo "failed to install Bun ${version} after ${attempts} attempts" >&2
exit 1
