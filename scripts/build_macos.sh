#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cargo build --manifest-path "$root/Cargo.toml" --lib
swift build --package-path "$root/macos" --scratch-path "$root/target/swift"
app="$root/target/macos/DCRMonitor.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks"
cp "$root/target/debug/libdcr_transcribe.dylib" "$app/Contents/Frameworks/"
codesign --force --sign - "$app/Contents/Frameworks/libdcr_transcribe.dylib"
cp "$root/target/swift/debug/DCRMonitor" "$app/Contents/MacOS/DCRMonitor"
cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>DCRMonitor</string>
<key>CFBundleIdentifier</key><string>net.dcr-transcribe.monitor</string>
<key>CFBundleName</key><string>DCR Monitor</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSMicrophoneUsageDescription</key><string>無線機の音声を録音・文字起こしするために音声入力を使用します。</string>
</dict></plist>
PLIST
codesign --force --sign - "$app"
printf '%s\n' "$app"
