# j2534-0404-mock Testing Guide

## Overview

`j2534-0404-mock` is an in-process J2534 v04.04 DLL substitute for testing without real hardware. It exports the standard PassThru API (`PassThruOpen`, `PassThruConnect`, etc.) with the same C ABI as a real vendor DLL, plus a set of back-door exports (`__mock_*`) for test setup, inspection, and reset.

Build artifact: `cdylib` (`.dll` / `.so`) + `rlib`.

---

## Loading the Mock in Tests

Load the mock DLL the same way as any J2534 DLL — via `J2534Api0404::new(path)` in the `j2534-0404` crate:

```rust
use j2534_0404::J2534Api0404;

let lib_path = env!("CARGO_CDYLIB_FILE_J2534_0404_MOCK");  // set by build script
let api = J2534Api0404::new(lib_path).expect("failed to load mock");
```

Before each test, reset all mock state:

```rust
unsafe { api.raw.__mock_reset(); }
```

---

## Mock Behavior

### PassThru API

| Function | Behavior |
|----------|----------|
| `PassThruOpen` | Always succeeds. Returns `MOCK_DEVICE_ID` (1). Increments open counter. |
| `PassThruClose` | Always succeeds. Increments close counter. |
| `PassThruConnect` | Creates a new channel slot. Returns a new `ChannelId`. Appends the `Flags` argument to the connect-flags log (see `__mock_get_connect_flags_log_*` below). |
| `PassThruDisconnect` | Removes the channel slot. Returns success even if channel not found. |
| `PassThruReadMsgs` | Dequeues messages from the channel's RX queue. Returns `ERR_BUFFER_EMPTY` when empty. |
| `PassThruWriteMsgs` | Appends messages to `written_msgs`. If loopback is enabled, also queues them into the RX queue. |
| `PassThruStartPeriodicMsg` | Stores the periodic message; does not actually transmit. |
| `PassThruStopPeriodicMsg` | Removes the stored periodic message. |
| `PassThruStartMsgFilter` | Allocates a filter ID and records `FilterType`/`pMaskMsg`/`pPatternMsg`/`pFlowControlMsg` for inspection (see `__mock_get_filter_*` below); does not actually filter (all messages still reach the RX queue). |
| `PassThruStopMsgFilter` | Frees the filter ID and discards its recorded data. |
| `PassThruGetLastError` | Returns the last stored error string. |
| `PassThruGetNextCarDAQ` | Not implemented (returns error). |

### IOCTL Commands

