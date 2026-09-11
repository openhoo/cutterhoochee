#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <appimage> <version>" >&2
  exit 2
}
[[ $# -eq 2 ]] || usage
appimage=$1
version=$2
expected_name="Cutterhoochee_${version}_amd64.AppImage"
appimage=$(realpath "$1")
if [[ "$(basename "$appimage")" != "$expected_name" ]]; then
  echo "error: AppImage filename must be $expected_name" >&2
  exit 1
fi
[[ -f "$appimage" ]] || { echo "error: AppImage does not exist: $appimage" >&2; exit 1; }
[[ -x "$appimage" ]] || { echo "error: AppImage is not executable: $appimage" >&2; exit 1; }

extract_dir=$(mktemp -d "${TMPDIR:-/tmp}/cutterhoochee-appimage.XXXXXXXX")
trap 'rm -rf "$extract_dir"' EXIT
(
  cd "$extract_dir"
  "$appimage" --appimage-extract >/dev/null
)
appdir="$extract_dir/squashfs-root"
[[ -d "$appdir" ]] || { echo "error: AppImage extraction did not produce squashfs-root" >&2; exit 1; }
for required in \
  AppRun \
  usr/bin/cutterhoochee \
  usr/share/applications/Cutterhoochee.desktop \
  usr/lib/Cutterhoochee/resources/notices/sidecar-manifest.json \
  usr/lib/Cutterhoochee/resources/notices/Cutterhoochee-LICENSE.txt \
  usr/lib/Cutterhoochee/resources/notices/FFmpeg-SOURCE.txt \
  usr/lib/Cutterhoochee/resources/notices/FFmpeg-BUILD-NOTICE.txt \
  usr/lib/Cutterhoochee/resources/agent/agent/dist/main.js \
  usr/lib/Cutterhoochee/binaries/node-x86_64-unknown-linux-gnu; do
  [[ -e "$appdir/$required" ]] || { echo "error: AppImage is missing $required" >&2; exit 1; }
done
[[ -x "$appdir/AppRun" ]] || { echo "error: AppRun is not executable" >&2; exit 1; }
[[ -x "$appdir/usr/bin/cutterhoochee" ]] || { echo "error: bundled executable is not executable" >&2; exit 1; }

printf 'Verified %s: executable, desktop entry, sidecar manifest, and source/build notices present.\n' "$expected_name"
