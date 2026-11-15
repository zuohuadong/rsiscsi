#!/bin/bash
# Build a DMG for the user-space iSCSI app.
# No DriverKit extension, SIP changes, or system settings changes are required.
# Build under /tmp: Documents may be synced by iCloud, and extended attributes
# such as FinderInfo cause codesign to reject signing.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP_NAME="iSCSI"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
ARCH="$(uname -m)"
DIST="$ROOT/dist"
DMG="$DIST/rsiscsi-$VERSION-$ARCH.dmg"

export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p iscsi-gui

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
APP="$WORK/$APP_NAME.app"
STAGE="$WORK/stage"

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$STAGE"
cp target/release/iscsi-gui "$APP/Contents/MacOS/iscsi-gui"
sed "s/@VERSION@/$VERSION/g" packaging/Info.plist > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
xattr -cr "$APP"

# Ad-hoc signing needs no developer account; users can right-click to open on first launch.
codesign --force --sign - "$APP"
codesign --verify --strict --verbose=2 "$APP"

cp -R "$APP" "$STAGE/"
cp packaging/使用说明.txt "$STAGE/使用说明.txt"
ln -s /Applications "$STAGE/Applications"

mkdir -p "$DIST"
rm -f "$DMG"
hdiutil create -quiet -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
shasum -a 256 "$DMG"
echo "已生成 $DMG"
