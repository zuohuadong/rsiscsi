#!/bin/sh
#
# Publish release artifacts to CTFile through its REST API.
#
# Unlike WebDAV this needs only a session token, and it creates missing
# folders. It mirrors the flow used by qnap-pkg-sync:
#   1. POST folder/list and folder/create to resolve the target folder
#   2. POST file/upload to obtain a signed upload URL
#   3. POST the file as multipart/form-data to that URL
#
# Required environment:
#   CTFILE_SESSION    session token from a logged-in browser session
#   CTFILE_FOLDER_ID  numeric id of the parent folder (0 is the drive root)
#   DMG               path to the built disk image
# Optional:
#   CTFILE_TARGET_DIR folder path below the parent (default: 软件集合/rsiscsi)
#   VERSION           release version used as a subfolder (default: from DMG name)
#   CURL_BIN          curl executable, overridable for tests
set -eu

API=${CTFILE_API:-https://rest.ctfile.com/v1/public}
CURL=${CURL_BIN:-curl}
SESSION=${CTFILE_SESSION:?CTFILE_SESSION is required}
PARENT=${CTFILE_FOLDER_ID:?CTFILE_FOLDER_ID is required}
TARGET=${CTFILE_TARGET_DIR:-软件集合/rsiscsi}
DMG=${DMG:?DMG is required}
VERSION=${VERSION:-$(basename "$DMG" | sed -e 's/^rsiscsi-//' -e 's/-arm64\.dmg$//')}

folder_key() {
	case "$1" in
		0) echo 0 ;;
		*) echo "d${1#d}" ;;
	esac
}

json_request() {
	endpoint=$1
	shift
	body=$(SESSION="$SESSION" python3 - "$@" <<'PY'
import json, os, sys

body = {"session": os.environ["SESSION"]}
for pair in sys.argv[1:]:
    key, _, value = pair.partition("=")
    body[key] = value
print(json.dumps(body))
PY
)
	"$CURL" --silent --show-error --fail-with-body \
		--header 'Content-Type: application/json' \
		--data "$body" \
		"$API/$endpoint"
}

# Print "id<TAB>name" for every child folder of the given folder id.
list_folders() {
	json_request folder/list "folder_id=$(folder_key "$1")" page=1 page_size=200 |
		python3 -c '
import json, sys

data = json.load(sys.stdin)
if str(data.get("code")) != "200":
    sys.exit("folder/list failed with code %s" % data.get("code"))
for row in data.get("results") or data.get("data") or []:
    key = str(row.get("key") or row.get("folder_id") or row.get("id") or "")
    name = str(row.get("name") or row.get("folder_name") or "")
    if key.startswith("d") or row.get("icon") == "folder":
        print("%s\t%s" % (key.lstrip("d"), name))
'
}

# Print "id<TAB>name" for every file in the given folder id.
list_files() {
	json_request file/list "folder_id=$(folder_key "$1")" page=1 page_size=200 |
		python3 -c '
import json, sys

data = json.load(sys.stdin)
if str(data.get("code")) != "200":
    sys.exit("file/list failed with code %s" % data.get("code"))
for row in data.get("results") or data.get("data") or []:
    key = str(row.get("key") or row.get("file_id") or row.get("id") or "")
    name = str(row.get("name") or row.get("file_name") or "")
    if not key.startswith("d"):
        print("%s\t%s" % (key.lstrip("f"), name))
'
}

child_id() {
	list_folders "$1" | awk -F'\t' -v want="$2" '$2 == want { print $1; exit }'
}

folder=$PARENT
rest=$TARGET
while [ -n "$rest" ]; do
	case "$rest" in
		*/*) segment=${rest%%/*}; rest=${rest#*/} ;;
		*) segment=$rest; rest="" ;;
	esac
	[ -n "$segment" ] || continue
	id=$(child_id "$folder" "$segment")
	if [ -z "$id" ]; then
		echo "creating folder $segment"
		json_request folder/create "folder_id=$(folder_key "$folder")" "name=$segment" >/dev/null
		id=$(child_id "$folder" "$segment")
	fi
	[ -n "$id" ] || { echo "cannot create folder $segment" >&2; exit 1; }
	folder=$id
done

id=$(child_id "$folder" "$VERSION")
if [ -z "$id" ]; then
	echo "creating folder $VERSION"
	json_request folder/create "folder_id=$(folder_key "$folder")" "name=$VERSION" >/dev/null
	id=$(child_id "$folder" "$VERSION")
fi
[ -n "$id" ] || { echo "cannot create folder $VERSION" >&2; exit 1; }

for file in "$DMG" "$DMG.sha256"; do
	[ -f "$file" ] || { echo "missing $file" >&2; exit 1; }
	name=$(basename "$file")
	size=$(wc -c <"$file" | tr -d ' ')
	checksum=$(md5 -q "$file")

	response=$(json_request file/upload \
		"folder_id=$(folder_key "$id")" "name=$name" "size=$size" "checksum=$checksum")
	upload_url=$(printf '%s' "$response" | python3 -c '
import json, sys
print(json.load(sys.stdin).get("upload_url") or "")
')
	[ -n "$upload_url" ] || { echo "file/upload returned no upload_url: $response" >&2; exit 1; }

	echo "uploading $name ($size bytes)"
	"$CURL" --silent --show-error --fail-with-body \
		--form-string "name=$name" \
		--form-string "filesize=$size" \
		--form "file=@$file;filename=$name;type=application/octet-stream" \
		"$upload_url" >/dev/null

	list_files "$id" | awk -F'\t' -v want="$name" '$2 == want { found = 1 } END { exit !found }' ||
		{ echo "upload verification failed for $name" >&2; exit 1; }
done

echo "published $VERSION to $TARGET/$VERSION"
