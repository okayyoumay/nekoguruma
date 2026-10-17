# The routine procedure of the end-to-end definition test (M2 exit criterion 1): the programming
# session, whose P2* the response gives in tens of milliseconds (a linear conversion), then the
# check-programming-dependencies routine, which the simulated ECU refuses with
# requestSequenceError (0x24) while no image has been transferred. It changes the session, which
# a debug-build agent sends only to `sim-vci` (ADR-247).

def main():
    session = diag.request(0x10, {"sub": 0x02})
    if not session["ok"] or session["fields"]["p2StarServerMax"] != 5000.0:
        diag.fail("SESSION", "the programming session gave another P2*")
    check = diag.routine(0xFF01, 0x01, {})
    if check["ok"] or check["nrc"] != 0x24:
        diag.fail("ROUTINE", "the routine did not refuse for want of an image")
    diag.log(1, "routine")
