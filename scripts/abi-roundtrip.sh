#!/usr/bin/env bash
# ABI round-trip check between the worker service and the VCI simulator (7.1.2, 13.4).
#
# Current scope: verifies that the sim-vci cdylib built for the target exports
# every J2534 entry point the worker service resolves.
# TODO: run the 7.3 launch test (j2534-0404-service -> GetVersion ->
#       PassThruReadVersion) against the library and compare the returned
#       version strings. Run ARM targets under qemu-user.
set -euo pipefail

TARGET="${1:-i686-unknown-linux-gnu}"
PROFILE="${PROFILE:-debug}"
LIB="target/${TARGET}/${PROFILE}/libsim_vci.so"

if [[ ! -f "$LIB" ]]; then
  echo "error: $LIB not found. Build it first: cargo build -p sim-vci --target $TARGET" >&2
  exit 1
fi

SYMBOLS=(
  PassThruOpen PassThruClose PassThruConnect PassThruDisconnect
  PassThruReadMsgs PassThruWriteMsgs PassThruStartPeriodicMsg PassThruStopPeriodicMsg
  PassThruStartMsgFilter PassThruStopMsgFilter PassThruSetProgrammingVoltage
  PassThruReadVersion PassThruGetLastError PassThruIoctl
)

exported="$(nm -D --defined-only "$LIB")"
missing=0
for sym in "${SYMBOLS[@]}"; do
  if ! grep -qE "[[:space:]]${sym}(@.*)?$" <<<"$exported"; then
    echo "missing export: $sym" >&2
    missing=1
  fi
done

if [[ $missing -ne 0 ]]; then
  echo "ABI round-trip: FAILED ($LIB)" >&2
  exit 1
fi
echo "ABI round-trip: all ${#SYMBOLS[@]} J2534 exports present in $LIB"
