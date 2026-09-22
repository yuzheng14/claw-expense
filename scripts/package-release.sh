#!/bin/bash
# Compatible with the /bin/bash 3.2 supplied by macOS.
set -euo pipefail

fail() {
  printf 'package-release: %s\n' "$*" >&2
  exit 1
}

if [ "$#" -ne 1 ]; then
  fail 'usage: bash scripts/package-release.sh vX.Y.Z'
fi

release_tag=$1
if ! [[ "$release_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  fail 'tag must have the form vX.Y.Z'
fi
release_version=${release_tag#v}
project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$project_root"

package_version=$(awk '
  /^\[package\][[:space:]]*$/ { in_package = 1; next }
  /^\[/ { in_package = 0 }
  in_package && /^[[:space:]]*version[[:space:]]*=/ {
    sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
  }
' Cargo.toml)
[ "$package_version" = "$release_version" ] || fail "Cargo version $package_version does not match $release_tag"

skill_dir=skills/claw-expense
[ -f "$skill_dir/VERSION" ] || fail "missing $skill_dir/VERSION"
skill_version=$(cat "$skill_dir/VERSION")
[ "$skill_version" = "$release_tag" ] || fail "Skill VERSION $skill_version does not match $release_tag"
[ -f "$skill_dir/SKILL.md" ] || fail "missing $skill_dir/SKILL.md"

binary=target/aarch64-apple-darwin/release/claw-expense
for required in "$binary" LICENSE README.md; do
  [ -f "$required" ] && [ ! -L "$required" ] || fail "missing regular file: $required"
done
[ -x "$binary" ] || fail "binary is not executable: $binary"
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || fail 'package on an Apple silicon Mac to verify the executable'
binary_type=$(LC_ALL=C file -b "$binary")
case "$binary_type" in
  'Mach-O 64-bit executable arm64'*) ;;
  *) fail "expected a native ARM64 macOS executable, found: $binary_type" ;;
esac
binary_version=$("$project_root/$binary" --version)
[ "$binary_version" = "claw-expense $release_version" ] || fail "binary version does not match tag: $binary_version"

# The complete Skill folder is included, but accidental private data or build
# output must make packaging fail instead of leaking into a public release.
[ ! -L "$skill_dir" ] || fail 'Skill directory must not be a symlink'
unexpected=$(find "$skill_dir" \( -type l -o \( ! -type f ! -type d \) \
  -o -name .git -o -name target -o -name '*.db' -o -name '*.sqlite' \
  -o -name '*.sqlite3' -o -name '*-wal' -o -name '*-shm' \) -print)
[ -z "$unexpected" ] || fail "unexpected file in Skill directory: $unexpected"

binary_asset="claw-expense-${release_tag}-aarch64-apple-darwin.tar.gz"
skill_asset="claw-expense-skill-${release_tag}.tar.gz"
dist_dir="$project_root/dist"
[ ! -L "$dist_dir" ] || fail 'dist must not be a symlink'
mkdir -p "$dist_dir"
for asset in "$binary_asset" "$skill_asset" SHA256SUMS; do
  [ ! -e "$dist_dir/$asset" ] && [ ! -L "$dist_dir/$asset" ] || fail "output already exists: dist/$asset"
done

package_stage=$(mktemp -d "$dist_dir/.package.XXXXXX")
cleanup() {
  # Only remove the staging directory created by this invocation.
  case "$package_stage" in
    "$dist_dir"/.package.*) rm -rf -- "$package_stage" ;;
  esac
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir "$package_stage/cli" "$package_stage/skill"
install -m 755 "$binary" "$package_stage/cli/claw-expense"
install -m 644 LICENSE "$package_stage/cli/LICENSE"
install -m 644 README.md "$package_stage/cli/README.md"
cp -R "$skill_dir" "$package_stage/skill/claw-expense"
install -m 644 LICENSE "$package_stage/skill/claw-expense/LICENSE"

# COPYFILE_DISABLE prevents macOS AppleDouble metadata from adding extra files.
COPYFILE_DISABLE=1 tar --no-xattrs -czf "$package_stage/$binary_asset" \
  -C "$package_stage/cli" claw-expense LICENSE README.md
COPYFILE_DISABLE=1 tar --no-xattrs -czf "$package_stage/$skill_asset" \
  -C "$package_stage/skill" claw-expense
(
  cd "$package_stage"
  shasum -a 256 "$binary_asset" "$skill_asset" > SHA256SUMS
  shasum -a 256 -c SHA256SUMS
)

# Hard links atomically create each output and refuse to replace existing files.
for asset in "$binary_asset" "$skill_asset" SHA256SUMS; do
  ln "$package_stage/$asset" "$dist_dir/$asset"
done
printf 'Release assets written to %s\n' "$dist_dir"
