#!/usr/bin/env bash
# ABI round-trip check between the worker service and the VCI simulator (7.1.2, 13.4).
#
# Verifies that the sim-vci cdylib built for the target exports every J2534
# entry point the worker service resolves, and its control function. On Windows
# targets the names must be undecorated: the worker looks them up as plain names,
# so a stdcall export decorated as `_Name@N` would not resolve. Windows targets
# need llvm-objdump (llvm-mingw) on PATH.
#
# With LAUNCH=1 it then runs the 7.3 launch test: the j2534-0404-service built
# for the target is launched against that sim-vci, with the `unsigned long`
# width the ABI interpretation table (7.1.2) gives for the library, and the
# host-built end-to-end test checks the version strings and reads the VIN
# through it (tests/sim_vci_end_to_end.rs). A Linux target runs on a Linux host
# (ARM targets under qemu-user, with the target's libraries from the cross
# sysroot); a Windows target runs on a Windows host under Git Bash (win-x86
# under WOW64). Both crates must be built for the target in the debug profile,
# which keeps the VCI_CONFIG_PATH override the test uses (ADR-073). With
# ABI_TEST set to a prebuilt end-to-end test executable (built with
# `cargo test --no-run` for any target the host runs), the script runs that
# instead of building the test with cargo, so the host needs no Rust toolchain.
set -euo pipefail

TARGET="${1:-i686-unknown-linux-gnu}"
PROFILE="${PROFILE:-debug}"
case "$TARGET" in
  *-windows-*) LIB="target/${TARGET}/${PROFILE}/sim_vci.dll" ;;
  *) LIB="target/${TARGET}/${PROFILE}/libsim_vci.so" ;;
esac

if [[ ! -f "$LIB" ]]; then
  echo "error: $LIB not found. Build it first: cargo build -p sim-vci --target $TARGET" >&2
  exit 1
fi

SYMBOLS=(
  PassThruOpen PassThruClose PassThruConnect PassThruDisconnect
  PassThruReadMsgs PassThruWriteMsgs PassThruStartPeriodicMsg PassThruStopPeriodicMsg
  PassThruStartMsgFilter PassThruStopMsgFilter PassThruSetProgrammingVoltage
  PassThruReadVersion PassThruGetLastError PassThruIoctl
  # Not J2534: the simulator's control function (ADR-238).
  NgrSimVciControl
)

case "$TARGET" in
  *-windows-*)
    # The export table lists one name per line, after the ordinal and the RVA.
    exported="$(llvm-objdump -p "$LIB" | sed -n '/^Export Table:/,/^$/p')"
    pattern_suffix='' ;;
  *)
    # An ELF name may carry a symbol version after `@`.
    exported="$(nm -D --defined-only "$LIB")"
    pattern_suffix='(@.*)?' ;;
esac
missing=0
for sym in "${SYMBOLS[@]}"; do
  if ! grep -qE "[[:space:]]${sym}${pattern_suffix}$" <<<"$exported"; then
    echo "missing export: $sym" >&2
    missing=1
  fi
done

if [[ $missing -ne 0 ]]; then
  echo "exports found:" >&2
  echo "$exported" >&2
  echo "ABI round-trip: FAILED ($LIB)" >&2
  exit 1
fi
echo "ABI round-trip: all ${#SYMBOLS[@]} exports present in $LIB"

if [[ "${LAUNCH:-0}" != 1 ]]; then
  exit 0
fi
if [[ "$PROFILE" != debug ]]; then
  echo "error: LAUNCH=1 needs debug builds (the test's config override is debug-only, ADR-073)" >&2
  exit 1
fi
# The ABI the library must have, and how to run a program built for the target.
case "$TARGET" in
  x86_64-unknown-linux-gnu) ABI=linux-x86_64; RUNNER=() ;;
  i686-unknown-linux-gnu) ABI=linux-x86; RUNNER=() ;;
  aarch64-unknown-linux-gnu) ABI=linux-arm64; RUNNER=(qemu-aarch64 -L /usr/aarch64-linux-gnu) ;;
  armv7-unknown-linux-gnueabihf) ABI=linux-armhf; RUNNER=(qemu-arm -L /usr/arm-linux-gnueabihf) ;;
  x86_64-pc-windows-gnullvm) ABI=win-x64 ;;
  i686-pc-windows-gnullvm) ABI=win-x86 ;;
  *)
    echo "error: LAUNCH=1 supports the six worker targets only, not $TARGET" >&2
    exit 1 ;;
esac
case "$TARGET:$(uname -s)" in
  *-linux-*:Linux | *-windows-*:MINGW* | *-windows-*:MSYS*) ;;
  *)
    echo "error: LAUNCH=1 runs a Linux target on Linux and a Windows target on Windows (Git Bash)" >&2
    exit 1 ;;
esac
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
LIBRARY="$PWD/$LIB"
case "$TARGET" in
  *-windows-*)
    SERVICE="$PWD/target/${TARGET}/${PROFILE}/j2534-0404-service.exe"
    if [[ ! -f "$SERVICE" ]]; then
      echo "error: $SERVICE not found. Build it first: cargo build -p j2534-0404-service --target $TARGET" >&2
      exit 1
    fi
    # The test and the service are Windows programs: give them Windows paths.
    SERVICE="$(cygpath -w "$SERVICE")"
    LIBRARY="$(cygpath -w "$LIBRARY")" ;;
  *)
    SERVICE="$PWD/target/${TARGET}/${PROFILE}/j2534-0404-service"
    if [[ ! -x "$SERVICE" ]]; then
      echo "error: $SERVICE not found. Build it first: cargo build -p j2534-0404-service --target $TARGET" >&2
      exit 1
    fi
    # worker-host starts the service as one executable, so a target that needs an
    # emulator gets a launcher that runs the service under it.
    LAUNCHER="$WORK/j2534-0404-service"
    {
      echo '#!/bin/sh'
      printf 'exec'
      printf ' %q' "${RUNNER[@]}" "$SERVICE"
      echo ' "$@"'
    } >"$LAUNCHER"
    chmod +x "$LAUNCHER"
    SERVICE="$LAUNCHER" ;;
esac
# The test must run, not be filtered or compiled out: require exactly one passed test.
OUTPUT="$WORK/test.log"
if [[ -n "${ABI_TEST:-}" ]]; then
  TEST=("$ABI_TEST")
else
  TEST=(cargo test --locked -p j2534-0404-service --test sim_vci_end_to_end --)
fi
NGR_ABI_SERVICE="$SERVICE" NGR_ABI_LIBRARY="$LIBRARY" NGR_ABI_EXPECT="$ABI" \
  "${TEST[@]}" --exact service_reads_the_vin_from_sim_vci --nocapture 2>&1 | tee "$OUTPUT"
if ! grep -q '^test result: ok. 1 passed;' "$OUTPUT"; then
  echo "ABI launch test: FAILED for $TARGET ($ABI): the test did not run and pass" >&2
  exit 1
fi
echo "ABI launch test: passed for $TARGET ($ABI)"
