#!/usr/bin/env bash
# ABI round-trip check between the worker service and the VCI simulator (7.1.2, 13.4).
#
# Current scope: verifies that the sim-vci cdylib built for the target exports
# every J2534 entry point the worker service resolves, and its control function.
# On Windows targets the names must be undecorated: the worker looks them up as
# plain names, so a stdcall export decorated as `_Name@N` would not resolve.
# Windows targets need llvm-objdump (llvm-mingw) on PATH.
# TODO: run the 7.3 launch test (j2534-0404-service -> GetVersion ->
#       PassThruReadVersion) against the library and compare the returned
#       version strings. Run ARM targets under qemu-user.
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
