#!/usr/bin/env bash
# Refuses a tag that is not a release tag, or that does not name the version
# in Cargo.toml. The comparison is exact once the leading "v" is off, so
# "v0.1.01", "V0.1.1" or a stray suffix is a mismatch, not a near miss.
#
# Prints "version=<version>" to $GITHUB_OUTPUT when running in Actions, so
# the jobs that follow name the crate file without parsing anything again.
set -euo pipefail

tag="${1:?usage: check-tag-version.sh <tag>}"
name="${CRATE_NAME:-librespeed-cli}"

# A release tag is v followed by a semantic version, as SemVer 2.0.0 spells
# it: no leading zeroes in the numbers. The push trigger only says "v*", so
# this is where vfoo, v1, v1.2, v01.2.3 and v1.2.3.4 are turned away.
num='(0|[1-9][0-9]*)'
semver="${num}\.${num}\.${num}(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?"
if ! printf '%s' "$tag" | grep -Eq "^v${semver}$"; then
  echo "::error::tag ${tag} is not a release tag: expected v<major>.<minor>.<patch>"
  exit 1
fi

# Selected by name rather than by position: a workspace member added later
# must not be able to decide what this release is called.
version="$(cargo metadata --format-version 1 --no-deps |
  jq -er --arg name "$name" \
    'first(.packages[] | select(.name == $name) | .version)')" || {
  echo "::error::no package named ${name} in cargo metadata"
  exit 1
}

if [ "${tag#v}" != "$version" ]; then
  echo "::error::tag ${tag} does not match" \
    "version = \"${version}\" in Cargo.toml"
  exit 1
fi

echo "tag ${tag} matches version = \"${version}\" in Cargo.toml"
[ -n "${GITHUB_OUTPUT:-}" ] && echo "version=${version}" >> "$GITHUB_OUTPUT"
exit 0
