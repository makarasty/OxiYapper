#!/usr/bin/env bash
# The decisions of release.yml and prerelease.yml, here so scripts/release-test.sh can test them
# (contract: docs/RELEASING.md "Assets and tags"). Input from arguments and stdin, answer on stdout.
#
#   release.sh tag-version TAG CARGO_VERSION
#       The version of a hand-pushed tag (`v0.2.0` -> `0.2.0`); fails when TAG is no semver
#       `vX.Y.Z[-<pre>]` (numbers without leading zeros, as crates/oxi-core/src/update.rs parses
#       them) or its `X.Y.Z` differs from CARGO_VERSION.
#   release.sh pre-skip CARGO_VERSION SHA
#       stdin: `git ls-remote --tags` lines. Prints why the automatic pre-release of commit SHA must
#       not be published (a stable tag `v<CARGO_VERSION>` exists, or SHA carries a stable tag);
#       prints nothing when it may.
#   release.sh prune TAG
#       stdin: `<tag> <isPrerelease>` lines of every release. Prints the releases to delete once TAG
#       is published: a stable TAG replaces every other release, a pre-release TAG every other
#       pre-release. So at most one stable release and one pre-release exist.
set -euo pipefail

num='(0|[1-9][0-9]*)'
ident="($num|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
semver="^$num\\.$num\\.$num(-$ident(\\.$ident)*)?\$"
stable="^v$num\\.$num\\.$num\$"

fail() {
  echo "::error::$*" >&2
  exit 1
}

tag_version() {
  local tag=$1 cargo=$2
  local version=${tag#v}
  [[ $tag == v* && $version =~ $semver ]] \
    || fail "tag $tag is not vX.Y.Z or vX.Y.Z-<pre> by semver (no leading zeros: v0.2.0-rc.1, not v0.2.0-rc.01)"
  [[ ${version%%-*} == "$cargo" ]] \
    || fail "tag $tag does not match the Cargo.toml version $cargo"
  echo "$version"
}

pre_skip() {
  local cargo=$1 sha=$2 ref_sha ref name
  while read -r ref_sha ref; do
    name=${ref#refs/tags/}
    name=${name%'^{}'} # the commit of an annotated tag
    [[ $name =~ $stable ]] || continue
    if [[ $name == "v$cargo" ]]; then
      echo "the stable tag $name exists: bump the version in Cargo.toml after a stable release (docs/RELEASING.md)"
      return
    fi
    if [[ $ref_sha == "$sha" ]]; then
      echo "commit $sha is the stable release $name"
      return
    fi
  done
}

prune() {
  local keep=$1 tag pre all=false
  [[ $keep =~ $stable ]] && all=true
  while read -r tag pre; do
    [[ -n $tag && $tag != "$keep" ]] || continue
    if [[ $all == true || $pre == true ]]; then
      echo "$tag"
    fi
  done
}

cmd=${1-}
shift || true
case $cmd in
  tag-version) tag_version "$@" ;;
  pre-skip) pre_skip "$@" ;;
  prune) prune "$@" ;;
  *) fail "usage: release.sh tag-version TAG CARGO_VERSION | pre-skip CARGO_VERSION SHA | prune TAG" ;;
esac
