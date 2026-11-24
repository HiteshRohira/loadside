#!/bin/bash
# Build Loadside.app (SwiftUI UI + Rust daemon + CLI) into dist/.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')"
APP="$ROOT/dist/Loadside.app"

echo "==> Building Rust daemon + CLI (release)"
cargo build --release -p loadsided -p loadside-cli

echo "==> Building SwiftUI app (release)"
swift build -c release --package-path app
SWIFT_BIN="$(swift build -c release --package-path app --show-bin-path)/Loadside"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$SWIFT_BIN" "$APP/Contents/MacOS/Loadside"
cp target/release/loadsided "$APP/Contents/MacOS/loadsided"
# Named loadside-cli: on case-insensitive APFS "loadside" would clobber "Loadside".
cp target/release/loadside "$APP/Contents/MacOS/loadside-cli"

# The app icon is pre-rendered from docs/icons/src (scripts/render-icon.sh).
cp "$ROOT/assets/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
cp "$ROOT/assets/Assets.car" "$APP/Contents/Resources/Assets.car"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Loadside</string>
    <key>CFBundleDisplayName</key><string>Loadside</string>
    <key>CFBundleIdentifier</key><string>dev.loadside.app</string>
    <key>CFBundleExecutable</key><string>Loadside</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundleIconName</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>14.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>iOS App Archive</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key><array><string>com.apple.itunes.ipa</string></array>
            <key>CFBundleTypeExtensions</key><array><string>ipa</string></array>
        </dict>
    </array>
</dict>
</plist>
PLIST

# Keychain remembers "Always Allow" per code identity. An ad-hoc signature
# changes with every build, so each rebuild would ask again for every item
# (password, anisette identity, signing key). A real certificate keeps the
# identity stable. Override with LOADSIDE_SIGN_IDENTITY ("-" for ad-hoc).
SIGN_ID="${LOADSIDE_SIGN_IDENTITY:-}"
if [ -z "$SIGN_ID" ]; then
    SIGN_ID=$(security find-identity -v -p codesigning 2>/dev/null \
        | grep -E '"(Developer ID Application|Apple Development|Mac Developer):' \
        | head -1 | awk '{print $2}')
fi
if [ -z "$SIGN_ID" ] || [ "$SIGN_ID" = "-" ]; then
    SIGN_ID="-"
    echo "==> Ad-hoc signing (no signing certificate found; Keychain will ask again after each rebuild)"
else
    echo "==> Signing with certificate ${SIGN_ID:0:8}…"
fi
codesign --force --timestamp=none --sign "$SIGN_ID" --identifier dev.loadside.daemon "$APP/Contents/MacOS/loadsided"
codesign --force --timestamp=none --sign "$SIGN_ID" --identifier dev.loadside.cli "$APP/Contents/MacOS/loadside-cli"
codesign --force --timestamp=none --sign "$SIGN_ID" "$APP"

echo "==> Done: $APP"
du -sh "$APP" | awk '{print "    size: "$1}'
