//! ADR-219: `j2534-0404-service`'s vendor-range (`>= 0x10000`) passthrough --
//! Decision item 2 (`IoCtl` `IoctlID`) and Decision item 3 (`SetComParam`/
//! `GetComParam` `ConfigParameterID`).

use serial_test::serial;
use vci_service_interface::{
    ComLogicalLinkHandle, DataItem, GetComParamRequest, IoBytearray, IoCtlRequest, ModuleHandle,
    ParamItem, PduError, SetComParamRequest, SystemHandle, data_item, error_detail_from_status,
    get_com_param_request, io_ctl_request, param_item, vci_service_client::VciServiceClient,
};

use crate::harness::*;

/// Packs ADR-219 Decision item 2's `bytearray_data` header: `u32 flags`
/// (LE), `u32 output_capacity` (LE), then raw input bytes.
fn pack_vendor_ioctl_header(flags: u32, output_capacity: u32, input: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + input.len());
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&output_capacity.to_le_bytes());
    bytes.extend_from_slice(input);
    bytes
}

fn vendor_ioctl_request(
    handle: io_ctl_request::Handle,
    cmd_id: u32,
    header_bytes: Vec<u8>,
) -> IoCtlRequest {
    IoCtlRequest {
        handle: Some(handle),
        io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(cmd_id)),
        input_data: Some(DataItem {
            data: Some(data_item::Data::BytearrayData(IoBytearray {
                data: header_bytes,
            })),
        }),
        has_output: false,
    }
}

fn bytearray_output(item: DataItem) -> Vec<u8> {
    match item.data {
        Some(data_item::Data::BytearrayData(arr)) => arr.data,
        other => panic!("expected BytearrayData output_data, got {other:?}"),
    }
}

// ── Decision item 2: vendor IoctlID ─────────────────────────────────────────

/// Raw mode (flags bit 2 clear): `pInput`/`pOutput` are a direct `u32 *`
/// pair (`MOCK_VENDOR_IOCTL_RAW_U32`'s own documented shape). Exercised on
/// `module_handle` -- the other handle-resolution half from the CLL-scoped
/// wrapped-mode test below.
#[tokio::test]
#[serial]
async fn vendor_ioctl_raw_mode_round_trips_input_and_output() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    // Opens the device for MOCK_MODULE_HANDLE (ADR-107 lazy-open).
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let input = 41u32.to_le_bytes();
    // flags: bit0 input present, bit1 output requested, bit2 = 0 (raw).
    let header = pack_vendor_ioctl_header(0b011, 4, &input);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_RAW_U32,
        header,
    );

    let response = client
        .io_ctl(request)
        .await
        .expect("raw-mode vendor IOCTL should succeed")
        .into_inner();
    let output_bytes = bytearray_output(
        response
            .output_data
            .expect("raw mode with output requested must return output_data"),
    );
    assert_eq!(
        output_bytes.len(),
        4,
        "raw mode returns the entire allocated output_capacity"
    );
    let value = u32::from_le_bytes(output_bytes.try_into().unwrap());
    // MOCK_VENDOR_IOCTL_RAW_U32 echoes input + 1.
    assert_eq!(value, 42);

    server.shutdown().await;
}

