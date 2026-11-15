#!/bin/sh
#
# Build the read-only iSCSI kext. Produces kext/build/ISCSIVirtualDisk.kext.
# The bundle is ad-hoc signed; loading it needs a lowered startup security
# policy because this project has no Developer ID certificate.
#
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
BUILD="$HERE/build"
BUNDLE="$BUILD/ISCSIVirtualDisk.kext"

if [ "$(uname -m)" != "arm64" ]; then
	echo "error: this kext targets arm64e on Apple silicon" >&2
	exit 1
fi

SDK=$(xcrun --sdk macosx --show-sdk-path)
KERNEL_HEADERS="$SDK/System/Library/Frameworks/Kernel.framework/Headers"

if [ ! -d "$KERNEL_HEADERS" ]; then
	echo "error: Kernel.framework headers not found in $SDK" >&2
	exit 1
fi

CXXFLAGS="-arch arm64e -mkernel -std=c++17 -O2 -Wall -Wextra
	-fapple-kext -fno-exceptions -fno-rtti -fno-threadsafe-statics
	-fno-common -nostdinc++ -isysroot $SDK
	-I$KERNEL_HEADERS -I$HERE
	-DKERNEL -DKERNEL_PRIVATE -DDRIVER_PRIVATE -DAPPLE -DNeXT"

rm -rf "$BUILD"
mkdir -p "$BUNDLE/Contents/MacOS"

# shellcheck disable=SC2086
clang++ $CXXFLAGS -c "$HERE/ISCSIVirtualDisk.cpp" \
	-o "$BUILD/ISCSIVirtualDisk.o"
# shellcheck disable=SC2086
clang++ $CXXFLAGS -c "$HERE/ISCSIVirtualDiskUserClient.cpp" \
	-o "$BUILD/ISCSIVirtualDiskUserClient.o"

clang++ -arch arm64e -mkernel -nostdlib -r -Xlinker -kext \
	-o "$BUNDLE/Contents/MacOS/ISCSIVirtualDisk" \
	"$BUILD/ISCSIVirtualDisk.o" "$BUILD/ISCSIVirtualDiskUserClient.o" \
	-lkmodc++ -lkmod

cp "$HERE/Info.plist" "$BUNDLE/Contents/Info.plist"
plutil -lint "$BUNDLE/Contents/Info.plist" >/dev/null

# Files under iCloud Drive carry Finder metadata that codesign rejects.
xattr -cr "$BUNDLE"
codesign --force --sign - --timestamp=none "$BUNDLE"
codesign --verify --deep --strict "$BUNDLE"

echo "built $BUNDLE"
