#!/bin/sh
#
# Build the DMG and upload it to the CTFile drive from a machine in mainland
# China. CI cannot publish: CTFile answers "Access denied: Login from different
# location detected" to WebDAV logins from foreign IPs.
#
# Credentials come from .env.release (git-ignored) or the environment; see
# .env.release.example.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

if [ -f .env.release ]; then
	set -a
	# shellcheck disable=SC1091
	. ./.env.release
	set +a
fi

: "${WEBDAV_URL:?set WEBDAV_URL in .env.release, see .env.release.example}"
: "${WEBDAV_USERNAME:?set WEBDAV_USERNAME in .env.release}"
: "${WEBDAV_PASSWORD:?set WEBDAV_PASSWORD in .env.release}"

./scripts/package-dmg.sh

dmg=$(ls -t dist/rsiscsi-*-arm64.dmg | head -n 1)
[ -n "$dmg" ] || { echo "no DMG produced" >&2; exit 1; }
VERSION=${VERSION:-$(basename "$dmg" | sed -e 's/^rsiscsi-//' -e 's/-arm64\.dmg$//')}
export VERSION
DMG=$dmg
export DMG

./scripts/publish-webdav.sh
