# The procedure the IR golden test lowers for every variant of the example ECU and runs against
# a host that records each request and answers it positively from responses.txt. The VIN write
# is a section function (subset 4.5) whose helper is also called outside it, so the program holds
# two copies of the helper and a section over the write's copies (ADR-301, ADR-329). The security
# access sends the profile's seed and key requests, which `securityAccess` declares without
# fields, so the host answers them from responses.txt. `main` names every precondition, so the
# program carries all five of the variant's `preconditions.csv` beside its `identity.csv`. The
# `write_uint` and `read_uint` take an offset known only at run time, so they lower to their
# per-byte sequences (subset 2.4). The DTC request's status mask is computed from a response, so
# its operand is written at run time and range-checked before it is sent (subset 3.2, ADR-341).
# `write_application` is a flash plan for session 1 of the example ECU (subset 4.9, ADR-336): the
# lowering expands it in place at its call in `main`, the program holds its `FlashRecovery`
# (`Program.flash`) and the transfer's loop of `FlashBlock` and `FlashTransfer`, and the host
# serves a fake artifact and sums the blocks it is given. `diag.hmi` asks the operator to confirm
# the session, with a constant and a run-time param, so it lowers to `HmiRequestFromStack`, and
# `diag.record` lowers to `RecordInput`; the host writes each form it is given and answers it
# (subset 3.1, 3.4, ADR-340). The plan calls a plain helper with a parameter and a return value,
# twice, and a section function, all expanded in place in the plan (subset 4.9, ADR-336 item 3):
# the plan's range holds no call, and the section function's pieces lie inside the plan's.

def read_vin():
    diag.request(0x22, {"did": 0xF190})

# @section uninterruptible expected=2000
def write_vin():
    read_vin()
    diag.request(0x2E, {"did": 0xF190, "vin": "WVWZZZ1JZXW000001"})

def routine_passed(what):
    result = diag.routine(0xFF01, 0x01, {})
    if not result["ok"]:
        diag.log(1, what + " refused")
    return result["ok"]

# @section uninterruptible expected=500
def reset_ecu():
    reset = diag.request(0x11, {"reset": 0x01})
    if not reset["ok"]:
        diag.fail("RESET_REFUSED", "the ECU reset was refused")

# @section uninterruptible expected=120000
# @flashPlan session=1
def write_application():
    diag.request(0x10, {"sub": 0x02})
    diag.security_access(0x01)
    # @erase
    diag.routine(0xFF00, 0x01, {})
    exit = diag.flash_transfer(1)
    if not exit["ok"]:
        diag.fail("TRANSFER_EXIT_REFUSED", "the transfer exit was refused")
    if not routine_passed("the dependencies check"):
        diag.fail("CHECK_FAILED", "the programming dependencies check failed")
    if not routine_passed("the second check"):
        diag.fail("CHECK_FAILED", "the second dependencies check failed")
    reset_ecu()

# @preconditions voltage externalSupply ignition engine vehicleSpeed
def main():
    diag.request(0x10, {"sub": 0x03})
    read_vin()
    session = diag.request(0x22, {"did": 0xF186})
    if session["fields"]["session"] == 3:
        diag.log(1, "extended session")
    confirm = diag.hmi("confirm_session", {"title": "Confirm the session", "session": session["fields"]["session"]})
    if not confirm["ok"]:
        diag.fail("OPERATOR_DECLINED", "the operator did not confirm the session")
    frame = write_uint(b"\x00\x00\x00", session["fields"]["session"] - 3, 16, 0x1234)
    if read_uint(frame, 0, 16) == 0x1234:
        diag.log(1, "uint round trip")
    diag.request(0x19, {"sub": 0x02, "statusMask": session["fields"]["session"] + 6})
    dtcs = diag.read_dtc(0x09)
    if dtcs["ok"] and dtcs["fields"]["mask"] == 0xFF:
        diag.log(1, "all DTC status bits supported")
        records = dtcs["fields"]["dtcs"]
        if len(records) == 2 and records[1]["code"] == 0xC0FFEE and records[1]["status"] == 0x28:
            diag.log(1, "two DTCs")
    check = diag.routine(0xFF01, 0x01, {})
    if check["ok"] and check["fields"]["result"] == 0:
        diag.log(1, "programming dependencies correct")
    unlock = diag.security_access(0x01)
    if unlock["ok"]:
        diag.log(1, "unlocked")
    write_vin()
    write_application()
    diag.wait(2000)
    record = diag.record("final_check")
    if record["ok"] and record["value"] == "ok":
        diag.log(1, "record passed")
    diag.log(1, "done")
