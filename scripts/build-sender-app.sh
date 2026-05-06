#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
  echo "usage: $0 <sender-binary> <version> [output-dir]" >&2
  exit 1
fi

binary_path="$1"
version="$2"
output_dir="${3:-target/macos-app}"

app_root="$output_dir/Hotswitch.app"
contents_dir="$app_root/Contents"
macos_dir="$contents_dir/MacOS"
resources_dir="$contents_dir/Resources"
icon_source="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/sender/assets/app-icon-1024.png"

rm -rf "$app_root"
mkdir -p "$macos_dir" "$resources_dir"

cp "$binary_path" "$macos_dir/hotswitch-sender"
chmod +x "$macos_dir/hotswitch-sender"

if [ -f "$icon_source" ]; then
  iconset_parent="$(mktemp -d)"
  iconset_dir="$iconset_parent/Hotswitch.iconset"
  mkdir -p "$iconset_dir"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$icon_source" --out "$iconset_dir/icon_${size}x${size}.png" >/dev/null
    retina_size=$((size * 2))
    sips -z "$retina_size" "$retina_size" "$icon_source" --out "$iconset_dir/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$iconset_dir" -o "$resources_dir/Hotswitch.icns"
  rm -rf "$iconset_parent"
fi

cat > "$contents_dir/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>Hotswitch</string>
  <key>CFBundleExecutable</key>
  <string>hotswitch-sender</string>
  <key>CFBundleIconFile</key>
  <string>Hotswitch.icns</string>
  <key>CFBundleIdentifier</key>
  <string>com.hotswitch.sender</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>Hotswitch</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${version}</string>
  <key>CFBundleVersion</key>
  <string>${version}</string>
  <key>GCSupportsGameMode</key>
  <true/>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.games</string>
  <key>LSSupportsGameMode</key>
  <true/>
  <key>LSUIElement</key>
  <true/>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
EOF

echo "created $app_root"