| Command | Behavior |
|---------|----------|
| `IOCTL_GET_CONFIG` | Returns stored per-channel config value for each requested param ID. |
| `IOCTL_SET_CONFIG` | Stores config values per-channel. |
| `IOCTL_READ_VBATT` | Returns `MOCK_VBATT_MV` = 12000 mV. |
| `IOCTL_READ_PROG_VOLTAGE` | Returns `MOCK_PROG_VOLTAGE_MV` = 0 mV. |
| `IOCTL_FIVE_BAUD_INIT` | Validates the channel's connected protocol (base id via `base_protocol_id`, so a `_PS` K-line channel is accepted too); returns `ERR_NOT_SUPPORTED` unless it is `ISO9141` or `ISO14230`, and `ERR_INVALID_CHANNEL_ID` if `channel_id` does not exist. On a `_PS` channel, also checked (after the protocol check, before anything else) against `channel.pins_assigned`, `ERR_PIN_INVALID` if not yet assigned — see "Pin Selection (`_PS`) Gating" below; 5-baud init communicates over the channel's DLC pins, same as any other I/O. Otherwise records the input address byte (`pInput`'s `SBYTE_ARRAY.BytePtr[0]`, if non-null — see `__mock_get_five_baud_init_input` below), seeds `CONFIG_DATA_RATE` to `MOCK_FIVE_BAUD_NEGOTIATED_BAUD` (10 400, simulating the adapter's post-init baud calculation), and returns canned init response `[0x55, 0x8F, 0xEA]`. |
| `IOCTL_FAST_INIT` | Validates the channel's connected protocol (base id via `base_protocol_id`, so a `_PS` K-line channel is accepted too); returns `ERR_NOT_SUPPORTED` unless it is `ISO9141` or `ISO14230` (per J2534-1 v04.04, FAST_INIT is valid on both K-line protocols), and `ERR_INVALID_CHANNEL_ID` if `channel_id` does not exist. On a `_PS` channel, also gated on `channel.pins_assigned` the same way `IOCTL_FIVE_BAUD_INIT` is (see "Pin Selection (`_PS`) Gating" below), checked before the error-injection override next. If `__mock_set_fast_init_error` has installed an override (see below), returns that code instead — checked after the channel/protocol/pin-assignment validity checks above but before anything else, so a forced failure never increments the success counter or touches the recorded input/output. Otherwise: with a non-null `pInput`, records the input frame (`PASSTHRU_MSG.Data[..DataSize]` — see `__mock_get_fast_init_input` below) and returns canned init response `[0x83, 0xF1, 0x10, 0xC1, 0xE9, 0x8F]` (`MOCK_FAST_INIT_RESPONSE`, a realistic ISO14230 StartCommunication positive response) as `PASSTHRU_MSG`. With a null `pInput` (wakeup-only fast init, ADR-077), clears any previously recorded input frame and writes a zero-length response (`DataLength = 0`) to a non-null `pOutput` instead of the canned response — no request was sent, so no ECU response is simulated. The success counter increments either way (non-null or null `pInput`), but not when the error override fires. |
| `IOCTL_CLEAR_RX_BUFFER` | Empties the channel's RX queue. |
| `IOCTL_CLEAR_TX_BUFFER` | No-op (succeeds). |
| `IOCTL_CLEAR_PERIODIC_MSGS` | Clears all stored periodic messages for the channel. |
| `IOCTL_CLEAR_MSG_FILTERS` | Discards all recorded filters for the channel (succeeds). |
| `IOCTL_CLEAR_FUNCT_MSG_LOOKUP_TABLE` | No-op (succeeds). |
| `IOCTL_GET_DEVICE_INFO` (SAE J2534-2 §25, ADR-153/ADR-156 Decision 4/Phase 2b) | Reports `Supported=1` with a clause-shaped `0xPPQQRRSS` value for the `*_SUPPORTED`/`*_SIMULTANEOUS` parameters of the 10 base J2534-1 protocols this mock implements: for the seven `_CHx`-in-scope families (the six base families' own `_SUPPORTED` param, plus the consolidated `DEVICE_INFO_J2610_SUPPORTED`), `QQ` (bits 16-23) carries the current SAE J2534-2 clause 7 Additional Channels capacity (`DEFAULT_CHX_CAPACITY`, overridable per test via `__mock_set_chx_capacity`), `RR=0` (no `_PS` channels beyond the one `_PS` id itself), `SS=1`; every other `*_SUPPORTED`/`*_SIMULTANEOUS` param reports the flat `QQ=RR=0`, `SS=1`. `DEVICE_INFO_SHORT_TO_GND_J1962`/`DEVICE_INFO_PGM_VOLTAGE_J1962` (clause 15.4/25.3.2.2) are per-pin queries: `Value` is an INPUT pin-selector bitmap (`0xHHHHLLLL`, left un-altered in the response) — `SHORT_TO_GND_J1962` reads the selector from the low 16 bits, `PGM_VOLTAGE_J1962` from the high 16 bits — and `Supported=1` only when exactly one bit is set and it selects pin 9 or pin 15, `Supported=0` otherwise (including zero-bit or multi-bit input). `Supported=0` for every other known `DEVICE_INFO_*` parameter, including the rest of the pin-bitmask-input family (every `*_PS_*`). `channel_id` (really the `DeviceID`) is not validated, matching `IOCTL_READ_VBATT`/`IOCTL_READ_PROG_VOLTAGE`'s existing device-scoped IOCTL pattern. |
| `IOCTL_GET_PROTOCOL_INFO` (SAE J2534-2 §25, ADR-153) | Returns `ERR_INVALID_PROTOCOL_ID` for any protocol ID outside the 10 base J2534-1 IDs. For a known protocol: `Supported=1` for `MAX_RX_BUFFER_SIZE` (4128, matching `PASSTHRU_MSG::Data`'s real size), `MAX_PASS_FILTER`/`MAX_BLOCK_FILTER` (10), `CAN_11_29_IDS_SUPPORTED` (only for `CAN`/`ISO15765`), `MAX_REPEAT_MESSAGING` (clause 14.3/25.3.2.3, value = `MAX_REPEAT_SLOTS_PER_CHANNEL` = 10) and `MAX_REPEAT_MESSAGING_LENGTH` (ADR-186's periodic-message cap, mirroring `ioctl_start_repeat_message`'s own enforcement, via the shared `max_repeat_messaging_length` helper: 11 for `ISO15765`, 10 for `J1850PWM` (its own ordinary TX size range tops out below 12), 12 for every other protocol this handler serves); `Supported=0` for every other known `PROTOCOL_INFO_*` parameter. |
| `IOCTL_SET_CONFIG(CONFIG_J1962_PINS)` on a `_PS` channel (SAE J2534-2 clause 6.3.3.2, ADR-156 Decision 2) | See "Pin Selection (`_PS`) Gating" below. |
| `IOCTL_START_REPEAT_MESSAGE` (SAE J2534-2 clause 14, ADR-165/ADR-186) | Rejects with `ERR_INVALID_MSG` when `RepeatMsgData[0]`'s `DataSize` exceeds the same per-protocol cap `IOCTL_GET_PROTOCOL_INFO`'s `MAX_REPEAT_MESSAGING_LENGTH` answer advertises for the channel's base protocol (`max_repeat_messaging_length`, shared by both call sites so the advertised and enforced values cannot drift apart); rejects with `ERR_EXCEEDED_LIMIT` once `MAX_REPEAT_SLOTS_PER_CHANNEL` (10) active slots already exist on the channel. Otherwise allocates a `MsgId` and stores the slot (no actual periodic retransmission is simulated). |

### Loopback Mode

When `CONFIG_LOOPBACK` is set to a non-zero value via `IOCTL_SET_CONFIG`, each message written via `PassThruWriteMsgs` is also pushed into the channel's RX queue. This allows send-then-read round-trip tests without real hardware.

```rust
// Enable loopback
api.ioctl_set_config(channel_id, &[(CONFIG_LOOPBACK, 1)])?;

// Write a message
api.write_msgs(channel_id, &[msg], 100)?;

// Read it back
let received = api.read_msgs(channel_id, 1, 100)?;
assert_eq!(received[0].data(), msg.data());
```

### Pin Selection (`_PS`) Gating

Per SAE J2534-2 clause 6.3.3.2 (ADR-156 Decision 2, Phase 2a), a channel opened
via `PassThruConnect` with one of the seven Pin Selection protocol IDs
(`PROTOCOL_J1850VPW_PS`, `PROTOCOL_J1850PWM_PS`, `PROTOCOL_ISO9141_PS`,
`PROTOCOL_ISO14230_PS`, `PROTOCOL_CAN_PS`, `PROTOCOL_ISO15765_PS`,
`PROTOCOL_J2610_PS`) starts with its DLC pins unassigned. Until
`IOCTL_SET_CONFIG(CONFIG_J1962_PINS)` binds them, the mock rejects
`PassThruWriteMsgs`, `PassThruReadMsgs`, `PassThruStartMsgFilter`,
`PassThruStartPeriodicMsg`, `IOCTL_FIVE_BAUD_INIT`, and `IOCTL_FAST_INIT` on
that channel with `ERR_PIN_INVALID`, checked before any of each call's other
logic runs. `PassThruDisconnect`, `PassThruStopMsgFilter`,
`PassThruStopPeriodicMsg`, and every other IOCTL are never gated.

`CONFIG_J1962_PINS = 0x00000000` — clause 6.3.3.2's own "no selection
performed" sentinel for the packed bitmask, never produced by
`j2534-0404-service`'s `compute_pin_select` (every packed entry requires a
concrete nonzero pin number) — does not count as a real assignment: the
`SET_CONFIG` call itself still succeeds, but `pins_assigned` stays `false`
and the I/O gate above stays in effect. A direct mock client (or test)
sending the sentinel value cannot bypass the gate this way, and a follow-up
`SET_CONFIG` with a real value on the same channel remains legitimate.

Once `IOCTL_SET_CONFIG(CONFIG_J1962_PINS)` succeeds on such a channel, its
pins are bound for the life of the channel: a second
`IOCTL_SET_CONFIG(CONFIG_J1962_PINS)` call on the same channel returns
`ERR_CHANNEL_IN_USE` instead of applying it, and none of that second call's
config entries (`CONFIG_J1962_PINS` or otherwise) are applied. A channel
opened with a non-`_PS` protocol ID is never gated (pins are
meaningless/already-implicit for it), but a `CONFIG_J1962_PINS` `SET_CONFIG`
call on it is treated the same as an already-assigned `_PS` channel
(`ERR_CHANNEL_IN_USE`) — the mock has no way to distinguish "never needed
pins" from "already has them", and clause 6.3.3.2 leaves neither case a
legitimate second assignment.

```rust
// A _PS channel starts pin-gated.
let channel_id = api.connect(device_id, PROTOCOL_CAN_PS, 0, 500_000)?;
let write_err = api.write_messages(channel_id, &[msg], 100).unwrap_err();
assert!(matches!(write_err, Error::ApiStatus { code, .. } if code.as_u32() == ERR_PIN_INVALID));

// Assign pins once -- gate lifts for the life of the channel.
api.set_config(channel_id, &[(CONFIG_J1962_PINS, 0x0000_030B)])?;
api.write_messages(channel_id, &[msg], 100)?; // now succeeds

// A second pin assignment is rejected outright.
let second_err = api
    .set_config(channel_id, &[(CONFIG_J1962_PINS, 0x0000_0402)])
    .unwrap_err();
assert!(matches!(second_err, Error::ApiStatus { code, .. } if code.as_u32() == ERR_CHANNEL_IN_USE));
```

---

## Back-Door API (FFI)

All back-door functions are exported with C ABI using the `__mock_` prefix. They follow the same calling convention as the PassThru functions.

### State Reset

| Function | Description |
|----------|-------------|
| `__mock_reset()` | Reset all mock state: channels, counters, error string, the `__mock_set_max_channels` cap (back to unlimited), and the `__mock_set_fast_init_error` override (back to none). Call between tests. |
| `__mock_set_max_channels(limit)` | Cap the number of simultaneously open channels; `PassThruConnect` returns `ERR_EXCEEDED_LIMIT` once `limit` channels are open. `limit = 0` removes the cap. Used to simulate a device that cannot hold an `ISO15765` and a companion `CAN` channel open at once, for `j2534-0404-service`'s `can_channel_mode = "auto"` capability probing (ADR-047). |
| `__mock_set_fast_init_error(code)` | Force every subsequent `IOCTL_FAST_INIT` call, on every channel, to fail with the raw J2534 error `code` instead of its normal behavior. `code == STATUS_NOERROR` (`0`) clears the override (the default after `__mock_reset`), mirroring `__mock_set_max_channels`'s `limit = 0` convention. Used to simulate a failing fast-init (`PduErrEvtInitError` / temp-param revert / `PduCopstFinished` in `j2534-0404-service`, ADR-077) without a real adapter. |

### Call Counters

| Function | Description |
|----------|-------------|
| `__mock_get_open_count()` | Number of `PassThruOpen` calls |
| `__mock_get_close_count()` | Number of `PassThruClose` calls |
| `__mock_get_connect_count()` | Number of `PassThruConnect` calls |
| `__mock_get_disconnect_count()` | Number of `PassThruDisconnect` calls |
| `__mock_get_read_msgs_count()` | Number of `PassThruReadMsgs` calls |
| `__mock_get_write_msgs_count()` | Number of `PassThruWriteMsgs` calls |
| `__mock_get_start_periodic_count()` | Number of `PassThruStartPeriodicMsg` calls |
| `__mock_get_stop_periodic_count()` | Number of `PassThruStopPeriodicMsg` calls |
| `__mock_get_set_config_count()` | Number of `IOCTL_SET_CONFIG` calls |
| `__mock_get_get_config_count()` | Number of `IOCTL_GET_CONFIG` calls |
| `__mock_get_five_baud_init_count()` | Number of successful `IOCTL_FIVE_BAUD_INIT` calls (rejected calls do not increment this) |
| `__mock_get_fast_init_count()` | Number of successful `IOCTL_FAST_INIT` calls (rejected calls do not increment this) |
| `__mock_get_get_device_info_count()` | Number of `IOCTL_GET_DEVICE_INFO` calls (ADR-153) |
| `__mock_get_get_protocol_info_count()` | Number of `IOCTL_GET_PROTOCOL_INFO` calls, including calls rejected with `ERR_INVALID_PROTOCOL_ID` (ADR-153) |

These counters are process-wide: every test in a process shares them, so a test that asserts an exact
before/after delta sees other tests' calls when tests run in parallel (`cargo test` runs a binary's tests on
several threads). For that case three calls also have a per-thread counter, which counts only the calls made on
the calling thread and is not cleared by `__mock_reset`. A current-thread `#[tokio::test]` that drives the
service directly makes its native calls on its own thread, so its delta on these is exact. A call the service
makes on another thread (for example through `spawn_blocking`) is counted on that thread, not on the test's, so
an "unchanged" assertion would pass without proving anything for it; pair such an assertion with a test that
expects the count to rise:

