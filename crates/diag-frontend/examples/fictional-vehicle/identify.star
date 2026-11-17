# Reads what identifies one ECU of the fictional vehicle: the active session, the spare part
# number, the software version and the VIN, then the confirmed DTCs. Every ECU of the vehicle
# declares these under the same names, so the one procedure is lowered for each of them. It only
# reads.

def main():
    session = diag.request(0x22, {"did": 0xF186})
    if not session["ok"] or session["fields"]["session"] != 1:
        diag.fail("SESSION", "not in the default session")
    part = diag.request(0x22, {"did": 0xF187})
    version = diag.request(0x22, {"did": 0xF189})
    vin = diag.request(0x22, {"did": 0xF190})
    if not part["ok"] or not version["ok"] or not vin["ok"]:
        diag.fail("IDENTIFICATION", "an identification DID was refused")
    dtcs = diag.read_dtc(0x08)
    if not dtcs["ok"]:
        diag.fail("DTC_READ", "the DTC read failed")
    diag.log(1, "identified")
