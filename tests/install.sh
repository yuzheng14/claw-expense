#!/bin/bash
# Installer behavior tests: all downloads and binary metadata are mocked.
set -euo pipefail

repo_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
installer="$repo_dir/skills/claw-expense/scripts/ensure-cli.sh"
runner="$repo_dir/skills/claw-expense/scripts/run.sh"
release_tag=$(<"$repo_dir/skills/claw-expense/VERSION")
release_version=${release_tag#v}
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/claw-expense-installer-tests.XXXXXX")
test_dir=$(CDPATH= cd -- "$test_dir" && pwd -P)
trap 'rm -rf -- "$test_dir"' EXIT
mock_bin="$test_dir/mock-bin"
fixture_dir="$test_dir/fixtures"
mkdir -p "$mock_bin" "$fixture_dir/package"
asset="claw-expense-$release_tag-aarch64-apple-darwin.tar.gz"

cat > "$fixture_dir/package/claw-expense" <<'CLI'
#!/bin/bash
printf 'executed\n' >> "$MOCK_EXEC_LOG"
if [ "${1:-}" = --version ]; then
  printf 'claw-expense %s\n' "${MOCK_BINARY_VERSION:-$MOCK_DEFAULT_VERSION}"
else
  printf '<%s>\n' "$@"
fi
CLI
chmod 755 "$fixture_dir/package/claw-expense"
printf 'MIT\n' > "$fixture_dir/package/LICENSE"
printf 'Fixture only\n' > "$fixture_dir/package/README.md"
tar -czf "$fixture_dir/$asset" -C "$fixture_dir/package" claw-expense LICENSE README.md
(cd "$fixture_dir" && shasum -a 256 "$asset") > "$fixture_dir/SHA256SUMS"
printf '%064d  %s\n' 0 "$asset" > "$fixture_dir/BAD_SUMS"

cat > "$mock_bin/uname" <<'UNAME'
#!/bin/bash
case "$1" in
  -s) printf '%s\n' "${MOCK_OS:-Darwin}" ;;
  -m) printf '%s\n' "${MOCK_ARCH:-arm64}" ;;
  *) exit 1 ;;
esac
UNAME
cat > "$mock_bin/file" <<'FILE'
#!/bin/bash
printf '%s\n' "${MOCK_FILE_TYPE:-Mach-O 64-bit executable arm64}"
FILE
cat > "$mock_bin/curl" <<'CURL'
#!/bin/bash
set -euo pipefail
output=''
url=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) output=$2; shift 2 ;;
    --proto|--proto-redir|--tlsv1.2|--connect-timeout|--max-time|--retry)
      if [ "$1" = --tlsv1.2 ]; then shift; else shift 2; fi ;;
    --fail|--silent|--show-error|--location) shift ;;
    https://*) url=$1; shift ;;
    *) printf 'Unexpected curl option: %s\n' "$1" >&2; exit 1 ;;
  esac
done
printf '%s\n' "$url" >> "$MOCK_CURL_LOG"
case "$url" in
  "https://github.com/yuzheng14/claw-expense/releases/download/$MOCK_RELEASE_TAG/$MOCK_RELEASE_ASSET")
    cp "$MOCK_FIXTURES/${MOCK_ARCHIVE:-$MOCK_RELEASE_ASSET}" "$output" ;;
  "https://github.com/yuzheng14/claw-expense/releases/download/$MOCK_RELEASE_TAG/SHA256SUMS")
    cp "$MOCK_FIXTURES/${MOCK_SUMS:-SHA256SUMS}" "$output" ;;
  *) printf 'Unexpected download URL: %s\n' "$url" >&2; exit 1 ;;
esac
CURL
chmod 755 "$mock_bin/uname" "$mock_bin/file" "$mock_bin/curl"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
reset_case() {
  case_dir="$test_dir/$1"
  mkdir -p "$case_dir"
  export PATH="$mock_bin:/usr/bin:/bin"
  export CLAW_EXPENSE_INSTALL_DIR="$case_dir/install"
  export CLAW_EXPENSE_VERSION="$release_tag"
  export MOCK_DEFAULT_VERSION="$release_version"
  export MOCK_RELEASE_TAG="$release_tag"
  export MOCK_RELEASE_ASSET="$asset"
  export MOCK_FIXTURES="$fixture_dir"
  export MOCK_CURL_LOG="$case_dir/curl.log"
  export MOCK_EXEC_LOG="$case_dir/exec.log"
  unset MOCK_OS MOCK_ARCH MOCK_SUMS MOCK_ARCHIVE MOCK_FILE_TYPE MOCK_BINARY_VERSION
}
expect_failure() {
  if "$installer" > "$case_dir/stdout" 2> "$case_dir/stderr"; then
    fail 'installation unexpectedly succeeded'
  fi
  [ ! -s "$case_dir/stdout" ] || fail 'a failed installation wrote to stdout'
}
assert_no_download() { [ ! -s "$MOCK_CURL_LOG" ] || fail 'unexpected download'; }
assert_no_execution() { [ ! -s "$MOCK_EXEC_LOG" ] || fail 'unverified download was executed'; }

reset_case existing_path
mkdir "$case_dir/bin"
cp "$fixture_dir/package/claw-expense" "$case_dir/bin/claw-expense"
export PATH="$case_dir/bin:$PATH" MOCK_OS=Linux
result=$("$installer")
[ "$result" = "$case_dir/bin/claw-expense" ] || fail 'PATH executable was not returned as an absolute path'
assert_no_download

