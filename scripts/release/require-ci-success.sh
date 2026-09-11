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
  runs_json=$(gh api \
    "repos/$GITHUB_REPOSITORY/actions/workflows/ci.yml/runs?event=push&branch=main&head_sha=$commit&per_page=100")
  run_id=$(jq -r \
    --arg repository "$GITHUB_REPOSITORY" \
    --arg commit "$commit" \
    '[.workflow_runs[]?
      | select(
          .name == "CI" and
          .repository.full_name == $repository and
          .head_repository.full_name == $repository and
          .event == "push" and
          .head_branch == "main" and
          .head_sha == $commit and
          .status == "completed" and
          .conclusion == "success"
        )]
     | sort_by(.id)
     | reverse
     | .[0].id // empty' <<<"$runs_json")
  if [[ "$run_id" =~ ^[1-9][0-9]*$ ]]; then
    artifact_name="cutterhoochee-ci-appimage-$commit"
    artifacts_json=$(gh api \
      "repos/$GITHUB_REPOSITORY/actions/runs/$run_id/artifacts?per_page=100")
    artifact_count=$(jq -r \
      --arg artifact_name "$artifact_name" \
      '[.artifacts[]? | select(.name == $artifact_name)] | length' <<<"$artifacts_json")
    if [[ "$artifact_count" != 1 ]]; then
      echo "error: expected exactly one CI AppImage artifact named $artifact_name for run $run_id; found $artifact_count" >&2
      exit 1
    fi
    artifact=$(jq -c \
      --arg artifact_name "$artifact_name" \
      '[.artifacts[]? | select(.name == $artifact_name)][0]' <<<"$artifacts_json")
    jq -e \
      --arg artifact_name "$artifact_name" \
      --arg commit "$commit" \
      --argjson run_id "$run_id" \
      '.name == $artifact_name and
       .expired == false and
       (.size_in_bytes | type == "number") and
       (.size_in_bytes > 0) and
       (.expires_at | type == "string" and length > 0) and
       .workflow_run.id == $run_id and
       .workflow_run.head_branch == "main" and
       .workflow_run.head_sha == $commit' <<<"$artifact" >/dev/null || {
      echo "error: CI AppImage artifact $artifact_name for run $run_id is expired or not bound to the exact run and commit" >&2
      exit 1
    }
    artifact_id=$(jq -r '.id // empty' <<<"$artifact")
    [[ "$artifact_id" =~ ^[1-9][0-9]*$ ]] || {
      echo "error: CI AppImage artifact ID is invalid for run $run_id" >&2
      exit 1
    }
    if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
      {
        printf 'run_id=%s\n' "$run_id"
        printf 'artifact_id=%s\n' "$artifact_id"
        printf 'artifact_name=%s\n' "$artifact_name"
      } >> "$GITHUB_OUTPUT"
    fi
    echo "Found successful exact-main CI run $run_id and unexpired artifact $artifact_id for $commit."
    exit 0
  fi
  echo "Waiting for successful exact-main CI run for $commit ($attempt/12)."
  sleep 10
done

echo "error: no successful exact-main CI run found for $commit" >&2
exit 1
