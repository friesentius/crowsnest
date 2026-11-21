#!/usr/bin/env bash
# Decides whether a push to main should cut a release: it should exactly when
# Cargo.toml's version has no GitHub release yet. A version that was already
# released is never re-tagged or re-published, which makes merging a main
# back into dev, or a release PR for an already released version, a no-op.
#
# Usage: release-decision.sh [path/to/Cargo.toml]
#
# Existing release tags come from `gh release list`, or from the newline
# separated RELEASE_TAGS variable when it is set (used by the tests). Prints
# `version=` and `should_release=` lines, and appends them to $GITHUB_OUTPUT
# when that is set.
set -euo pipefail

# The version of the [package] table: the first `version = "..."` line after
# `[package]` and before the next table header.
read_version() {
  awk '
    /^\[/ { in_package = ($0 == "[package]"); next }
    in_package && /^version[[:space:]]*=/ {
      gsub(/^version[[:space:]]*=[[:space:]]*"/, "")
      gsub(/".*$/, "")
      print
      exit
    }
  ' "$1"
}

# Succeeds (prints nothing) when $1 is a plain release version with no "v"
# prefix, matching how this project names its tags.
valid_version() {
  [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]
}

# decide VERSION TAGS: prints "true" when no existing tag equals VERSION
# exactly, "false" otherwise. TAGS is newline separated.
decide() {
  local version=$1 tags=$2
  if grep -Fxq -- "$version" <<<"$tags"; then
    echo false
  else
    echo true
  fi
}

existing_tags() {
  if [[ -n ${RELEASE_TAGS+set} ]]; then
    printf '%s\n' "$RELEASE_TAGS"
  else
    gh release list --limit 1000 --json tagName --jq '.[].tagName'
  fi
}

main() {
  local manifest=${1:-Cargo.toml} version should
  version=$(read_version "$manifest")
  if ! valid_version "$version"; then
    echo "release-decision: invalid or missing version '$version' in $manifest" >&2
    exit 1
  fi
  should=$(decide "$version" "$(existing_tags)")

  local lines="version=$version
should_release=$should"
  if [[ -n ${GITHUB_OUTPUT:-} ]]; then
    printf '%s\n' "$lines" >>"$GITHUB_OUTPUT"
  fi
  printf '%s\n' "$lines"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  main "$@"
fi
