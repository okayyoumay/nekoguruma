#!/usr/bin/env bash
# Fetches the pinned RustFS binary for this machine and checks it (ADR-491): the one place a
# release is downloaded and hashed, for installations and for CI. The server itself never hashes
# the binary; it only refuses one that group or others can write.
#
# Usage: scripts/fetch-rustfs.sh
#   Prints the absolute path of the binary on standard output, and nothing else there. Messages
#   go to standard error. Exit 0 with the binary in place and its hash right, 1 otherwise.
#
# What it does: reads scripts/rustfs-pin.txt for this platform (linux, macos or windows, and
# x86_64 or aarch64); if the binary is in place and its SHA-256 is the pinned one it stops
# (no download); otherwise it downloads the pinned asset, checks the asset's SHA-256, extracts
# only the one named member, checks the member's SHA-256, makes it mode 0755 and moves it into
# place. A failed download or a wrong hash leaves nothing at the final path.
#
# Environment (for tests and for installations that mirror the release):
#   NGR_RUSTFS_PIN       the pin file (default: rustfs-pin.txt beside this script)
#   NGR_RUSTFS_BASE_URL  where the assets are (default: the release's download URL on GitHub)
#   NGR_RUSTFS_DIR       the directory the version's directory is made in (default: target/rustfs
#                        of the repository); the binary is <dir>/<version>/<member>
#   NGR_RUSTFS_PLATFORM  the platform name instead of the detected one, e.g. linux-x86_64
set -euo pipefail
umask 022

here="$(cd "$(dirname "$0")" && pwd)"
pin="${NGR_RUSTFS_PIN:-$here/rustfs-pin.txt}"

fail() {
  echo "fetch-rustfs: $*" >&2
  exit 1
}

# Tools first, so that a missing one is named before anything is downloaded.
if ! command -v curl > /dev/null 2>&1; then
  fail "curl is required but not installed"
fi
if command -v sha256sum > /dev/null 2>&1; then
  hash_tool=sha256sum
elif command -v shasum > /dev/null 2>&1; then
  hash_tool=shasum
else
  fail "sha256sum (or shasum) is required but not installed"
fi
if command -v unzip > /dev/null 2>&1; then
  unzip_tool=unzip
elif command -v bsdtar > /dev/null 2>&1; then
  unzip_tool=bsdtar
elif command -v 7z > /dev/null 2>&1; then
  unzip_tool=7z
else
  fail "unzip (or bsdtar or 7z) is required but not installed"
fi

sha256_of() {
  local sum
  if [ "$hash_tool" = sha256sum ]; then
    sum="$(sha256sum "$1")"
  else
    sum="$(shasum -a 256 "$1")"
  fi
  echo "${sum%% *}"
}

platform="${NGR_RUSTFS_PLATFORM:-}"
if [ -z "$platform" ]; then
  case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    MINGW* | MSYS* | CYGWIN*) os=windows ;;
    *) os="$(uname -s | tr '[:upper:]' '[:lower:]')" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) arch="$(uname -m)" ;;
  esac
  platform="$os-$arch"
fi

[ -r "$pin" ] || fail "cannot read the pin file $pin"
version=""
asset=""
asset_sha=""
member=""
bin_sha=""
while read -r kind a b c d e _; do
  case "$kind" in
    version) version="$a" ;;
    platform)
      if [ "$a" = "$platform" ]; then
        asset="$b"
        asset_sha="$c"
        member="$d"
        bin_sha="$e"
      fi
      ;;
  esac
done < <(grep -v '^[[:space:]]*#' "$pin")
[ -n "$version" ] || fail "the pin file $pin names no version"
if [ -z "$asset" ] || [ -z "$member" ] || [ -z "$asset_sha" ] || [ -z "$bin_sha" ]; then
  fail "no pinned RustFS $version binary for the platform $platform (see $pin)"
fi

root="${NGR_RUSTFS_DIR:-$here/../target/rustfs}"
mkdir -p "$root"
dest="$(cd "$root" && pwd)/$version"
final="$dest/$member"

if [ -f "$final" ] && [ "$(sha256_of "$final")" = "$bin_sha" ]; then
  echo "$final"
  exit 0
fi

base="${NGR_RUSTFS_BASE_URL:-https://github.com/rustfs/rustfs/releases/download/$version}"
mkdir -p "$dest"
chmod 0755 "$dest"
tmp="$(mktemp -d "$dest/.fetch.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

echo "fetch-rustfs: downloading $asset" >&2
if ! curl --fail --silent --show-error --location --retry 3 --connect-timeout 30 \
  --output "$tmp/asset.zip" "$base/$asset"; then
  fail "cannot download $base/$asset"
fi
got="$(sha256_of "$tmp/asset.zip")"
if [ "$got" != "$asset_sha" ]; then
  fail "the downloaded $asset has the wrong SHA-256: expected $asset_sha, got $got"
fi

# Only the named member is read out of the archive, never the whole archive extracted.
if [ "$unzip_tool" = unzip ]; then
  unzip -p "$tmp/asset.zip" "$member" > "$tmp/member" || fail "cannot extract $member from $asset"
elif [ "$unzip_tool" = bsdtar ]; then
  bsdtar -xOf "$tmp/asset.zip" "$member" > "$tmp/member" || fail "cannot extract $member from $asset"
else
  7z e -so "$tmp/asset.zip" "$member" > "$tmp/member" || fail "cannot extract $member from $asset"
fi
got="$(sha256_of "$tmp/member")"
if [ "$got" != "$bin_sha" ]; then
  fail "the extracted $member has the wrong SHA-256: expected $bin_sha, got $got"
fi
chmod 0755 "$tmp/member"
mv -f "$tmp/member" "$final"
echo "$final"
