#!/usr/bin/env bash
# Tests for release-decision.sh: a version that already has a release is
# skipped, a new version is released.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script=$here/release-decision.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

failures=0
check() { # name expected actual
  if [[ $2 == "$3" ]]; then
    echo "ok   $1"
  else
    echo "FAIL $1: expected '$2', got '$3'"
    failures=$((failures + 1))
  fi
}

manifest() { # version
  cat >"$work/Cargo.toml" <<TOML
[package]
name = "crowsnest"
version = "$1"
edition = "2024"

[dependencies]
serde = { version = "9.9.9" }
TOML
}

decision() { # tags -> prints the should_release value, or "error"
  local out
  if out=$(RELEASE_TAGS=$1 "$script" "$work/Cargo.toml" 2>/dev/null); then
    sed -n 's/^should_release=//p' <<<"$out"
  else
    echo error
  fi
}

manifest 0.1.0
check "no releases yet -> release" true "$(decision "")"
check "other versions only -> release" true "$(decision $'0.0.9\n0.0.10')"
check "version already released -> skip" false "$(decision $'0.0.9\n0.1.0')"
check "only the exact tag counts, not a prefix" true "$(decision $'0.1.00\n0.1.0-rc.1')"
check "a v-prefixed tag is a different tag" true "$(decision 'v0.1.0')"

manifest 0.2.0
check "bumped version -> release" true "$(decision $'0.1.0')"

check "dependency versions are not the package version" 0.2.0 \
  "$(RELEASE_TAGS="" "$script" "$work/Cargo.toml" | sed -n 's/^version=//p')"

manifest "not-a-version"
check "invalid version -> error" error "$(decision "")"

manifest 0.1.0
: >"$work/out"
GITHUB_OUTPUT=$work/out RELEASE_TAGS="" "$script" "$work/Cargo.toml" >/dev/null
check "writes GITHUB_OUTPUT" $'version=0.1.0\nshould_release=true' "$(cat "$work/out")"

((failures == 0)) || { echo "$failures failed"; exit 1; }
echo "all passed"
