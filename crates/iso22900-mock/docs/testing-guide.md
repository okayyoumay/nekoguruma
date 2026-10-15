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
| `PDUGetStatus` | Returns `PDU_CLLST_ONLINE` for CLL handles; `PDU_MODST_READY` for module handles. For a ComPrimitive handle it reports the status code `PDU_COPST_FINISHED`, or `PDU_COPST_EXECUTING` while the ComPrimitive's events are still scheduled (ADR-299). |
| `PDUGetLastError` | Returns `PDU_ERR_EVT_NOERROR` (0). |
| `PDUGetModuleIds` | Returns a single module entry (`MOCK_MODULE_HANDLE` = 1001). |
| `PDUGetResourceIds` | Returns a single resource ID (`MOCK_RESOURCE_ID` = 2001). |
| `PDUGetConflictingResources` | Returns empty conflict list. |
| `PDUGetResourceStatus` | Returns available (not locked) status. |
| `PDUCreateComLogicalLink` | Allocates a CLL handle (returns `MOCK_LOGICAL_LINK_HANDLE` = 3001). |
| `PDUDestroyComLogicalLink` | Always succeeds. |
| `PDUConnect` | Always succeeds. |
| `PDUDisconnect` | Always succeeds. |
| `PDULockResource` / `PDUUnlockResource` | Always succeed. |
| `PDUGetComParam` | Returns `PDU_ERR_ID_NOT_SUPPORTED` (params not stored). |
| `PDUSetComParam` | Always succeeds (params not stored). |
| `PDUStartComPrimitive` | Allocates a new COP handle. Immediately queues a completion event via the registered callback, or, with a simulated ECU attached, the ECU's answer to a send-and-receive request (see "Simulated ECU"). |
| `PDUCancelComPrimitive` | Always succeeds. |
| `PDUGetEventItem` | Dequeues one pending event. Returns `PDU_ERR_EVENT_QUEUE_EMPTY` when empty. |
| `PDURegisterEventCallback` | Stores the callback function for the given CLL handle. |
| `PDUGetObjectId` | Returns `PDU_ERR_ID_NOT_SUPPORTED` for unknown names. |
| `PDUGetUniqueRespIdTable` | Returns empty table. |
| `PDUSetUniqueRespIdTable` | Always succeeds. |
| `PDUGetTimestamp` | Returns `MOCK_TIMESTAMP` = 4242. |
| `PDUIoCtl` | Returns `PDU_STATUS_NOERROR` for most commands; `PDU_ERR_VALUE_NOT_SUPPORTED` for unsupported. For `MOCK_IOCTL_NULL_OUTPUT` it returns `PDU_STATUS_NOERROR` and leaves the output item null. |
| `PDUDestroyItem` | Frees heap-allocated PDU items, including a result item's data bytes and a status or error item's payload. |

### Event Delivery

When `PDUStartComPrimitive` is called, the mock immediately enqueues a synthetic completion event and fires the registered callback. `PDUGetEventItem` then returns the event. This allows tests to exercise the event subscription path without real hardware timing.

### Simulated ECU

With a `sim-ecu` attached (ADR-299), a `PDU_COPT_SENDRECV` ComPrimitive with request bytes is answered by the ECU instead, through `SimEcu::exchange` with physical addressing. Its events are, in order:

1. a `PDU_COPST_EXECUTING` status item;
2. a result item for each response-pending message (`7F <SID> 78`): the mock does no negative-response handling of its own;
3. a result item with the response, or, when the ECU sends none, an error item `PDU_ERR_EVT_RX_TIMEOUT`;
4. a `PDU_COPST_FINISHED` status item.

The control data's cycles decide what is sent and received: with no send cycles nothing reaches the ECU and only the two status items follow; with no receive cycles the request reaches the ECU and only the two status items follow. Any other combination runs as one send and one response, and so does a ComPrimitive without control data.

While an ECU is attached, every ComPrimitive's events (the synthetic completion event of the other ComPrimitive types too) go into one schedule, each at the delay the ECU gives it counted from the request and none before the events already scheduled, so ComPrimitives run one after another. A delivery thread queues each event when it is due and fires the callback. `PDUConstruct` and a reset discard the schedule; `PDUDestruct` discards it and waits for the delivery thread to end. The receive timeout comes right after the last message the ECU sent, since the mock keeps no ComParams and so has no P2 to wait for. `PDUGetStatus` reports `PDU_COPST_EXECUTING` for a ComPrimitive with events still scheduled. Without an ECU every ComPrimitive gets the synthetic completion event at once.

An ECU is attached in one of two ways:

- `__mock_sim_ecu_control` (below), for tests that load the mock in their own process;
- `NGR_SIM_ECU_CONFIG`, the variable `sim-vci` reads: when it names a `sim_ecu::EcuConfig` JSON file at `PDUConstruct` and no ECU is attached, the mock attaches one with that configuration. A file that cannot be read or parsed makes `PDUConstruct` fail with `PDU_ERR_FCT_FAILED`.

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
| `__mock_sim_ecu_control(command)` | Applies a NUL-terminated JSON command; returns 0 when it applied, 1 otherwise. `{"attach":{}}` attaches an ECU with `sim-vci`'s default configuration, `{"attach":{"config":{...}}}` one with that `sim_ecu::EcuConfig`; `{"detach":{}}` detaches it; `{"inject_fault":{"fault":...}}` arms a `sim_ecu::Fault`, with an optional `"service"` (SID) or `"request"` (byte prefix) to wait for, not both. A fault with no ECU attached returns 1. |

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
| `MOCK_LOGICAL_LINK_HANDLE` | 3001 | Base CLL handle (first `PDUCreateComLogicalLink`) |
| `MOCK_COM_PRIMITIVE_HANDLE` | 4001 | Base COP handle (first `PDUStartComPrimitive`) |
| `MOCK_TIMESTAMP` | 4242 | Timestamp returned by `PDUGetTimestamp` |
| `MOCK_IOCTL_NULL_OUTPUT` | `0x7FFF_FF00` | IOCTL command id that succeeds without an output item (public, for null-item tests) |

---

## Known Limitations

- ComParams are not stored or returned (`PDUGetComParam` returns `PDU_ERR_ID_NOT_SUPPORTED`).
- The UniqueRespIdTable is accepted but not applied to event routing.
- The mock uses a process-global singleton. Tests in the same process share state; always call `__mock_reset()` between tests.
- No actual bus communication or timing is simulated.