| Function | Description |
|----------|-------------|
| `__mock_get_get_device_info_count_on_current_thread()` | `IOCTL_GET_DEVICE_INFO` calls made on the calling thread |
| `__mock_get_stop_periodic_count_on_current_thread()` | `PassThruStopPeriodicMsg` calls made on the calling thread |
| `__mock_get_clear_tx_buffer_count_on_current_thread()` | `PassThruIoctl(CLEAR_TX_BUFFER)` calls made on the calling thread |

### Connect-Flags Log Inspection

The `Flags` argument of every successful `PassThruConnect` is recorded in call
order, independently of channel state, so entries survive a later
`PassThruDisconnect` (e.g. the dual-channel-capability probe channel).

| Function | Description |
|----------|-------------|
| `__mock_get_connect_flags_log_len()` | Number of successful `PassThruConnect` calls recorded (same count as `__mock_get_connect_count()`). |
| `__mock_get_connect_flags_log_entry(index, out_value)` | Read the `Flags` of the `index`-th (0-based, call order) successful `PassThruConnect`. Returns `STATUS_NOERROR` or `ERR_INVALID_CHANNEL_ID` if `index` is out of range. |

### RX Queue Injection

| Function | Description |
|----------|-------------|
| `__mock_inject_rx_msg(channel_id, data, data_len, protocol_id, rx_status)` | Push a message into the channel's RX queue, with `TxFlags` implicitly `0`. Returns `STATUS_NOERROR` or `ERR_INVALID_CHANNEL_ID`. |