/// Regression (Codex review, PR #133): raw mode's backing output buffer
/// must be allocated at a fixed, safe maximum regardless of the client's
/// requested `output_capacity` -- never at `output_capacity` itself -- since
/// raw mode has no way to tell the native side how large the buffer actually
/// is (see `VendorIoCtl::Raw`'s own doc comment). Requests `output_capacity:
/// 1` against `MOCK_VENDOR_IOCTL_RAW_U32`, which unconditionally writes 4
/// bytes through any non-null raw output pointer -- before the fix, this
/// allocated only a 1-byte `Vec<u8>` and handed its pointer to the native
/// call, an out-of-bounds heap write. The call must still succeed (the
/// larger, safety-padded backing allocation absorbs the native write
/// harmlessly), and the bytes returned to the client must be truncated to
/// exactly the requested `output_capacity` (1), not the mock's full 4-byte
/// write -- proving both the truncation and (structurally) that the backing
/// allocation was never undersized in the first place.
#[tokio::test]
#[serial]
async fn vendor_ioctl_raw_mode_small_output_capacity_does_not_overflow() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let input = 41u32.to_le_bytes();
    // flags: bit0 input present, bit1 output requested, bit2 = 0 (raw).
    // output_capacity is deliberately far smaller than the 4 bytes
    // MOCK_VENDOR_IOCTL_RAW_U32 always writes.
    let header = pack_vendor_ioctl_header(0b011, 1, &input);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_RAW_U32,
        header,
    );

    let response = client
        .io_ctl(request)
        .await
        .expect(
            "a small output_capacity must not corrupt the service process; the fix pads the \
             backing allocation, not the client-visible response",
        )
        .into_inner();
    let output_bytes = bytearray_output(
        response
            .output_data
            .expect("raw mode with output requested must return output_data"),
    );
    assert_eq!(
        output_bytes.len(),
        1,
        "the response must be truncated to the client's requested output_capacity (1), not the \
         mock's full 4-byte native write nor the larger safety-padded backing allocation"
    );

    server.shutdown().await;
}

/// Regression (Codex review, PR #133, second round): raw mode's backing
/// INPUT buffer must be zero-padded up to the same safe maximum as the
/// output side -- never left at exactly the client-supplied length -- since
/// `MOCK_VENDOR_IOCTL_RAW_U32` unconditionally dereferences `pInput` as a
/// 4-byte `u32 *` regardless of how many bytes the client actually sent.
/// Requests a 1-byte input; before the fix, this allocated only a 1-byte
/// `Vec<u8>` and handed its pointer to the native call, which read 3 bytes
/// past the end of that allocation (an out-of-bounds heap read). The call
/// must still succeed, and the echoed value must reflect the client's
/// single real byte followed by zero padding (41 -> 42), proving the
/// padding bytes are deterministic zeros rather than uninitialized/
/// adjacent-heap garbage.
#[tokio::test]
#[serial]
async fn vendor_ioctl_raw_mode_small_input_does_not_overflow() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // A single byte -- MOCK_VENDOR_IOCTL_RAW_U32 reads 4 bytes through
    // pInput regardless, so the remaining 3 bytes must come from the
    // fix's zero padding, not out-of-bounds memory.
    let input = [41u8];
    // flags: bit0 input present, bit1 output requested, bit2 = 0 (raw).
    let header = pack_vendor_ioctl_header(0b011, 4, &input);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_RAW_U32,
        header,
    );

    let response = client
        .io_ctl(request)
        .await
        .expect(
            "an undersized raw-mode input must not corrupt the service process; the fix pads \
             the backing allocation, not the client-visible input length",
        )
        .into_inner();
    let output_bytes = bytearray_output(
        response
            .output_data
            .expect("raw mode with output requested must return output_data"),
    );
    let value = u32::from_le_bytes(
        output_bytes
            .try_into()
            .expect("output_capacity 4 must return exactly 4 bytes"),
    );
    // MOCK_VENDOR_IOCTL_RAW_U32 echoes input + 1. A little-endian u32 read
    // of [41, 0, 0, 0] (the client's one real byte followed by zero
    // padding) is 41, so the echoed value is 42 -- had the padding bytes
    // instead been uninitialized/adjacent-heap garbage, this would be
    // nondeterministic.
    assert_eq!(value, 42);

    server.shutdown().await;
}

/// Wrapped mode (flags bit 2 set): `pInput`/`pOutput` are each an
/// `SBYTE_ARRAY` (`MOCK_VENDOR_IOCTL_WRAPPED_ECHO`'s own documented shape,
/// which reverses the input and self-reports the echoed length via
/// `NumOfBytes`). Exercised on `cll_handle`.
#[tokio::test]
#[serial]
async fn vendor_ioctl_wrapped_mode_round_trips_input_and_output() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let input = [0x01u8, 0x02, 0x03, 0x04];
    // flags: bit0 input present, bit1 output requested, bit2 = 1 (wrapped).
    let header = pack_vendor_ioctl_header(0b111, 16, &input);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::CllHandle(cll_handle),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_WRAPPED_ECHO,
        header,
    );

    let response = client
        .io_ctl(request)
        .await
        .expect("wrapped-mode vendor IOCTL should succeed")
        .into_inner();
    let output_bytes = bytearray_output(
        response
            .output_data
            .expect("wrapped mode with output requested must return output_data"),
    );
    // The mock echoes the reversed input (4 bytes), self-reporting NumOfBytes
    // = 4 even though output_capacity was 16 -- proving the
    // min(NumOfBytes, output_capacity) read-back, not the full buffer.
    assert_eq!(output_bytes, vec![0x04, 0x03, 0x02, 0x01]);

    server.shutdown().await;
}

