#!/usr/bin/env bash
set -euo pipefail

arch="$1"
src="raw/bin-tako-darwin-$arch"
name="tako-darwin-$arch.tar.gz"
out="signed-dist/$name"

for var in TAKO_CODESIGN_CERTIFICATE_BASE64 TAKO_CODESIGN_IDENTITY TAKO_APPLE_TEAM_ID TAKO_KEYCHAIN_ACCESS_GROUP TAKO_PROVISION_PROFILE_BASE64; do
  if [ -z "${!var:-}" ]; then
    echo "::error::Missing secret $var"
    exit 1
  fi
done

tmp="$(mktemp -d)"
keychain="$tmp/tako-signing.keychain-db"
cert="$tmp/tako-signing.p12"
trap 'security delete-keychain "$keychain" >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT

keychain_password="$(openssl rand -hex 24)"
if printf '%s' "$TAKO_CODESIGN_CERTIFICATE_BASE64" | base64 --decode > "$cert" 2>/dev/null; then
  :
else
  printf '%s' "$TAKO_CODESIGN_CERTIFICATE_BASE64" | base64 -D > "$cert"
fi
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
if ! security import "$cert" -k "$keychain" -P "${TAKO_CODESIGN_CERTIFICATE_PASSWORD:-}" -T /usr/bin/codesign; then
  if [ -n "${TAKO_CODESIGN_CERTIFICATE_PASSWORD:-}" ]; then
    echo "Certificate import failed with TAKO_CODESIGN_CERTIFICATE_PASSWORD; retrying with an empty password"
    security import "$cert" -k "$keychain" -P "" -T /usr/bin/codesign
  else
    exit 1
  fi
fi
security list-keychains -d user -s "$keychain" login.keychain-db
security default-keychain -s "$keychain"
security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain"
security find-identity -v -p codesigning "$keychain"
export TAKO_CODESIGN_KEYCHAIN="$keychain"

export TAKO_CODESIGN_PAGESIZE=""
if [ "$arch" = "aarch64" ]; then
  TAKO_CODESIGN_PAGESIZE=16384
fi

codesign_binary() {
  if [ -n "$TAKO_CODESIGN_PAGESIZE" ]; then
    codesign --force --pagesize "$TAKO_CODESIGN_PAGESIZE" --sign "$TAKO_CODESIGN_IDENTITY" --keychain "$keychain" "$@"
  else
    codesign --force --sign "$TAKO_CODESIGN_IDENTITY" --keychain "$keychain" "$@"
  fi
}

sh scripts/package-tako-app.sh "$src/tako" "$src/Tako.app"
codesign_binary "$src/tako-dev-server"
codesign_binary "$src/tako-dev-proxy"

mkdir -p signed-dist "$tmp/archive" "$tmp/verify"
cp "$src/tako-dev-server" "$src/tako-dev-proxy" "$tmp/archive/"
ditto "$src/Tako.app" "$tmp/archive/Tako.app"
tar -czf "$out" -C "$tmp/archive" tako-dev-server tako-dev-proxy Tako.app
(cd signed-dist && shasum -a 256 "$name") > "$out.sha256"

tar -xzf "$out" -C "$tmp/verify"
codesign --verify --strict --deep --verbose=4 "$tmp/verify/Tako.app"
codesign --verify --strict --verbose=4 "$tmp/verify/Tako.app/Contents/MacOS/tako"
codesign --verify --strict --verbose=4 "$tmp/verify/tako-dev-server"
codesign --verify --strict --verbose=4 "$tmp/verify/tako-dev-proxy"
