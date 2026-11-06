# iso22900-mock Testing Guide

## Overview

`iso22900-mock` is an in-process D-PDU API DLL substitute for testing without real hardware. It exports the standard D-PDU API (`PDUConstruct`, `PDUConnect`, `PDUStartComPrimitive`, etc.) with the same C ABI as a real vendor DLL, plus back-door exports (`__mock_*`) for test inspection and reset, and Rust-native helper functions for locating the mock DLL and generating a test RDF.

Build artifact: `cdylib` (`.dll` / `.so`) + `rlib`.

---

## Loading the Mock in Tests

The mock crate provides a Rust helper to locate the compiled DLL:

```rust
use iso22900_mock::mock_library_path;

// Path to the compiled mock DLL
let lib_path = mock_library_path().expect("build the mock first");

// Use directly with the iso22900 wrapper
let api = iso22900::DPduApi::new(&lib_path).expect("failed to load mock");
```

`iso22900-service` tests instead write a root description file naming
`lib_path` with `write_root_file()` and point the debug-only
`NGR_PDU_API_ROOT_FILE` override at it (ADR-282), so `Iso22900Service::new`
resolves the mock through `ngr-library-resolver` the way it resolves a vendor
library; `config.toml` (via `VCI_CONFIG_PATH`) then holds only per-library
settings such as `vendor_struct_types`. See `iso22900-service`'s
`src/test_support.rs`, `tests/grpc_mock.rs` and `tests/stdio_startup.rs` for
the pattern.

Before each test, reset all mock state via the back-door export:

```rust
// Access via the raw FFI handle
unsafe { libloading::Library::new(&lib_path).unwrap().get::<fn()>(b"__mock_reset").unwrap()(); }
```

---

## Mock Behavior

### D-PDU API Functions

| Function | Behavior |
|----------|----------|
| `PDUConstruct` | Initializes the mock; stores the API tag for callback routing. Returns `PDU_STATUS_NOERROR`. |
| `PDUDestruct` | Tears down the mock. |
| `PDUModuleConnect` | Always succeeds. |
| `PDUModuleDisconnect` | Always succeeds. |
| `PDUGetVersion` | Returns canned version data (`"iso22900-mock"` strings). |
| `PDUGetStatus` | For a link with no ComPrimitive (hCoP `PDU_HANDLE_UNDEF`) it reports `PDU_CLLST_ONLINE`, or `PDU_CLLST_COMM_STARTED` after a `PDU_COPT_STARTCOMM` finished (ADR-423); the module status is as before. For a ComPrimitive handle it reports the status code `PDU_COPST_FINISHED`, `PDU_COPST_IDLE` while the ComPrimitive waits its turn, or `PDU_COPST_EXECUTING` while its events are still to come (ADR-299). |
| `PDUGetLastError` | Returns `PDU_ERR_EVT_NOERROR` (0). |
| `PDUGetModuleIds` | Returns a single module entry (`MOCK_MODULE_HANDLE` = 1001). |
| `PDUGetResourceIds` | Returns a single resource ID (`MOCK_RESOURCE_ID` = 2001). |
| `PDUGetConflictingResources` | Returns empty conflict list. |
| `PDUGetResourceStatus` | Returns available (not locked) status. |
| `PDUCreateComLogicalLink` | Allocates a CLL handle (returns `MOCK_LOGICAL_LINK_HANDLE` = 3001). |
| `PDUDestroyComLogicalLink` | Succeeds. Resets the link state and the tester-present ComParams (ADR-423). |
| `PDUConnect` | Always succeeds. |
| `PDUDisconnect` | Succeeds. The link is `PDU_CLLST_ONLINE` again and its tester-present stops (ADR-423). |
| `PDULockResource` / `PDUUnlockResource` | Always succeed. |
| `PDUGetComParam` | Returns an item whose data is the number 0, except for a numeric tester-present ComParam, which reads back its working value (class `PDU_PC_TESTER_PRESENT`; ADR-423). |
| `PDUSetComParam` | Stores the eight tester-present ComParams in the working buffer ("Tester-present" below); succeeds and forgets every other ComParam. Refuses a tester-present ComParam of the wrong data type or a byte field over 12 bytes with `PDU_ERR_INVALID_PARAMETERS`. |
| `PDUStartComPrimitive` | Allocates a new COP handle. Immediately queues a completion event via the registered callback, or, with a simulated ECU attached, the ECU's answer to a send-and-receive request, or to a `PDU_COPT_STARTCOMM` with a request (see "Simulated ECU" and "Tester-present"). |
| `PDUCancelComPrimitive` | Always succeeds. |
| `PDUGetEventItem` | Dequeues one pending event. Returns `PDU_ERR_EVENT_QUEUE_EMPTY` when empty. |
| `PDURegisterEventCallback` | Stores the callback function for the given CLL handle. |
| `PDUGetObjectId` | Resolves the mock's own test names (`mock-bus`, `mock-protocol`, `mock-resource`, `mock-com-param`, `mock-pin-type`) and the ISO 22900-2 (2022) short names in the constants below; returns `PDU_ERR_INVALID_PARAMETERS` for any other name. |
| `PDUGetUniqueRespIdTable` | Returns empty table. |
| `PDUSetUniqueRespIdTable` | Always succeeds. |
| `PDUGetTimestamp` | Returns the module clock (see "Timestamps"). |
| `PDUIoCtl` | Returns `PDU_STATUS_NOERROR` for most commands; `PDU_ERR_VALUE_NOT_SUPPORTED` for unsupported. For `MOCK_IOCTL_NULL_OUTPUT` it returns `PDU_STATUS_NOERROR` and leaves the output item null. |
| `PDUDestroyItem` | Frees heap-allocated PDU items, including a result item's data bytes and a status or error item's payload. |