/// ADR-219 Decision item 2: `system_handle` is rejected -- a J2534 IOCTL
/// targets a device or a channel natively, never this service's own
/// top-level system handle.
#[tokio::test]
#[serial]
async fn vendor_ioctl_rejects_system_handle() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let header = pack_vendor_ioctl_header(0, 0, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::SystemHandle(SystemHandle {}),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_RAW_U32,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("a vendor IOCTL against system_handle should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// ADR-219 Decision item 2: every flags bit above bit 2 is reserved and
/// must be zero.
#[tokio::test]
#[serial]
async fn vendor_ioctl_rejects_a_reserved_flag_bit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let header = pack_vendor_ioctl_header(1 << 5, 0, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_RAW_U32,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("a reserved flags bit should be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Regression (ADR-219): an unhandled `cmd_id` BELOW the vendor range
/// (`< 0x10000`) must still fall through to `rpc_io_ctl_legacy`'s own
/// catch-all and reject `Unimplemented`/`PDU_ERR_ID_NOT_SUPPORTED`, exactly
/// as before this ADR -- unaffected by the new `>= 0x10000` dispatch arm.
#[tokio::test]
#[serial]
async fn cmd_id_below_vendor_range_still_rejects_unimplemented() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(0x9999)),
            input_data: None,
            has_output: false,
        })
        .await
        .expect_err("an unrecognized cmd_id below 0x10000 should still be rejected Unimplemented");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrIdNotSupported as i32
    );

    server.shutdown().await;
}

/// Exact-boundary regression, mirroring `comparam_id.rs`'s
/// `just_below_vendor_range_stays_unsupported`: `0xFFFF` is the top of SAE
/// J2534-2's reserved range (Context), one below the ADR-219 vendor
/// boundary. Distinct from `cmd_id_below_vendor_range_still_rejects_unimplemented`
/// above (which uses a far-from-boundary `0x9999`) -- this pins the exact
/// value just below `>= 0x10000` rather than merely "some value below it".
#[tokio::test]
#[serial]
async fn cmd_id_at_top_of_reserved_range_still_rejects_unimplemented() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let status = client
        .io_ctl(IoCtlRequest {
            handle: Some(io_ctl_request::Handle::CllHandle(cll_handle)),
            io_ctrl_command: Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(0xFFFF)),
            input_data: None,
            has_output: false,
        })
        .await
        .expect_err(
            "cmd_id 0xFFFF (just below the vendor range) should still be rejected Unimplemented",
        );
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrIdNotSupported as i32
    );

    server.shutdown().await;
}

