#!/usr/bin/env bash
# ABI round-trip check between the worker service and the VCI simulator (7.1.2, 13.4).
#
# Verifies that the sim-vci cdylib built for the target exports every J2534
# entry point the worker service resolves, and its control function. On Windows
# targets the names must be undecorated: the worker looks them up as plain names,
# so a stdcall export decorated as `_Name@N` would not resolve. Windows targets
# need llvm-objdump (llvm-mingw) on PATH.
#
# With LAUNCH=1, on a Linux target, it then runs the 7.3 launch test: the
# j2534-0404-service built for the target is launched against that sim-vci
# (ARM targets under qemu-user, with the target's libraries from the cross
# sysroot), with the `unsigned long` width the ABI interpretation table (7.1.2)
# gives for the library, and the host-built end-to-end test checks the version
# strings and reads the VIN through it (tests/sim_vci_end_to_end.rs). Both
# crates must be built for the target in the debug profile, which keeps the
# VCI_CONFIG_PATH override the test uses (ADR-073).
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
# The ABI the library must have, and how to run a program built for the target.
case "$TARGET" in
  x86_64-unknown-linux-gnu) ABI=linux-x86_64; RUNNER=() ;;
  i686-unknown-linux-gnu) ABI=linux-x86; RUNNER=() ;;
  aarch64-unknown-linux-gnu) ABI=linux-arm64; RUNNER=(qemu-aarch64 -L /usr/aarch64-linux-gnu) ;;
  armv7-unknown-linux-gnueabihf) ABI=linux-armhf; RUNNER=(qemu-arm -L /usr/arm-linux-gnueabihf) ;;
  *)
    echo "error: LAUNCH=1 supports the Linux worker targets only, not $TARGET" >&2
    exit 1 ;;
esac
SERVICE="$PWD/target/${TARGET}/${PROFILE}/j2534-0404-service"
if [[ ! -x "$SERVICE" ]]; then
  echo "error: $SERVICE not found. Build it first: cargo build -p j2534-0404-service --target $TARGET" >&2
  exit 1
fi
# worker-host starts the service as one executable, so a target that needs an
# emulator gets a launcher that runs the service under it.
LAUNCHER="$(mktemp -d)/j2534-0404-service"
trap 'rm -rf "$(dirname "$LAUNCHER")"' EXIT
{
  echo '#!/bin/sh'
  printf 'exec'
  printf ' %q' "${RUNNER[@]}" "$SERVICE"
  echo ' "$@"'
} >"$LAUNCHER"
chmod +x "$LAUNCHER"
NGR_ABI_SERVICE="$LAUNCHER" NGR_ABI_LIBRARY="$PWD/$LIB" NGR_ABI_EXPECT="$ABI" \
  cargo test --locked -p j2534-0404-service --test sim_vci_end_to_end -- --nocapture
echo "ABI launch test: passed for $TARGET ($ABI)"