**Back-door FFI export ABI-stability policy:** never add a parameter to an existing exported `__mock_*` symbol in place — consumers resolve these symbols dynamically (`libloading`) without any compile-time type-checking, so a widened signature silently corrupts a caller still using the original arity (it reads whatever value happens to sit in the next argument slot/register). Always add a new symbol instead when a signature needs to grow. ADR-165 PR #42 round 13 added `__mock_inject_rx_msg_with_flags` this way to carry an extra `TxFlags` argument for a SAE J2534-2 clause 14 repeat slot's format check; round 14 found that extension itself was unnecessary once the check was corrected to read the right `PASSTHRU_MSG` field (`RxStatus`, already present on the original five-argument symbol), so `__mock_inject_rx_msg_with_flags` was removed again — but the policy that motivated splitting it out in the first place still stands for any future signature change.

### Written Message Inspection

| Function | Description |
|----------|-------------|
| `__mock_get_written_msg_count(channel_id)` | Number of messages written via `PassThruWriteMsgs` on this channel |
| `__mock_get_written_msg(channel_id, index, buf, buf_len, out_len)` | Copy data bytes of written message at `index` into `buf`. Sets `*out_len` to actual size. |

### Fast-Init Input Inspection

| Function | Description |
|----------|-------------|
| `__mock_get_fast_init_input(channel_id, buf, buf_len, out_len)` | Copy the input frame (`PASSTHRU_MSG.Data[..DataSize]`) of the most recent `IOCTL_FAST_INIT` call on `channel_id` into `buf`. Sets `*out_len` to actual size. Returns `STATUS_NOERROR` when an input frame was recorded (a `FAST_INIT` call with a non-null `pInput`), or `ERR_INVALID_CHANNEL_ID` when the channel doesn't exist or no input frame has been recorded — including when the most recent call passed a null `pInput` (wakeup-only, ADR-077), which clears the recorded frame; combine with `__mock_get_fast_init_count` to distinguish "wakeup-only fast init ran" from "no fast init at all". Also available as the in-process Rust helper `mock_get_fast_init_input(channel_id) -> Option<Vec<u8>>`. |