/// Exact-boundary regression, the vendor-range-floor companion to
/// `cmd_id_at_top_of_reserved_range_still_rejects_unimplemented` above:
/// `cmd_id == 0x10000` (`0x0001_0000`, the lowest value in ADR-219 Decision
/// item 2's vendor range) must be routed by `rpc_io_ctl`'s `id >= 0x0001_0000`
/// arm to `rpc_io_ctl_vendor`, not fall through to `rpc_io_ctl_legacy`'s
/// catch-all.
///
/// `0x00010000` is allowlisted in `VENDOR_IOCTLS_TOML` as a bufferless
/// `shape = "raw"` entry (ADR-219's seventh-round amendment requires every
/// vendor `cmd_id` to be allowlisted, bufferless ones included) so this
/// test's NULL/NULL request reaches native dispatch rather than being
/// rejected `FAILED_PRECONDITION` before ever getting there. This mock has
/// no registered native handler for the bare value `0x10000` itself (only
/// `MOCK_VENDOR_IOCTL_RAW_U32`/`MOCK_VENDOR_IOCTL_WRAPPED_ECHO`, at
/// `0x10001`/`0x10002`, simulate an actual successful vendor IOCTL), so a
/// well-formed request at this exact boundary still fails at the native
/// call -- but with the generic `PDU_ERR_ID_NOT_SUPPORTED`-via-`Code::Internal`
/// signature `map_native_error_for_link` produces for an ordinary native
/// failure (`error.rs`'s blanket "every `PDUError` except
/// `PDU_ERR_INVALID_HANDLE` maps to `Code::Internal`" rule), never the
/// legacy path's hardcoded `Code::Unimplemented`. That distinct signature is
/// the proof the vendor dispatch arm was reached -- i.e. that the request
/// was "dispatched through the new vendor path successfully" -- as opposed
/// to the pre-ADR-219 short-circuit the sibling test above still exercises
/// one value lower.
#[tokio::test]
#[serial]
async fn cmd_id_at_vendor_range_floor_dispatches_through_vendor_path() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let header = pack_vendor_ioctl_header(0, 0, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        0x0001_0000,
        header,
    );

    let status = client
        .io_ctl(request)
        .await
        .expect_err("cmd_id 0x10000 is unregistered at the native mock, but must still reach it");
    assert_ne!(
        status.code(),
        tonic::Code::Unimplemented,
        "0x10000 must not take the legacy rpc_io_ctl_legacy catch-all path"
    );
    assert_eq!(status.code(), tonic::Code::Internal);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrIdNotSupported as i32
    );

    server.shutdown().await;
}

// ── ADR-219 amendment: `vendor_ioctls` config contract ──────────────────────
//
// The mock library's `vendor_ioctls` config (`harness.rs::VENDOR_IOCTLS_TOML`,
// baked into every `TestServer::start*` call) configures `0x00010000`
// (unregistered at the mock, shape "raw", both byte counts 0 -- the
// bufferless-allowlist entry), `0x00010001` (MOCK_VENDOR_IOCTL_RAW_U32, shape
// "raw", input_bytes/output_bytes 4), `0x00010002`
// (MOCK_VENDOR_IOCTL_WRAPPED_ECHO, shape "sbyte_array"), `0x00010003`
// (unregistered at the mock, shape "raw", input_bytes 0, output_bytes 4), and
// `0x00010005` (unregistered at the mock, shape "sbyte_array",
// input_required/output_required both true). Any other cmd_id (e.g.
// 0x00010004 below) is deliberately left unconfigured -- and, as of ADR-219's
// seventh-round amendment, rejected FailedPrecondition even for a NULL/NULL
// request (see `vendor_ioctl_null_null_on_unconfigured_cmd_id_is_failed_precondition`).

/// A `cmd_id` with no `vendor_ioctls` entry at all, carrying a non-NULL
/// output request in RAW mode: rejected `FailedPrecondition` before any
/// native call or lock is taken.
#[tokio::test]
#[serial]
async fn vendor_ioctl_unconfigured_cmd_id_with_raw_output_is_failed_precondition() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit1 output requested, bit2 = 0 (raw).
    let header = pack_vendor_ioctl_header(0b010, 4, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        0x0001_0004,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("an unconfigured cmd_id with a non-NULL output request must be rejected");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);

    server.shutdown().await;
}

/// Same as above, but WRAPPED mode (flags bit 2 set) -- the config
/// requirement applies to both shapes equally, not just raw mode.
#[tokio::test]
#[serial]
async fn vendor_ioctl_unconfigured_cmd_id_with_wrapped_output_is_failed_precondition() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit1 output requested, bit2 = 1 (wrapped).
    let header = pack_vendor_ioctl_header(0b110, 4, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        0x0001_0004,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("an unconfigured cmd_id with a non-NULL output request must be rejected");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);

    server.shutdown().await;
}

