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

if [[ -e "$appdir/usr/lib/libglycin-2.so.0" ]]; then
  marker="$appdir/usr/share/cutterhoochee/gtk-runtime.json"
  [[ -s "$marker" ]] || { echo "error: bundled Glycin has no runtime manifest" >&2; exit 1; }
  jq -e '
    .version == 1 and .glycinCompatVersion == "2+" and
    (.loaders | type == "array") and
    (.loaders | length == (unique | length)) and
    all(.loaders[]; type == "string" and test("^glycin-(image-rs|svg|heif|jxl)$")) and
    (.loaders | index("glycin-image-rs") != null) and
    (.loaders | index("glycin-svg") != null)
  ' "$marker" >/dev/null
  for required in \
    usr/bin/bwrap \
    usr/bin/cutterhoochee-bwrap \
    usr/share/mime/mime.cache \
    usr/lib/Cutterhoochee/resources/notices/gtk-runtime/gtk-runtime-SOURCE.txt \
    usr/lib/Cutterhoochee/resources/notices/gtk-runtime/linuxdeploy-plugin-gtk-LICENSE.txt \
    usr/lib/Cutterhoochee/resources/notices/gtk-runtime/gdk-pixbuf-LICENSE.txt \
    usr/lib/Cutterhoochee/resources/notices/gtk-runtime/glycin-LICENSE.txt \
    usr/lib/Cutterhoochee/resources/notices/gtk-runtime/libheif-LICENSE.txt \
    usr/lib/Cutterhoochee/resources/notices/gtk-runtime/bubblewrap-LICENSE.txt \
    usr/lib/Cutterhoochee/resources/notices/gtk-runtime/shared-mime-info-LICENSE.txt; do
    [[ -s "$appdir/$required" ]] || { echo "error: GTK runtime is missing $required" >&2; exit 1; }
  done
  [[ -x "$appdir/usr/bin/bwrap" ]] || { echo "error: bundled sandbox wrapper is not executable" >&2; exit 1; }
  mapfile -t gtk_loaders < <(jq -r '.loaders[]' "$marker")
  for executable in cutterhoochee-bwrap "${gtk_loaders[@]}"; do
    binary="$appdir/usr/bin/$executable"
    [[ -x "$binary" ]] || { echo "error: GTK runtime executable is missing: $executable" >&2; exit 1; }
    readelf -d "$binary" | grep -E '\((RPATH|RUNPATH)\).*[$]ORIGIN/\.\./lib(:|])' >/dev/null ||
      { echo "error: $executable cannot resolve bundled libraries inside its sandbox" >&2; exit 1; }
  done
  for loader in "${gtk_loaders[@]}"; do
    [[ -s "$appdir/usr/share/glycin-loaders/2+/conf.d/$loader.conf" ]] ||
      { echo "error: GTK runtime config is missing: $loader" >&2; exit 1; }
  done
fi

printf 'Verified %s: executable, desktop entry, GTK runtime, sidecar manifest, and source/build notices present.\n' "$expected_name"
