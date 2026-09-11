#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <artifact-dir> <version> <commit> <run-id> <artifact-id> <artifact-name>" >&2
  exit 2
}
[[ $# -eq 6 ]] || usage
artifact_dir=$1
version=$2
commit=$3
run_id=$4
artifact_id=$5
artifact_name=$6

: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"

[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "error: commit must be a full SHA" >&2; exit 2; }
[[ "$run_id" =~ ^[1-9][0-9]*$ ]] || { echo "error: CI run ID is invalid" >&2; exit 2; }
[[ "$artifact_id" =~ ^[1-9][0-9]*$ ]] || { echo "error: CI artifact ID is invalid" >&2; exit 2; }
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] || {
  echo "error: invalid release version: $version" >&2
  exit 2
}

expected_artifact_name="cutterhoochee-ci-appimage-$commit"
[[ "$artifact_name" == "$expected_artifact_name" ]] || {
  echo "error: CI artifact name is not bound to release commit" >&2
  exit 1
}
[[ -d "$artifact_dir" ]] || { echo "error: CI artifact directory is missing: $artifact_dir" >&2; exit 1; }
artifact_dir=$(realpath "$artifact_dir")
appimage_name="Cutterhoochee_${version}_amd64.AppImage"
appimage="$artifact_dir/$appimage_name"
metadata="$artifact_dir/cutterhoochee-ci-appimage-metadata.json"

mapfile -t entries < <(find "$artifact_dir" -mindepth 1 -maxdepth 1 -printf '%f\n')
[[ "${#entries[@]}" -eq 2 ]] || {
  echo "error: CI artifact must contain exactly the AppImage and its metadata" >&2
  exit 1
}
[[ -f "$appimage" ]] || { echo "error: downloaded CI AppImage is missing: $appimage" >&2; exit 1; }
[[ -s "$metadata" ]] || { echo "error: downloaded CI artifact metadata is missing" >&2; exit 1; }

appimage_sha256=$(sha256sum "$appimage" | awk '{ print $1 }')
jq -e \
  --arg repository "$GITHUB_REPOSITORY" \
  --arg commit "$commit" \
  --arg run_id "$run_id" \
  --arg artifact_name "$artifact_name" \
  --arg appimage_name "$appimage_name" \
  --arg version "$version" \
  --arg appimage_sha256 "$appimage_sha256" \
  'type == "object" and
   .schema_version == 1 and
   .repository == $repository and
   .workflow == "CI" and
   .workflow_file == ".github/workflows/ci.yml" and
   .event == "push" and
   .head_branch == "main" and
   .head_sha == $commit and
   ((.run_id | tostring) == $run_id) and
   .artifact_name == $artifact_name and
   .version == $version and
   .target == "x86_64-unknown-linux-gnu" and
   .appimage_name == $appimage_name and
   .appimage_sha256 == $appimage_sha256' \
  "$metadata" >/dev/null

# GitHub artifact downloads normalize files to mode 0644.
chmod 0755 "$appimage"

printf 'Verified CI artifact %s from run %s (artifact %s) for %s.\n' \
  "$artifact_name" "$run_id" "$artifact_id" "$commit"