/// `0x00010005` is configured `shape = "sbyte_array", input_required = true,
/// output_required = true` -- a client sending neither input nor requesting
/// output is rejected `InvalidArgument` before any native call, the wrapped
/// counterpart to `vendor_ioctl_raw_mode_non_empty_input_with_input_bytes_zero_is_invalid_argument`-style
/// raw-mode required-direction checks (ADR-219's seventh-round amendment:
/// `SbyteArray` has no byte count to double as a presence requirement the
/// way `Raw` does, so `input_required`/`output_required` fill that role).
#[tokio::test]
#[serial]
async fn vendor_ioctl_wrapped_mode_rejects_missing_input_when_input_required() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit0 = 0 (no input), bit2 = 1 (wrapped); no output requested.
    let header = pack_vendor_ioctl_header(0b100, 0, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        0x0001_0005,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("input_required is configured but the request carries no input");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Same `0x00010005` contract as above, but the client supplies input (so
/// `input_required` is satisfied) and requests no output -- rejected for
/// `output_required` instead.
#[tokio::test]
#[serial]
async fn vendor_ioctl_wrapped_mode_rejects_missing_output_when_output_required() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit0 = 1 (input present), bit2 = 1 (wrapped); no output requested.
    let header = pack_vendor_ioctl_header(0b101, 0, &[1, 2, 3]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        0x0001_0005,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("output_required is configured but the request did not request any output");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// Same `0x00010005` contract as the two tests above (`input_required` AND
/// `output_required` both `true`), but this time BOTH directions are
/// satisfied together -- proving the two independent presence checks don't
/// interact badly when both must pass at once (edge-case-hunter, PR #133).
/// `0x00010005` is unregistered at the native mock, so a request that
/// clears validation still fails at the native call -- the distinct
/// `Internal`/`PDU_ERR_ID_NOT_SUPPORTED` signature (never `InvalidArgument`
/// or `FailedPrecondition`) is the proof dispatch was actually reached,
/// mirroring `cmd_id_at_vendor_range_floor_dispatches_through_vendor_path`'s
/// identical pattern for the `Raw` shape.
#[tokio::test]
#[serial]
async fn vendor_ioctl_wrapped_mode_accepts_both_directions_when_both_required() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit0 = 1 (input present), bit1 = 1 (output requested), bit2 = 1 (wrapped).
    let header = pack_vendor_ioctl_header(0b111, 4, &[1, 2, 3]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        0x0001_0005,
        header,
    );
    let status = client.io_ctl(request).await.expect_err(
        "0x00010005 is unregistered at the native mock, but a request satisfying both \
         input_required and output_required must still reach it",
    );
    assert_ne!(
        status.code(),
        tonic::Code::InvalidArgument,
        "a request satisfying both required directions must not be rejected for either"
    );
    assert_eq!(status.code(), tonic::Code::Internal);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrIdNotSupported as i32
    );

    server.shutdown().await;
}

/// `0x00010001` is configured `shape = "raw"`, but the client sets flags bit
/// 2 (wrapped): the client-selected mode must match the configured shape.
#[tokio::test]
#[serial]
async fn vendor_ioctl_configured_raw_but_client_selects_wrapped_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit1 output requested, bit2 = 1 (wrapped) -- but 0x00010001 is
    // configured "raw".
    let header = pack_vendor_ioctl_header(0b110, 4, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_RAW_U32,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("a shape mismatch (configured raw, client requests wrapped) must be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// `0x00010002` is configured `shape = "sbyte_array"`, but the client leaves
/// flags bit 2 clear (raw): the client-selected mode must match the
/// configured shape.
#[tokio::test]
#[serial]
async fn vendor_ioctl_configured_wrapped_but_client_selects_raw_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit1 output requested, bit2 = 0 (raw) -- but 0x00010002 is
    // configured "sbyte_array".
    let header = pack_vendor_ioctl_header(0b010, 4, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::CllHandle(cll_handle),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_WRAPPED_ECHO,
        header,
    );
    let status = client.io_ctl(request).await.expect_err(
        "a shape mismatch (configured sbyte_array, client requests raw) must be rejected",
    );
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// `0x00010001` is configured `output_bytes = 4`; a client-requested
/// `output_capacity` greater than that is rejected, not silently clamped or
/// forwarded with an oversized allocation.
#[tokio::test]
#[serial]
async fn vendor_ioctl_raw_mode_output_capacity_above_configured_output_bytes_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit1 output requested, bit2 = 0 (raw). output_capacity (5)
    // exceeds the configured output_bytes (4).
    let header = pack_vendor_ioctl_header(0b010, 5, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_RAW_U32,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("output_capacity above the configured output_bytes must be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// `0x00010001` is configured `input_bytes = 4`; a client input longer than
/// that is rejected, not silently truncated or forwarded past the
/// configured native contract.
#[tokio::test]
#[serial]
async fn vendor_ioctl_raw_mode_input_longer_than_configured_input_bytes_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit0 input present, bit2 = 0 (raw). 5 bytes of input exceeds
    // the configured input_bytes (4).
    let header = pack_vendor_ioctl_header(0b001, 0, &[1, 2, 3, 4, 5]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_RAW_U32,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("input longer than the configured input_bytes must be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// `0x00010003` is configured `input_bytes = 0` (that direction takes no
/// input buffer at all); a client that still sends non-empty input is
/// rejected, not silently accepted with the extra bytes discarded.
#[tokio::test]
#[serial]
async fn vendor_ioctl_raw_mode_non_empty_input_with_input_bytes_zero_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit0 input present, bit2 = 0 (raw). 0x00010003 is configured
    // input_bytes = 0.
    let header = pack_vendor_ioctl_header(0b001, 0, &[1]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        0x0001_0003,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("non-empty input with configured input_bytes == 0 must be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// A completely unconfigured `cmd_id` with NO buffers at all (no input,
/// `output_capacity == 0`) is STILL rejected `FAILED_PRECONDITION` --
/// distinct from `cmd_id_at_vendor_range_floor_dispatches_through_vendor_path`
/// above (which pins the exact vendor-range floor value, now itself
/// allowlisted as a bufferless `shape = "raw"` entry). This test used to
/// assert the opposite (a bufferless request against an unconfigured
/// `cmd_id` "still forwards", needing no config): a design-advisor consult
/// (Codex review, PR #133 seventh round) reversed that decision, since a
/// vendor `cmd_id`'s native contract can unconditionally require a buffer
/// regardless of what a particular client request asks for -- every vendor
/// `cmd_id` must now be allowlisted in `vendor_ioctls`, including nominally
/// bufferless ones (via `shape = "raw"` with both byte counts at `0`, as
/// `cmd_id_at_vendor_range_floor_dispatches_through_vendor_path` now does).
#[tokio::test]
#[serial]
async fn vendor_ioctl_null_null_on_unconfigured_cmd_id_is_failed_precondition() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    let header = pack_vendor_ioctl_header(0, 0, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::ModuleHandle(ModuleHandle {
            module_handle: MOCK_MODULE_HANDLE,
        }),
        0x0001_0004,
        header,
    );
    let status = client.io_ctl(request).await.expect_err(
        "0x00010004 has no vendor_ioctls entry, so even a NULL/NULL request must be rejected",
    );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);

    server.shutdown().await;
}

/// Regression, moved from the old fixed-64-KiB-raw-cap unit test
/// (`rpc_misc.rs`'s `unpack_vendor_ioctl_request_tests`) now that the fixed
/// cap applies to WRAPPED mode only (ADR-219, as amended) -- raw mode's
/// bound now comes from the per-`cmd_id` `vendor_ioctls` config contract
/// instead. `0x00010002` is configured `shape = "sbyte_array"`, so the
/// wrapped-mode cap (64 KiB) applies.
#[tokio::test]
#[serial]
async fn vendor_ioctl_wrapped_mode_rejects_an_output_capacity_above_the_cap() {
    const VENDOR_IOCTL_MAX_WRAPPED_CAPACITY: u32 = 64 * 1024;

    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    // flags: bit1 output requested, bit2 = 1 (wrapped).
    let header = pack_vendor_ioctl_header(0b110, VENDOR_IOCTL_MAX_WRAPPED_CAPACITY + 1, &[]);
    let request = vendor_ioctl_request(
        io_ctl_request::Handle::CllHandle(cll_handle),
        j2534_0404_mock::MOCK_VENDOR_IOCTL_WRAPPED_ECHO,
        header,
    );
    let status = client
        .io_ctl(request)
        .await
        .expect_err("output_capacity above the 64 KiB wrapped-mode cap must be rejected");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

// ── Decision item 3: vendor ConfigParameterID ───────────────────────────────

/// A protocol id this service does not itself recognize. Originally the
/// ONLY protocol a vendor ComParamId was reachable on, via
/// `is_param_allowed`'s pre-existing "Unknown protocol -- allow" fallback
/// (`comparam_support.rs`) -- `is_param_allowed` gated `SetComParam`/
/// `GetComParam` before `to_j2534_config_id` was ever consulted, so a
/// vendor id was rejected outright on any protocol this service recognizes
/// (e.g. CAN). A design-advisor-decided ADR-219 amendment added a vendor
/// (`ComParamId::is_vendor()`, `>= 0x10000`) early return to
/// `is_param_allowed`, admitting a vendor id unconditionally on every
/// protocol; kept here as a still-valid exercise of the allowlist's
/// generic "Unknown protocol" fallback, now alongside (not instead of) the
/// recognized-protocol coverage below.
const UNKNOWN_PROTOCOL_ID: u32 = 0x1234_5678;
const VENDOR_COMPARAM_ID: u32 = 0x0001_1234;

async fn create_and_connect_unknown_protocol_cll(
    client: &mut VciServiceClient<tonic::transport::Channel>,
) -> ComLogicalLinkHandle {
    create_and_connect_cll(client, UNKNOWN_PROTOCOL_ID, &[]).await
}

/// A vendor `ConfigParameterID`'s `SetComParam`/`GetComParam` round trip via
/// `Working` staging -- the ordinary (non-live) path, unaffected by
/// Decision item 3's unstaged-read behavior.
#[tokio::test]
#[serial]
async fn vendor_comparam_set_get_round_trips_via_working() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_and_connect_unknown_protocol_cll(&mut client).await;

    set_com_param_unum32(&mut client, cll_handle, VENDOR_COMPARAM_ID, 777).await;
    let value = get_com_param_unum32(&mut client, cll_handle, VENDOR_COMPARAM_ID).await;
    assert_eq!(value, 777);

    server.shutdown().await;
}

/// Unstaged + connected: `GetComParam` on a vendor id never `SetComParam`'d
/// on this link performs a live `PassThruGetConfig` read instead of
/// fabricating `Unum32(0)`, and does NOT insert the result into `Working`
/// (a second `GetComParam` reads the live value again, not a cached one).
#[tokio::test]
#[serial]
async fn vendor_comparam_unstaged_connected_reads_live_value_without_staging() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_and_connect_unknown_protocol_cll(&mut client).await;

    // Nothing has ever staged VENDOR_COMPARAM_ID on this link -- the mock's
    // own GET_CONFIG default for an unset channel param is 0 (channel.params
    // falls back to `unwrap_or(0)`), so an unstaged live read reports 0 too
    // -- distinct from Working default 0 only in HOW it was obtained,
    // verified below by staging then observing GetComParam still ignores
    // the live read path (Working wins once present).
    let live_read = get_com_param_unum32(&mut client, cll_handle, VENDOR_COMPARAM_ID).await;
    assert_eq!(live_read, 0);

    // The live read above must NOT have staged anything into Working: a
    // vendor SetComParam afterward still succeeds and is the ONLY value
    // GetComParam subsequently reports (if the live read had staged 0 into
    // Working, this would be indistinguishable from this same assertion --
    // the real proof is the disconnected case below, where a prior staged
    // 0 would silently avoid ever exercising failed_precondition).
    set_com_param_unum32(&mut client, cll_handle, VENDOR_COMPARAM_ID, 55).await;
    let staged_read = get_com_param_unum32(&mut client, cll_handle, VENDOR_COMPARAM_ID).await;
    assert_eq!(staged_read, 55);

    server.shutdown().await;
}

/// Unstaged + disconnected: `GetComParam` on a vendor id never
/// `SetComParam`'d, on a CLL that is `Created` but never `Connected`,
/// rejects `FailedPrecondition`/`PDU_ERR_CLL_NOT_CONNECTED` -- no Working
/// value and no live value to read either; never a fabricated `Unum32(0)`.
#[tokio::test]
#[serial]
async fn vendor_comparam_unstaged_disconnected_rejects_failed_precondition() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_cll(&mut client, UNKNOWN_PROTOCOL_ID, 1).await;

    let status = client
        .get_com_param(GetComParamRequest {
            cll_handle: Some(cll_handle),
            param: Some(get_com_param_request::Param::ParamId(VENDOR_COMPARAM_ID)),
        })
        .await
        .expect_err(
            "an unstaged vendor ComParamId on a disconnected CLL must reject, not fabricate 0",
        );
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error_detail_from_status(&status).unwrap().pdu_error,
        PduError::PduErrCllNotConnected as i32
    );

    server.shutdown().await;
}

/// Regression: `SetComParam` of a vendor ComParamId still stores a plain
/// `Unum32` (never routed into the Bytefield/Structfield arms) -- the
/// oneof-shape half of Decision item 3's "Unum32 only" rule, exercised
/// directly against the RPC surface rather than relying solely on the
/// round-trip test above to imply it.
#[tokio::test]
#[serial]
async fn vendor_comparam_set_accepts_only_unum32() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_and_connect_unknown_protocol_cll(&mut client).await;

    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(VENDOR_COMPARAM_ID)),
                com_param_class: vci_service_interface::PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Bytefield(vec![1, 2, 3])),
            }),
        })
        .await
        .expect_err("a vendor ComParamId is Unum32-only; Bytefield must be rejected");
    // `rpc_set_com_param`'s Bytefield arm rejects any `param_id` outside
    // `BYTEFIELD_PARAMS` (a vendor id is never a member) with
    // `Status::unimplemented`, the same generic "unsupported bytefield
    // com_param_id" catch-all every other unrecognized Bytefield id hits --
    // not a vendor-specific rejection path.
    assert_eq!(status.code(), tonic::Code::Unimplemented);

    server.shutdown().await;
}

