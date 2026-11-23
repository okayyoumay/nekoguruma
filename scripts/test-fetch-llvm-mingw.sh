#!/usr/bin/env bash
# Tests scripts/fetch-llvm-mingw.sh against a pin file and an asset that the test makes itself
# (a local directory read through a file:// URL, so nothing is downloaded from the network): the
# toolchain is unpacked and its bin directory printed, a second run downloads nothing, a wrong
# hash and an asset without the compiler leave nothing at the final path, and a toolchain
# unpacked from another asset is fetched again. Needs bash, curl, sha256sum, tar and xz, as the
# script does, on Linux x86_64.
#
# Usage: scripts/test-fetch-llvm-mingw.sh
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/fetch-llvm-mingw.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

failures=0
check() { # check <name> <command...>: passes when the command succeeds
  local name="$1"
  shift
  if "$@"; then echo "ok   $name"; else
    echo "FAIL $name" >&2
    failures=$((failures + 1))
  fi
}
fails() { ! "$@"; }
sha() {
  local sum
  sum="$(sha256sum "$1")"
  echo "${sum%% *}"
}

# Two assets: one with the compiler, one without.
assets="$tmp/assets"
mkdir -p "$assets" "$tmp/good/tc/bin" "$tmp/empty/tc/bin"
printf '#!/bin/sh\necho a fake compiler\n' > "$tmp/good/tc/bin/x86_64-w64-mingw32-clang"
chmod 0755 "$tmp/good/tc/bin/x86_64-w64-mingw32-clang"
tar -cJf "$assets/good.tar.xz" -C "$tmp/good" tc
tar -cJf "$assets/empty.tar.xz" -C "$tmp/empty" tc

write_pin() { # write_pin <file> <asset> <sha>
  printf '# a test pin\nversion 1\nasset %s %s tc\n' "$2" "$3" > "$1"
}
run() { # run <pin> <dir> <base url>: the script's output, or its failure
  NGR_LLVM_MINGW_PIN="$1" NGR_LLVM_MINGW_DIR="$2" NGR_LLVM_MINGW_BASE_URL="$3" "$script" 2> /dev/null
}

good_sha="$(sha "$assets/good.tar.xz")"
write_pin "$tmp/pin" good.tar.xz "$good_sha"
dir="$tmp/out"
out="$(run "$tmp/pin" "$dir" "file://$assets")" || out=""
check "the bin directory is printed" test "$out" = "$dir/1/tc/bin"
check "the compiler is in place" test -x "$dir/1/tc/bin/x86_64-w64-mingw32-clang"
again="$(run "$tmp/pin" "$dir" "file://$tmp/nowhere")" || again=""
check "a second run downloads nothing" test "$again" = "$dir/1/tc/bin"

write_pin "$tmp/wrong" good.tar.xz "$(printf '0%.0s' {1..64})"
check "a wrong hash fails" fails run "$tmp/wrong" "$tmp/out2" "file://$assets"
check "a wrong hash leaves nothing" test ! -e "$tmp/out2/1"

write_pin "$tmp/empty-pin" empty.tar.xz "$(sha "$assets/empty.tar.xz")"
check "an asset without the compiler fails" fails run "$tmp/empty-pin" "$tmp/out3" "file://$assets"
check "an asset without the compiler leaves nothing" test ! -e "$tmp/out3/1"

# A toolchain unpacked from another asset (another recorded hash) is fetched again.
echo "another" > "$dir/1/.asset-sha256"
out="$(run "$tmp/pin" "$dir" "file://$assets")" || out=""
check "another asset's toolchain is fetched again" test "$(cat "$dir/1/.asset-sha256")" = "$good_sha"

check "a failed download fails" fails run "$tmp/pin" "$tmp/out4" "file://$tmp/nowhere"
check "a failed download leaves nothing" test ! -e "$tmp/out4/1"

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed" >&2
  exit 1
fi
echo "all checks passed"
