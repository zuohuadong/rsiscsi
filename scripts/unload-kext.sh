#!/bin/sh
#
# Unload the read-only iSCSI kext. Requires root.
# Stop iscsi-blockd first so every pending request is failed instead of
# hanging while the user client disappears.
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

if pgrep -x iscsi-blockd >/dev/null 2>&1; then
	echo "error: iscsi-blockd is running; stop it first (Ctrl-C)" >&2
	exit 1
fi

if ! kmutil showloaded 2>/dev/null | grep -q "$BUNDLE_ID"; then
	echo "$BUNDLE_ID is not loaded"
	exit 0
fi

if [ -d "$BUNDLE" ]; then
	kmutil unload -p "$BUNDLE" || kextutil -u "$BUNDLE_ID"
else
	kmutil unload -b "$BUNDLE_ID"
fi

echo "unloaded $BUNDLE_ID"
