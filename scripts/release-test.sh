#!/usr/bin/env bash
# Tests of scripts/release.sh, run by ci (hygiene): `bash scripts/release-test.sh`.
set -uo pipefail

script="$(dirname "$0")/release.sh"
fails=0

run() { bash "$script" "$@" 2>/dev/null; }

same() { # name expected actual
  if [[ $2 != "$3" ]]; then
    printf 'FAIL %s\n  expected: %q\n  got:      %q\n' "$1" "$2" "$3"
    fails=$((fails + 1))
  fi
}

refused() { # name command...
  local name=$1
  shift
  if run "$@" >/dev/null; then
    echo "FAIL $name: accepted"
    fails=$((fails + 1))
  fi
}

# tag-version
same "stable tag" 0.2.0 "$(run tag-version v0.2.0 0.2.0)"
same "rc tag" 0.2.0-rc.1 "$(run tag-version v0.2.0-rc.1 0.2.0)"
same "zero and hyphen fields" 0.2.0-0.x-y.0a "$(run tag-version v0.2.0-0.x-y.0a 0.2.0)"
for bad in v0.2.0-rc.01 v0.2.0-00 v0.2.00 v00.2.0 0.2.0 v0.2 v0.2.0.1 v0.2.0- v0.2.0-rc..1 \
  v0.2.0-r_c v0.2.0+b1 v0.2.0-rc.1+b1; do
  refused "tag $bad" tag-version "$bad" 0.2.0
done
refused "tag of another version" tag-version v0.3.0 0.2.0
refused "pre tag of another version" tag-version v0.3.0-rc.1 0.2.0
msg=$(bash "$script" tag-version v0.2.0-rc.01 0.2.0 2>&1 >/dev/null)
[[ $msg == *"leading zeros"* ]] || { echo "FAIL leading zero message: $msg"; fails=$((fails + 1)); }

# pre-skip: `git ls-remote --tags` output, v0.2.0 annotated (peeled line), v0.1.0 lightweight.
refs=$'aaa\trefs/tags/v0.1.0\nbbb\trefs/tags/v0.2.0\nccc\trefs/tags/v0.2.0^{}\nddd\trefs/tags/v0.3.0-rc.1\neee\trefs/tags/v0.2.1-pre.7'
same "new version, plain commit" "" "$(run pre-skip 0.2.1 fff <<<"$refs")"
same "no tags at all" "" "$(run pre-skip 0.1.0 fff </dev/null)"
same "a test tag does not block" "" "$(run pre-skip 0.3.0 ddd <<<"$refs")"
same "a pre-release tag does not block" "" "$(run pre-skip 0.2.1 eee <<<"$refs")"
[[ $(run pre-skip 0.2.0 fff <<<"$refs") == *"bump the version"* ]] \
  || { echo "FAIL version already released"; fails=$((fails + 1)); }
[[ $(run pre-skip 0.2.1 ccc <<<"$refs") == *"stable release v0.2.0"* ]] \
  || { echo "FAIL commit with an annotated stable tag"; fails=$((fails + 1)); }
[[ $(run pre-skip 0.2.1 aaa <<<"$refs") == *"stable release v0.1.0"* ]] \
  || { echo "FAIL commit with a lightweight stable tag"; fails=$((fails + 1)); }

# prune: `<tag> <isPrerelease>` lines.
list=$'v0.1.0 false\nv0.2.0-pre.5 true\nv0.2.0-rc.1 true\nv0.2.0 false\nv0.2.1-pre.7 true'
same "a pre-release removes only other pre-releases" $'v0.2.0-pre.5\nv0.2.0-rc.1' \
  "$(run prune v0.2.1-pre.7 <<<"$list")"
same "a stable release removes every other release" $'v0.1.0\nv0.2.0-pre.5\nv0.2.0-rc.1\nv0.2.1-pre.7' \
  "$(run prune v0.2.0 <<<"$list")"
same "a test tag removes other pre-releases" $'v0.2.0-pre.5\nv0.2.1-pre.7' \
  "$(run prune v0.2.0-rc.1 <<<"$list")"
same "nothing to remove" "" "$(run prune v0.2.1-pre.7 <<<"v0.2.1-pre.7 true")"

refused "unknown command" nonsense

if ((fails)); then
  echo "release.sh: $fails failed"
  exit 1
fi
echo "release.sh: all checks passed"
