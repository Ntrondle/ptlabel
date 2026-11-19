#!/bin/bash
# Build PTLabel.app (release, ad-hoc signed).
#   --install  copy it to ~/Applications
#   --dmg      also make target/PTLabel.dmg (LZMA-compressed, drag-to-Applications)
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP=target/PTLabel.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/ptlabel "$APP/Contents/MacOS/PTLabel"
cp assets/PTLabel.icns "$APP/Contents/Resources/PTLabel.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>PTLabel</string>
  <key>CFBundleDisplayName</key><string>PTLabel</string>
  <key>CFBundleIdentifier</key><string>local.ptlabel</string>
  <key>CFBundleExecutable</key><string>PTLabel</string>
  <key>CFBundleIconFile</key><string>PTLabel</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>PTLabel finds and prints to P-touch label printers on your network.</string>
  <key>NSBonjourServices</key>
  <array><string>_pdl-datastream._tcp</string></array>
  <key>NSBluetoothAlwaysUsageDescription</key>
  <string>PTLabel prints to Bluetooth P-touch label printers.</string>
</dict>
</plist>
EOF

codesign --force --deep -s - "$APP" >/dev/null
echo "built $APP"

if [[ " $* " == *" --dmg "* ]]; then
  STAGE=$(mktemp -d)
  cp -R "$APP" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  # hdiutil is occasionally "Resource busy" on CI runners: retry a few times.
  for i in 1 2 3 4 5; do
    hdiutil create -quiet -ov -volname PTLabel -srcfolder "$STAGE" -fs HFS+ -format ULMO target/PTLabel.dmg && break
    [[ $i == 5 ]] && exit 1
    sleep 3
  done
  rm -rf "$STAGE"
  echo "built target/PTLabel.dmg ($(du -k target/PTLabel.dmg | cut -f1) KB)"
fi

if [[ " $* " == *" --install "* ]]; then
  mkdir -p ~/Applications
  rm -rf ~/Applications/PTLabel.app
  cp -R "$APP" ~/Applications/
  # refresh Finder/Dock icon cache for the new bundle
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f ~/Applications/PTLabel.app || true
  echo "installed ~/Applications/PTLabel.app"
fi
