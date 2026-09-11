#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <version> <tag> [base-branch]" >&2
  exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage
version=$1
tag=$2
base_branch=${3:-main}

if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: invalid release version: $version" >&2
  exit 2
fi
if [[ "$tag" != "v$version" ]]; then
  echo "error: release tag $tag does not match version $version" >&2
  exit 2
fi
if [[ ! "$base_branch" =~ ^[A-Za-z0-9._/-]+$ ]]; then
  echo "error: invalid base branch: $base_branch" >&2
  exit 2
fi

branch="release/$tag"
remote_head=$(git ls-remote --heads origin "refs/heads/$branch" | awk 'NR == 1 { print $1 }')
if [[ ! "$remote_head" =~ ^[0-9a-f]{40}$ ]]; then
  echo "error: release branch $branch is not present on origin" >&2
  exit 1
fi
git fetch --no-tags origin "refs/heads/$branch:refs/remotes/origin/$branch"
release_commit=$(git rev-parse "refs/remotes/origin/$branch")
[[ "$release_commit" == "$remote_head" ]] || { echo "error: release branch moved while preparing PR" >&2; exit 1; }
release_subject=$(git show -s --format=%s "$release_commit")
release_body=$(git show -s --format=%b "$release_commit")
[[ "$release_subject" == "chore(release): cutterhoochee $version" ]] || {
  echo "error: unexpected Hooversion release subject: $release_subject" >&2
  exit 1
}

existing_url=$(gh pr list \
  --repo "$GITHUB_REPOSITORY" \
  --base "$base_branch" \
  --head "$branch" \
  --state open \
  --json url \
  --jq '.[0].url // ""')
if [[ -n "$existing_url" ]]; then
  echo "Release PR already open: $existing_url"
  {
    echo "## Release pull request"
    echo
    echo "Existing protected release PR: [$branch]($existing_url)"
  } >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
  exit 0
fi

title_query=$(jq -rn --arg value "$release_subject" '$value | @uri')
body_query=$(jq -rn --arg value "$release_body" '$value | @uri')
url="${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY}/compare/${base_branch}...${branch}?expand=1&title=${title_query}"
if (( ${#url} + ${#body_query} < 7000 )); then
  url="${url}&body=${body_query}"
fi
echo "Release branch prepared. Open the pull request manually: $url"
{
  echo "## Protected release pull request"
  echo
  echo "The organization forbids GitHub Actions from opening pull requests."
  echo "Open [$branch]($url), preserving the generated title and body below."
  echo "For long release notes, paste the body manually if it is not prefilled."
  echo "Merge only after the PR checks pass. Preserve this exact squash subject and body."
  echo
  printf "Subject: \`%s\`\n\n" "$release_subject"
  printf "\`\`\`\n%s\n\`\`\`\n" "$release_body"
} >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