reset_case existing_install
mkdir "$CLAW_EXPENSE_INSTALL_DIR"
cp "$fixture_dir/package/claw-expense" "$CLAW_EXPENSE_INSTALL_DIR/claw-expense"
result=$("$installer")
[ "$result" = "$CLAW_EXPENSE_INSTALL_DIR/claw-expense" ] || fail 'installed executable was not reused'
assert_no_download

reset_case download
unset CLAW_EXPENSE_VERSION
export CLAW_EXPENSE_INSTALL_DIR="$case_dir/with spaces/bin"
result=$("$installer" 2> "$case_dir/stderr")
[ "$result" = "$CLAW_EXPENSE_INSTALL_DIR/claw-expense" ] || fail 'unexpected installation path'
[ -x "$result" ] || fail 'CLI was not installed as executable'
[ "$(wc -l < "$MOCK_CURL_LOG" | tr -d ' ')" = 2 ] || fail 'expected exactly two official release downloads'
result=$("$runner" 'argument with spaces' '--json')
[ "$result" = $'<argument with spaces>\n<--json>' ] || fail 'run.sh did not preserve arguments'
[ "$(wc -l < "$MOCK_CURL_LOG" | tr -d ' ')" = 2 ] || fail 'run.sh downloaded an already installed CLI'

# Skill copy/install tools can discard executable mode bits on script files.
mkdir -p "$case_dir/copied-skill/scripts"
cp "$runner" "$case_dir/copied-skill/scripts/run.sh"
cp "$installer" "$case_dir/copied-skill/scripts/ensure-cli.sh"
cp "$repo_dir/skills/claw-expense/VERSION" "$case_dir/copied-skill/VERSION"
chmod 644 "$case_dir/copied-skill/scripts/run.sh" "$case_dir/copied-skill/scripts/ensure-cli.sh"
result=$(/bin/bash "$case_dir/copied-skill/scripts/run.sh" '--json')
[ "$result" = '<--json>' ] || fail 'run.sh required an executable permission bit on its helper'
[ "$(wc -l < "$MOCK_CURL_LOG" | tr -d ' ')" = 2 ] || fail 'copied Skill downloaded an already installed CLI'

reset_case bad_checksum
export MOCK_SUMS=BAD_SUMS
expect_failure
assert_no_execution
[ ! -e "$CLAW_EXPENSE_INSTALL_DIR/claw-expense" ] || fail 'checksum failure installed a file'
grep -q 'SHA-256 verification failed' "$case_dir/stderr" || fail 'checksum failure was not explained'

reset_case occupied_target
mkdir "$CLAW_EXPENSE_INSTALL_DIR"
printf 'existing user data\n' > "$CLAW_EXPENSE_INSTALL_DIR/claw-expense"
expect_failure
[ "$(<"$CLAW_EXPENSE_INSTALL_DIR/claw-expense")" = 'existing user data' ] || fail 'existing file was overwritten'
assert_no_download
assert_no_execution

reset_case unsupported_platform
export MOCK_OS=Linux
expect_failure
assert_no_download
grep -q 'macOS ARM64 only' "$case_dir/stderr" || fail 'unsupported platform was not explained'

reset_case rosetta
export MOCK_ARCH=x86_64
expect_failure
assert_no_download
grep -q 'Rosetta' "$case_dir/stderr" || fail 'Rosetta rejection was not explained'

reset_case invalid_tag
export CLAW_EXPENSE_VERSION="../$release_tag"
expect_failure
assert_no_download

reset_case wrong_binary
export MOCK_FILE_TYPE='POSIX shell script, ASCII text executable'
expect_failure
assert_no_execution
[ ! -e "$CLAW_EXPENSE_INSTALL_DIR/claw-expense" ] || fail 'wrong architecture was installed'

reset_case wrong_version
export MOCK_BINARY_VERSION="$release_version-wrong"
expect_failure
[ ! -e "$CLAW_EXPENSE_INSTALL_DIR/claw-expense" ] || fail 'wrong release version was installed'

reset_case extra_archive_member
printf 'unexpected\n' > "$fixture_dir/package/extra"
tar -czf "$fixture_dir/extra.tar.gz" -C "$fixture_dir/package" claw-expense LICENSE README.md extra
hash=$(shasum -a 256 "$fixture_dir/extra.tar.gz" | awk '{ print $1 }')
printf '%s  %s\n' "$hash" "$asset" > "$fixture_dir/EXTRA_SUMS"
export MOCK_ARCHIVE=extra.tar.gz MOCK_SUMS=EXTRA_SUMS
expect_failure
assert_no_execution

reset_case archive_symlink
mkdir "$fixture_dir/link-package"
cp "$fixture_dir/package/claw-expense" "$fixture_dir/link-package/claw-expense"
cp "$fixture_dir/package/README.md" "$fixture_dir/link-package/README.md"
ln -s /etc/passwd "$fixture_dir/link-package/LICENSE"
tar -czf "$fixture_dir/link.tar.gz" -C "$fixture_dir/link-package" claw-expense LICENSE README.md
hash=$(shasum -a 256 "$fixture_dir/link.tar.gz" | awk '{ print $1 }')
printf '%s  %s\n' "$hash" "$asset" > "$fixture_dir/LINK_SUMS"
export MOCK_ARCHIVE=link.tar.gz MOCK_SUMS=LINK_SUMS
expect_failure
assert_no_execution

reset_case wrong_checksum_name
hash=$(shasum -a 256 "$fixture_dir/$asset" | awk '{ print $1 }')
printf '%s  other-release.tar.gz\n' "$hash" > "$fixture_dir/OTHER_SUMS"
export MOCK_SUMS=OTHER_SUMS
expect_failure
assert_no_execution

printf 'Installer tests passed (14 scenarios; no network or user installation paths used).\n'
