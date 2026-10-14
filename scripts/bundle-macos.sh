#!/bin/sh
# Build xodus-ui + xodus-cli + xodus-service and wrap them in target/Xodus.app (macOS only).
set -eu
cd "$(dirname "$0")/.."

cargo build --release -p xodus-ui -p xodus-cli -p xodus-service

app=target/Xodus.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp target/release/xodus-ui target/release/xodus-cli target/release/xodus-service "$app/Contents/MacOS/"
cat > "$app/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Xodus</string>
  <key>CFBundleIdentifier</key><string>dev.xodus.ui</string>
  <key>CFBundleExecutable</key><string>xodus-ui</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
# Ad-hoc sign with fixed identifiers. Don't use an "Apple Development" identity:
# Gatekeeper rejects non-notarized dev-signed apps as possible malware, while
# locally built ad-hoc apps open fine. Trade-off: the keychain asks again
# (choose "Always Allow") after each rebuild, since ad-hoc ACLs pin the cdhash.
codesign --force --sign - --identifier dev.xodus.cli "$app/Contents/MacOS/xodus-cli"
codesign --force --sign - --identifier dev.xodus.service "$app/Contents/MacOS/xodus-service"
codesign --force --sign - --identifier dev.xodus.ui "$app"
echo "built $app"
