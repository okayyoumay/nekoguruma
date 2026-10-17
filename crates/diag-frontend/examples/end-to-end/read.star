# The read procedure of the end-to-end definition test (M2 exit criterion 1): the example ECU's
# identification DIDs, a DID through a text-table conversion and the confirmed DTCs, each checked
# against the simulated ECU's configuration (`ecu.json`). It only reads, so it runs through
# either worker. A value that differs ends the run as failed with its own code.

def main():
    session = diag.request(0x22, {"did": 0xF186})
    if not session["ok"] or session["fields"]["session"] != 1:
        diag.fail("SESSION", "not in the default session")
    part = diag.request(0x22, {"did": 0xF187})
    if part["fields"]["sparePartNumber"] != "NGR-SIM-ECU":
        diag.fail("PART_NUMBER", "another spare part number")
    version = diag.request(0x22, {"did": 0xF189})
    if version["fields"]["softwareVersion"] != "1.0.0":
        diag.fail("SW_VERSION", "another software version")
    vin = diag.request(0x22, {"did": 0xF190})
    if vin["fields"]["vin"] != "NGRSIMECU00000001":
        diag.fail("VIN", "another VIN")
    dtcs = diag.read_dtc(0x08)
    if not dtcs["ok"] or dtcs["fields"]["mask"] != 0xFF:
        diag.fail("DTC_READ", "the DTC read failed")
    records = dtcs["fields"]["dtcs"]
    if len(records) != 2:
        diag.fail("DTC_COUNT", "not two confirmed DTCs")
    if records[0]["code"] != 0x123456 or records[0]["status"] != 0x09:
        diag.fail("DTC_FIRST", "another first DTC")
    if records[1]["code"] != 0xABCDEF or records[1]["status"] != 0x2F:
        diag.fail("DTC_SECOND", "another second DTC")
    diag.log(1, "read")
