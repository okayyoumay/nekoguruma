# j2534-0404 Implementation Note

## Scope

Safe Rust wrapper for SAE J2534-1 04.04 APIs from j2534-0404-sys.

## Design Policy

- Keep unsafe blocks at FFI boundaries only.
- Represent device/channel/message identifiers with newtypes.
- Use a flat API with explicit DeviceId/ChannelId parameters.
- Require callers to close/disconnect explicitly instead of relying on Drop.
- Use owned/borrowed message wrappers over `PASSTHRU_MSG` to reduce payload copying.
- Validate message payload length against PASSTHRU_MSG Data capacity.

## Initial API Surface

- Library loading and status/error mapping.
- PassThruOpen/Close via explicit DeviceId.
- PassThruConnect/Disconnect via explicit ChannelId.
- PassThruReadVersion and PassThruGetLastError.
- PassThruReadMsgs/WriteMsgs.
- PassThruStart/StopPeriodicMsg and Start/StopMsgFilter.
- PassThruIoctl helpers for common clear operations.

## Message Ownership Strategy

- Message wrappers are defined in `src/message.rs` and re-exported from the crate root.
- `PassThruMessage` is the owned wrapper around native `PASSTHRU_MSG` storage.
- `BorrowedPassThruMessage` provides borrowed field/data access without allocating a `Vec<u8>`.
- `read_messages()` returns owned `PassThruMessage` values directly from the native buffer.
- `write_messages()` accepts mutable owned messages and passes them straight to the native API.

## Examples and Live Tests

- ISO15765 helper module in src/iso15765.rs:
	- Build normal/extended single-frame messages.
	- Optional frame padding helper for classic CAN payload length.
- Safe example in examples/read_version_0404.rs using this wrapper instead of raw sys calls.
- Live integration smoke test in tests/live_smoke.rs:
	- Uses J2534_DLL_PATH environment variable.
	- Auto-skips if environment variable is not set.

- Live channel integration test in tests/live_channel.rs:
	- Requires J2534_RUN_CHANNEL_TEST=1 (or true) and J2534_DLL_PATH.
	- Supports optional configuration through:
		- J2534_PROTOCOL_ID (default ISO15765)
		- J2534_CONNECT_FLAGS (default 0)
		- J2534_BAUD_RATE (default 500000)
		- J2534_WRITE_TIMEOUT_MS (default 200)
		- J2534_READ_TIMEOUT_MS (default 50)
		- J2534_REQUEST_DATA as comma-separated hex bytes (default 00)
	- Read path accepts either a successful message receive or ERR_BUFFER_EMPTY timeout.

- Live ISO15765 helper integration test in tests/live_iso15765.rs:
	- Requires J2534_RUN_ISO15765_TEST=1 (or true) and J2534_DLL_PATH.
	- Uses safe helper builders from src/iso15765.rs for request framing.
	- Optional mode setting via J2534_ISO15765_MODE:
		- normal11 (default): single_frame()
		- extended29: single_frame_extended()
	- Optional J2534_ISO15765_CAN_ID controls CAN identifier used by helper.
	- Writes padded ISO15765 request and accepts read success or ERR_BUFFER_EMPTY timeout.

## SAE J2534-2 Discovery Mechanism (ADR-153)

`src/discovery.rs` adds `DiscoveryParam`/`DiscoveryResult` (re-exported from
the crate root) plus `J2534Api0404::get_device_info`/`get_protocol_info`
(defined in `src/lib.rs`, alongside the other IOCTL wrapper methods): general
`SPARAM_LIST` marshaling for the two SAE J2534-2 (DEC2020) clause 25
Discovery IOCTLs, mirroring `get_config`/`set_config`'s `SCONFIG_LIST`
pattern but with an extra per-parameter `Supported` output flag. Consumed by
`j2534-0404-service`'s epoch-tagged discovery cache
(`service/discovery.rs`), which is internal-only for now — see that crate's
`implementation-notes.md` and ADR-153 for the caching/exposure design.

## Example: Registry DLL Lookup

- List installed 04.04 devices and print FunctionLibrary path/name (auto-select first key):
	- `cargo run -p j2534-0404 --example read_registry_library_0404`
- Query a specific device key (example key string shown):
	- `cargo run -p j2534-0404 --example read_registry_library_0404 -- "ACME Interfaces-Flasher"`

## Real-Device Examples

- `examples/channel_read_write_0404.rs`: full native-library flow against a real
  J2534 device — open, connect an ISO15765 channel (defaults matching
  `tests/live_channel.rs`), write a request, poll for a reply, disconnect, close.
	- `cargo run -p j2534-0404 --example channel_read_write_0404 -- <path-to-j2534-dll>`
- `examples/iso15765_send_0404.rs`: demonstrates the `src/iso15765.rs` single-frame
  builder helpers (normal/extended addressing plus padding) against a real
  ISO15765 channel, mirroring `tests/live_iso15765.rs`'s call sequence.
	- `cargo run -p j2534-0404 --example iso15765_send_0404 -- <path-to-j2534-dll>`
- Both require a physical device and vendor DLL at runtime; neither is gated behind
  an env var or `#[cfg]` — they compile like `read_version_0404.rs` with no device
  present and only need hardware to actually run.
