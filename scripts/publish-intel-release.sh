#!/usr/bin/env bash
set -euo pipefail

# Both publishers hold the release-assets concurrency group during this check and upload.
sha="$1"
latest=$(gh api "repos/$GH_REPO/git/ref/tags/latest" --jq .object.sha)
if [ "$latest" != "$sha" ]; then
  echo "Skipping Intel upload: latest has moved to $latest"
  exit 0
fi

gh release upload latest --clobber \
  signed-dist/tako-darwin-x86_64.tar.gz \
  signed-dist/tako-darwin-x86_64.tar.gz.sha256
