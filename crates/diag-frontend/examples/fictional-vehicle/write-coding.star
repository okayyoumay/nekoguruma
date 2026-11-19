# Codes cruise control into the engine controller's coding (README 11): reads and saves the
# coding, opens the extended session, unlocks security level 1, writes the coding with the
# cruise-control option bit (bit 1) set and the other fields as read, reads it back, and writes
# the saved coding back when the read-back differs. A coding that already has the bit finishes
# with nothing sent. It is attached to the engine controller only.
#
# The seed is turned into the key with the engine controller's fixed mask, as the simulator
# does, in raw 0x27 requests: the agent refuses `diag.security_access` until the host has a key
# source (ADR-259), so a procedure for the simulators sends the requests itself. The gateway's
# authentication is not a step of the procedure.

KEY_MASK = 0x5A5A5A5A
CRUISE_CONTROL = 0x02
RECORD_LENGTH = 7  # the positive response: SID, the DID and the four-byte record
SEED_LENGTH = 6  # the positive response: SID, the level and the four-byte seed

# True when the coding reads back as the given record. A read that fails or is not the full
# record is not a match.
def coding_is(market, options, idle):
    res = diag.request(0x22, {"did": 0x0120})
    if not res["ok"] or len(res["raw"]) != RECORD_LENGTH:
        return False
    return (res["fields"]["market"] == market and
            res["fields"]["options"] == options and
            res["fields"]["targetIdleSpeed"] == idle)

# Everything after the first write is one section, so that a cancel is held between the write
# and the decision about the roll-back, and the outcome is reported inside it (a crash there ends
# the job on site, README 11).
# @section uninterruptible expected=2000
def change(market, saved, wanted, idle):
    written = diag.request(0x2E, {"did": 0x0120, "market": market, "options": wanted, "targetIdleSpeed": idle})
    if written["ok"]:
        if coding_is(market, wanted, idle):
            diag.log(1, "coding written")
            return
    elif coding_is(market, saved, idle):
        diag.fail("WRITE_REFUSED", "the write was refused and the coding is unchanged")
    undo = diag.request(0x2E, {"did": 0x0120, "market": market, "options": saved, "targetIdleSpeed": idle})
    if not undo["ok"]:
        diag.fail("ROLLBACK_REFUSED", "the saved coding could not be written back")
    if not coding_is(market, saved, idle):
        diag.fail("ROLLBACK_UNCONFIRMED", "the saved coding was written back but did not read back")
    diag.fail("CODING_ROLLED_BACK", "the new coding did not read back; the saved coding is restored")

def main():
    before = diag.request(0x22, {"did": 0x0120})
    if not before["ok"] or len(before["raw"]) != RECORD_LENGTH:
        diag.fail("READ_CODING", "the current coding could not be read")
    market = before["fields"]["market"]
    options = before["fields"]["options"]
    idle = before["fields"]["targetIdleSpeed"]
    if options & CRUISE_CONTROL != 0:
        diag.log(1, "cruise control is already coded")
        return
    session = diag.request(0x10, {"sub": 0x03})
    if not session["ok"]:
        diag.fail("SESSION", "the extended session was refused")
    seed = diag.request(0x27, {"sub": 0x01})
    if not seed["ok"] or len(seed["raw"]) != SEED_LENGTH:
        diag.fail("SECURITY", "the seed request was refused or its answer is not a seed")
    value = seed["fields"]["seed"]
    if value != 0:
        key = diag.request(0x27, {"sub": 0x02, "key": value ^ KEY_MASK})
        if not key["ok"]:
            diag.fail("SECURITY", "the key was refused")
    change(market, options, options | CRUISE_CONTROL, idle)
