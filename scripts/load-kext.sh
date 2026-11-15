#!/bin/sh
#
# Build and load the read-only iSCSI kext. Requires root.
# This script never changes SIP or the startup security policy; see
# docs/kext-mount.md for the one-time manual step on Apple silicon.
#
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
BUNDLE="$ROOT/kext/build/ISCSIVirtualDisk.kext"
BUNDLE_ID="com.iscsi-rs.ISCSIVirtualDisk"

if [ "$(id -u)" -ne 0 ]; then
	echo "error: run this script with sudo" >&2
	exit 1
fi

# Build as the invoking user so artifacts stay writable without sudo.
# A previous root build leaves files owned by root; take them back first.
if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
	BUILD_USER=$SUDO_USER
	BUILD_GROUP=$(id -gn "$SUDO_USER")
	if [ -d "$ROOT/kext/build" ]; then
		chown -R "$BUILD_USER:$BUILD_GROUP" "$ROOT/kext/build"
	fi
	sudo -u "$BUILD_USER" "$ROOT/kext/build.sh"
else
	"$ROOT/kext/build.sh"
fi

if kmutil showloaded 2>/dev/null | grep -q "$BUNDLE_ID"; then
	echo "$BUNDLE_ID is already loaded"
	exit 0
fi

echo "loading $BUNDLE"
if ! kmutil load -p "$BUNDLE"; then
	echo "" >&2
	echo "load failed. On Apple silicon an ad-hoc signed kext needs:" >&2
	echo "  1. Startup Security Utility (recoveryOS): Permissive Security" >&2
	echo "  2. System Settings > Privacy & Security > Allow for $BUNDLE_ID" >&2
	echo "then run this script again." >&2
	exit 1
fi

kmutil showloaded | grep "$BUNDLE_ID" || true
echo "loaded $BUNDLE_ID"