### Five-Baud Init Input Inspection

| Function | Description |
|----------|-------------|
| `__mock_get_five_baud_init_input(channel_id, out_address)` | Copy the target address byte (`SBYTE_ARRAY.BytePtr[0]`) of the most recent `IOCTL_FIVE_BAUD_INIT` call on `channel_id` into `*out_address`. Returns `STATUS_NOERROR` when an address byte was recorded (a `FIVE_BAUD_INIT` call with a non-null `pInput`), or `ERR_INVALID_CHANNEL_ID` when the channel doesn't exist or no address has been recorded. Also available as the in-process Rust helper `mock_get_five_baud_init_input(channel_id) -> Option<u8>`. |

### Config Inspection

| Function | Description |
|----------|-------------|
| `__mock_get_config_value(channel_id, param_id, out_value)` | Read the stored config value for `param_id` on `channel_id`. Returns `STATUS_NOERROR` or `ERR_INVALID_CHANNEL_ID`. |
| `__mock_get_channel_baud_rate(channel_id, out_value)` | Read the baud rate `channel_id` was opened with via `PassThruConnect`. `DATA_RATE` is never written through `IOCTL_SET_CONFIG` (it is supplied as the `PassThruConnect` argument), so it is not observable via `__mock_get_config_value`; use this accessor instead. Returns `STATUS_NOERROR` or `ERR_INVALID_CHANNEL_ID`. |
| `__mock_get_written_msg_protocol_id(channel_id, index, out_value)` | Read the J2534 protocol ID of the written message at `index` for `channel_id`. Returns `STATUS_NOERROR` or `ERR_INVALID_CHANNEL_ID`. |
| `__mock_get_written_msg_tx_flags(channel_id, index, out_value)` | Read the `TxFlags` of the written message at `index` for `channel_id`. Returns `STATUS_NOERROR` or `ERR_INVALID_CHANNEL_ID`. |

