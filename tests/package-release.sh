#!/bin/bash
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
fixture_parent=$(mktemp -d "${TMPDIR:-/tmp}/claw-expense-package-test.XXXXXX")
cleanup() {
  case "$fixture_parent" in
    "${TMPDIR:-/tmp}"/claw-expense-package-test.*) rm -rf -- "$fixture_parent" ;;
  esac
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
  printf 'packaging test failed: %s\n' "$*" >&2
  exit 1
}

# A fixture with spaces also verifies quoting of checkout/output paths.
fixture="$fixture_parent/project with spaces"
mkdir -p "$fixture/scripts" "$fixture/skills" "$fixture/target/aarch64-apple-darwin/release"
cp "$project_root/Cargo.toml" "$project_root/LICENSE" "$project_root/README.md" "$fixture/"
cp "$project_root/scripts/package-release.sh" "$fixture/scripts/"
cp -R "$project_root/skills/claw-expense" "$fixture/skills/"
cp "$project_root/target/aarch64-apple-darwin/release/claw-expense" "$fixture/target/aarch64-apple-darwin/release/"
tag=$(cat "$fixture/skills/claw-expense/VERSION")
version=${tag#v}

package() {
  /bin/bash "$fixture/scripts/package-release.sh" "$@"
}

expect_failure() {
  if package "$@" >"$fixture_parent/failure.log" 2>&1; then
    fail "expected packaging to reject: $*"
  fi
}

expect_failure '../invalid-tag'
expect_failure 'v999999.0.0'
printf '999999.0.0\n' > "$fixture/skills/claw-expense/VERSION"
expect_failure "$tag"
cp "$project_root/skills/claw-expense/VERSION" "$fixture/skills/claw-expense/VERSION"

printf 'private bookkeeping data\n' > "$fixture/skills/claw-expense/accidental.sqlite3"
expect_failure "$tag"
rm "$fixture/skills/claw-expense/accidental.sqlite3"

# Files elsewhere in the checkout must never enter either release archive.
printf 'private bookkeeping data\n' > "$fixture/ledger.sqlite3"
printf 'unrelated build output\n' > "$fixture/target/private.db"
package "$tag"

binary_asset="claw-expense-${tag}-aarch64-apple-darwin.tar.gz"
skill_asset="claw-expense-skill-${tag}.tar.gz"
(
  cd "$fixture/dist"
  shasum -a 256 -c SHA256SUMS
)
[ "$(tar -tzf "$fixture/dist/$binary_asset")" = "$(printf 'claw-expense\nLICENSE\nREADME.md')" ] || fail 'unexpected binary archive layout'
mkdir "$fixture_parent/cli" "$fixture_parent/skill"
tar -xzf "$fixture/dist/$binary_asset" -C "$fixture_parent/cli"
tar -xzf "$fixture/dist/$skill_asset" -C "$fixture_parent/skill"
[ "$("$fixture_parent/cli/claw-expense" --version)" = "claw-expense $version" ] || fail 'packaged executable version mismatch'
diff -r -x LICENSE "$project_root/skills/claw-expense" "$fixture_parent/skill/claw-expense"
cmp "$project_root/LICENSE" "$fixture_parent/skill/claw-expense/LICENSE"

before=$(shasum -a 256 "$fixture/dist/$binary_asset" "$fixture/dist/$skill_asset" "$fixture/dist/SHA256SUMS")
expect_failure "$tag"
after=$(shasum -a 256 "$fixture/dist/$binary_asset" "$fixture/dist/$skill_asset" "$fixture/dist/SHA256SUMS")
[ "$before" = "$after" ] || fail 'repeat packaging changed existing release assets'
printf 'Packaging validation passed.\n'