### Event Delivery

When `PDUStartComPrimitive` is called, the mock immediately enqueues a synthetic completion event and fires the registered callback. `PDUGetEventItem` then returns the event. This allows tests to exercise the event subscription path without real hardware timing.

### Simulated ECU

With a `sim-ecu` attached (ADR-299), a `PDU_COPT_SENDRECV` ComPrimitive with request bytes is answered by the ECU instead, through `SimEcu::exchange` with physical addressing. Its events are, in order:

1. a `PDU_COPST_EXECUTING` status item;
2. a result item for each response-pending message (`7F <SID> 78`), unless the response pending handling is on (see "Response pending handling" below), when they are absorbed;
3. a result item with the response, or, when the ECU sends none, an error item `PDU_ERR_EVT_RX_TIMEOUT` with the additional error code `MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP`;
4. a `PDU_COPST_FINISHED` status item.

The control data's cycles decide what is sent and received: with no send cycles nothing reaches the ECU and only the two status items follow; with no receive cycles the request reaches the ECU and only the two status items follow. Any other combination runs as one send and one response, and so does a ComPrimitive without control data.

While an ECU is attached, and while other ComPrimitives still wait or run after it is detached, a ComPrimitive waits its turn (the synthetic completion event of the other ComPrimitive types too): it begins when the one before it has finished, and only then does its request reach the ECU, so the ECU's timers run from then. Its events are due at the delays the ECU gives them, counted from when it begins. A delivery thread begins each ComPrimitive in turn, queues each event when it is due and fires the callback. `PDUConstruct` and a reset discard waiting ComPrimitives and scheduled events; `PDUDestruct` discards them and waits for the delivery thread to end, so no callback runs after it returns. With the response pending handling off, the receive timeout comes right after the last message the ECU sent, since the mock keeps no P2Max to wait for. Without an ECU every ComPrimitive gets the synthetic completion event at once.

### Response pending handling