### Message Filter Inspection

Filters are recorded in installation order and re-indexed (0-based) whenever
one is stopped or `IOCTL_CLEAR_MSG_FILTERS` runs, so re-fetch `__mock_get_filter_count`
after any filter-mutating call before iterating by index.

| Function | Description |
|----------|-------------|
| `__mock_get_filter_count(channel_id)` | Number of filters currently installed (not yet stopped or cleared) on `channel_id`. |
| `__mock_get_filter_type(channel_id, index, out_value)` | Read the `FilterType` (`PASS_FILTER` / `BLOCK_FILTER` / `FLOW_CONTROL_FILTER`) of filter `index`. Returns `STATUS_NOERROR` or `ERR_INVALID_CHANNEL_ID`. |
| `__mock_get_filter_mask(channel_id, index, buf, buf_len, out_len)` | Copy `pMaskMsg` data bytes of filter `index` into `buf`. Sets `*out_len` to actual size. |
| `__mock_get_filter_pattern(channel_id, index, buf, buf_len, out_len)` | Copy `pPatternMsg` data bytes of filter `index` into `buf`. Sets `*out_len` to actual size. |
| `__mock_get_filter_pattern_tx_flags(channel_id, index, out_value)` | Read the `TxFlags` (e.g. `CAN_29BIT_ID`, `ISO15765_ADDR_TYPE`) of `pPatternMsg` for filter `index`. |
| `__mock_get_filter_has_flow_control(channel_id, index, out_value)` | Writes 1 to `*out_value` if filter `index` was installed with a non-null `pFlowControlMsg`, 0 otherwise. |
| `__mock_get_filter_flow_control(channel_id, index, buf, buf_len, out_len)` | Copy `pFlowControlMsg` data bytes of filter `index` into `buf`. Returns `ERR_INVALID_CHANNEL_ID` if the filter has no flow-control message — check `__mock_get_filter_has_flow_control` first. |
| `__mock_get_filter_flow_control_tx_flags(channel_id, index, out_value)` | Read the `TxFlags` of `pFlowControlMsg` for filter `index`. Same `ERR_INVALID_CHANNEL_ID` precondition as `__mock_get_filter_flow_control`. |

