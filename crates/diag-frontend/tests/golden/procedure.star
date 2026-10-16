# The procedure the IR golden test lowers for every variant of the example ECU and runs against
# a host that records each request and answers it positively.

def main():
    diag.request(0x10, {"sub": 0x03})
    diag.request(0x22, {"did": 0xF190})
    diag.request(0x22, {"did": 0xF186})
    diag.request(0x19, {"sub": 0x02, "statusMask": 0x09})
    diag.request(0x2E, {"did": 0xF190, "vin": "WVWZZZ1JZXW000001"})
    diag.log(1, "done")
