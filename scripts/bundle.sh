#!/usr/bin/env bash
# Builds sepia and packages it as a real macOS .app bundle at
# target/Sepia.app, ad-hoc code-signed. Needed for anything that checks
# the running process's bundle identity — e.g. QuickLook's thumbnail
# generation XPC service, which silently slow-paths/ignores requests
# from a bare, unsigned, bundle-less binary.
#
# Usage: scripts/bundle.sh [release] [--install]
#   release    build with `cargo build --release` instead of the debug profile
#   --install  copy the finished bundle to /Applications/Sepia.app afterward
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="debug"
INSTALL=0
for arg in "$@"; do
    case "$arg" in
        release) PROFILE="release" ;;
        --install) INSTALL=1 ;;
        *)
            echo "Unknown argument: $arg" >&2
            echo "Usage: scripts/bundle.sh [release] [--install]" >&2
            exit 1
            ;;
    esac
done

if [ "$PROFILE" = "release" ]; then
    CARGO_FLAGS=(--release)
    BIN_DIR="target/release"
else
    CARGO_FLAGS=()
    BIN_DIR="target/debug"
fi

cargo build "${CARGO_FLAGS[@]}"

APP="target/Sepia.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN_DIR/sepia" "$APP/Contents/MacOS/sepia"

# App icon: rasterize app/icon.svg into a proper .icns. Rebuilt every
# time (rsvg-convert on 10 small PNGs is well under a second) rather
# than cached, so editing the SVG always takes effect on the next build.
ICONSET="target/Sepia.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for spec in "16 icon_16x16" "32 icon_16x16@2x" "32 icon_32x32" "64 icon_32x32@2x" \
            "128 icon_128x128" "256 icon_128x128@2x" "256 icon_256x256" \
            "512 icon_256x256@2x" "512 icon_512x512" "1024 icon_512x512@2x"; do
    set -- $spec
    rsvg-convert -w "$1" -h "$1" app/icon.svg -o "$ICONSET/$2.png"
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

cat >"$APP/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>sepia</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.markzz.sepia</string>
    <key>CFBundleName</key>
    <string>Sepia</string>
    <key>CFBundleDisplayName</key>
    <string>Sepia</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

# Prefer a real "Apple Development" identity (free via Xcode + an Apple
# ID, no paid membership needed) over ad-hoc signing — ad-hoc alone
# wasn't enough to get full-speed QuickLook thumbnail generation, which
# apparently wants an actual Team ID, not just any signature.
IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null | grep -m1 "Apple Development" | sed -E 's/.*"(.*)"/\1/')
if [ -z "$IDENTITY" ]; then
    echo "No 'Apple Development' identity found, falling back to ad-hoc signing." >&2
    IDENTITY="-"
fi
codesign --force --deep --sign "$IDENTITY" "$APP"

echo "Built $APP (signed: $IDENTITY)"

if [ "$INSTALL" = "1" ]; then
    # A running copy at the destination would otherwise make the `cp -R`
    # below fail/produce a broken bundle (files locked, partial overwrite).
    pkill -f "/Applications/Sepia.app/Contents/MacOS/sepia" 2>/dev/null || true
    rm -rf /Applications/Sepia.app
    cp -R "$APP" /Applications/Sepia.app
    echo "Installed /Applications/Sepia.app"
fi
