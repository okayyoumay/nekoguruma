# The procedure the IR golden test lowers for every variant of the example ECU and runs against
# a host that records each request and answers it positively from responses.txt. The VIN write
# is a section function (subset 4.5) whose helper is also called outside it, so the program holds
# two copies of the helper and a section over the write's copies (ADR-301, ADR-329). The security
# access sends the profile's seed and key requests, which no declared service answers, so the
# host answers them with their bare positive response. `main` names every precondition, so the
# program carries all five of the variant's `preconditions.csv` beside its `identity.csv`. The
# `write_uint` and `read_uint` take an offset known only at run time, so they lower to their
# per-byte sequences (subset 2.4). The DTC request's status mask is computed from a response, so
# its operand is written at run time and range-checked before it is sent (subset 3.2, ADR-341).

def read_vin():
    diag.request(0x22, {"did": 0xF190})

# @section uninterruptible expected=2000
def write_vin():
    read_vin()
    diag.request(0x2E, {"did": 0xF190, "vin": "WVWZZZ1JZXW000001"})

# @preconditions voltage externalSupply ignition engine vehicleSpeed
def main():
    diag.request(0x10, {"sub": 0x03})
    read_vin()
    session = diag.request(0x22, {"did": 0xF186})
    if session["fields"]["session"] == 3:
        diag.log(1, "extended session")
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
    diag.log(1, "done")
