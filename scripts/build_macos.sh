#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# SwiftUI のコンパイラプラグインを含む Xcode 本体を使う。
# PATH 上の別配布の Swift や Command Line Tools と SDK を混在させない。
if [ -z "${DEVELOPER_DIR:-}" ]; then
    DEVELOPER_DIR=$(xcode-select -p 2>/dev/null || true)
    if [ ! -x "$DEVELOPER_DIR/usr/bin/xcodebuild" ] && [ -x /Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild ]; then
        DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
    fi
fi
case "$DEVELOPER_DIR" in
    *.app) DEVELOPER_DIR="$DEVELOPER_DIR/Contents/Developer" ;;
esac
if [ ! -x "$DEVELOPER_DIR/usr/bin/xcodebuild" ]; then
    cat >&2 <<'ERROR'
DCR Monitor のビルドには Xcode 本体が必要です（Command Line Tools のみでは不足します）。
Xcode をインストールして一度起動し、初期設定を完了してください。
別の場所にインストールした場合:
  DEVELOPER_DIR=/path/to/Xcode.app/Contents/Developer scripts/build_macos.sh
ERROR
    exit 1
fi
export DEVELOPER_DIR
TOOLCHAINS=com.apple.dt.toolchain.XcodeDefault
export TOOLCHAINS
SDKROOT=$(xcrun --sdk macosx --show-sdk-path)
export SDKROOT
printf 'Xcode: %s\nSDK: %s\n' "$DEVELOPER_DIR" "$SDKROOT"
xcrun --sdk macosx swift --version
cargo build --manifest-path "$root/Cargo.toml" --lib
xcrun --sdk macosx swift build --package-path "$root/macos" --scratch-path "$root/target/swift"
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
