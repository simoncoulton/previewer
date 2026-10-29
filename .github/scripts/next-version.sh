#!/usr/bin/env bash
# Prints the next release version (no leading "v"), or nothing when there is
# nothing to release. The last "vX.Y.Z" tag is the current version and the
# commit messages since it decide the bump:
#
#   breaking change ("type!:" or "BREAKING CHANGE") -> major
#   feat                                            -> minor
#   anything else                                   -> patch
#
# While the major version is 0, a breaking change bumps the minor instead.
# Put "[skip release]" in the latest commit message to skip a release.
set -euo pipefail

last=$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null || true)
if [ -z "$last" ]; then
  echo "0.1.0"
  exit 0
fi

range="$last..HEAD"
if [ -z "$(git rev-list -n 1 "$range")" ]; then
  exit 0
fi
if git log -n 1 --format=%B | grep -qiF '[skip release]'; then
  exit 0
fi

IFS=. read -r major minor patch <<<"${last#v}"

breaking=false
feature=false
if git log --format=%s "$range" | grep -qE '^[a-z]+(\([^)]*\))?!:'; then breaking=true; fi
if git log --format=%B "$range" | grep -qE '^BREAKING[ -]CHANGE'; then breaking=true; fi
if git log --format=%s "$range" | grep -qE '^feat(\([^)]*\))?:'; then feature=true; fi

if $breaking && [ "$major" -gt 0 ]; then
  echo "$((major + 1)).0.0"
elif $breaking || $feature; then
  echo "$major.$((minor + 1)).0"
else
  echo "$major.$minor.$((patch + 1))"
fi