These accessors are also the way to inspect mock state from a process
that loaded this library dynamically (e.g. `libloading::Library::new(path)`
against the same file path a service under test resolved via its config)
rather than linking it as an `rlib` — the `__mock_*` exports operate on the
loaded module's own copy of the state, whereas the safe Rust functions below
operate on the caller's statically-linked copy. Loading the identical file
path twice (once by the service under test, once by the test itself) resolves
to the same shared object, so both callers observe the same state through the
`__mock_*` exports.

---

## Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `MOCK_DEVICE_ID` | 1 | Device ID returned by `PassThruOpen` |
| `MOCK_VBATT_MV` | 12000 | Battery voltage in millivolts |
| `MOCK_PROG_VOLTAGE_MV` | 0 | Programming voltage in millivolts |
| `MOCK_TIMESTAMP` | 4242 | Timestamp value used in injected messages |
| `FIRMWARE_VERSION` | `"1.0.0"` | Returned by `PassThruReadVersion` |
| `DLL_VERSION` | `"1.0.0"` | Returned by `PassThruReadVersion` |
| `API_VERSION` | `"04.04"` | Returned by `PassThruReadVersion` |

---

## Known Limitations

- Message filters are allocated (IDs are tracked) but do not actually filter incoming messages — all messages reach the RX queue.
- Periodic messages are stored but not autonomously transmitted into the RX queue at set intervals.
- `PassThruGetNextCarDAQ` is not implemented.
- The mock uses a process-global singleton (`OnceLock<Mutex<MockState>>`). Tests that run in the same process share state; always call `__mock_reset()` between tests.
