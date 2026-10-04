// Example: verification procedure after ECU reprogramming
// For the supported subset, see docs/js-subset.md

function readSwVersion() {
  const res = diag.request(0x22, { did: 0xF195 });
  if (!res.ok) {
    diag.fail("READ_SW_VERSION_FAILED", "NRC=" + res.nrc);
  }
  return res.fields.swVersion;
}

// Check preconditions (8.9)
const volt = diag.precondition("batteryMilliVolt");
if (volt < 12500) {
  diag.fail("POWER_CONDITION", "Insufficient voltage: " + volt + "mV");
}

// Transition to extended session
// @section interruptible
const session = diag.request(0x10, { sub: 0x03 });
if (!session.ok) {
  diag.fail("SESSION_FAILED", "NRC=" + session.nrc);
}
// @endsection

// Write (uninterruptible; expected 2 minutes)
// @section uninterruptible expected=120000
// @idempotency checkState
diag.securityAccess(0x11);
diag.flashTransfer(1);
// @endsection

// Read-back verification
const after = readSwVersion();
diag.log(1, "Version after write: " + after);

// Capture the minimum voltage from monitoring (4.6)
diag.capture(10000);

// Request work record input (4.3.1)
diag.record("com.example.record.reprogram-check");
