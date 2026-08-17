#!/usr/bin/env bash
# Render the release notes body for one calendar release.
#
# Usage: render-release-notes.sh <prev-tag|""> <sha> <tag> <output-file>
#
# Why this exists: 49 of this repository's 51 commits are direct pushes with no
# pull request reference, so `gh release create --generate-notes` on its own
# produces little more than a compare link. The workflow still passes
# --generate-notes (GitHub's contributor block costs nothing and fills in
# automatically if a PR-based history is ever adopted); this script supplies the
# actual changelog from the commit range.
#
# Injection safety: commit subjects are untrusted text. They are produced by
# `git log` into a temporary file and copied into the output file verbatim. No
# subject is ever interpolated into a command line, so backticks and $(...)
# inside a subject cannot execute.
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 <prev-tag|\"\"> <sha> <tag> <output-file>" >&2
  exit 2
fi

prev_tag=$1
sha=$2
tag=$3
output=$4

# Defence in depth. The workflow already constrains these, but this script must
# not build a URL or a git revision out of arbitrary text.
if [[ -n "$prev_tag" && ! "$prev_tag" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
  echo "previous tag is not a plain tag name: $prev_tag" >&2
  exit 2
fi
if [[ ! "$sha" =~ ^[0-9a-fA-F]{7,40}$ ]]; then
  echo "sha is not a hexadecimal commit id: $sha" >&2
  exit 2
fi
if [[ ! "$tag" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
  echo "tag is not a plain tag name: $tag" >&2
  exit 2
fi
if [[ -z "$output" ]]; then
  echo "output file must not be empty" >&2
  exit 2
fi

repository=${GITHUB_REPOSITORY:-}
server=${GITHUB_SERVER_URL:-https://github.com}

git rev-parse --verify --quiet "$sha^{commit}" >/dev/null || {
  echo "sha does not resolve to a commit in this repository: $sha" >&2
  exit 1
}

if [[ -n "$prev_tag" ]]; then
  git rev-parse --verify --quiet "$prev_tag^{commit}" >/dev/null || {
    echo "previous tag does not resolve to a commit: $prev_tag" >&2
    exit 1
  }
  range="$prev_tag..$sha"
  heading="## Changes since $prev_tag"
else
  range="$sha"
  heading="## Initial release"
fi

subjects=$(mktemp)
trap 'rm -f -- "$subjects"' EXIT

# `--` terminates revision parsing so a tag that collides with a path name
# cannot be reinterpreted as a pathspec.
git log --no-merges --format='- %s (%h)' "$range" -- > "$subjects"

{
  printf '%s\n\n' "$heading"
  if [[ -s "$subjects" ]]; then
    cat -- "$subjects"
  else
    printf '%s\n' '_No non-merge commits in this range._'
  fi
  if [[ -n "$repository" ]]; then
    printf '\n'
    if [[ -n "$prev_tag" ]]; then
      printf '**Full changelog:** %s/%s/compare/%s...%s\n' \
        "$server" "$repository" "$prev_tag" "$tag"
    else
      printf '**Full changelog:** %s/%s/commits/%s\n' "$server" "$repository" "$tag"
    fi
  fi
} > "$output"
