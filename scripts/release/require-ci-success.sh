#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <commit>" >&2
  exit 2
}
[[ $# -eq 1 ]] || usage
commit=$1
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "error: commit must be a full SHA" >&2; exit 2; }
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GH_TOKEN:?GH_TOKEN is required}"

for attempt in $(seq 1 12); do
  run_id=$(gh api \
    "repos/$GITHUB_REPOSITORY/actions/workflows/ci.yml/runs?event=push&branch=main&head_sha=$commit&per_page=100" \
    --jq ".workflow_runs[] | select(.head_sha == \"$commit\" and .head_branch == \"main\" and .event == \"push\" and .status == \"completed\" and .conclusion == \"success\" and .head_repository.full_name == \"$GITHUB_REPOSITORY\") | .id" \
    | awk 'NR == 1 { print; exit }')
  if [[ "$run_id" =~ ^[0-9]+$ ]]; then
    echo "Found successful exact-main CI run $run_id for $commit."
    exit 0
  fi
  echo "Waiting for successful CI run for $commit ($attempt/12)."
  sleep 10
done

echo "error: no successful exact-main CI run found for $commit" >&2
exit 1
