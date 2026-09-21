#!/usr/bin/env bash
# Refuses to go on when a tag no longer points at the commit this release is
# built from. Annotated tags are dereferenced, so the comparison is against
# the commit either way.
#
# This is detection, not a lock: the tag can still move a moment after the
# check returns. Protecting v* tags against force-pushes and deletion with a
# repository ruleset is what actually prevents it; this catches the rest.
#
# Usage: check-tag-target.sh <tag> <expected-commit>
set -euo pipefail

tag="${1:?usage: check-tag-target.sh <tag> <expected-commit>}"
expected="${2:?usage: check-tag-target.sh <tag> <expected-commit>}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is not set}"

ref="$(gh api "repos/${repo}/git/ref/tags/${tag}" \
  --jq '.object.type + " " + .object.sha')" || {
  echo "::error::tag ${tag} no longer exists"
  exit 1
}

type="${ref%% *}"
sha="${ref##* }"
if [ "$type" = "tag" ]; then
  sha="$(gh api "repos/${repo}/git/tags/${sha}" --jq '.object.sha')"
fi

if [ "$sha" != "$expected" ]; then
  echo "::error::tag ${tag} moved: this release was resolved at ${expected}" \
    "and the tag names ${sha} now"
  exit 1
fi

echo "tag ${tag} still points at ${expected}"
