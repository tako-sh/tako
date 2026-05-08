#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <tako-binary> <output-app-path>" >&2
  exit 1
fi

binary="$1"
app="$2"
bundle_id="${TAKO_BUNDLE_ID:-sh.tako.Tako}"
bundle_name="${TAKO_BUNDLE_NAME:-Tako}"
codesign_identity="${TAKO_CODESIGN_IDENTITY:-}"
codesign_keychain="${TAKO_CODESIGN_KEYCHAIN:-}"
codesign_pagesize="${TAKO_CODESIGN_PAGESIZE:-}"
team_id="${TAKO_APPLE_TEAM_ID:-}"
keychain_group="${TAKO_KEYCHAIN_ACCESS_GROUP:-}"
profile_base64="${TAKO_PROVISION_PROFILE_BASE64:-}"
profile_path="${TAKO_PROVISION_PROFILE_PATH:-}"

if [ ! -f "$binary" ]; then
  echo "error: binary not found: $binary" >&2
  exit 1
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS"

cp "$binary" "$app/Contents/MacOS/tako"
chmod 0755 "$app/Contents/MacOS/tako"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>tako</string>
  <key>CFBundleIdentifier</key>
  <string>$bundle_id</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>$bundle_name</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.0.0</string>
  <key>CFBundleVersion</key>
  <string>0</string>
</dict>
</plist>
EOF

if [ -n "$profile_path" ]; then
  cp "$profile_path" "$app/Contents/embedded.provisionprofile"
elif [ -n "$profile_base64" ]; then
  if printf '%s' "$profile_base64" | base64 --decode > "$app/Contents/embedded.provisionprofile" 2>/dev/null; then
    :
  else
    printf '%s' "$profile_base64" | base64 -D > "$app/Contents/embedded.provisionprofile"
  fi
fi

if [ -n "$codesign_identity" ]; then
  codesign_app() {
    if [ -n "$codesign_keychain" ]; then
      if [ -n "$codesign_pagesize" ]; then
        codesign --force --pagesize "$codesign_pagesize" --sign "$codesign_identity" --keychain "$codesign_keychain" "$@"
      else
        codesign --force --sign "$codesign_identity" --keychain "$codesign_keychain" "$@"
      fi
    else
      if [ -n "$codesign_pagesize" ]; then
        codesign --force --pagesize "$codesign_pagesize" --sign "$codesign_identity" "$@"
      else
        codesign --force --sign "$codesign_identity" "$@"
      fi
    fi
  }

  entitlements="$(mktemp)"
  trap 'rm -f "$entitlements"' EXIT

  if [ -n "$team_id" ]; then
    if [ -z "$keychain_group" ]; then
      keychain_group="$team_id.$bundle_id"
    fi
    cat > "$entitlements" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.application-identifier</key>
  <string>$team_id.$bundle_id</string>
  <key>com.apple.developer.team-identifier</key>
  <string>$team_id</string>
  <key>keychain-access-groups</key>
  <array>
    <string>$keychain_group</string>
  </array>
</dict>
</plist>
EOF
    codesign_app --generate-entitlement-der --entitlements "$entitlements" "$app"
  else
    codesign_app "$app"
    echo "warning: signed Tako.app without keychain entitlements; iCloud Keychain will be unavailable." >&2
  fi
else
  echo "warning: Tako.app was packaged without code signing; iCloud Keychain will be unavailable." >&2
fi
