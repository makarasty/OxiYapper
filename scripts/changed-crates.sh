#!/usr/bin/env sh
# Prints the packages touched by uncommitted changes (staged, unstaged, untracked)
# and the scoped check line for each. `--self-test` checks the path mapping.
set -eu

# crates/<name>/... -> <name>; src/... -> the root package; anything else -> nothing.
map() {
    sed -n -e 's#^crates/\([^/]*\)/.*#\1#p' -e 's#^src/.*#oxi-yapper#p'
}

if [ "${1:-}" = "--self-test" ]; then
    got=$(printf 'crates/oxi-core/src/lib.rs\nsrc/main.rs\ndocs/SPEC.md\ncrates/x\n' | map | tr '\n' ' ')
    [ "$got" = "oxi-core oxi-yapper " ] || { echo "self-test failed: '$got'" >&2; exit 1; }
    echo "self-test ok"
    exit 0
fi

cd "$(git rev-parse --show-toplevel)"
files=$({ git diff --name-only HEAD; git ls-files --others --exclude-standard; })

if printf '%s\n' "$files" | grep -qxE 'Cargo\.(toml|lock)'; then
    echo "# root Cargo.toml or Cargo.lock changed: every package may be affected"
fi

if cargo nextest --version >/dev/null 2>&1; then test_cmd="cargo nextest run"; else test_cmd="cargo test"; fi

crates=$(printf '%s\n' "$files" | map | sort -u)
[ -n "$crates" ] || echo "# no package changed"
for c in $crates; do
    echo "cargo clippy -p $c --all-targets -- -D warnings && $test_cmd -p $c"
done