`PDUSetComParam` of `CP_RC78Handling` (0 to 2), `CP_P2Star` (microseconds, up to 655 350 000), `CP_RC78CompletionTimeout` (microseconds; 0 is no ceiling, as the repository reads it elsewhere, since B.5.1 does not say, and any value is kept) and `CP_RCByteOffset` (not 0; the index of the response code, counted from 0, as the agent sets 2 for `7F SID code`; 0xFFFFFFFF is the last byte) writes the working copy (8.4.15.6). `PDUConnect` copies it to the active copy, and so does every `PDU_COPT_UPDATEPARAM` when it is started, before it is queued and whether or not an ECU is attached; each ComPrimitive takes the active copy when `PDUStartComPrimitive` is called, so a value set after `PDUConnect` applies to ComPrimitives started after an UPDATEPARAM only, and does not touch one already queued. The `TempParamUpdate` flag of the control data (a ComPrimitive on the working copy) is not supported. `PDUGetComParam` reads the working copy, in the class B.5.1 gives (timing for P2*, error handling for the others). A wrong data type or a value out of range (handling, P2*, offset) is refused. A final message `7F SID 78` with no pending list is absorbed like a pending one, and a message counts as a pending only if its second byte is the request's service id (ADR-424). The copy starts with the handling off, so the mock passes each pending message on as a result item (the B.5.1 default of the UDS protocols is on, which the mock does not follow unless a ComParam turns it on), P2* 5.05 s, completion timeout 25 s (the B.5.1 values of the UDS protocols), offset 0xFFFFFFFF; `__mock_reset`, `PDUConstruct` and a destroy restore it.

With the handling on (ISO 22900-2:2022 B.5.1, Table 9, Figure I.4), a send-receive absorbs the ECU's pending messages as a library does: no result item is queued for them, each reloads the receive timer with P2*, and the next message, the response included, must come within P2* of the last pending message and, with handling 1, within the completion timeout of the first. A message that comes in time is delivered (only the response, as a result item). If one comes late, or the ECU sends no response, the receive timeout error item is queued at the instant the timer ran out (P2* after the last pending message, or the end of the completion timeout) and the response, if any, is not delivered. Handling 2 has no completion limit. A suppressed request whose ECU sends a pending and then nothing therefore ends in the receive timeout alone. The ECU's answers to a tester-present are not affected.

### Tester-present

The mock keeps the link's state (`PDU_CLLST_ONLINE` or `PDU_CLLST_COMM_STARTED`) and the tester-present ComParams, and with an ECU attached sends the link's tester-present (ADR-423). `PDUGetObjectId` resolves `CP_TesterPresentHandling`, `Message`, `ReqRsp`, `ExpPosResp`, `ExpNegResp`, `SendType`, `Time` and `AddrMode` (ISO 22900-2:2022 Annex B.5.1); `PDUSetComParam` writes them to a working buffer that starts at the B.5.1 defaults of UDS on CAN (handling on, message `3E 80`, no response expected, functional addressing, 2 s). A `PDU_COPT_STARTCOMM` or `PDU_COPT_UPDATEPARAM` copies the working buffer to the active one when it finishes (8.2.6.1), so a ComParam set alone changes nothing on the wire.

