# Example: verification procedure after ECU reprogramming
# For the supported subset, see docs/starlark-subset.md

MIN_MILLIVOLT = 12500

def read_sw_version():
    res = diag.request(0x22, {"did": 0xF195})
    if not res["ok"]:
        diag.fail("READ_SW_VERSION_FAILED", "NRC=%d" % res["nrc"])
    return res["fields"]["swVersion"]

def main():
    # Check preconditions (8.9)
    volt = diag.precondition("batteryMilliVolt")
    if volt < MIN_MILLIVOLT:
        diag.fail("POWER_CONDITION", "Insufficient voltage: %dmV" % volt)

    # Transition to extended session
    # @section interruptible
    session = diag.request(0x10, {"sub": 0x03})
    if not session["ok"]:
        diag.fail("SESSION_FAILED", "NRC=%d" % session["nrc"])
    # @endsection

    # Write (uninterruptible; expected 2 minutes)
    # @section uninterruptible expected=120000
    # @idempotency checkState
    diag.security_access(0x11)
    diag.flash_transfer(1)
    # @endsection

    # Read-back verification
    after = read_sw_version()
    diag.log(1, "Version after write: " + after)

    # Capture the minimum voltage from monitoring (4.6)
    diag.capture(10000)

    # Request work record input (4.3.1)
    diag.record("com.example.record.reprogram-check")
