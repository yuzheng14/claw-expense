#!/bin/bash
# Discover an existing CLI or install the pinned official macOS ARM64 release.
# stdout is reserved for the absolute executable path.
set -euo pipefail
umask 077

fail() {
  printf 'claw-expense: %s\n' "$*" >&2
  exit 1
}

absolute_file() {
  local parent name
  parent=$(dirname -- "$1")
  name=$(basename -- "$1")
  (CDPATH= cd -- "$parent" && printf '%s/%s\n' "$(pwd -P)" "$name")
}

usable_cli() {
  local reported
  [ -f "$1" ] && [ -x "$1" ] || return 1
  reported=$("$1" --version 2>/dev/null) || return 1
  [[ "$reported" =~ ^claw-expense\ [0-9]+\.[0-9]+\.[0-9]+$ ]]
}

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
install_dir=${CLAW_EXPENSE_INSTALL_DIR:-${HOME:?HOME is required}/.local/bin}
case "$install_dir" in
  /*) ;;
  *) install_dir="$PWD/$install_dir" ;;
esac
target="$install_dir/claw-expense"

existing=$(type -P claw-expense || true)
if [ -n "$existing" ] && usable_cli "$existing"; then
  absolute_file "$existing"
  exit 0
fi
if usable_cli "$target"; then
  absolute_file "$target"
  exit 0
fi

version=${CLAW_EXPENSE_VERSION:-$(<"$script_dir/../VERSION")}
[[ "$version" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  fail 'CLAW_EXPENSE_VERSION must be a release tag such as v0.1.0.'

os=$(uname -s)
arch=$(uname -m)
[ "$os" = Darwin ] && [ "$arch" = arm64 ] ||
  fail "Automatic installation supports macOS ARM64 only (found $os $arch). On Apple Silicon, run from a native ARM64 shell; Rosetta is not supported."

if [ -e "$target" ] || [ -L "$target" ]; then
  fail "The installation target already exists but is not a usable CLI: $target. Choose another CLAW_EXPENSE_INSTALL_DIR; this file will not be overwritten."
fi
for tool in curl tar shasum awk file mktemp; do
  command -v "$tool" >/dev/null 2>&1 || fail "Required tool is unavailable: $tool"
done

temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/claw-expense-install.XXXXXX")
staged=''
cleanup() {
  if [ -n "$staged" ]; then rm -f -- "$staged"; fi
  rm -rf -- "$temp_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

asset="claw-expense-$version-aarch64-apple-darwin.tar.gz"
base_url="https://github.com/yuzheng14/claw-expense/releases/download/$version"
archive="$temp_dir/$asset"
checksums="$temp_dir/SHA256SUMS"
printf 'claw-expense: Downloading official release %s for macOS ARM64…\n' "$version" >&2
for name in "$asset" SHA256SUMS; do
  curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --connect-timeout 15 --max-time 180 --retry 2 \
    --output "$temp_dir/$name" "$base_url/$name" ||
    fail "Could not download $name from the official release."
done

expected=$(awk -v asset="$asset" '
  {
    name = $2
    sub(/^\*/, "", name)
    if (name == asset) {
      matches++
      if (NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-fA-F]/) invalid = 1
      hash = tolower($1)
    }
  }
  END {
    if (matches != 1 || invalid) exit 1
    print hash
  }
' "$checksums") || fail "SHA256SUMS must contain exactly one valid entry for $asset."
actual=$(shasum -a 256 "$archive" | awk '{ print tolower($1) }')
[ "$actual" = "$expected" ] || fail 'SHA-256 verification failed; nothing was installed or executed.'

# Only these three regular files may be extracted; links, directories, duplicate
# names, additional executables and paths containing traversal are all rejected.
tar -tzf "$archive" > "$temp_dir/members" || fail 'The downloaded archive cannot be read.'
awk '
  $0 != "claw-expense" && $0 != "LICENSE" && $0 != "README.md" { invalid = 1 }
  { count[$0]++; total++ }
  END {
    if (invalid || total != 3 || count["claw-expense"] != 1 || count["LICENSE"] != 1 || count["README.md"] != 1) exit 1
  }
' "$temp_dir/members" || fail 'The release archive contains unexpected or unsafe paths.'
tar -tvzf "$archive" > "$temp_dir/member-types" || fail 'The archive member types cannot be read.'
awk 'substr($0, 1, 1) != "-" { invalid = 1 } END { if (invalid || NR != 3) exit 1 }' \
  "$temp_dir/member-types" || fail 'The release archive must contain only regular files, without links.'
mkdir "$temp_dir/unpacked"
tar -xzf "$archive" -C "$temp_dir/unpacked" --no-same-owner --no-same-permissions ||
  fail 'The release archive could not be extracted.'
binary="$temp_dir/unpacked/claw-expense"
[ -f "$binary" ] && [ ! -L "$binary" ] || fail 'The archive has no regular CLI executable.'
binary_type=$(file -b "$binary")
[[ "$binary_type" =~ ^Mach-O\ 64-bit\ executable\ arm64($|[[:space:]]) ]] ||
  fail "Expected a Mach-O ARM64 executable, found: $binary_type"
chmod 755 "$binary"
reported=$("$binary" --version) || fail 'The verified executable could not run on this machine.'
[ "$reported" = "claw-expense ${version#v}" ] ||
  fail "Release version mismatch: expected claw-expense ${version#v}, received $reported"

mkdir -p -- "$install_dir"
install_dir=$(CDPATH= cd -- "$install_dir" && pwd -P)
target="$install_dir/claw-expense"
staged=$(mktemp "$install_dir/.claw-expense.XXXXXX")
cp "$binary" "$staged"
chmod 755 "$staged"
# A same-directory hard link publishes the complete file atomically and cannot
# replace an existing destination, including one created by another installer.
if ! ln "$staged" "$target" 2>/dev/null; then
  if usable_cli "$target"; then
    printf '%s\n' "$target"
    exit 0
  fi
  fail "The installation target was created concurrently and was left untouched: $target"
fi
printf 'claw-expense: Installed %s at %s\n' "$version" "$target" >&2
printf '%s\n' "$target"
