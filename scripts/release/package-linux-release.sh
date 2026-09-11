#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <version> <tag> <commit> <arch-image> <arch-snapshot>" >&2
  exit 2
}
[[ $# -eq 5 ]] || usage
version=$1
tag=$2
commit=$3
arch_image=$4
arch_snapshot=$5

root=$(git rev-parse --show-toplevel)
cd "$root"

if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: invalid release version: $version" >&2
  exit 2
fi
if [[ "$tag" != "v$version" ]]; then
  echo "error: release tag $tag does not match version $version" >&2
  exit 2
fi
if [[ ! "$commit" =~ ^[0-9a-f]{40}$ ]]; then
  echo "error: release commit is not a full SHA: $commit" >&2
  exit 2
fi
[[ "$(git rev-parse HEAD)" == "$commit" ]] || { echo "error: checkout is not the requested release commit" >&2; exit 1; }
tag_commit=$(git rev-parse --verify "refs/tags/$tag^{commit}")
[[ "$tag_commit" == "$commit" ]] || { echo "error: release tag does not identify the requested commit" >&2; exit 1; }
[[ "$(tr -d '[:space:]' < VERSION)" == "$version" ]] || { echo "error: VERSION does not match release version" >&2; exit 1; }
node scripts/release/sync-version.mjs --check
[[ -z "$(git status --porcelain --untracked-files=normal)" ]] || { echo "error: release source tree is not clean" >&2; exit 1; }

manifest=src-tauri/resources/notices/sidecar-manifest.json
[[ -s "$manifest" ]] || { echo "error: missing generated sidecar manifest" >&2; exit 1; }
jq -e --arg target x86_64-unknown-linux-gnu '.status == "ready" and .target == $target and .requirements.noPathFallback == true and .requirements.releaseRequiresAllBinaryNotices == true' "$manifest" >/dev/null
cmp "$manifest" sidecars/manifest.json

appimage="src-tauri/target/release/bundle/appimage/Cutterhoochee_${version}_amd64.AppImage"
[[ -f "$appimage" ]] || { echo "error: exact versioned AppImage is missing: $appimage" >&2; exit 1; }
[[ -x "$appimage" ]] || { echo "error: exact versioned AppImage is not executable" >&2; exit 1; }
scripts/release/verify-appimage.sh "$appimage" "$version"
appimage=$(realpath "$appimage")

# Packaging adds GTK runtime notices after sidecar preparation. Publish the
# notices from the actual installer rather than an earlier source staging tree.
tmp_notices=$(mktemp -d "${TMPDIR:-/tmp}/cutterhoochee-notices.XXXXXXXX")
trap 'rm -rf "$tmp_notices"' EXIT
(
  cd "$tmp_notices"
  "$appimage" --appimage-extract usr/lib/Cutterhoochee/resources/notices >/dev/null
)
bundled_notices="$tmp_notices/squashfs-root/usr/lib/Cutterhoochee/resources/notices"
[[ -d "$bundled_notices" ]] || { echo "error: bundled release notices are missing" >&2; exit 1; }
cmp "$manifest" "$bundled_notices/sidecar-manifest.json"

# dist is this script's sole output directory; never reuse a cached release bundle.
rm -rf dist
mkdir -p dist
package="cutterhoochee-${version}-x86_64-unknown-linux-gnu"
stage="dist/$package"
mkdir -p "$stage/notices"
install -m 0755 "$appimage" "$stage/Cutterhoochee_${version}_amd64.AppImage"
install -m 0644 LICENSE "$stage/LICENSE"
cp -a "$bundled_notices/." "$stage/notices/"
install -m 0644 sidecars/manifest.json "$stage/notices/sidecar-manifest.json"

appimage_sha256=$(sha256sum "$appimage" | awk '{ print $1 }')
manifest_sha256=$(sha256sum "$manifest" | awk '{ print $1 }')
cat > "$stage/SOURCE-IDENTITY.txt" <<EOF
project=openhoo/cutterhoochee
version=$version
tag=$tag
commit=$commit
target=x86_64-unknown-linux-gnu
arch_image=$arch_image
arch_snapshot=$arch_snapshot
ffmpeg_package=$(pacman -Q ffmpeg)
x264_package=$(pacman -Q x264)
gdk_pixbuf_package=$(pacman -Q gdk-pixbuf2)
glycin_package=$(pacman -Q glycin)
libheif_package=$(pacman -Q libheif)
bubblewrap_package=$(pacman -Q bubblewrap)
shared_mime_info_package=$(pacman -Q shared-mime-info)
sidecar_manifest_sha256=$manifest_sha256
appimage_sha256=$appimage_sha256
EOF

archive="dist/${package}.tar.gz"
tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner --format=posix --pax-option=delete=atime,delete=ctime -C dist -cf "${archive%.gz}" "$package"
gzip -n -f "${archive%.gz}"
rm -rf "$stage"

install -m 0755 "$appimage" "dist/Cutterhoochee_${version}_amd64.AppImage"
install -m 0644 LICENSE "dist/Cutterhoochee_${version}_LICENSE"
# Keep a standalone identity file and a deterministic notices archive beside the installer.
tar -xOzf "dist/${package}.tar.gz" "$package/SOURCE-IDENTITY.txt" > "dist/Cutterhoochee_${version}_SOURCE-IDENTITY.txt"
notices_archive="dist/Cutterhoochee_${version}_notices.tar.gz"
cp -a "$bundled_notices" "$tmp_notices/"
install -m 0644 sidecars/manifest.json "$tmp_notices/notices/sidecar-manifest.json"
tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner --format=posix --pax-option=delete=atime,delete=ctime -C "$tmp_notices" -cf "${notices_archive%.gz}" notices
gzip -n -f "${notices_archive%.gz}"

printf 'Packaged %s and exact AppImage %s.\n' "$archive" "dist/Cutterhoochee_${version}_amd64.AppImage"
