#!/bin/bash
# Wrap the locally patched release build in "OpenMicro Local.app" so it can be
# launched from Spotlight and used as a login item, separate from the upstream
# /Applications/OpenMicro.app. Rebuild first:
#   cd app && cargo build --release --bin openmicro-app
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$HOME/Applications/OpenMicro Local.app"
CONTENTS="$APP/Contents"
EXE_NAME="OpenMicro Local"
BIN="$REPO_ROOT/app/target/release/openmicro-app"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/app/Cargo.toml" | head -1)"

[ -x "$BIN" ] || { echo "missing $BIN; build first" >&2; exit 1; }

pkill -f "$CONTENTS/MacOS/" 2>/dev/null || true
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "$BIN" "$CONTENTS/MacOS/$EXE_NAME"
cp "$REPO_ROOT/app/macos/OpenMicro.icns" "$CONTENTS/Resources/OpenMicro.icns"
printf 'APPL????' > "$CONTENTS/PkgInfo"

cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleDisplayName</key><string>OpenMicro Local</string>
    <key>CFBundleExecutable</key><string>$EXE_NAME</string>
    <key>CFBundleIconFile</key><string>OpenMicro</string>
    <key>CFBundleIdentifier</key><string>local.sxie.OpenMicroLocal</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>OpenMicro Local</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>11.0.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>OpenMicroAutomaticUpdates</key><false/>
</dict>
</plist>
PLIST
plutil -lint "$CONTENTS/Info.plist" >/dev/null

# Sign with the stable self-signed "OpenMicro Local Signing" identity when it
# exists, so macOS keeps Accessibility / Input Monitoring across rebuilds;
# fall back to ad-hoc (permissions then need re-granting after each build).
IDENTITY="$(security find-identity -p codesigning | sed -n 's/.*\([0-9A-F]\{40\}\) "OpenMicro Local Signing".*/\1/p' | head -1)"
codesign --force --sign "${IDENTITY:--}" --identifier local.sxie.OpenMicroLocal "$APP"
codesign --verify --strict "$APP"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP"
echo "installed $APP ($VERSION)"
