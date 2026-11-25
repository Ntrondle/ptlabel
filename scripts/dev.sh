#!/bin/bash
# Build & launch PTLabel Dev.app (yellow icon, separate PTLabel-dev profile, live dev-ui.json).
#   --install  also copy to ~/Applications/PTLabel Dev.app
#   --no-run   build the bundle without opening it
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ ! -f assets/PTLabel-Dev.icns || ! -f assets/icon-dev.png ]]; then
  python3 scripts/make_icon.py
fi

cargo build --features dev
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP="target/PTLabel Dev.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/debug/ptlabel "$APP/Contents/MacOS/PTLabelDev"
cp assets/PTLabel-Dev.icns "$APP/Contents/Resources/PTLabel-Dev.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>PTLabel Dev</string>
  <key>CFBundleDisplayName</key><string>PTLabel Dev</string>
  <key>CFBundleIdentifier</key><string>local.ptlabel.dev</string>
  <key>CFBundleExecutable</key><string>PTLabelDev</string>
  <key>CFBundleIconFile</key><string>PTLabel-Dev</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION-dev</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>PTLabel Dev finds and prints to P-touch label printers on your network.</string>
  <key>NSBonjourServices</key>
  <array><string>_pdl-datastream._tcp</string></array>
  <key>NSBluetoothAlwaysUsageDescription</key>
  <string>PTLabel Dev prints to Bluetooth P-touch label printers.</string>
</dict>
</plist>
EOF

codesign --force --deep -s - "$APP" >/dev/null
echo "built $APP"

DEST="$APP"
if [[ " $* " == *" --install "* ]]; then
  mkdir -p ~/Applications
  rm -rf "$HOME/Applications/PTLabel Dev.app"
  cp -R "$APP" "$HOME/Applications/"
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$HOME/Applications/PTLabel Dev.app" || true
  DEST="$HOME/Applications/PTLabel Dev.app"
  echo "installed $DEST"
fi

if [[ " $* " != *" --no-run "* ]]; then
  pkill -x PTLabelDev 2>/dev/null || true
  sleep 0.2
  open "$DEST"
  echo "launched $DEST (live UI config: $(pwd)/dev-ui.json)"
fi
