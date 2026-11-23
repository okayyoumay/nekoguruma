#!/usr/bin/env bash
# Fetches the pinned llvm-mingw toolchain and checks it: the C compilers for the
# *-pc-windows-gnullvm targets, which the Windows type check needs for the crates that build C
# (the TLS provider, ADR-505) and which CI's worker-windows job cross-builds with.
#
# Usage: scripts/fetch-llvm-mingw.sh
#   Prints the absolute path of the toolchain's bin directory on standard output, and nothing
#   else there; put it on PATH. Messages go to standard error. Exit 0 with the toolchain in place,
#   1 otherwise. Linux x86_64 only (the asset is built for it).
#
# What it does: reads scripts/llvm-mingw-pin.txt; if the version's directory holds a toolchain
# unpacked from an asset with the pinned SHA-256 it stops (no download); otherwise it downloads
# the asset, checks its SHA-256, unpacks it into a temporary directory and moves it into place.
# A failed download or a wrong hash leaves nothing at the final path.
#
# Environment (for tests and for mirrors):
#   NGR_LLVM_MINGW_PIN       the pin file (default: llvm-mingw-pin.txt beside this script)
#   NGR_LLVM_MINGW_BASE_URL  where the asset is (default: the release's download URL on GitHub)
#   NGR_LLVM_MINGW_DIR       the directory the version's directory is made in (default:
#                            target/llvm-mingw of the repository)
set -euo pipefail
umask 022

here="$(cd "$(dirname "$0")" && pwd)"
pin="${NGR_LLVM_MINGW_PIN:-$here/llvm-mingw-pin.txt}"

fail() {
  echo "fetch-llvm-mingw: $*" >&2
  exit 1
}

for tool in curl sha256sum tar xz; do
  command -v "$tool" > /dev/null 2>&1 || fail "$tool is required but not installed"
done
[ "$(uname -s)-$(uname -m)" = Linux-x86_64 ] || fail "only Linux x86_64 is supported"

[ -r "$pin" ] || fail "cannot read the pin file $pin"
version=""
asset=""
asset_sha=""
top=""
while read -r kind a b c _; do
  case "$kind" in
    version) version="$a" ;;
    asset)
      asset="$a"
      asset_sha="$b"
      top="$c"
      ;;
  esac
done < <(grep -v '^[[:space:]]*#' "$pin")
if [ -z "$version" ] || [ -z "$asset" ] || [ -z "$asset_sha" ] || [ -z "$top" ]; then
  fail "the pin file $pin names no version or no asset"
fi

root="${NGR_LLVM_MINGW_DIR:-$here/../target/llvm-mingw}"
mkdir -p "$root"
dest="$(cd "$root" && pwd)/$version"
# The asset's hash is kept beside the unpacked toolchain: a directory without it, or with
# another, is fetched again.
if [ -x "$dest/$top/bin/x86_64-w64-mingw32-clang" ] && [ -f "$dest/.asset-sha256" ] &&
  [ "$(cat "$dest/.asset-sha256")" = "$asset_sha" ]; then
  echo "$dest/$top/bin"
  exit 0
fi

base="${NGR_LLVM_MINGW_BASE_URL:-https://github.com/mstorsjo/llvm-mingw/releases/download/$version}"
tmp="$(mktemp -d "$root/.fetch.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

echo "fetch-llvm-mingw: downloading $asset" >&2
if ! curl --fail --silent --show-error --location --retry 3 --connect-timeout 30 \
  --output "$tmp/asset" "$base/$asset"; then
  fail "cannot download $base/$asset"
fi
got="$(sha256sum "$tmp/asset")"
got="${got%% *}"
if [ "$got" != "$asset_sha" ]; then
  fail "the downloaded $asset has the wrong SHA-256: expected $asset_sha, got $got"
fi
mkdir "$tmp/unpacked"
tar -xJf "$tmp/asset" -C "$tmp/unpacked" || fail "cannot unpack $asset"
[ -x "$tmp/unpacked/$top/bin/x86_64-w64-mingw32-clang" ] ||
  fail "$asset holds no $top/bin/x86_64-w64-mingw32-clang"
echo "$asset_sha" > "$tmp/unpacked/.asset-sha256"
rm -rf "$dest"
mv "$tmp/unpacked" "$dest"
echo "$dest/$top/bin"
