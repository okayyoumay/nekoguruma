# The procedure the IR golden test lowers for every variant of the example ECU and runs against
# a host that records each request and answers it positively from responses.txt.

def main():
    diag.request(0x10, {"sub": 0x03})
    diag.request(0x22, {"did": 0xF190})
    session = diag.request(0x22, {"did": 0xF186})
    if session["fields"]["session"] == 3:
        diag.log(1, "extended session")
    dtcs = diag.read_dtc(0x09)
    if dtcs["ok"] and dtcs["fields"]["mask"] == 0xFF:
        diag.log(1, "all DTC status bits supported")
    diag.request(0x2E, {"did": 0xF190, "vin": "WVWZZZ1JZXW000001"})
    diag.log(1, "done")