- A `PDU_COPT_STARTCOMM` with request bytes runs as a send-receive (the events above, the control data's cycles included); with none it gets the synthetic completion. Either way the link is `PDU_CLLST_COMM_STARTED` when it finishes, whatever the ECU answered, and a second one on a started link is a send-receive and leaves it started. `PDU_COPT_STOPCOMM`, `PDUDisconnect`, `PDUDestroyComLogicalLink` and `__mock_reset` make the link `PDU_CLLST_ONLINE` again; a `PDU_COPT_STARTCOMM` or other state-changing ComPrimitive still running or waiting then changes nothing when it finishes.
- While the link is COMM_STARTED and the active handling is 1, the delivery thread sends the active message to the ECU (`SimEcu::exchange`; physical addressing for address mode 0, functional otherwise): at once when the link starts or an UPDATEPARAM changes an active value, then every `CP_TesterPresentTime` microseconds (at least 1 ms) after each send. Send type 1 is sent as type 0. Without an ECU a send is skipped without an error.
- It goes only when no ComPrimitive runs or waits; the mock holds every tester-present behind the ComPrimitives, which is stricter than 8.1.3 Table 1 (a functional tester-present with no response may go at once behind a physical request), a simplification of the mock. A waiting ComPrimitive goes first and the tester-present stays due. With `CP_TesterPresentReqRsp` 1 the send holds the next ComPrimitive back until the ECU's final answer is due. The answer is positive when it starts with `CP_TesterPresentExpPosResp`; with that field empty it is not enabled (B.5.1 compares only the bytes the field holds), so any answer is positive unless it starts with a non-empty `CP_TesterPresentExpNegResp`. A missing answer, the expected negative one and, with a non-empty expected positive field, any other answer queue one `PDU_IT_ERROR` item `PDU_ERR_EVT_TESTER_PRESENT_ERROR` with hCop `PDU_HANDLE_UNDEF` and the additional error code `MOCK_XTRA_ERR_TP_NO_RSP` or `MOCK_XTRA_ERR_TP_NEG_RSP`, once per send, and the sending goes on. Nothing is queued for a positive answer, with `CP_TesterPresentReqRsp` 0, or when the tester-present was switched off or the link left COMM_STARTED before the answer was due. The ECU's answer is never delivered as a result item.
- `PDUGetComParam` reads only the numeric ones back; a byte field reads as 0.
- `NGR_SIM_ECU_COMMANDS` (JSON array of `__mock_sim_ecu_control` commands) is applied by `PDUConstruct` after the ECU of `NGR_SIM_ECU_CONFIG` is attached, when none is attached yet; for a worker process under test, e.g. `[{"attach":{"config":{"s3_server_ms":1500}}},{"inject_fault":{"fault":"drop_response","service":62}}]` loses the answer to the first tester-present. A command that does not parse or cannot apply fails `PDUConstruct` and leaves no ECU attached, so a retried construct does not succeed without the faults; a variable that is set but blank names no commands.

### Timestamps

The mock has one module clock: microseconds since the mock state was created, from a monotonic clock, truncated to `UNUM32`. `PDUGetTimestamp` and `PDUGetStatus` return it, and every event item's `Timestamp` is the clock when the item is produced: for the attached ECU's items, at their scheduled times when the ComPrimitive begins (so delivery-thread scheduling does not enter a latency, and items stay in order), otherwise when queued. The epoch starts at the first use of the mock state or the clock; `__mock_reset` does not restart it.

A result item that carries a response of the attached ECU has `TimestampFlags` of the default four bytes, `[0xC0, 0, 0, 0]` (ISO 22900-2 (2022) 10.1.4.11.4, D.2.4): `TxMsgDoneTimestamp` is the clock when the ComPrimitive began and its request was sent, and `StartMsgTimestamp` is the clock at the response's scheduled delivery time (as `sim-vci` stamps), which is also the item's `Timestamp`. The response is always at least 1 microsecond after the transmit done, so a latency measured between them is positive even when the ECU answers at once. A result with no transmission behind it (the synthetic completion event, with or without an ECU) has no flag bytes and both times 0. The mock produces no result item for a send-only ComPrimitive, so there is no transmit-done-only item. The transmit and start-of-message indication items are described next.

### Indications

`CP_TransmitIndEnable` and `CP_StartMsgIndEnable` (ISO 22900-2:2022 Annex B.5.1; value 0 or 1, anything else and a data type other than `PDU_PT_UNUM32` are refused) are disabled at the start. They have a working and an active buffer like the response pending ComParams above: `PDUSetComParam` writes the working buffer, `PDUConnect` and the start of every `PDU_COPT_UPDATEPARAM` copy it to the active buffer, `PDUGetComParam` reads the working buffer (class `PDU_PC_COM`), and a ComPrimitive takes the active buffer as it is when `PDUStartComPrimitive` is called. `__mock_reset`, `PDUConstruct` and a destroy restore them.

With an ECU attached, an indication is a `PDU_IT_RESULT` item with no data bytes, bound to the ComPrimitive (`hCop` is its handle; the sequences of 8.2.6.4.1 and 8.2.6.4.3 have the ComPrimitive queue it), whose `RxFlag` is the default four bytes with only byte 3 set (D.2.2: `0x08` `TX_INDICATION`, `0x02` `START_OF_MESSAGE`). Its time is the item's own `Timestamp` on the module clock (8.1.6.2, 8.1.6.4, 8.1.6.5); `TimestampFlags` has no bytes and the two times in the result are 0, so a client takes the time from the event item. The `PDU_COPT_SENDRECV` (and the `PDU_COPT_STARTCOMM` with a request) items are, in order:

- `PDU_COPST_EXECUTING`;
- with `CP_TransmitIndEnable` on, the transmit indication, for every ComPrimitive that sends, a send-only one (`NumReceiveCycles` 0) included, at the time the request was sent, which is also the `TxMsgDoneTimestamp` of the response items; none for a ComPrimitive with `NumSendCycles` 0;
- for each result item the ECU's messages give, with `CP_StartMsgIndEnable` on and a message of more than one frame, the start-of-message indication, at the delivery time of the message it announces (the item's `Timestamp`, as the `StartMsgTimestamp` of the message). The mock delivers a message whole, so the start of a multi-frame message is its delivery time. B.5.1 and D.2.2 give the start-of-message indication for the first frame of a multi-frame message (and the first byte of a UART message) only, so a message of seven bytes or fewer (a single frame with normal addressing, ISO 15765-2:2024 9.6.2, Table 11; the mock's ECU has no addressing format) gets none. A response pending message absorbed by `CP_RC78Handling` is no result item and gets none;
- the result items and the receive timeout error item;
- `PDU_COPST_FINISHED`.

The agent (`crates/agent/src/host.rs`) and the hardware-check guard's handle (`ngr-hw-checks`) both read a bound indication by its RxFlag bits and its item time, so the items serve both. `SimEcuCommand::ResponseTimestamps` (below) lets a test make the indication items the only source of those times.

An ECU is attached in one of two ways:

- `__mock_sim_ecu_control` (below), for tests that load the mock in their own process;
- `NGR_SIM_ECU_CONFIG`, the variable `sim-vci` reads: when it names a `sim_ecu::EcuConfig` JSON file at `PDUConstruct` and no ECU is attached, the mock attaches one with that configuration. A file that cannot be read or parsed, or that holds a value `EcuConfig::validate` refuses (a part number or software version that is not ASCII or is too long, a VIN that is not 17 ASCII bytes), makes `PDUConstruct` fail with `PDU_ERR_FCT_FAILED`.

`__mock_reset` detaches the ECU.

---

## Back-Door API (FFI)

All back-door functions are exported with C ABI using the `__mock_` prefix.

### State Reset

| Function | Description |
|----------|-------------|
| `__mock_reset()` | Reset all mock state: counters, events, callback, the simulated ECU. Call between tests. |

### Simulated ECU Control

| Function | Description |
|----------|-------------|
| `__mock_sim_ecu_control(command)` | Applies a NUL-terminated JSON command; returns 0 when it applied, 1 otherwise. `{"attach":{}}` attaches an ECU with `sim-vci`'s default configuration, `{"attach":{"config":{...}}}` one with the `sim_ecu::EcuConfig` fields given set over that default (for example `{"response_delay_ms":500}`; a field `EcuConfig` does not have, or a value `EcuConfig::validate` refuses such as a software version longer than 5 bytes, returns 1); `{"detach":{}}` detaches it; `{"response_timestamps":{"enabled":false}}` makes the ECU's response items carry no `TimestampFlags` times (flags empty, both times 0) until it is set to `true` again, which `{"detach":{}}` does not undo and `__mock_reset` does, so a client has only the indication items to take the transmit-done and start-of-message times from; `{"inject_fault":{"fault":...}}` arms a `sim_ecu::Fault`, with an optional `"service"` (SID) or `"request"` (byte prefix) to wait for, not both. A fault with no ECU attached returns 1. |

### Call Counters

| Function | Description |
|----------|-------------|
| `__mock_get_construct_count()` | Number of `PDUConstruct` calls |
| `__mock_get_destruct_count()` | Number of `PDUDestruct` calls |
| `__mock_get_version_count()` | Number of `PDUGetVersion` calls |
| `__mock_get_status_count()` | Number of `PDUGetStatus` calls |
| `__mock_get_create_com_logical_link_count()` | Number of `PDUCreateComLogicalLink` calls |
| `__mock_get_destroy_com_logical_link_count()` | Number of `PDUDestroyComLogicalLink` calls |
| `__mock_get_connect_count()` | Number of `PDUConnect` calls |
| `__mock_get_disconnect_count()` | Number of `PDUDisconnect` calls |
| `__mock_get_start_com_primitive_count()` | Number of `PDUStartComPrimitive` calls |
| `__mock_get_event_item_count()` | Number of `PDUGetEventItem` calls |

---

## Rust Helper Functions (rlib)

The `rlib` portion exports the following Rust functions for use in test setup:

| Function | Description |
|----------|-------------|
| `mock_library_path() -> Result<PathBuf, io::Error>` | Locate the compiled mock DLL in the Cargo target directory. Searches `CARGO_TARGET_DIR` first, then `<workspace>/target/`. |
| `mock_library_file_name() -> &'static str` | Return the platform-specific file name (`iso22900_mock.dll`, `libiso22900_mock.so`, `libiso22900_mock.dylib`). |
| `write_root_file(dir, short_name, library) -> Result<PathBuf, io::Error>` | Write a D-PDU API root description file `<dir>/pdu_api_root.xml` with one entry, `short_name`, whose `LIBRARY_FILE` names `library`, and return its path. |
| `file_uri(path) -> String` | The `file:` URI of an absolute path as a `LIBRARY_FILE` names it (`file:///opt/x.so`, `file:///C:/x.dll`), percent-encoding bytes outside the unreserved set. |

### Generated root description file

```xml
<?xml version="1.0" encoding="UTF-8"?>
<MVCI_PDU_API_ROOT>
  <MVCI_PDU_API>
    <SHORT_NAME>TestLib</SHORT_NAME>
    <LIBRARY_FILE URI="file:///<absolute-path-to-mock-library>"/>
  </MVCI_PDU_API>
</MVCI_PDU_API_ROOT>
```

---

## Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `MOCK_MODULE_HANDLE` | 1001 | Handle returned by `PDUGetModuleIds` |
| `MOCK_RESOURCE_ID` | 2001 | Resource ID returned by `PDUGetResourceIds` |
| `STANDARD_BUS_TYPE_DWCAN` | 3111 | `PDUGetObjectId` of the bus type `ISO_11898_2_DWCAN` (ADR-319) |
| `STANDARD_PROTOCOL_UDS_ON_CAN` | 3112 | `PDUGetObjectId` of the protocol `ISO_14229_3_on_ISO_15765_2` (ADR-319) |
| `STANDARD_PIN_TYPE_HI`, `STANDARD_PIN_TYPE_LOW` | 3311, 3312 | `PDUGetObjectId` of the pin types `HI` and `LOW` (ADR-319) |
| `STANDARD_COMPARAMS` | 3210 to 3241 | `PDUGetObjectId` of the ISO 22900-2 (2022) ComParams (Annex B.3, B.5) the agent sets on a UDS on CAN link: `CP_Baudrate`, `CP_P2Max`, `CP_P2Star`, `CP_RC78Handling`, `CP_RCByteOffset`, `CP_RC78CompletionTimeout`, `CP_CanPhysReqId`, `CP_CanRespUSDTId` (3210 to 3217), `CP_RC21Handling`, `CP_RC23Handling`, `CP_TransmitIndEnable`, `CP_StartMsgIndEnable` (3220 to 3223); and `CP_BlockSize` and `CP_StMin` (3218, 3219), the block size and minimum separation time of ISO 15765-2 (2024) 9.6.5.3 and 9.6.5.4, which the example ECU declares (ADR-324); and those the hardware-check guard sets when it completes a job's link from its profile (ADR-334): `CP_CanFuncReqId`, `CP_RequestAddrMode`, `CP_TesterPresentAddrMode`, `CP_CanPhysReqFormat`, `CP_CanRespUSDTFormat`, `CP_CanFuncReqFormat`, `CP_CanPhysReqExtAddr`, `CP_CanRespUSDTExtAddr`, `CP_CanFuncReqExtAddr`, `CP_CanFillerByteHandling`, `CP_CanFillerByte`, `CP_TesterPresentMessage` (3224 to 3235); and the other tester-present ComParams of the agent's keep-alive (ADR-418): `CP_TesterPresentHandling`, `CP_TesterPresentReqRsp`, `CP_TesterPresentExpPosResp`, `CP_TesterPresentExpNegResp`, `CP_TesterPresentSendType`, `CP_TesterPresentTime` (3236 to 3241). The mock keeps the values of the eight tester-present ComParams only (ADR-423) |
| `MOCK_LOGICAL_LINK_HANDLE` | 3001 | Base CLL handle (first `PDUCreateComLogicalLink`) |
| `MOCK_COM_PRIMITIVE_HANDLE` | 4001 | Base COP handle (first `PDUStartComPrimitive`) |
| `MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP` | `0x0000_0101` | The additional error code (ISO 22900-2:2022 D.4.2, `ExtraErrorInfoId`) of the `PDU_ERR_EVT_RX_TIMEOUT` error item for a ComPrimitive the attached ECU does not answer; D.4.2 leaves these codes to the vendor's MDF, so the value is the mock's own (ADR-350) |
| `MOCK_XTRA_ERR_TP_NO_RSP` | `0x0000_0102` | The additional error code of a `PDU_ERR_EVT_TESTER_PRESENT_ERROR` item for a tester-present the ECU did not answer (ADR-423); the mock's own value, as D.4.2 leaves them to the MDF |
| `MOCK_XTRA_ERR_TP_NEG_RSP` | `0x0000_0103` | The same, for a tester-present answered with anything but the expected positive response (ADR-423) |
| `ECU_COMMANDS_ENV` | `NGR_SIM_ECU_COMMANDS` | Environment variable with the JSON array of ECU commands `PDUConstruct` applies (ADR-423) |
| `MOCK_IOCTL_NULL_OUTPUT` | `0x7FFF_FF00` | IOCTL command id that succeeds without an output item (public, for null-item tests) |

---

## Known Limitations

- Only the eight tester-present ComParams are stored (ADR-423); every other ComParam is accepted and forgotten, and `PDUGetComParam` returns the number 0 for it.
- The UniqueRespIdTable is accepted but not applied to event routing.
- The mock uses a process-global singleton. Tests in the same process share state; always call `__mock_reset()` between tests.
- Send-receive and `PDU_COPT_STARTCOMM` requests always go to the ECU with physical addressing; only the tester-present uses functional addressing (address mode other than 0).
- No bus communication is simulated. With a `sim-ecu` attached, the ECU's response delays are applied, but there is no P2 timing and no `PDUCancelComPrimitive` (ADR-299). The tester-present is sent functionally for address mode other than 0, but the mock models no bus, no P3 timing and one link (ADR-423).