/// Duplicate of `vendor_comparam_set_get_round_trips_via_working` above, but
/// targeting a RECOGNIZED protocol (CAN) instead of `UNKNOWN_PROTOCOL_ID` --
/// proves the ADR-219 amendment's fix: before it, `is_param_allowed`
/// rejected a vendor ComParamId outright on any protocol this service
/// itself recognizes, and only the "Unknown protocol" fallback above ever
/// reached `to_j2534_config_id`'s passthrough logic.
#[tokio::test]
#[serial]
async fn vendor_comparam_set_get_round_trips_via_working_on_recognized_protocol() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000)],
    )
    .await;

    set_com_param_unum32(&mut client, cll_handle, VENDOR_COMPARAM_ID, 777).await;
    let value = get_com_param_unum32(&mut client, cll_handle, VENDOR_COMPARAM_ID).await;
    assert_eq!(value, 777);

    server.shutdown().await;
}

/// Mock-level regression: a vendor ComParamId staged via `SetComParam`
/// before `ConnectComLogicalLink`, on a CAN CLL, is forwarded by identity
/// (ADR-219 Decision item 3) into the connect-time `SET_CONFIG` batch
/// (`apply_j2534_params`) -- observed directly through the mock's
/// per-channel config-value store and `SET_CONFIG` param log, not just
/// through the `GetComParam` round trip the tests above exercise.
#[tokio::test]
#[serial]
async fn vendor_comparam_staged_before_connect_is_forwarded_in_connect_set_config_batch() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let _cll_handle = create_and_connect_cll(
        &mut client,
        j2534_0404::CAN,
        &[(j2534_0404::DATA_RATE, 500_000), (VENDOR_COMPARAM_ID, 4242)],
    )
    .await;

    assert_eq!(
        server
            .backdoor
            .config_value(MOCK_CHANNEL_ID, VENDOR_COMPARAM_ID),
        4242,
        "a vendor ComParamId staged before connect should be forwarded by identity in the \
         connect-time SET_CONFIG batch"
    );
    let log = server.backdoor.set_config_param_log(MOCK_CHANNEL_ID);
    assert!(
        log.contains(&VENDOR_COMPARAM_ID),
        "VENDOR_COMPARAM_ID should have been SET_CONFIG'd during connect; log was {log:#x?}"
    );

    server.shutdown().await;
}
