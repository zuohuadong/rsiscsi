#!/bin/sh
#
# Publish release artifacts over WebDAV. Fallback for CTFile accounts where
# the REST session token is not available.
#
# Required environment:
#   WEBDAV_URL       server root, for example https://webdav.ctfile.com/ws35579
#   WEBDAV_USERNAME  WebDAV user
#   WEBDAV_PASSWORD  WebDAV password (an app password when 2FA is enabled)
#   DMG              path to the built disk image
# Optional:
#   WEBDAV_ROOT_PATH         folder below the server root (default: /qnaporg-github)
#   SOFTWARE_COLLECTION_DIR  target folder (default: 软件集合/rsiscsi);
#                            a leading / ignores WEBDAV_ROOT_PATH
#   VERSION                  release version (default: from DMG name)
#   CURL_BIN                 curl executable, overridable for tests
set -eu

CURL=${CURL_BIN:-curl}
BASE=${WEBDAV_URL:?WEBDAV_URL is required}
BASE=${BASE%/}
USER=${WEBDAV_USERNAME:?WEBDAV_USERNAME is required}
PASS=${WEBDAV_PASSWORD:?WEBDAV_PASSWORD is required}
ROOT=${WEBDAV_ROOT_PATH:-/qnaporg-github}
TARGET=${SOFTWARE_COLLECTION_DIR:-软件集合/rsiscsi}
DMG=${DMG:?DMG is required}
VERSION=${VERSION:-$(basename "$DMG" | sed -e 's/^rsiscsi-//' -e 's/-arm64\.dmg$//')}

# Percent-encode every path segment; the target folder name is Chinese.
encode() {
	python3 -c 'import sys,urllib.parse;print("/".join(urllib.parse.quote(p) for p in sys.argv[1].split("/") if p))' "$1"
}

# An absolute TARGET ignores WEBDAV_ROOT_PATH, e.g. /软件集合/rsiscsi
# when the folder sits directly under the drive root.
case "$TARGET" in
	/*) base_path=${TARGET#/} ;;
	*) base_path="${ROOT#/}/$TARGET" ;;
esac
target_path="${base_path%/}/$VERSION"

# Create parent folders one level at a time; 405 means it already exists.
rest=$target_path
prefix=""
while [ -n "$rest" ]; do
	case "$rest" in
		*/*) part=${rest%%/*}; rest=${rest#*/} ;;
		*) part=$rest; rest="" ;;
	esac
	prefix="$prefix/$part"
	code=$("$CURL" --silent --show-error --request MKCOL \
		--user "$USER:$PASS" \
		--output /dev/null --write-out '%{http_code}' \
		"$BASE$(encode "$prefix")" || true)
	case "$code" in
		201|405|301|302) ;;
		*) echo "MKCOL $prefix failed with HTTP $code" >&2; exit 1 ;;
	esac
done

for file in "$DMG" "$DMG.sha256"; do
	[ -f "$file" ] || { echo "missing $file" >&2; exit 1; }
	name=$(basename "$file")
	echo "uploading $name"
	"$CURL" --silent --show-error --fail-with-body \
		--upload-file "$file" \
		--user "$USER:$PASS" \
		"$BASE/$(encode "$target_path/$name")"
	echo
done

echo "published $VERSION to $target_path"
