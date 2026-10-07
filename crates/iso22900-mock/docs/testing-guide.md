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

`iso22900-service` tests instead point `vci-service-launcher`'s
`config.toml` `library_path` override at `lib_path` (via `VCI_CONFIG_PATH`)
so `Iso22900Service::new` resolves the mock without needing an RDF file at
all — see `iso22900-service/tests/grpc_mock.rs` and `stdio_startup.rs` for
the pattern. `iso22900-registry`'s RDF-based resolution has no env-var
override; a test RDF can still be generated with
`mock_root_definition_file_path()` for scenarios that specifically exercise
RDF parsing (e.g. `iso22900-registry`'s own tests), but it requires either
the Windows registry or the hardcoded `/etc/pdu_api_root.xml` path to be
pointed at it, which `library_path` avoids entirely.

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
| `PDUGetStatus` | Returns `PDU_CLLST_ONLINE` for CLL handles; `PDU_MODST_READY` for module handles. |
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
| `PDUStartComPrimitive` | Allocates a new COP handle. Immediately queues a completion event via the registered callback. |
| `PDUCancelComPrimitive` | Always succeeds. |
| `PDUGetEventItem` | Dequeues one pending event. Returns `PDU_ERR_EVENT_QUEUE_EMPTY` when empty. |
| `PDURegisterEventCallback` | Stores the callback function for the given CLL handle. |
| `PDUGetObjectId` | Returns `PDU_ERR_ID_NOT_SUPPORTED` for unknown names. |
| `PDUGetUniqueRespIdTable` | Returns empty table. |
| `PDUSetUniqueRespIdTable` | Always succeeds. |
| `PDUGetTimestamp` | Returns `MOCK_TIMESTAMP` = 4242. |
| `PDUIoCtl` | Returns `PDU_STATUS_NOERROR` for most commands; `PDU_ERR_VALUE_NOT_SUPPORTED` for unsupported. For `MOCK_IOCTL_NULL_OUTPUT` it returns `PDU_STATUS_NOERROR` and leaves the output item null. |
| `PDUDestroyItem` | Frees heap-allocated PDU items. |

### Event Delivery

When `PDUStartComPrimitive` is called, the mock immediately enqueues a synthetic completion event and fires the registered callback. `PDUGetEventItem` then returns the event. This allows tests to exercise the event subscription path without real hardware timing.

---

## Back-Door API (FFI)

All back-door functions are exported with C ABI using the `__mock_` prefix.

### State Reset

| Function | Description |
|----------|-------------|
| `__mock_reset()` | Reset all mock state: counters, events, callback. Call between tests. |

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
| `mock_root_definition_file_path() -> Result<PathBuf, io::Error>` | Return or generate a Root Description File (XML) at `<CARGO_MANIFEST_DIR>/mock_root_definition.xml`. The generated RDF points to the located mock DLL. |

### Generated RDF Format

```xml
<MVCI_PDU_APIS>
    <MVCI_PDU_API>
        <SHORT_NAME>TestLib</SHORT_NAME>
        <DESCRIPTION>Test library</DESCRIPTION>
        <SUPPLIER_NAME>TestSupplier</SUPPLIER_NAME>
        <LIBRARY_FILE URI="file:///<absolute-path-to-mock-dll>"/>
    </MVCI_PDU_API>
</MVCI_PDU_APIS>
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
