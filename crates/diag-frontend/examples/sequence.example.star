# Example: write procedure for the example ECU, with a verification after the reprogramming.
# For the supported subset, see docs/starlark-subset.md; the tables the procedure relies on are
# in example-ecu/.

MIN_MILLIVOLT = 12500

def read_sw_version():
    res = diag.request(0x22, {"did": 0xF189})
    if not res["ok"]:
        diag.fail("READ_SW_VERSION_FAILED", "the software version read was refused")
    return res["fields"]["softwareVersion"]

# The write is a flash plan (subset 4.9, ADR-336): the lowering expands it in place in `main`
# and derives its recovery boundaries from the code. The plan calls no function of the
# procedure, so it needs no inlining, and it is a section that no interruption may split
# (expected 2 minutes). Flash session 1 of the example ECU gives the segment, the block size
# and the recovery timings (csv-tables 4.12).
# @section uninterruptible expected=120000
# @flashPlan session=1
def write_image():
    session = diag.request(0x10, {"sub": 0x02})
    if not session["ok"]:
        diag.fail("SESSION_FAILED", "the programming session was refused")
    unlock = diag.security_access(0x01)
    if not unlock["ok"]:
        diag.fail("UNLOCK_FAILED", "the security access was refused")
    # The erase point: nothing before it is lost if the write restarts, and a restart that finds
    # the ECU past it ends in on-site intervention.
    # @erase
    erase = diag.routine(0xFF00, 0x01, {})
    if not erase["ok"]:
        diag.fail("ERASE_FAILED", "the erase was refused")
    exit = diag.flash_transfer(1)
    if not exit["ok"]:
        diag.fail("TRANSFER_EXIT_REFUSED", "the transfer exit was refused")
    check = diag.routine(0xFF01, 0x01, {})
    if not check["ok"] or check["fields"]["result"] != 0:
        diag.fail("CHECK_FAILED", "the programming dependencies are not correct")
    diag.request(0x11, {"reset": 0x01})

# Preconditions (8.9): the program carries these three from preconditions.csv, and the agent
# checks them before the plan's entry.
# @preconditions voltage externalSupply ignition
def main():
    volt = diag.precondition("voltage")
    if volt < MIN_MILLIVOLT:
        diag.fail("POWER_CONDITION", "the supply voltage is too low")

    write_image()

    # The plan ends with the ECU reset; the agent waits the session's startup time only when it
    # restarts a write, so the read-back waits for the ECU here.
    diag.wait(2000)

    # Read-back verification
    after = read_sw_version()
    diag.log(1, "Version after write: " + after)

    # Capture the minimum voltage from monitoring (4.6)
    diag.capture(10000)
