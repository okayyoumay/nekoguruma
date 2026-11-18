#!/usr/bin/env bash
# Tests scripts/fetch-rustfs.sh against a pin file and release assets that the test makes itself
# (a local directory read through a file:// URL, so nothing is downloaded from the network):
# the download is skipped when the binary is in place and right, a wrong binary is replaced, a
# failed download or a wrong hash leaves nothing at the final path, an unpinned platform and a
# missing tool are named before anything is downloaded, and only the named archive member is
# extracted, mode 0755, in a directory that group and others cannot write. Needs bash, curl,
# unzip, python3 (to write the archive) and a SHA-256 tool, as the script does.
#
# Usage: scripts/test-fetch-rustfs.sh
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/fetch-rustfs.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

failures=0
pass() { echo "ok   $1"; }
bad() {
  echo "FAIL $1" >&2
  failures=$((failures + 1))
}
check() { # check <name> <command...>: passes when the command succeeds
  local name="$1"
  shift
  if "$@"; then pass "$name"; else bad "$name"; fi
}

sha() {
  local sum
  if command -v sha256sum > /dev/null 2>&1; then sum="$(sha256sum "$1")"; else sum="$(shasum -a 256 "$1")"; fi
  echo "${sum%% *}"
}

mode_of() {
  stat -c %a "$1" 2> /dev/null || stat -f %Lp "$1"
}

# The release: one archive with the member `rustfs` and another file that must not be extracted.
assets="$tmp/assets"
mkdir -p "$assets"
printf '#!/bin/sh\necho a fake object server\n' > "$tmp/rustfs"
printf 'not wanted\n' > "$tmp/other"
python3 -I - "$assets/a.zip" "$tmp/rustfs" "$tmp/other" << 'PY'
import sys, zipfile
out, member, other = sys.argv[1:4]
with zipfile.ZipFile(out, "w") as z:
    z.write(member, "rustfs")
    z.write(other, "other")
PY
asset_sha="$(sha "$assets/a.zip")"
bin_sha="$(sha "$tmp/rustfs")"

write_pin() { # write_pin <file> <asset sha>
  cat > "$1" << PIN
# a test pin
version 9.9.9
platform test-x86_64 a.zip $2 rustfs $bin_sha
PIN
}
pin="$tmp/pin.txt"
write_pin "$pin" "$asset_sha"

# Runs the script in a fresh install directory; sets $out, $err and $code.
run() { # run <install dir> [VAR=value ...]
  local root="$1"
  shift
  code=0
  out="$(env NGR_RUSTFS_PIN="$pin" NGR_RUSTFS_PLATFORM=test-x86_64 NGR_RUSTFS_DIR="$root" \
    NGR_RUSTFS_BASE_URL="file://$assets" "$@" bash "$script" 2> "$tmp/err")" || code=$?
  err="$(cat "$tmp/err")"
}

# F1, F7: a first run downloads and installs; a second run with no release at all downloads nothing.
root="$tmp/r1"
run "$root"
final="$root/9.9.9/rustfs"
check "F1 first run exits 0 and prints the binary's path only" test "$code" -eq 0 -a "$out" = "$final"
check "F1 the binary is the member" test "$(sha "$final")" = "$bin_sha"
run "$root" NGR_RUSTFS_BASE_URL="file://$tmp/nowhere"
check "F1 a binary in place with the right hash needs no download" test "$code" -eq 0 -a "$out" = "$final"
check "F7 the binary is mode 755" test "$(mode_of "$final")" = 755
check "F7 its directory is not writable by group or others" test "$(mode_of "$root/9.9.9")" = 755
check "F7 only the named member is extracted, and no temporary file is left" \
  test "$(ls -A "$root/9.9.9")" = rustfs

# F2: a binary with another hash is replaced.
echo tampered > "$final"
run "$root"
check "F2 a binary with the wrong hash is replaced" test "$code" -eq 0 -a "$(sha "$final")" = "$bin_sha"

# F3: a download that fails leaves nothing.
root="$tmp/r3"
run "$root" NGR_RUSTFS_BASE_URL="file://$tmp/nowhere"
check "F3 a failed download exits non-zero" test "$code" -ne 0
check "F3 nothing is at the final path and no temporary file is left" \
  test ! -e "$root/9.9.9/rustfs" -a -z "$(ls -A "$root/9.9.9" 2> /dev/null)"

# F4: a downloaded hash that differs is refused and removed, with both values shown.
root="$tmp/r4"
other_sha="$(printf 'x' | { sha256sum 2> /dev/null || shasum -a 256; } | cut -d' ' -f1)"
write_pin "$pin" "$other_sha"
run "$root"
check "F4 a wrong asset hash exits non-zero" test "$code" -ne 0
check "F4 the expected and the actual hash are shown" \
  bash -c '[[ "$1" == *"$2"* && "$1" == *"$3"* ]]' _ "$err" "$other_sha" "$asset_sha"
check "F4 nothing is left" test ! -e "$root/9.9.9/rustfs" -a -z "$(ls -A "$root/9.9.9")"
# A member that differs from the pinned binary hash is refused too.
cat > "$pin" << PIN
version 9.9.9
platform test-x86_64 a.zip $asset_sha rustfs $other_sha
PIN
root="$tmp/r4b"
run "$root"
check "F4 a wrong member hash exits non-zero and leaves nothing" \
  test "$code" -ne 0 -a ! -e "$root/9.9.9/rustfs" -a -z "$(ls -A "$root/9.9.9")"
write_pin "$pin" "$asset_sha"

# F5: a platform with no row is refused by name.
run "$tmp/r5" NGR_RUSTFS_PLATFORM=nosuch-arch
check "F5 an unpinned platform exits non-zero naming it" \
  bash -c '[ "$1" -ne 0 ] && [[ "$2" == *nosuch-arch* ]]' _ "$code" "$err"

# F6: a missing tool is named before anything is downloaded or made.
tools="$tmp/tools"
mkdir -p "$tools"
for t in bash dirname uname grep mkdir chmod mktemp rm mv tr cat cut ls env; do
  p="$(command -v "$t")" && ln -sf "$p" "$tools/$t"
done
for missing in curl unzip sha256sum; do
  bin="$tmp/bin-$missing"
  mkdir -p "$bin"
  cp -P "$tools"/* "$bin"/
  for t in curl unzip sha256sum; do
    if [ "$t" != "$missing" ]; then
      p="$(command -v "$t")" && ln -sf "$p" "$bin/$t"
    fi
  done
  # shasum would stand in for a missing sha256sum, so it is never on this PATH.
  root="$tmp/r6-$missing"
  code=0
  err="$(env -i PATH="$bin" NGR_RUSTFS_PIN="$pin" NGR_RUSTFS_PLATFORM=test-x86_64 \
    NGR_RUSTFS_DIR="$root" NGR_RUSTFS_BASE_URL="file://$assets" \
    "$bin/bash" "$script" 2>&1 > /dev/null)" || code=$?
  check "F6 a missing $missing exits non-zero naming it" \
    bash -c '[ "$1" -ne 0 ] && [[ "$2" == *"$3"* ]]' _ "$code" "$err" "$missing"
  check "F6 a missing $missing is found before anything is made" test ! -e "$root"
done

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed" >&2
  exit 1
fi
echo "all checks passed"
