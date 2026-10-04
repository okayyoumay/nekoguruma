use super::{
    EcuUniqueRespEntry, comparam_support, discovery, events,
    rpc_link::{InstallFilterFailure, install_client_message_filters, install_pass_all_filter},
    rpc_primitive,
    rpc_primitive::tx_flag_bit_to_j2534,
    tx_header, *,
};
use tokio::sync::MutexGuard;
use tonic::Code;
use tracing::{debug, warn};
use vci_service_interface::PduError;

use crate::error::{
    gm_uart_shared_channel_locked_status, map_native_error_as, map_native_error_for_link,
    state_guard_status, unknown_handle_status,
};

/// Hard ceiling for `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s client-supplied
/// `QueueSize`, applied in `ioctl_set_event_queue_properties`. A client can
/// request a materially larger RX ring buffer than the pre-existing
/// hard-coded `RX_BUF_CAPACITY` default, but not an effectively unbounded one
/// -- without this, a careless or malicious client could set `queue_size`
/// near `u32::MAX` and grow `rx_buf` without limit under sustained traffic if
/// it never drains via `GetEventItem`. Chosen as a fixed multiple of the
/// existing default cap rather than an unrelated constant.
const MAX_EVENT_QUEUE_CAP: usize = RX_BUF_CAPACITY * 16;

/// ADR-161 Phase 1's occupancy-epoch predicate, factored out of
/// `ioctl_reset` so it is directly unit-testable against bare
/// `shared_channels`/snapshot values (no interleaving hooks needed --
/// ADR-161's own accepted-residual note claims this). `true` only when the
/// channel's current `occupancy_epoch` (read live from `shared_channels`,
/// under that same guard) still equals the value RESET's snapshot recorded
/// for it. `None` on either side -- the channel no longer exists in
/// `shared_channels`, or (should not happen in practice, since both are
/// captured together) it was absent from the snapshot -- is a mismatch, not
/// a match: an unresolvable comparison must never authorize the
/// channel-wide clears the mismatch case exists to block. A `ref_count`
/// decrement alone never changes `occupancy_epoch` (ADR-161), so a plain
/// sibling disconnect with no join is correctly reported as unchanged here.
fn channel_occupancy_matches_snapshot(
    live_epoch: Option<u64>,
    snapshot_epoch: Option<u64>,
) -> bool {
    matches!((live_epoch, snapshot_epoch), (Some(live), Some(snapshot)) if live == snapshot)
}

/// What `J2534Service::take_broadcast_periodic_under_api_locked` took off a
/// CLL, together with the session identity it captured from that SAME
/// `logical_links` critical section (Codex review round 15, P1, PR #101,
/// ADR-193) -- so a caller that then has to restore-or-leak-track a failed
/// native stop can hand exactly those values to
/// [`CapturedBroadcastPeriodicSession`] without re-deriving any of them from
/// a later, possibly-raced lookup (the "captured, not re-derived" principle
/// rounds 6/10/11 established for this mechanism).
///
/// `channel_id`/`channel_key`/`connect_generation` are read LIVE at take
/// time, in the same critical section as the take, so they are consistent
/// with it. A caller that clears or takes those fields in an earlier critical
/// section of its own must not use this helper at all: it must take the
/// periodic entry in that SAME earlier section and keep its own captures
/// (ADR-193 Decision item 3; `DisconnectComLogicalLink` and
/// `ioctl_suspend_tx_queue` both do exactly that). Taking here instead would
/// read the already-cleared `None`s -- and, worse, would leave a window in
/// which the CLL carries a live entry with no `channel_id`, so any other
/// terminator reading under `logical_links` alone would skip its native stop
/// and orphan a still-transmitting periodic message.
pub(super) struct TakenBroadcastPeriodic {
    pub(super) periodic: Tp20BroadcastPeriodic,
    pub(super) channel_id: Option<ChannelId>,
    pub(super) queue_target: Option<events::CllQueueTarget>,
    pub(super) connect_generation: u64,
    pub(super) channel_key: Option<ChannelKey>,
}

/// Packs SAE J2534-2 clause 18 Device Configuration `parameter_id`/`value`
/// pairs (`NON_VOLATILE_STORE_1`.._10`) into the hand-packed little-endian
/// byte layout ADR-178 specifies for `DataItem.bytearray_data`
/// (`IOBytearray`): `u32 entry_count`, then `entry_count` × `{u32
/// parameter_id, u32 value}`. One shape serves both `PDU_IOCTL_SET_DEVICE_
/// CONFIG`'s input and `PDU_IOCTL_GET_DEVICE_CONFIG`'s input/output, exactly
/// as the removed `IODeviceConfigList` proto message did.
fn pack_device_config_entries(entries: &[(u32, u32)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + entries.len() * 8);
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (parameter_id, value) in entries {
        bytes.extend_from_slice(&parameter_id.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Inverse of [`pack_device_config_entries`]. Rejects a payload shorter than
/// the 4-byte `entry_count` itself, and one whose remaining byte length does
/// not exactly equal `entry_count * 8` -- either shape is a truncated or
/// malformed payload, not an empty list (an empty list is `entry_count = 0`,
/// i.e. exactly 4 zero bytes, per ADR-178).
///
/// `entry_count`'s expected byte length (`4 + entry_count * 8`) is computed
/// in `u64`, not `usize`, and only compared against `bytes.len()` (also
/// widened to `u64`) -- never cast down before the comparison
/// (`edge-case-hunter`, PR #67-series round 2). `entry_count` is an
/// attacker-controlled `u32` read straight from the wire: on this
/// workspace's 32-bit build targets (`i686-pc-windows-gnullvm`,
/// `armv5te-unknown-linux-gnueabi`, per `docs/worker-crates.md`'s target table),
/// `usize` is 32 bits, so a native `4 + entry_count * 8` computed in
/// `usize` can wrap for `entry_count` in roughly `2^29..2^32` -- a 4-byte
/// request claiming a huge `entry_count` would then pass the length check
/// with a wrapped-small `expected_len`, and the following
/// `Vec::with_capacity(entry_count)` would try to reserve gigabytes for
/// the still-huge (unwrapped) `entry_count`, aborting the process on
/// allocation failure rather than returning an ordinary `INVALID_ARGUMENT`.
/// `u64` cannot overflow for any `u32` `entry_count` (max requirement is
/// ~34 GB, far under `u64::MAX`), so this check is correct on every target
/// width without relying on `usize`'s own size at all.
fn unpack_device_config_entries(bytes: &[u8]) -> Result<Vec<(u32, u32)>, Status> {
    if bytes.len() < 4 {
        return Err(Status::invalid_argument(
            "malformed Device Configuration payload: bytearray_data must contain at least a \
             4-byte entry_count",
        ));
    }
    let entry_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let expected_len: u64 = 4 + u64::from(entry_count) * 8;
    if bytes.len() as u64 != expected_len {
        return Err(Status::invalid_argument(format!(
            "malformed Device Configuration payload: entry_count={entry_count} requires \
             {expected_len} total bytes, got {}",
            bytes.len()
        )));
    }
    // Safe to cast to usize now: expected_len == bytes.len() (checked
    // above), and bytes.len() already fits usize by construction.
    let mut entries = Vec::with_capacity(entry_count as usize);
    for chunk in bytes[4..].as_chunks::<8>().0 {
        let parameter_id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let value = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        entries.push((parameter_id, value));
    }
    Ok(entries)
}

/// `edge-case-hunter`, PR #67-series round 2: on a 32-bit build target
/// (`i686-pc-windows-gnullvm`/`armv5te-unknown-linux-gnueabi`), a naive
/// `4 + entry_count * 8` computed in `usize` wraps for `entry_count` in
/// roughly `2^29..2^32`, letting a bare 4-byte request (just the
/// `entry_count` header, no trailing data) pass the length check with a
/// wrapped-small `expected_len`, then attempt a multi-gigabyte
/// `Vec::with_capacity` allocation for the real (unwrapped) `entry_count`
/// -- aborting the process rather than returning `INVALID_ARGUMENT`. These
/// tests can't reproduce the historical *wraparound* itself on this
/// 64-bit-CI-host `cargo test` run (`usize` here is 64 bits, so the old
/// `usize`-width code was already correct for these exact inputs on this
/// host -- the bug was specific to 32-bit hosts, per
/// `docs/worker-crates.md`'s target table). What they DO pin down, correctly on every host width,
/// is the *fixed* function's actual contract: a large claimed
/// `entry_count` with insufficient real data is rejected as malformed
/// (never silently truncated/miscounted, never a panic/abort), and the
/// rejection reports the correct `u64`-computed `expected_len` rather than
/// a value that could itself have wrapped.
#[cfg(test)]
mod unpack_device_config_entries_tests {
    use super::*;

    #[test]
    fn rejects_a_huge_entry_count_with_only_the_header_present() {
        // The exact boundary where a naive `usize`-width `entry_count * 8`
        // wraps to 0 on a 32-bit usize (2^29 * 8 == 2^32 == 0 mod 2^32).
        let huge_entry_count: u32 = 1 << 29;
        let bytes = huge_entry_count.to_le_bytes().to_vec();
        let status = unpack_device_config_entries(&bytes)
            .expect_err("a claimed entry_count with no matching data must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains(&huge_entry_count.to_string()),
            "error should report the actual entry_count: {}",
            status.message()
        );
    }

    #[test]
    fn rejects_the_maximum_possible_entry_count_with_only_the_header_present() {
        let bytes = u32::MAX.to_le_bytes().to_vec();
        let status = unpack_device_config_entries(&bytes)
            .expect_err("u32::MAX entry_count with no matching data must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn round_trips_a_small_batch() {
        let entries = vec![(0x0000_C001, 7), (0x0000_C005, 42)];
        let packed = pack_device_config_entries(&entries);
        assert_eq!(unpack_device_config_entries(&packed).unwrap(), entries);
    }

    #[test]
    fn round_trips_an_empty_list_as_a_four_byte_zero_count_not_a_zero_length_array() {
        let packed = pack_device_config_entries(&[]);
        assert_eq!(packed, 0u32.to_le_bytes().to_vec());
        assert_eq!(unpack_device_config_entries(&packed).unwrap(), Vec::new());
        // A genuinely zero-length payload is a different, malformed case --
        // missing even the 4-byte count -- not an alternate empty-list
        // encoding.
        unpack_device_config_entries(&[]).expect_err("zero-length payload must be rejected");
    }
}

/// Packs a `NdisAdapterInfo` (SAE J2534-2 clause 24 Ethernet_NDIS,
/// ADR-194/Phase 16) into the hand-packed byte layout `PDU_IOCTL_GET_NDIS_
/// ADAPTER_INFO`'s output uses for `DataItem.bytearray_data` (`IOBytearray`),
/// mirroring the native `NDIS_ADAPTER_INFORMATION` struct's own field
/// order/widths exactly (byte layout documented in `docs/rpc-api-guide.md`):
///
/// | Field | Bytes | Encoding |
/// |---|---|---|
/// | `AdapterUniqueID` | 128 | raw byte span, null-padded per the native char array |
/// | `AdapterName` | 64 | raw byte span, null-padded per the native char array |
/// | `Status` | 4 | little-endian `u32` |
/// | `MAC_Address` | 6 | byte-for-byte (already network-order) |
/// | `IPV6_Address` | 16 | byte-for-byte (already network-order) |
/// | `IPV4_Address` | 4 | byte-for-byte (already network-order) |
/// | `EthernetPinConfig` | 4 | little-endian `u32` |
///
/// 226 bytes total.
fn pack_ndis_adapter_info(info: &j2534_0404::NdisAdapterInfo) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(226);
    bytes.extend_from_slice(&info.adapter_unique_id());
    bytes.extend_from_slice(&info.adapter_name());
    bytes.extend_from_slice(&info.status().to_le_bytes());
    bytes.extend_from_slice(&info.mac_address());
    bytes.extend_from_slice(&info.ipv6_address());
    bytes.extend_from_slice(&info.ipv4_address());
    bytes.extend_from_slice(&info.ethernet_pin_config().to_le_bytes());
    bytes
}

#[cfg(test)]
mod pack_ndis_adapter_info_tests {
    use super::*;

    /// Component fields decoded by [`unpack`]: `(adapter_unique_id,
    /// adapter_name, status, mac_address, ipv6_address, ipv4_address,
    /// ethernet_pin_config)`.
    type UnpackedNdisAdapterInfo = ([u8; 128], [u8; 64], u32, [u8; 6], [u8; 16], [u8; 4], u32);

    /// Decodes [`pack_ndis_adapter_info`]'s own byte layout back into its
    /// component fields -- test-only (this IOCTL has no client-supplied
    /// input to decode in production; only its output needs packing), used
    /// here purely to pin the serialization/deserialization round-trip.
    fn unpack(bytes: &[u8]) -> UnpackedNdisAdapterInfo {
        assert_eq!(
            bytes.len(),
            226,
            "packed NDIS_ADAPTER_INFORMATION must be exactly 226 bytes"
        );
        let adapter_unique_id: [u8; 128] = bytes[0..128].try_into().unwrap();
        let adapter_name: [u8; 64] = bytes[128..192].try_into().unwrap();
        let status = u32::from_le_bytes(bytes[192..196].try_into().unwrap());
        let mac_address: [u8; 6] = bytes[196..202].try_into().unwrap();
        let ipv6_address: [u8; 16] = bytes[202..218].try_into().unwrap();
        let ipv4_address: [u8; 4] = bytes[218..222].try_into().unwrap();
        let ethernet_pin_config = u32::from_le_bytes(bytes[222..226].try_into().unwrap());
        (
            adapter_unique_id,
            adapter_name,
            status,
            mac_address,
            ipv6_address,
            ipv4_address,
            ethernet_pin_config,
        )
    }

    #[test]
    fn round_trips_a_populated_struct() {
        let mut unique_id = [0u8; 128];
        unique_id[..6].copy_from_slice(b"ADAPT1");
        let mut name = [0u8; 64];
        name[..4].copy_from_slice(b"eth0");
        let raw = j2534_0404_sys::bindings::NDIS_ADAPTER_INFORMATION {
            AdapterUniqueID: unique_id.map(|b| b as std::os::raw::c_char),
            AdapterName: name.map(|b| b as std::os::raw::c_char),
            Status: 1,
            MAC_Address: [0x00, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E],
            IPV6_Address: [0xAB; 16],
            IPV4_Address: [192, 168, 1, 42],
            EthernetPinConfig: 2,
        };
        let info = j2534_0404::NdisAdapterInfo(raw);
        let packed = pack_ndis_adapter_info(&info);
        assert_eq!(packed.len(), 226);
        let (unique_id_out, name_out, status, mac, ipv6, ipv4, pin_config) = unpack(&packed);
        assert_eq!(unique_id_out, unique_id);
        assert_eq!(name_out, name);
        assert_eq!(status, 1);
        assert_eq!(mac, [0x00, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E]);
        assert_eq!(ipv6, [0xAB; 16]);
        assert_eq!(ipv4, [192, 168, 1, 42]);
        assert_eq!(pin_config, 2);
    }

    #[test]
    fn round_trips_an_all_zero_struct() {
        let raw = j2534_0404_sys::bindings::NDIS_ADAPTER_INFORMATION {
            AdapterUniqueID: [0; 128],
            AdapterName: [0; 64],
            Status: 0,
            MAC_Address: [0; 6],
            IPV6_Address: [0; 16],
            IPV4_Address: [0; 4],
            EthernetPinConfig: 0,
        };
        let info = j2534_0404::NdisAdapterInfo(raw);
        let packed = pack_ndis_adapter_info(&info);
        let (unique_id, name, status, mac, ipv6, ipv4, pin_config) = unpack(&packed);
        assert_eq!(unique_id, [0u8; 128]);
        assert_eq!(name, [0u8; 64]);
        assert_eq!(status, 0);
        assert_eq!(mac, [0u8; 6]);
        assert_eq!(ipv6, [0u8; 16]);
        assert_eq!(ipv4, [0u8; 4]);
        assert_eq!(pin_config, 0);
    }
}

/// SAE J2534-2 clause 14 Repeat Messaging (ADR-165) `PDU_IOCTL_START_
/// REPEAT_MESSAGE` input, re-expressed per ADR-178 as a hand-packed byte
/// payload riding `DataItem.bytearray_data` (`IOBytearray`) rather than the
/// removed dedicated proto message. Field names/types match the removed
/// `IORepeatMessageSetup` proto message exactly, so every `setup.<field>`
/// access site inside `ioctl_start_repeat_message` needed no change beyond
/// this struct's own extraction site.
#[derive(Debug)]
struct RepeatMessageSetup {
    time_interval: u32,
    condition: u32,
    repeat_msg_data: Vec<u8>,
    mask_data: Vec<u8>,
    pattern_data: Vec<u8>,
    /// Raw `TxFlagBit` enum wire values -- `i32` to match
    /// `tx_flag_bit_to_j2534`'s existing signature (`rpc_primitive.rs`),
    /// unchanged by this revert.
    tx_flag_bits: Vec<i32>,
    /// ADR-214's optional v2 trailing section: the mask/pattern response
    /// template's OWN addressing basis, independent of `tx_flag_bits`
    /// (which now drives only the transmitted `repeat_msg_data` message).
    /// `None` means no v2 section was present on the wire -- the response
    /// template inherits the request-side `tx_flag_bits` fold, exactly
    /// today's (ADR-199) behavior. `Some(vec)` (possibly empty) is the
    /// response template's own self-contained set of raw `TxFlagBit` wire
    /// values, folded through the identical
    /// `rpc_primitive::raw_mode_tx_flag_bit_to_j2534` gate independently of
    /// `tx_flag_bits` -- an empty `Some(vec![])` is a meaningful "response
    /// carries neither addressing bit," distinct from `None`'s "inherit the
    /// request side."
    response_tx_flag_bits: Option<Vec<i32>>,
}

/// Reads a little-endian `u32` at `*cursor` (a byte offset into `bytes`).
/// `*cursor` is `u64`, not `usize`, so it can never overflow while
/// advancing through an attacker-controlled length-prefixed payload -- see
/// [`unpack_repeat_message_setup`]'s doc comment for why this matters on
/// this workspace's 32-bit build targets. Advances `*cursor` past the four
/// bytes read on success; rejects a payload truncated before this field.
fn read_repeat_message_u32(bytes: &[u8], cursor: &mut u64, what: &str) -> Result<u32, Status> {
    let total_len = bytes.len() as u64;
    if *cursor + 4 > total_len {
        return Err(Status::invalid_argument(format!(
            "malformed Repeat Messaging payload: truncated before {what}"
        )));
    }
    let start = *cursor as usize;
    let value = u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap());
    *cursor += 4;
    Ok(value)
}

/// Reads a `u32`-length-prefixed byte span at `*cursor` (see
/// [`read_repeat_message_u32`], which reads the length prefix itself). The
/// span's end offset (`*cursor + len`) is computed in `u64` and checked
/// against `bytes.len()` (also widened to `u64`) before ever slicing or
/// advancing the cursor, so a huge claimed `len` is rejected as malformed
/// rather than silently truncated, overflowed, or read out of bounds.
fn read_repeat_message_span<'a>(
    bytes: &'a [u8],
    cursor: &mut u64,
    what: &str,
) -> Result<&'a [u8], Status> {
    let len = read_repeat_message_u32(bytes, cursor, &format!("the {what} length"))?;
    let total_len = bytes.len() as u64;
    // Cannot overflow: `*cursor` is bounded by `total_len` (a real slice
    // length, so it fits `usize`, hence far under `u64::MAX`), and `len` is
    // at most `u32::MAX` -- their sum stays far under `u64::MAX`.
    let end = *cursor + u64::from(len);
    if end > total_len {
        return Err(Status::invalid_argument(format!(
            "malformed Repeat Messaging payload: {what} claims {len} bytes but only {} remain",
            total_len - *cursor
        )));
    }
    let start = *cursor as usize;
    // Safe to cast to usize: `end <= total_len == bytes.len() as u64`,
    // and `bytes.len()` already fits usize by construction.
    let end_usize = end as usize;
    *cursor = end;
    Ok(&bytes[start..end_usize])
}

/// Reads a `u32 count` followed by that many raw `i32` `TxFlagBit` wire
/// values at `*cursor` -- the shared shape both `tx_flag_bits` and ADR-214's
/// optional `response_tx_flag_bits` v2 section use. `what` names the field
/// for error messages (e.g. `"tx_flag_bits"`, `"response_tx_flag_bits"`).
/// Same widened-`u64`-arithmetic overflow-safety pattern as
/// [`read_repeat_message_span`]: the required byte count is computed in
/// `u64` and checked against the payload's actual remaining length before
/// ever slicing, so a huge claimed count is rejected as malformed rather
/// than misread or allowed to allocate unboundedly.
fn read_repeat_message_flag_bits(
    bytes: &[u8],
    cursor: &mut u64,
    what: &str,
) -> Result<Vec<i32>, Status> {
    let count = read_repeat_message_u32(bytes, cursor, &format!("{what}_count"))?;
    let total_len = bytes.len() as u64;
    // Widened u64 arithmetic, same reasoning as the span lengths above.
    let required: u64 = u64::from(count) * 4;
    if *cursor + required > total_len {
        return Err(Status::invalid_argument(format!(
            "malformed Repeat Messaging payload: {what}_count={count} requires {required} more \
             bytes but only {} remain",
            total_len - *cursor
        )));
    }
    // Safe to cast to usize now: count * 4 <= total_len (checked above),
    // and total_len already fits usize by construction.
    let mut bits = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let start = *cursor as usize;
        let bit = i32::from_le_bytes(bytes[start..start + 4].try_into().unwrap());
        bits.push(bit);
        *cursor += 4;
    }
    Ok(bits)
}

/// Inverse of [`pack_repeat_message_setup`] (test-only helper; see its own
/// doc comment) -- the production-facing decode step for
/// `PDU_IOCTL_START_REPEAT_MESSAGE`'s `bytearray_data` input (ADR-178):
/// `u32 time_interval`, `u32 condition`, then three length-prefixed byte
/// spans (`u32 len` + that many bytes) for `repeat_msg_data`/`mask_data`/
/// `pattern_data`, then `u32 tx_flag_bits_count` followed by that many
/// `u32`s (each a `TxFlagBit` enum's raw wire value), then an OPTIONAL
/// ADR-214 v2 trailing section of the identical shape (`u32
/// response_tx_flag_bits_count` + that many `u32`s) carrying the mask/
/// pattern response template's OWN addressing basis, independent of
/// `tx_flag_bits`.
///
/// The v2 section's presence is detected purely by remaining-byte-count,
/// not an explicit version field: if the cursor reaches `bytes.len()`
/// exactly after `tx_flag_bits`, `response_tx_flag_bits` is `None` (a v1
/// client, or a v2 client that omits the section -- inherit the
/// request-side fold, today's behavior unchanged); otherwise the v2
/// section is read and `response_tx_flag_bits` is `Some(vec)` (possibly
/// empty). This is unambiguous because a v1 payload was already required
/// to end exactly at `tx_flag_bits`'s last byte (see below) -- ADR-214
/// Decision item 1.
///
/// Every length in this format (the three span lengths plus both trailing
/// counts) is an attacker-controlled `u32` read straight from the wire.
/// Mirroring `unpack_device_config_entries`'s fix (edge-case-hunter,
/// PR #67-series round 2): every offset/length computation that combines
/// these values -- including the running cursor position as it advances
/// past each successive section -- is done in `u64`, never in `usize`
/// (which is only 32 bits on this workspace's `i686-pc-windows-gnullvm`/
/// `armv5te-unknown-linux-gnueabi` targets, per `docs/worker-crates.md`'s
/// target table), and only cast down to `usize` once a check confirms the value
/// fits within the actual received payload. A payload truncated at any
/// point -- not enough bytes for the next length prefix, or not enough
/// bytes for a claimed span/count -- is rejected as malformed rather than
/// silently truncated, misread, or allowed to panic/abort on an
/// out-of-bounds slice or an oversized allocation. A payload with extra
/// trailing bytes after a structurally-valid `tx_flag_bits` array (with no
/// v2 section) or after a structurally-valid v2
/// `response_tx_flag_bits` array is rejected too (edge-case-hunter, PR
/// #3-series round 1; extended to the v2 section by ADR-214) -- the
/// removed typed `IORepeatMessageSetup` proto message made an "extra field
/// data" shape structurally impossible, and this hand-packed format
/// preserves that guarantee explicitly rather than silently accepting
/// inert trailing garbage that could mask a client-side encoding bug.
fn unpack_repeat_message_setup(bytes: &[u8]) -> Result<RepeatMessageSetup, Status> {
    let mut cursor: u64 = 0;
    let time_interval = read_repeat_message_u32(bytes, &mut cursor, "time_interval")?;
    let condition = read_repeat_message_u32(bytes, &mut cursor, "condition")?;
    let repeat_msg_data = read_repeat_message_span(bytes, &mut cursor, "repeat_msg_data")?.to_vec();
    let mask_data = read_repeat_message_span(bytes, &mut cursor, "mask_data")?.to_vec();
    let pattern_data = read_repeat_message_span(bytes, &mut cursor, "pattern_data")?.to_vec();

    let tx_flag_bits = read_repeat_message_flag_bits(bytes, &mut cursor, "tx_flag_bits")?;

    let total_len = bytes.len() as u64;
    let response_tx_flag_bits = if cursor == total_len {
        None
    } else {
        let bits = read_repeat_message_flag_bits(bytes, &mut cursor, "response_tx_flag_bits")?;
        if cursor != total_len {
            return Err(Status::invalid_argument(format!(
                "malformed Repeat Messaging payload: {} trailing byte(s) after \
                 response_tx_flag_bits",
                total_len - cursor
            )));
        }
        Some(bits)
    };

    Ok(RepeatMessageSetup {
        time_interval,
        condition,
        repeat_msg_data,
        mask_data,
        pattern_data,
        tx_flag_bits,
        response_tx_flag_bits,
    })
}

/// Packs a [`RepeatMessageSetup`] into the wire layout
/// [`unpack_repeat_message_setup`] decodes (ADR-178). Test-only: unlike
/// `pack_device_config_entries` (used both to encode `GET_DEVICE_CONFIG`'s
/// response and by that feature's own tests), `START_REPEAT_MESSAGE`'s
/// response is a bare `MsgId` (`unum32_value`, unchanged by ADR-178) --
/// nothing in production ever needs to re-encode a `RepeatMessageSetup`
/// back into bytes. Exists solely to build `bytearray_data` payloads for
/// this module's own embedded unit tests
/// (`tests/grpc_mock/repeat_message.rs`'s integration tests keep their own
/// small local duplicate, since this private function is not reachable
/// from that separate test binary).
#[cfg(test)]
fn pack_repeat_message_setup(setup: &RepeatMessageSetup) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(
        8 + 4
            + setup.repeat_msg_data.len()
            + 4
            + setup.mask_data.len()
            + 4
            + setup.pattern_data.len()
            + 4
            + setup.tx_flag_bits.len() * 4
            + setup
                .response_tx_flag_bits
                .as_ref()
                .map_or(0, |bits| 4 + bits.len() * 4),
    );
    bytes.extend_from_slice(&setup.time_interval.to_le_bytes());
    bytes.extend_from_slice(&setup.condition.to_le_bytes());
    for span in [
        &setup.repeat_msg_data,
        &setup.mask_data,
        &setup.pattern_data,
    ] {
        bytes.extend_from_slice(&(span.len() as u32).to_le_bytes());
        bytes.extend_from_slice(span);
    }
    bytes.extend_from_slice(&(setup.tx_flag_bits.len() as u32).to_le_bytes());
    for bit in &setup.tx_flag_bits {
        bytes.extend_from_slice(&bit.to_le_bytes());
    }
    // ADR-214's optional v2 trailing section: only written when `Some`, so
    // a `None` setup round-trips to the byte-for-byte v1 shape.
    if let Some(bits) = &setup.response_tx_flag_bits {
        bytes.extend_from_slice(&(bits.len() as u32).to_le_bytes());
        for bit in bits {
            bytes.extend_from_slice(&bit.to_le_bytes());
        }
    }
    bytes
}

/// `edge-case-hunter`-style coverage for [`unpack_repeat_message_setup`],
/// mirroring `unpack_device_config_entries_tests`'s own shape: a round trip
/// through [`pack_repeat_message_setup`], plus a truncation/oversized-claim
/// case at each of the format's four independent length prefixes (three
/// byte spans, one trailing count).
#[cfg(test)]
mod unpack_repeat_message_setup_tests {
    use super::*;

    #[test]
    fn round_trips_a_populated_setup() {
        let setup = RepeatMessageSetup {
            time_interval: 1000,
            condition: 1,
            repeat_msg_data: vec![0x01, 0x02, 0x03],
            mask_data: vec![0xFF, 0xFF],
            pattern_data: vec![0x00, 0x01],
            tx_flag_bits: vec![31, 32], // TX_FLAG_ISO15765_FRAME_PAD, TX_FLAG_ISO15765_ADDR_TYPE
            response_tx_flag_bits: None,
        };
        let packed = pack_repeat_message_setup(&setup);
        let unpacked = unpack_repeat_message_setup(&packed).unwrap();
        assert_eq!(unpacked.time_interval, setup.time_interval);
        assert_eq!(unpacked.condition, setup.condition);
        assert_eq!(unpacked.repeat_msg_data, setup.repeat_msg_data);
        assert_eq!(unpacked.mask_data, setup.mask_data);
        assert_eq!(unpacked.pattern_data, setup.pattern_data);
        assert_eq!(unpacked.tx_flag_bits, setup.tx_flag_bits);
        assert_eq!(unpacked.response_tx_flag_bits, setup.response_tx_flag_bits);
    }

    #[test]
    fn round_trips_a_populated_setup_with_a_response_v2_section() {
        let setup = RepeatMessageSetup {
            time_interval: 1000,
            condition: 1,
            repeat_msg_data: vec![0x01, 0x02, 0x03],
            mask_data: vec![0xFF, 0xFF],
            pattern_data: vec![0x00, 0x01],
            tx_flag_bits: vec![31],
            response_tx_flag_bits: Some(vec![32]),
        };
        let packed = pack_repeat_message_setup(&setup);
        let unpacked = unpack_repeat_message_setup(&packed).unwrap();
        assert_eq!(unpacked.tx_flag_bits, setup.tx_flag_bits);
        assert_eq!(unpacked.response_tx_flag_bits, setup.response_tx_flag_bits);
    }

    #[test]
    fn accepts_an_explicit_empty_response_tx_flag_bits_section() {
        // ADR-214 Decision item 2: `Some(vec![])` (a v2 section with
        // `response_tx_flag_bits_count == 0`, i.e. exactly 4 trailing zero
        // bytes) is a meaningful, distinct value from `None` -- "the
        // response template carries neither addressing bit" -- and must be
        // accepted, not rejected as trailing garbage.
        let setup = RepeatMessageSetup {
            time_interval: 1,
            condition: 0,
            repeat_msg_data: Vec::new(),
            mask_data: Vec::new(),
            pattern_data: Vec::new(),
            tx_flag_bits: vec![31],
            response_tx_flag_bits: Some(Vec::new()),
        };
        let packed = pack_repeat_message_setup(&setup);
        let unpacked = unpack_repeat_message_setup(&packed).unwrap();
        assert_eq!(unpacked.response_tx_flag_bits, Some(Vec::new()));
    }

    #[test]
    fn round_trips_an_empty_setup_with_no_tx_flag_bits() {
        let setup = RepeatMessageSetup {
            time_interval: 0,
            condition: 0,
            repeat_msg_data: Vec::new(),
            mask_data: Vec::new(),
            pattern_data: Vec::new(),
            tx_flag_bits: Vec::new(),
            response_tx_flag_bits: None,
        };
        let packed = pack_repeat_message_setup(&setup);
        let unpacked = unpack_repeat_message_setup(&packed).unwrap();
        assert_eq!(unpacked.repeat_msg_data, Vec::<u8>::new());
        assert!(unpacked.tx_flag_bits.is_empty());
        assert_eq!(unpacked.response_tx_flag_bits, None);
    }

    #[test]
    fn rejects_a_payload_too_short_for_the_fixed_header() {
        let status = unpack_repeat_message_setup(&[0x01, 0x02, 0x03])
            .expect_err("fewer than 8 bytes cannot even hold time_interval+condition");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_a_huge_repeat_msg_data_length_with_no_matching_bytes() {
        let mut bytes = 1000u32.to_le_bytes().to_vec(); // time_interval
        bytes.extend_from_slice(&0u32.to_le_bytes()); // condition
        bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // repeat_msg_data len
        let status = unpack_repeat_message_setup(&bytes).expect_err(
            "a huge claimed repeat_msg_data length with no matching bytes must be \
                         rejected, not misread or allowed to allocate",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_a_payload_truncated_within_mask_data() {
        let mut bytes = 0u32.to_le_bytes().to_vec(); // time_interval
        bytes.extend_from_slice(&0u32.to_le_bytes()); // condition
        bytes.extend_from_slice(&0u32.to_le_bytes()); // repeat_msg_data len = 0
        bytes.extend_from_slice(&4u32.to_le_bytes()); // mask_data len = 4
        bytes.extend_from_slice(&[0x00, 0x00]); // only 2 of the claimed 4 bytes
        let status = unpack_repeat_message_setup(&bytes)
            .expect_err("truncated within a claimed mask_data span must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_a_huge_tx_flag_bits_count_with_no_matching_data() {
        let setup = RepeatMessageSetup {
            time_interval: 1,
            condition: 0,
            repeat_msg_data: Vec::new(),
            mask_data: Vec::new(),
            pattern_data: Vec::new(),
            tx_flag_bits: Vec::new(),
            response_tx_flag_bits: None,
        };
        let mut bytes = pack_repeat_message_setup(&setup);
        // Overwrite the trailing tx_flag_bits_count (the last 4 bytes, since
        // no bits follow it) with a huge claimed count and no matching data.
        let count_start = bytes.len() - 4;
        bytes[count_start..].copy_from_slice(&u32::MAX.to_le_bytes());
        let status = unpack_repeat_message_setup(&bytes).expect_err(
            "a huge claimed tx_flag_bits_count with no matching data must be rejected, not \
             misread or allowed to allocate gigabytes",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_a_payload_truncated_within_tx_flag_bits() {
        let setup = RepeatMessageSetup {
            time_interval: 1,
            condition: 0,
            repeat_msg_data: Vec::new(),
            mask_data: Vec::new(),
            pattern_data: Vec::new(),
            tx_flag_bits: Vec::new(),
            response_tx_flag_bits: None,
        };
        let mut bytes = pack_repeat_message_setup(&setup);
        let count_start = bytes.len() - 4;
        bytes[count_start..].copy_from_slice(&2u32.to_le_bytes()); // claims 2 bits
        bytes.extend_from_slice(&31u32.to_le_bytes()); // only 1 of the claimed 2 follow
        let status = unpack_repeat_message_setup(&bytes)
            .expect_err("truncated within the claimed tx_flag_bits array must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_trailing_garbage_bytes_after_an_otherwise_valid_payload() {
        let setup = RepeatMessageSetup {
            time_interval: 1000,
            condition: 1,
            repeat_msg_data: vec![0x01],
            mask_data: vec![0xFF],
            pattern_data: vec![0x00],
            tx_flag_bits: vec![31],
            response_tx_flag_bits: None,
        };
        let mut bytes = pack_repeat_message_setup(&setup);
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00]);
        let status = unpack_repeat_message_setup(&bytes).expect_err(
            "extra bytes after a structurally-valid tx_flag_bits array must be rejected, not \
             silently ignored -- the removed typed IORepeatMessageSetup proto message made an \
             'extra field data' shape structurally impossible, and this hand-packed format \
             should preserve that guarantee",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_a_payload_truncated_within_the_response_tx_flag_bits_count() {
        // ADR-214: 1-3 stray bytes after a well-formed `tx_flag_bits` array
        // are not enough to hold a full `response_tx_flag_bits_count` u32,
        // and must be rejected rather than misread.
        let setup = RepeatMessageSetup {
            time_interval: 1,
            condition: 0,
            repeat_msg_data: Vec::new(),
            mask_data: Vec::new(),
            pattern_data: Vec::new(),
            tx_flag_bits: Vec::new(),
            response_tx_flag_bits: None,
        };
        let mut bytes = pack_repeat_message_setup(&setup);
        bytes.extend_from_slice(&[0x01, 0x02, 0x03]);
        let status = unpack_repeat_message_setup(&bytes).expect_err(
            "a payload with 1-3 trailing bytes (not enough for a full \
             response_tx_flag_bits_count u32) must be rejected",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_a_huge_response_tx_flag_bits_count_with_no_matching_data() {
        let setup = RepeatMessageSetup {
            time_interval: 1,
            condition: 0,
            repeat_msg_data: Vec::new(),
            mask_data: Vec::new(),
            pattern_data: Vec::new(),
            tx_flag_bits: Vec::new(),
            response_tx_flag_bits: Some(Vec::new()),
        };
        let mut bytes = pack_repeat_message_setup(&setup);
        // Overwrite the trailing response_tx_flag_bits_count (the last 4
        // bytes, since no bits follow it) with a huge claimed count and no
        // matching data.
        let count_start = bytes.len() - 4;
        bytes[count_start..].copy_from_slice(&u32::MAX.to_le_bytes());
        let status = unpack_repeat_message_setup(&bytes).expect_err(
            "a huge claimed response_tx_flag_bits_count with no matching data must be \
             rejected, not misread or allowed to allocate gigabytes",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_trailing_garbage_bytes_after_a_valid_response_tx_flag_bits_section() {
        let setup = RepeatMessageSetup {
            time_interval: 1000,
            condition: 1,
            repeat_msg_data: vec![0x01],
            mask_data: vec![0xFF],
            pattern_data: vec![0x00],
            tx_flag_bits: vec![31],
            response_tx_flag_bits: Some(vec![32]),
        };
        let mut bytes = pack_repeat_message_setup(&setup);
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let status = unpack_repeat_message_setup(&bytes).expect_err(
            "extra bytes after a structurally-valid response_tx_flag_bits array must be \
             rejected, not silently ignored",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}

/// SHORT_TO_GROUND (0xFFFFFFFE) -- one of `PassThruSetProgrammingVoltage`'s
/// three `Voltage` sentinels (SAE J2534-1 v04.04 §7.2.11.3 "Voltage Values"
/// table): a real millivolt value (0x00001388-0x00004E20), this
/// short-to-ground sentinel, or `VOLTAGE_OFF` (0xFFFFFFFF, distinct --
/// deliberately not defined here, since Stage 2 does not gate it). Used by
/// `ioctl_set_prog_voltage`'s pin-9 Discovery gate to distinguish the
/// short-to-ground case (checked against `DEVICE_INFO_SHORT_TO_GND_J1962`)
/// from a real voltage value or `VOLTAGE_OFF` (neither gated in Stage 2 --
/// ADR-185 Decision 2).
const SHORT_TO_GROUND: u32 = 0xFFFF_FFFE;

/// RAII guard clearing [`SharedChannel::become_master_in_flight`]
/// (ADR-189/Phase 8, Codex review P2 fix, PR #98) -- constructed as the
/// FIRST statement inside `ioctl_become_master`'s `spawn_blocking` closure,
/// never in the surrounding async fn. Constructing it there (rather than on
/// the async side, before `spawn_blocking` is even called) is what makes
/// the clear survive every exit path that matters:
///
/// - normal return from the blocking closure,
/// - a panic/unwind inside the native `become_master` call (`Drop` runs
///   during unwind same as normal return), and
/// - the awaiting async future being dropped/cancelled by a tonic RPC
///   cancellation -- once `spawn_blocking` has started running this
///   closure on its own worker thread, dropping the `JoinHandle`'s async
///   side does NOT stop or unwind the blocking closure itself, so this
///   guard still runs its `Drop` when the closure eventually finishes,
///   even though nothing async-side is left awaiting the result. An
///   async-side guard (constructed before `spawn_blocking`, dropped when
///   that outer future is dropped) would clear the flag the instant the
///   RPC is cancelled -- while the native call could still be genuinely
///   in flight on its worker thread -- reopening exactly the race this
///   flag exists to close.
struct BecomeMasterInFlightGuard(Arc<AtomicBool>);

impl Drop for BecomeMasterInFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// ADR-219 (as amended)'s documented cap on a WRAPPED-MODE (`shape =
/// "sbyte_array"`) vendor IOCTL's `output_capacity` ONLY: bounds a single
/// client-driven allocation. Not a J2534 spec requirement -- an ADR-chosen
/// constant, enforced as an outright rejection (`InvalidArgument`) rather
/// than a silent clamp, matching this module's existing convention for
/// hand-validated numeric fields (e.g. `unpack_device_config_entries`'s
/// `entry_count` checks) of rejecting an out-of-range claim rather than
/// truncating it into something the client never asked for. Not a
/// safety-critical bound for wrapped mode -- the native `SBYTE_ARRAY`'s own
/// `NumOfBytes` self-describes the real written length regardless of how
/// large the backing allocation is (`into_output_bytes`), so this purely
/// bounds how large a buffer the service is willing to allocate on a
/// client's request, not a defense against an out-of-bounds native write.
///
/// **RAW mode no longer uses this constant at all** (Codex review, PR #133
/// third round): a fixed cap applied uniformly to every raw-mode vendor
/// IOCTL was itself an invented bound with no basis in any specific vendor
/// command's real native contract -- and, independently, a raw-contract
/// command reached via the WRAPPED flag bit (client-selectable, see
/// `VendorIoCtl`'s own doc comment) could write through the small embedded
/// `SBYTE_ARRAY` struct field regardless of this cap. Raw mode's backing
/// allocations are now sized exactly to the per-`cmd_id` `vendor_ioctls`
/// config contract (`J2534Service::vendor_ioctls`,
/// `crate::config::VendorIoctlContract::Raw`), required for any command
/// carrying a non-NULL buffer -- see `validate_vendor_ioctl_buffers`.
const VENDOR_IOCTL_MAX_WRAPPED_CAPACITY: u32 = 64 * 1024;

/// Decoded form of a vendor IOCTL's (`cmd_id >= 0x10000`) `bytearray_data`
/// input, per ADR-219 Decision item 2's hand-packed header: `u32 flags`,
/// `u32 output_capacity`, then raw input bytes.
#[derive(Debug)]
struct VendorIoCtlRequest {
    /// flags bit 2 (0 = raw pointer mode, 1 = `SBYTE_ARRAY`-wrapped).
    wrapped: bool,
    /// `None` when flags bit 0 (input present) is clear -- no input pointer
    /// at all. `Some(bytes)` (possibly empty) when bit 0 is set: the
    /// trailing bytes after the 8-byte header, however many there are (see
    /// [`VendorIoCtl`]'s own doc comment for how an explicitly-empty-but-
    /// present input differs from "no input" in wrapped mode).
    input: Option<Vec<u8>>,
    /// `0` means no output pointer is passed at all; otherwise the size of
    /// the zeroed buffer the service allocates before the native call.
    /// Validated (and, for raw mode, bounded) at dispatch time by
    /// `validate_vendor_ioctl_buffers` against the per-`cmd_id`
    /// `vendor_ioctls` config contract (raw mode) or
    /// [`VENDOR_IOCTL_MAX_WRAPPED_CAPACITY`] (wrapped mode) -- not here,
    /// since which bound applies depends on the configured shape (ADR-219,
    /// as amended).
    output_capacity: u32,
}

/// Decodes a vendor IOCTL's `bytearray_data` input per ADR-219 Decision
/// item 2's header layout: `u32 flags` (LE), `u32 output_capacity` (LE),
/// then raw input bytes.
///
/// `flags` bit 0 is "input present" -- distinct from "remaining bytes is
/// non-empty" for wrapped mode's benefit: a client that wants an explicit,
/// present-but-zero-length `SBYTE_ARRAY` (bit 0 set, zero trailing bytes)
/// gets a different native `pInput` than a client that wants no input
/// pointer at all (bit 0 clear) -- see [`VendorIoCtl`]'s own doc comment.
/// Trailing bytes present while bit 0 is clear are rejected as a malformed/
/// inconsistent payload, mirroring this module's other hand-packed
/// formats' "reject trailing garbage" convention (e.g.
/// `unpack_repeat_message_setup`) rather than silently ignoring them.
///
/// `flags` bit 1 is "output requested"; bit 2 selects raw-pointer (0) vs.
/// `SBYTE_ARRAY`-wrapped (1) mode. Every other bit must be `0`
/// (`InvalidArgument` otherwise, ADR-219's own "reserved, must be zero" rule
/// so a future mode can be added by a new flag bit with no ID-space
/// change). `output_capacity == 0` always means no output pointer,
/// regardless of the "output requested" bit; that bit's only independent
/// effect is the `output_capacity == 0` rejection below.
fn unpack_vendor_ioctl_request(bytes: &[u8]) -> Result<VendorIoCtlRequest, Status> {
    const FLAG_INPUT_PRESENT: u32 = 1 << 0;
    const FLAG_OUTPUT_REQUESTED: u32 = 1 << 1;
    const FLAG_WRAPPED: u32 = 1 << 2;
    const FLAG_KNOWN_BITS: u32 = FLAG_INPUT_PRESENT | FLAG_OUTPUT_REQUESTED | FLAG_WRAPPED;

    if bytes.len() < 8 {
        return Err(Status::invalid_argument(
            "malformed vendor IOCTL payload: bytearray_data must contain at least the 8-byte \
             flags/output_capacity header (ADR-219)",
        ));
    }
    let flags = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let output_capacity = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);

    let reserved = flags & !FLAG_KNOWN_BITS;
    if reserved != 0 {
        return Err(Status::invalid_argument(format!(
            "malformed vendor IOCTL payload: flags {flags:#010x} sets reserved bit(s) \
             {reserved:#010x}, which ADR-219 requires to be zero"
        )));
    }

    let input_present = flags & FLAG_INPUT_PRESENT != 0;
    let output_requested = flags & FLAG_OUTPUT_REQUESTED != 0;
    let wrapped = flags & FLAG_WRAPPED != 0;

    let remaining = &bytes[8..];
    let input = if input_present {
        Some(remaining.to_vec())
    } else {
        if !remaining.is_empty() {
            return Err(Status::invalid_argument(format!(
                "malformed vendor IOCTL payload: {} trailing byte(s) after the header with \
                 flags bit 0 (input present) clear",
                remaining.len()
            )));
        }
        None
    };

    if output_requested && output_capacity == 0 {
        return Err(Status::invalid_argument(
            "malformed vendor IOCTL payload: flags bit 1 (output requested) is set but \
             output_capacity is 0 (ADR-219)",
        ));
    }
    // `output_capacity`'s upper bound depends on the configured shape (the
    // per-`cmd_id` `vendor_ioctls` contract for raw mode, or
    // `VENDOR_IOCTL_MAX_WRAPPED_CAPACITY` for wrapped mode) -- not known
    // here, so that check moved to `validate_vendor_ioctl_buffers` at
    // dispatch time (ADR-219, as amended).

    Ok(VendorIoCtlRequest {
        wrapped,
        input,
        output_capacity,
    })
}

#[cfg(test)]
mod unpack_vendor_ioctl_request_tests {
    use super::*;

    fn header(flags: u32, output_capacity: u32, input: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + input.len());
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes.extend_from_slice(&output_capacity.to_le_bytes());
        bytes.extend_from_slice(input);
        bytes
    }

    #[test]
    fn rejects_a_payload_shorter_than_the_header() {
        let status = unpack_vendor_ioctl_request(&[0u8; 4])
            .expect_err("a payload shorter than the 8-byte header must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn rejects_a_reserved_flag_bit() {
        let bytes = header(1 << 3, 0, &[]);
        let status = unpack_vendor_ioctl_request(&bytes)
            .expect_err("a reserved flag bit above bit 2 must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("reserved"));
    }

    #[test]
    fn rejects_output_requested_with_zero_capacity() {
        let bytes = header(0b010, 0, &[]);
        let status = unpack_vendor_ioctl_request(&bytes)
            .expect_err("output requested with output_capacity == 0 must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    // `output_capacity`'s upper bound is no longer checked in
    // `unpack_vendor_ioctl_request` at all -- it now depends on the
    // configured shape (raw mode: the per-`cmd_id` `vendor_ioctls` contract;
    // wrapped mode: `VENDOR_IOCTL_MAX_WRAPPED_CAPACITY`), so that check
    // moved to `validate_vendor_ioctl_buffers` at dispatch time (ADR-219, as
    // amended). See `vendor_passthrough.rs`'s
    // `vendor_ioctl_wrapped_mode_rejects_an_output_capacity_above_the_cap`
    // for the wrapped-mode regression this test used to cover.

    #[test]
    fn rejects_trailing_bytes_with_input_present_bit_clear() {
        let bytes = header(0, 0, &[0xAA]);
        let status = unpack_vendor_ioctl_request(&bytes).expect_err(
            "trailing bytes with flags bit 0 (input present) clear must be rejected as \
             malformed, not silently accepted as input",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn no_input_no_output_decodes_cleanly() {
        let bytes = header(0, 0, &[]);
        let decoded = unpack_vendor_ioctl_request(&bytes).unwrap();
        assert!(!decoded.wrapped);
        assert!(decoded.input.is_none());
        assert_eq!(decoded.output_capacity, 0);
    }

    #[test]
    fn input_present_with_explicit_empty_bytes_is_some_empty_not_none() {
        // flags bit 0 set, but zero trailing bytes -- distinct from bit 0
        // clear (see this function's own doc comment on why the distinction
        // matters for wrapped mode).
        let bytes = header(0b001, 0, &[]);
        let decoded = unpack_vendor_ioctl_request(&bytes).unwrap();
        assert_eq!(decoded.input, Some(Vec::new()));
    }

    #[test]
    fn round_trips_input_output_capacity_and_wrapped_bit() {
        let bytes = header(0b111, 16, &[1, 2, 3]);
        let decoded = unpack_vendor_ioctl_request(&bytes).unwrap();
        assert!(decoded.wrapped);
        assert_eq!(decoded.input, Some(vec![1, 2, 3]));
        assert_eq!(decoded.output_capacity, 16);
    }
}

/// ADR-219, as amended: validates a decoded vendor IOCTL request's
/// input/output buffers against `contract` (this service's startup-loaded
/// `vendor_ioctls` config for `cmd_id`, `None` when unconfigured) -- called
/// by `rpc_io_ctl_vendor` BEFORE any handle resolution or lock is taken, so
/// a rejected call never touches `shared_channels`/`api` (design-advisor
/// decision). Returns the contract on success, so the caller never has to
/// re-derive it from an `Option` that (as of this amendment) can no longer
/// legitimately be `None` at dispatch time.
///
/// **Every vendor `cmd_id` must be allowlisted in `vendor_ioctls` --
/// including one a client calls with no input and `output_capacity == 0`.**
/// An unconfigured `cmd_id` is unconditionally rejected `FAILED_PRECONDITION`
/// naming the remedy, regardless of what buffers the request carries. This
/// reverses an earlier version of this amendment (Codex review, PR #133
/// sixth round) that exempted a bufferless request against an unconfigured
/// `cmd_id`, reasoning there was "nothing to size or shape-check" -- a
/// design-advisor consult (PR #133 seventh round) found that reasoning
/// unsound: a vendor IOCTL's `pInput`/`pOutput` requirements are a property
/// of the `cmd_id` alone (SAE J2534-1 SS7.2.14, paraphrased), not of what a
/// particular client request happens to ask for, so a `cmd_id` whose native
/// contract always dereferences one of those pointers can still crash even
/// when the CLIENT chose to send neither -- exactly the same "trust the
/// client, not the operator config" gap ADR-219's Decision item 2 already
/// forbids for the buffer-carrying case. An operator who wants a genuinely
/// bufferless vendor command allowlisted configures it as
/// `shape = "raw"` with `input_bytes`/`output_bytes` both `0` (or omitted,
/// since both default to `0`) -- this contract is already exactly
/// "NULL/NULL only" per the `Raw` branch below, so no separate shape or
/// enum variant is needed for it.
///
/// A configured `cmd_id` whose shape does not match the client's flags bit 2
/// selection is rejected `INVALID_ARGUMENT` -- the client picks
/// raw-vs-wrapped via that bit, but only the operator-declared
/// `vendor_ioctls` contract knows which shape this `cmd_id`'s real native
/// contract actually expects (a wrong guess would otherwise write through
/// the wrong pointer shape, corrupting memory). `Raw` requires the request's
/// input length/`output_capacity` to fit within the configured
/// `input_bytes`/`output_bytes` exactly, AND (Codex review, PR #133 sixth
/// round) requires a present, non-empty input / a nonzero `output_capacity`
/// whenever the corresponding configured byte count is nonzero -- a
/// configured direction with a nonzero byte count is mandatory regardless of
/// whether the OTHER direction (or neither) carries a buffer. `SbyteArray`'s
/// own allocation stays bounded by [`VENDOR_IOCTL_MAX_WRAPPED_CAPACITY`]
/// instead of a configured byte count, since its `SBYTE_ARRAY`
/// self-describes the real written length -- but (Codex review, PR #133
/// seventh round) it still needs its OWN way to express "this direction is
/// mandatory", since it has no byte count to double as one the way `Raw`
/// does: `input_required`/`output_required` fill that role, checked as
/// outer-`SBYTE_ARRAY*` presence (`request.input.is_some()` -- unlike
/// `Raw`'s `!is_empty()` check, a present-but-empty wrapped input still
/// becomes a real non-NULL `SBYTE_ARRAY` with `NumOfBytes: 0`, which is
/// enough to satisfy a presence requirement; whether an empty array is
/// otherwise acceptable is the vendor DLL's own business).
fn validate_vendor_ioctl_buffers(
    contract: Option<crate::config::VendorIoctlContract>,
    cmd_id: u32,
    request: &VendorIoCtlRequest,
) -> Result<crate::config::VendorIoctlContract, Status> {
    let Some(contract) = contract else {
        return Err(Status::failed_precondition(format!(
            "vendor IOCTL cmd_id {cmd_id:#010x} has no configured native contract; every vendor \
             cmd_id must be allowlisted before use, including one that carries no buffer -- \
             configure vendor_ioctls for this library \
             (config.apis.j2534-0404.libs.\"<lib>\".vendor_ioctls.\"{cmd_id:#010x}\", ADR-219 as \
             amended); a command that genuinely takes no buffer is declared \
             shape = \"raw\" with input_bytes/output_bytes both 0 (or omitted)"
        )));
    };

    match contract {
        crate::config::VendorIoctlContract::Raw {
            input_bytes,
            output_bytes,
        } => {
            if request.wrapped {
                return Err(Status::invalid_argument(format!(
                    "vendor IOCTL cmd_id {cmd_id:#010x} is configured shape \"raw\" but the \
                     request set flags bit 2 (wrapped SBYTE_ARRAY mode); the client-selected \
                     mode must match the configured shape (ADR-219, as amended)"
                )));
            }
            let input_present = request
                .input
                .as_ref()
                .is_some_and(|input| !input.is_empty());
            if input_bytes > 0 && !input_present {
                return Err(Status::invalid_argument(format!(
                    "vendor IOCTL cmd_id {cmd_id:#010x} is configured with a nonzero \
                     input_bytes ({input_bytes}) but the request carries no (or an empty) \
                     input; this command's native contract always reads from pInput, so a \
                     NULL pointer here would crash the native call (ADR-219, as amended)"
                )));
            }
            if output_bytes > 0 && request.output_capacity == 0 {
                return Err(Status::invalid_argument(format!(
                    "vendor IOCTL cmd_id {cmd_id:#010x} is configured with a nonzero \
                     output_bytes ({output_bytes}) but the request did not request any \
                     output; this command's native contract always writes to pOutput, so a \
                     NULL pointer here would crash the native call (ADR-219, as amended)"
                )));
            }
            if let Some(input) = &request.input
                && input.len() as u32 > input_bytes
            {
                return Err(Status::invalid_argument(format!(
                    "vendor IOCTL cmd_id {cmd_id:#010x} input is {} byte(s), exceeding the \
                     configured input_bytes {input_bytes}",
                    input.len()
                )));
            }
            if request.output_capacity > output_bytes {
                return Err(Status::invalid_argument(format!(
                    "vendor IOCTL cmd_id {cmd_id:#010x} output_capacity {} exceeds the \
                     configured output_bytes {output_bytes}",
                    request.output_capacity
                )));
            }
            Ok(contract)
        }
        crate::config::VendorIoctlContract::SbyteArray {
            input_required,
            output_required,
        } => {
            if !request.wrapped {
                return Err(Status::invalid_argument(format!(
                    "vendor IOCTL cmd_id {cmd_id:#010x} is configured shape \"sbyte_array\" but \
                     the request left flags bit 2 (wrapped SBYTE_ARRAY mode) clear; the \
                     client-selected mode must match the configured shape (ADR-219, as amended)"
                )));
            }
            if input_required && request.input.is_none() {
                return Err(Status::invalid_argument(format!(
                    "vendor IOCTL cmd_id {cmd_id:#010x} is configured with input_required but \
                     the request carries no input; this command's native contract always reads \
                     a non-NULL SBYTE_ARRAY through pInput, so a NULL pointer here would crash \
                     the native call (ADR-219, as amended)"
                )));
            }
            if output_required && request.output_capacity == 0 {
                return Err(Status::invalid_argument(format!(
                    "vendor IOCTL cmd_id {cmd_id:#010x} is configured with output_required but \
                     the request did not request any output; this command's native contract \
                     always writes through a non-NULL SBYTE_ARRAY at pOutput, so a NULL pointer \
                     here would crash the native call (ADR-219, as amended)"
                )));
            }
            if request.output_capacity > VENDOR_IOCTL_MAX_WRAPPED_CAPACITY {
                return Err(Status::invalid_argument(format!(
                    "vendor IOCTL output_capacity {} exceeds the \
                     {VENDOR_IOCTL_MAX_WRAPPED_CAPACITY}-byte wrapped-mode cap (ADR-219)",
                    request.output_capacity
                )));
            }
            Ok(contract)
        }
    }
}

#[cfg(test)]
mod validate_vendor_ioctl_buffers_tests {
    use super::*;

    fn request(wrapped: bool, input: Option<Vec<u8>>, output_capacity: u32) -> VendorIoCtlRequest {
        VendorIoCtlRequest {
            wrapped,
            input,
            output_capacity,
        }
    }

    #[test]
    fn unconfigured_cmd_id_with_no_buffers_is_failed_precondition() {
        // ADR-219's seventh-round amendment: every vendor cmd_id must be
        // allowlisted, including one a client calls with no buffer at all --
        // an unconfigured cmd_id's own native contract might unconditionally
        // require one regardless of what a particular client request asks
        // for (design-advisor decision, reversing this test's earlier name/
        // assertion that a bufferless request against an unconfigured
        // cmd_id "still forwards").
        let status =
            validate_vendor_ioctl_buffers(None, 0x10000, &request(false, None, 0)).unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        let status =
            validate_vendor_ioctl_buffers(None, 0x10000, &request(true, None, 0)).unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn unconfigured_cmd_id_with_output_requested_is_failed_precondition() {
        let status =
            validate_vendor_ioctl_buffers(None, 0x10000, &request(false, None, 4)).unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn unconfigured_cmd_id_with_input_present_is_failed_precondition() {
        let status =
            validate_vendor_ioctl_buffers(None, 0x10000, &request(false, Some(vec![1]), 0))
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn a_fully_optional_raw_contract_allows_a_bufferless_request() {
        // The documented way to allowlist a genuinely bufferless vendor
        // command: shape = "raw" with both byte counts left at 0.
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 0,
            output_bytes: 0,
        };
        assert!(
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(false, None, 0))
                .is_ok()
        );
    }

    #[test]
    fn raw_contract_rejects_wrapped_flag_bit() {
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 4,
            output_bytes: 4,
        };
        let status =
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(true, None, 4))
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn sbyte_array_contract_rejects_raw_flag_bit() {
        let contract = crate::config::VendorIoctlContract::SbyteArray {
            input_required: false,
            output_required: false,
        };
        let status =
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(false, None, 4))
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn sbyte_array_contract_rejects_missing_input_when_input_required() {
        let contract = crate::config::VendorIoctlContract::SbyteArray {
            input_required: true,
            output_required: false,
        };
        let status =
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(true, None, 0))
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn sbyte_array_contract_accepts_a_present_but_empty_input_when_input_required() {
        // A present-but-empty wrapped input still becomes a real non-NULL
        // SBYTE_ARRAY (NumOfBytes: 0) -- unlike Raw's `!is_empty()` presence
        // check, this satisfies a wrapped-mode presence requirement.
        let contract = crate::config::VendorIoctlContract::SbyteArray {
            input_required: true,
            output_required: false,
        };
        assert!(
            validate_vendor_ioctl_buffers(
                Some(contract),
                0x10000,
                &request(true, Some(Vec::new()), 0)
            )
            .is_ok()
        );
    }

    #[test]
    fn sbyte_array_contract_rejects_missing_output_when_output_required() {
        let contract = crate::config::VendorIoctlContract::SbyteArray {
            input_required: false,
            output_required: true,
        };
        let status =
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(true, None, 0))
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn sbyte_array_contract_allows_missing_buffers_when_neither_direction_is_required() {
        let contract = crate::config::VendorIoctlContract::SbyteArray {
            input_required: false,
            output_required: false,
        };
        assert!(
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(true, None, 0)).is_ok()
        );
    }

    #[test]
    fn raw_contract_rejects_output_capacity_above_configured_output_bytes() {
        // input_bytes: 0 so the new "input required" presence check (this
        // test isn't exercising it) can't fire first -- isolates the
        // "exceeds configured size" check this test is actually about.
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 0,
            output_bytes: 4,
        };
        let status =
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(false, None, 5))
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn raw_contract_rejects_input_longer_than_configured_input_bytes() {
        // output_bytes: 0 so the new "output required" presence check (this
        // test isn't exercising it) can't fire first -- isolates the
        // "exceeds configured size" check this test is actually about.
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 4,
            output_bytes: 0,
        };
        let status = validate_vendor_ioctl_buffers(
            Some(contract),
            0x10000,
            &request(false, Some(vec![0; 5]), 0),
        )
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn raw_contract_rejects_non_empty_input_when_input_bytes_is_zero() {
        // output_bytes: 0 so the new "output required" presence check (this
        // test isn't exercising it) can't fire first.
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 0,
            output_bytes: 0,
        };
        let status = validate_vendor_ioctl_buffers(
            Some(contract),
            0x10000,
            &request(false, Some(vec![1]), 0),
        )
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn raw_contract_rejects_missing_input_when_input_bytes_is_nonzero() {
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 4,
            output_bytes: 0,
        };
        let status =
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(false, None, 0))
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn raw_contract_rejects_explicitly_empty_input_when_input_bytes_is_nonzero() {
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 4,
            output_bytes: 0,
        };
        let status = validate_vendor_ioctl_buffers(
            Some(contract),
            0x10000,
            &request(false, Some(Vec::new()), 0),
        )
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn raw_contract_rejects_missing_output_when_output_bytes_is_nonzero() {
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 0,
            output_bytes: 4,
        };
        let status =
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(false, None, 0))
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn raw_contract_allows_missing_direction_when_its_own_byte_count_is_zero() {
        // input_bytes: 0 means this direction is genuinely optional -- only
        // output (output_bytes: 4) is mandatory here, and it's supplied.
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 0,
            output_bytes: 4,
        };
        assert!(
            validate_vendor_ioctl_buffers(Some(contract), 0x10000, &request(false, None, 4))
                .is_ok()
        );
    }

    #[test]
    fn raw_contract_accepts_exact_configured_sizes() {
        let contract = crate::config::VendorIoctlContract::Raw {
            input_bytes: 4,
            output_bytes: 4,
        };
        assert!(
            validate_vendor_ioctl_buffers(
                Some(contract),
                0x10000,
                &request(false, Some(vec![0; 4]), 4)
            )
            .is_ok()
        );
    }

    #[test]
    fn sbyte_array_contract_rejects_output_capacity_above_wrapped_cap() {
        let contract = crate::config::VendorIoctlContract::SbyteArray {
            input_required: false,
            output_required: false,
        };
        let status = validate_vendor_ioctl_buffers(
            Some(contract),
            0x10000,
            &request(true, None, VENDOR_IOCTL_MAX_WRAPPED_CAPACITY + 1),
        )
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn sbyte_array_contract_accepts_output_capacity_at_wrapped_cap() {
        let contract = crate::config::VendorIoctlContract::SbyteArray {
            input_required: false,
            output_required: false,
        };
        assert!(
            validate_vendor_ioctl_buffers(
                Some(contract),
                0x10000,
                &request(true, None, VENDOR_IOCTL_MAX_WRAPPED_CAPACITY)
            )
            .is_ok()
        );
    }
}

/// ADR-219 Decision item 2's two-mode vendor IOCTL passthrough command,
/// implementing the native wrapper crate's generic `IoCtlCommand` extension
/// point (`j2534_0404::J2534Api0404::ioctl`).
///
/// Owns every buffer it hands the native call a pointer into -- the
/// input/output byte buffers, plus (wrapped mode) the scratch `SBYTE_ARRAY`
/// structs describing them -- and never resizes any of them after
/// construction. `input_ptr`/`output_ptr` build the wrapped-mode
/// `SBYTE_ARRAY` in place, at call time, into a field already living at
/// `self`'s own (by-then-final) address -- not by pre-computing a pointer
/// before moving the owning value, which would need extra reasoning about
/// `Vec`/struct moves; this way there is none to reason about.
enum VendorIoCtl {
    /// flags bit 2 == 0: `pInput`/`pOutput` point directly at the raw byte
    /// buffers (e.g. a vendor IOCTL expecting a direct-value pointer like
    /// `u32 *`). A present-but-zero-length input still passes `NULL` --
    /// raw mode has no way to represent "an empty but present" buffer
    /// distinct from "no buffer at all" (ADR-219 Decision item 2).
    ///
    /// Unlike wrapped mode, raw mode has no way to tell the native side how
    /// large `output`'s backing allocation actually is -- that's inherent
    /// to what a raw pointer is, mirroring the real J2534 API's own `void*`
    /// IOCTL parameters -- so the native side (or vendor-specific mock) can
    /// write however many bytes that particular vendor command's contract
    /// calls for. **As amended (Codex review, PR #133 third round):** rather
    /// than a fixed cap applied uniformly to every raw-mode command
    /// (invented, with no basis in any specific command's real contract,
    /// and still unsafe for a command whose real need exceeds it), `output`'s
    /// backing `Vec` is allocated EXACTLY at the operator-configured
    /// `output_bytes` for this `cmd_id` (`crate::config::VendorIoctlContract::Raw`,
    /// looked up and validated by `validate_vendor_ioctl_buffers` before this
    /// is ever constructed -- a command carrying a non-NULL buffer with no
    /// configured contract is rejected before reaching here at all). Only the
    /// client-requested `output_capacity` field's worth of leading bytes are
    /// ever handed back to the client (`into_output_bytes`); dispatch already
    /// validated `output_capacity <= output_bytes`.
    ///
    /// The same problem exists in the read direction: the native side (or
    /// vendor-specific mock) can read more bytes from `pInput` than the
    /// client supplied (e.g. a command that unconditionally dereferences
    /// `pInput` as a fixed-size value wider than the client's buffer). So a
    /// non-empty `input` is, symmetrically, always zero-extended up to the
    /// configured `input_bytes` (`pad_to_capacity`) before construction --
    /// the client's real bytes stay at the front, followed by zero padding,
    /// so a native read past the client-supplied length finds zeros rather
    /// than out-of-bounds/uninitialized memory. A present-but-zero-length
    /// input (`Some(vec![])`) is deliberately left unpadded so it stays
    /// empty, preserving the NULL-pointer behavior documented above --
    /// padding it would turn a deliberate "no buffer" signal into a real (if
    /// all-zero) pointer. An input already at or above `input_bytes` is left
    /// exactly as the client sent it (dispatch already rejected an input
    /// longer than `input_bytes`, so this can only be exactly at the bound).
    ///
    /// **`input`/`output` are backed by [`AlignedByteBuf`], not a plain
    /// `Vec<u8>`** (Codex review, PR #133 fourth round): a raw-mode vendor
    /// command's native contract can dereference `pInput`/`pOutput` as a
    /// type wider than `u8` (e.g. `u32 *`), and a `Vec<u8>`'s allocation is
    /// only guaranteed `align_of::<u8>()` (1-byte) alignment by Rust's
    /// memory model -- dereferencing it as a wider type through the native
    /// side is undefined behavior regardless of what any particular
    /// allocator happens to do in practice. See [`AlignedByteBuf`]'s own
    /// doc comment for the fix.
    Raw {
        ioctl_id: u32,
        input: Option<AlignedByteBuf>,
        output: Option<AlignedByteBuf>,
        /// The client's originally-requested `output_capacity` -- distinct
        /// from `output`'s (exactly-configured-size) backing allocation
        /// size.
        output_capacity: usize,
    },
    /// flags bit 2 == 1: `pInput`/`pOutput` each point at an `SBYTE_ARRAY`.
    /// `input`/`output` being `None` means the corresponding pointer is
    /// `NULL` outright (input absent entirely / output not requested);
    /// `Some(vec)` (possibly empty) means the pointer is a real, non-null
    /// `SBYTE_ARRAY` -- unlike `Raw`, a present-but-zero-length input here
    /// still gets an actual empty `SBYTE_ARRAY` object.
    Wrapped {
        ioctl_id: u32,
        input: Option<Vec<u8>>,
        input_array: j2534_0404::SBYTE_ARRAY,
        output: Option<Vec<u8>>,
        output_array: j2534_0404::SBYTE_ARRAY,
    },
}

/// A 16-byte, 16-aligned chunk -- [`AlignedByteBuf`]'s backing element.
///
/// 16 is not an arbitrary round number: `PassThruIoctl`'s `pInput`/`pOutput`
/// are `void *` into memory the caller allocated, so by the C standard
/// (C11/C17 SS7.22.3, paraphrased) a conforming vendor DLL can only assume
/// *fundamental* alignment through them -- the alignment an ordinary
/// `malloc` call guarantees, never an extended (`_Alignas`/`__m256`-class)
/// alignment, since a real C client passing an ordinary heap buffer could
/// never satisfy that either. 16 bytes is at or above the fundamental
/// alignment on every target this workspace's `*-sys` crates are built for
/// (`x86_64`/`i686` Windows and Linux, `armv5te`) -- unlike `u128`, whose
/// own alignment varies by target and toolchain (8 bytes on 32-bit ARM),
/// `#[repr(align(16))]` is a fixed, target-independent language guarantee.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Align16Chunk([u8; 16]);

/// Byte-buffer storage for [`VendorIoCtl::Raw`]'s `input`/`output` fields,
/// backed by a `Vec<Align16Chunk>` rather than a plain `Vec<u8>` so the
/// allocation's actual alignment is 16 bytes regardless of its requested
/// byte length.
///
/// Raw mode hands the native call a bare pointer (`pInput`/`pOutput`) that
/// the vendor DLL's own command contract can dereference as any type it
/// chooses -- e.g. `u32 *`, exactly `MOCK_VENDOR_IOCTL_RAW_U32`'s own
/// documented shape. A `Vec<u8>` is only guaranteed `align_of::<u8>()`
/// (1-byte) alignment by Rust's memory model, so handing the native side a
/// `Vec<u8>`-backed pointer it dereferences as a wider type is undefined
/// behavior even though every allocator this workspace has been run
/// against happens to return well-aligned memory for any nontrivial size
/// in practice (Codex review, PR #133 fourth round). An earlier version of
/// this type used a `Vec<u64>` (8-byte alignment) instead, which Codex's
/// PR #133 sixth round correctly pointed out is still insufficient for a
/// fundamentally-16-byte-aligned native type (e.g. `long double` on the
/// x86_64 System V ABI) -- see [`Align16Chunk`]'s own doc comment for why
/// 16 (not a wider, e.g. `u128`-backed, chunk) is the right fixed ceiling
/// rather than another guess of the same kind.
///
/// The requested byte length is rounded up to a whole number of 16-byte
/// chunks (`byte_len.div_ceil(16)`); any trailing bytes in the last partial
/// chunk are implicitly zero, since the backing `Vec<Align16Chunk>` is
/// always zero-initialized at construction and never resized afterward
/// (see `VendorIoCtl`'s own safety comment). Every accessor below still
/// reports exactly the requested byte length, never the chunk-rounded-up
/// one.
struct AlignedByteBuf {
    chunks: Vec<Align16Chunk>,
    len: usize,
}

impl AlignedByteBuf {
    /// A zeroed buffer of exactly `len` requested bytes (backed by a
    /// chunk-rounded-up `Vec<Align16Chunk>` allocation).
    ///
    /// `len` is always one of `raw_input_bytes`/`raw_output_bytes` from an
    /// already-validated `VendorIoctlContract::Raw` (`crate::config`'s
    /// `VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES` startup ceiling), never a
    /// client-controlled value directly -- the `debug_assert!` below exists
    /// purely to catch a future caller that bypasses that config validation
    /// in tests, not as a runtime safety net (Codex review, PR #133
    /// eleventh round; design-advisor decision).
    fn zeroed(len: usize) -> Self {
        debug_assert!(
            len <= crate::config::VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES as usize,
            "AlignedByteBuf::zeroed called with len ({len}) above the configured startup \
             ceiling -- every caller must go through validate_vendor_ioctl_buffers first"
        );
        AlignedByteBuf {
            chunks: vec![Align16Chunk([0u8; 16]); len.div_ceil(16)],
            len,
        }
    }

    /// A zeroed buffer of exactly `target` requested bytes, with `bytes`
    /// copied into its front -- mirrors the old (pre-fix) `pad_to_capacity`
    /// free function's zero-extension behavior for raw mode's `input`
    /// padding (see `VendorIoCtl::Raw`'s doc comment): `target` is the
    /// operator-configured `input_bytes` for this `cmd_id` (ADR-219, as
    /// amended), and `bytes.len()` must already be `<= target` -- the only
    /// way this is ever called, after `validate_vendor_ioctl_buffers`.
    fn zero_padded_from(bytes: &[u8], target: usize) -> Self {
        debug_assert!(bytes.len() <= target);
        let mut buf = Self::zeroed(target);
        buf.as_bytes_mut()[..bytes.len()].copy_from_slice(bytes);
        buf
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A byte-slice view of exactly the requested (not chunk-rounded-up)
    /// byte length.
    fn as_bytes(&self) -> &[u8] {
        // Safety: `chunks` is a valid, initialized `Vec<Align16Chunk>`
        // allocation of at least `self.len` bytes (rounded up to whole
        // chunks at construction); reinterpreting it as `u8` never
        // violates alignment (every address satisfies `align_of::<u8>() ==
        // 1`), and `self.len` never exceeds the allocation's actual byte
        // length.
        unsafe { std::slice::from_raw_parts(self.chunks.as_ptr().cast::<u8>(), self.len) }
    }

    /// The mutable counterpart of [`Self::as_bytes`].
    fn as_bytes_mut(&mut self) -> &mut [u8] {
        // Safety: see `as_bytes` -- same allocation, mutable borrow.
        unsafe { std::slice::from_raw_parts_mut(self.chunks.as_mut_ptr().cast::<u8>(), self.len) }
    }

    /// The raw pointer to hand the native call -- points into this
    /// buffer's own `Vec<Align16Chunk>` allocation, so it is aligned to at
    /// least 16 bytes.
    fn as_mut_ptr(&mut self) -> *mut std::ffi::c_void {
        self.chunks.as_mut_ptr().cast()
    }

    /// Consumes `self`, returning an owned `Vec<u8>` of exactly the
    /// requested byte length (never the chunk-rounded-up one) -- used by
    /// `into_output_bytes`'s existing truncation logic.
    fn into_bytes(self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod aligned_byte_buf_tests {
    use super::*;

    /// The actual property this type exists to establish: the pointer
    /// handed to the native call must be aligned to at least 16 bytes,
    /// regardless of the requested byte length -- including lengths that
    /// are not themselves multiples of 16, which is exactly the case a
    /// plain `Vec<u8>` (1-byte-aligned by Rust's memory model) cannot
    /// guarantee. This is a direct test of the alignment property, not an
    /// inference from the existing behavioral round-trip tests
    /// (`vendor_ioctl_raw_mode_round_trips_input_and_output` et al. in
    /// `tests/grpc_mock/vendor_passthrough.rs`) continuing to pass.
    #[test]
    fn backing_allocation_is_16_byte_aligned_for_a_range_of_byte_lengths() {
        for len in [0, 1, 3, 4, 7, 8, 9, 15, 16, 17] {
            let mut buf = AlignedByteBuf::zeroed(len);
            let ptr = buf.as_mut_ptr();
            assert_eq!(
                ptr as usize % 16,
                0,
                "AlignedByteBuf::zeroed({len})'s pointer must be 16-byte-aligned"
            );
        }
    }

    #[test]
    fn align16chunk_is_actually_16_byte_aligned() {
        assert_eq!(std::mem::align_of::<Align16Chunk>(), 16);
        assert_eq!(std::mem::size_of::<Align16Chunk>(), 16);
    }

    /// `zero_padded_from` must copy the client's bytes into the front of
    /// the buffer and zero-extend the rest up to `target` -- mirroring the
    /// old (pre-fix) `pad_to_capacity` free function's behavior for a
    /// plain `Vec<u8>`, now for the chunk-backed storage.
    #[test]
    fn zero_padded_from_copies_input_and_zero_extends_the_rest() {
        let buf = AlignedByteBuf::zero_padded_from(&[1, 2, 3], 7);
        assert_eq!(buf.len(), 7);
        assert_eq!(buf.into_bytes(), vec![1, 2, 3, 0, 0, 0, 0]);
    }

    /// `into_bytes` must report exactly the requested byte length, never
    /// the chunk-rounded-up backing allocation's length.
    #[test]
    fn into_bytes_reports_exactly_the_requested_length_not_the_chunk_rounded_one() {
        let buf = AlignedByteBuf::zeroed(3);
        assert_eq!(buf.into_bytes().len(), 3);
    }
}

impl VendorIoCtl {
    /// `raw_input_bytes`/`raw_output_bytes` are the operator-configured
    /// `vendor_ioctls` contract's `input_bytes`/`output_bytes` for this
    /// `cmd_id` (`0`/`0` when `wrapped` or when the command carries no
    /// buffer at all, in which case they are never consulted) -- already
    /// validated by `validate_vendor_ioctl_buffers` against `input`'s actual
    /// length and `output_capacity` before this is called (ADR-219, as
    /// amended).
    fn new(
        ioctl_id: u32,
        wrapped: bool,
        input: Option<Vec<u8>>,
        output_capacity: usize,
        raw_input_bytes: usize,
        raw_output_bytes: usize,
    ) -> Self {
        if wrapped {
            // Wrapped mode self-reports the actual written length via the
            // `SBYTE_ARRAY`'s `NumOfBytes` (read back in `into_output_bytes`),
            // so the native side already knows the buffer's real size and
            // this allocation can safely stay at exactly what the client
            // asked for (bounded at dispatch time by
            // `VENDOR_IOCTL_MAX_WRAPPED_CAPACITY`).
            let output = (output_capacity > 0).then(|| vec![0u8; output_capacity]);
            VendorIoCtl::Wrapped {
                ioctl_id,
                input,
                input_array: j2534_0404::SBYTE_ARRAY {
                    NumOfBytes: 0,
                    BytePtr: std::ptr::null_mut(),
                },
                output,
                output_array: j2534_0404::SBYTE_ARRAY {
                    NumOfBytes: 0,
                    BytePtr: std::ptr::null_mut(),
                },
            }
        } else {
            // See `Raw`'s own doc comment: the backing allocation is sized
            // EXACTLY at the operator-configured `raw_output_bytes`, not at
            // the client-supplied `output_capacity` -- dispatch already
            // validated `output_capacity <= raw_output_bytes`.
            let output = (output_capacity > 0).then(|| AlignedByteBuf::zeroed(raw_output_bytes));
            // Symmetric fix for the read direction (see `Raw`'s own doc
            // comment): a present, non-empty input is zero-extended up to
            // the configured `raw_input_bytes` so a native command that
            // reads more bytes than the client supplied finds zero padding,
            // never an out-of-bounds read. An explicitly empty input is left
            // as-is so `input_ptr` still resolves it to `NULL`, not a padded
            // all-zero buffer.
            let input = input.map(|bytes| {
                if bytes.is_empty() {
                    AlignedByteBuf::zeroed(0)
                } else {
                    AlignedByteBuf::zero_padded_from(&bytes, raw_input_bytes)
                }
            });
            VendorIoCtl::Raw {
                ioctl_id,
                input,
                output,
                output_capacity,
            }
        }
    }

    /// Consumes `self`, returning the bytes to report back to the client via
    /// `DataItem.bytearray_data` -- `None` when no output was requested at
    /// all (`output_capacity == 0`), matching every other no-output IOCTL
    /// arm's `output_data: None`. ADR-219 Decision item 2's output rule: raw
    /// mode returns the first `output_capacity` bytes of the (exactly
    /// configured-size) backing buffer -- never the whole backing
    /// allocation, which would leak uninitialized/other-command memory
    /// beyond what the client actually asked for; wrapped mode returns
    /// `min(output_array.NumOfBytes, output_capacity)` bytes -- the
    /// `SBYTE_ARRAY`'s own self-reported length, read back after the native
    /// call wrote it via the pointer `output_ptr` handed out.
    fn into_output_bytes(self) -> Option<Vec<u8>> {
        match self {
            VendorIoCtl::Raw {
                output,
                output_capacity,
                ..
            } => output.map(|buf| {
                let len = output_capacity.min(buf.len());
                let mut bytes = buf.into_bytes();
                bytes.truncate(len);
                bytes
            }),
            VendorIoCtl::Wrapped {
                output,
                output_array,
                ..
            } => output.map(|buf| {
                let reported = output_array.NumOfBytes as usize;
                let len = reported.min(buf.len());
                buf[..len].to_vec()
            }),
        }
    }
}

// Safety: `input_ptr`/`output_ptr` each return either `null_mut()` or a
// pointer into one of this enum's own owned buffers -- raw mode's
// `AlignedByteBuf` (its own `Vec<Align16Chunk>`-backed allocation,
// 16-byte-aligned; see its doc comment) directly, or (wrapped mode) a
// `Vec<u8>` via a scratch
// `SBYTE_ARRAY` field whose `BytePtr` is written from that same owned
// buffer on each call -- valid and writable for the native call's entire
// duration, since neither buffer is resized after construction and `self`
// is never moved while `J2534Api0404::ioctl` holds its `&mut self` borrow.
unsafe impl j2534_0404::IoCtlCommand for VendorIoCtl {
    fn ioctl_id(&self) -> u32 {
        match self {
            VendorIoCtl::Raw { ioctl_id, .. } | VendorIoCtl::Wrapped { ioctl_id, .. } => *ioctl_id,
        }
    }

    fn input_ptr(&mut self) -> *mut std::ffi::c_void {
        match self {
            VendorIoCtl::Raw { input, .. } => match input {
                Some(buf) if !buf.is_empty() => buf.as_mut_ptr(),
                _ => std::ptr::null_mut(),
            },
            VendorIoCtl::Wrapped {
                input, input_array, ..
            } => match input {
                Some(buf) => {
                    *input_array = j2534_0404::SBYTE_ARRAY {
                        NumOfBytes: buf.len() as u32,
                        BytePtr: buf.as_mut_ptr(),
                    };
                    std::ptr::addr_of_mut!(*input_array).cast()
                }
                None => std::ptr::null_mut(),
            },
        }
    }

    fn output_ptr(&mut self) -> *mut std::ffi::c_void {
        match self {
            VendorIoCtl::Raw { output, .. } => match output {
                Some(buf) => buf.as_mut_ptr(),
                None => std::ptr::null_mut(),
            },
            VendorIoCtl::Wrapped {
                output,
                output_array,
                ..
            } => match output {
                Some(buf) => {
                    *output_array = j2534_0404::SBYTE_ARRAY {
                        NumOfBytes: buf.len() as u32,
                        BytePtr: buf.as_mut_ptr(),
                    };
                    std::ptr::addr_of_mut!(*output_array).cast()
                }
                None => std::ptr::null_mut(),
            },
        }
    }
}

impl J2534Service {
    /// Extracts a `cll_handle` from an `IoCtlRequest.handle`, for the
    /// ComLogicalLink-scoped (`L`) commands (ADR-079) and the legacy raw
    /// J2534 IDs, which all operate on a specific channel.
    fn require_cll_handle_for_ioctl(
        handle: Option<vci_service_interface::io_ctl_request::Handle>,
    ) -> Result<u32, Status> {
        match handle {
            Some(vci_service_interface::io_ctl_request::Handle::CllHandle(h)) => Ok(h.cll_handle),
            Some(vci_service_interface::io_ctl_request::Handle::ModuleHandle(_)) => {
                Err(Status::invalid_argument(
                    "this IOCTL command operates on a ComLogicalLink; use a cll_handle, not a \
                     module_handle",
                ))
            }
            Some(vci_service_interface::io_ctl_request::Handle::SystemHandle(_)) | None => {
                Err(Status::invalid_argument(
                    "this IOCTL command operates on a ComLogicalLink; a cll_handle is required",
                ))
            }
        }
    }

    /// Extracts (and validates) a `module_handle` from an `IoCtlRequest.handle`,
    /// for the module-scoped (`M`) commands (ADR-079). Returns the validated
    /// `module_handle` value itself (not just `()`) so call sites that need
    /// to open a specific module's device (`ensure_open_device_for`, ADR-107
    /// follow-up fix) don't have to re-extract it from `request.handle`. An
    /// instance method (not `Self::`-static, unlike its sibling
    /// `require_cll_handle_for_ioctl`) so it can pass `self.modules.len()`
    /// through to `require_module_handle`'s generalized range check
    /// (ADR-107).
    fn require_module_handle_for_ioctl(
        &self,
        handle: Option<vci_service_interface::io_ctl_request::Handle>,
    ) -> Result<u32, Status> {
        match handle {
            Some(vci_service_interface::io_ctl_request::Handle::ModuleHandle(h)) => {
                Self::require_module_handle(Some(h), self.modules.len())?;
                Ok(h.module_handle)
            }
            _ => Err(Status::invalid_argument(
                "this IOCTL command operates on the module; a module_handle is required",
            )),
        }
    }

    pub(super) async fn rpc_io_ctl(
        &self,
        request: Request<vci_service_interface::IoCtlRequest>,
    ) -> Result<Response<vci_service_interface::IoCtlResponse>, Status> {
        let request = request.into_inner();
        let cmd_id = match request.io_ctrl_command {
            Some(vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(id)) => id,
            Some(vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandName(_)) => {
                return Err(Status::unimplemented(
                    "named IOCTL commands are not supported directly; resolve the numeric \
                     io_ctrl_command_id via GetObjectId(OBJT_IO_CTRL, ...) first (ADR-079)",
                ));
            }
            None => {
                return Err(Status::invalid_argument(
                    "io_ctrl_command is required (io_ctrl_command_id or io_ctrl_command_name)",
                ));
            }
        };

        // The 25 D-PDU-style adapter commands (17 from ADR-079,
        // SW_CAN_HS/SW_CAN_NS from ADR-164 Decision 3/Phase 4,
        // START/QUERY/STOP_REPEAT_MESSAGE from ADR-165/Phase 12,
        // READ_J1962PIN_VOLTAGE from Phase 13, and GET/SET_DEVICE_CONFIG
        // from ADR-176/Phase 14) each declare their own
        // module_handle-vs-cll_handle target (M/L) and dispatch through a
        // dedicated handler below; any other numeric id -- including the 4
        // legacy raw J2534 IDs (CLEAR_RX_BUFFER et al.) -- falls through to
        // `rpc_io_ctl_legacy`, which always operates on a connected cll_handle.
        let output_data: Option<vci_service_interface::DataItem> = match cmd_id {
            id if id == PDU_IOCTL_RESET => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                self.ioctl_reset(module_handle).await?;
                None
            }
            id if id == PDU_IOCTL_READ_VBATT => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                let millivolts = self.ioctl_read_vbatt(module_handle).await?;
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::Unum32Value(
                        millivolts,
                    )),
                })
            }
            id if id == PDU_IOCTL_SET_PROG_VOLTAGE => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                self.ioctl_set_prog_voltage(module_handle, request.input_data)
                    .await?;
                None
            }
            id if id == PDU_IOCTL_READ_PROG_VOLTAGE => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                let millivolts = self.ioctl_read_prog_voltage(module_handle).await?;
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::Unum32Value(
                        millivolts,
                    )),
                })
            }
            id if id == PDU_IOCTL_READ_J1962PIN_VOLTAGE => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                let millivolts = self
                    .ioctl_read_j1962_pin_voltage(module_handle, request.input_data)
                    .await?;
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::Unum32Value(
                        millivolts,
                    )),
                })
            }
            id if id == PDU_IOCTL_GET_DEVICE_CONFIG => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                let entries = self
                    .ioctl_get_device_config(module_handle, request.input_data)
                    .await?;
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::BytearrayData(
                        vci_service_interface::IoBytearray {
                            data: pack_device_config_entries(&entries),
                        },
                    )),
                })
            }
            id if id == PDU_IOCTL_SET_DEVICE_CONFIG => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                self.ioctl_set_device_config(module_handle, request.input_data)
                    .await?;
                None
            }
            id if id == PDU_IOCTL_GENERIC => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                let _ = self.require_connected_device_for(module_handle).await?;
                let last_error = Some(self.module_state.lock().await.last_error.clone());
                return Err(state_guard_status(
                    Code::Unimplemented,
                    "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_GENERIC is not supported by this adapter",
                    PduError::PduErrIdNotSupported,
                    last_error,
                ));
            }
            id if id == PDU_IOCTL_GET_CABLE_ID => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                let _ = self.require_connected_device_for(module_handle).await?;
                let last_error = Some(self.module_state.lock().await.last_error.clone());
                return Err(state_guard_status(
                    Code::Unimplemented,
                    "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_GET_CABLE_ID is not supported by this adapter",
                    PduError::PduErrIdNotSupported,
                    last_error,
                ));
            }
            id if id == PDU_IOCTL_READ_IGNITION_SENSE_STATE => {
                let module_handle = self.require_module_handle_for_ioctl(request.handle)?;
                let _ = self.require_connected_device_for(module_handle).await?;
                let last_error = Some(self.module_state.lock().await.last_error.clone());
                return Err(state_guard_status(
                    Code::Unimplemented,
                    "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_READ_IGNITION_SENSE_STATE is not \
                     supported by this adapter",
                    PduError::PduErrIdNotSupported,
                    last_error,
                ));
            }
            id if id == PDU_IOCTL_CLEAR_TX_QUEUE => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_clear_tx_queue(cll_handle).await?;
                None
            }
            id if id == PDU_IOCTL_SUSPEND_TX_QUEUE => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_suspend_tx_queue(cll_handle).await?;
                None
            }
            id if id == PDU_IOCTL_RESUME_TX_QUEUE => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_resume_tx_queue(cll_handle).await?;
                None
            }
            id if id == PDU_IOCTL_CLEAR_RX_QUEUE => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_clear_rx_queue(cll_handle).await?;
                None
            }
            id if id == PDU_IOCTL_SET_BUFFER_SIZE => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_set_buffer_size(cll_handle, request.input_data)
                    .await?;
                None
            }
            id if id == PDU_IOCTL_START_MSG_FILTER => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_start_msg_filter(cll_handle, request.input_data)
                    .await?;
                None
            }
            id if id == PDU_IOCTL_STOP_MSG_FILTER => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_stop_msg_filter(cll_handle, request.input_data)
                    .await?;
                None
            }
            id if id == PDU_IOCTL_CLEAR_MSG_FILTER => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_clear_msg_filter(cll_handle).await?;
                None
            }
            id if id == PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_set_event_queue_properties(cll_handle, request.input_data)
                    .await?;
                None
            }
            id if id == PDU_IOCTL_SW_CAN_HS => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_sw_can_hs(cll_handle).await?;
                None
            }
            id if id == PDU_IOCTL_SW_CAN_NS => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_sw_can_ns(cll_handle).await?;
                None
            }
            id if id == PDU_IOCTL_SET_POLL_RESPONSE => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_set_poll_response(cll_handle, request.input_data)
                    .await?;
                None
            }
            id if id == PDU_IOCTL_BECOME_MASTER => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_become_master(cll_handle, request.input_data)
                    .await?;
                None
            }
            id if id == PDU_IOCTL_GET_NDIS_ADAPTER_INFO => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                let info = self.ioctl_get_ndis_adapter_info(cll_handle).await?;
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::BytearrayData(
                        vci_service_interface::IoBytearray {
                            data: pack_ndis_adapter_info(&info),
                        },
                    )),
                })
            }
            id if id == PDU_IOCTL_START_REPEAT_MESSAGE => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                let msg_id = self
                    .ioctl_start_repeat_message(cll_handle, request.input_data)
                    .await?;
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::Unum32Value(msg_id)),
                })
            }
            id if id == PDU_IOCTL_QUERY_REPEAT_MESSAGE => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                let status = self
                    .ioctl_query_repeat_message(cll_handle, request.input_data)
                    .await?;
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::Unum32Value(status)),
                })
            }
            id if id == PDU_IOCTL_STOP_REPEAT_MESSAGE => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                self.ioctl_stop_repeat_message(cll_handle, request.input_data)
                    .await?;
                None
            }
            id if id == PDU_IOCTL_SEND_BREAK => {
                let cll_handle = Self::require_cll_handle_for_ioctl(request.handle)?;
                let last_error = self
                    .logical_links
                    .lock()
                    .await
                    .get(&cll_handle)
                    .ok_or_else(|| {
                        unknown_handle_status(format!("unknown cll_handle {cll_handle}"))
                    })?
                    .last_error
                    .clone();
                return Err(state_guard_status(
                    Code::Unimplemented,
                    "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_SEND_BREAK is not supported by this \
                     adapter",
                    PduError::PduErrIdNotSupported,
                    last_error,
                ));
            }
            // ADR-219 Decision item 2: any `cmd_id` in the SAE J2534-1
            // §7.2.14.3 tool-manufacturer vendor range
            // (`0x0001_0000..=0xFFFF_FFFF`) not already matched by one of the
            // 25 arms above -- forwarded raw to native `PassThruIoctl` via
            // `rpc_io_ctl_vendor`, a sibling of `rpc_io_ctl_legacy` (not a
            // call site of it: unlike that function, this one carries a
            // payload and is not `cll_handle`-only). Every `cmd_id` below
            // `0x10000` still falls through to `rpc_io_ctl_legacy` exactly
            // as before this ADR.
            id if id >= 0x0001_0000 => {
                self.rpc_io_ctl_vendor(request.handle, id, request.input_data)
                    .await?
            }
            _ => {
                self.rpc_io_ctl_legacy(request.handle, cmd_id).await?;
                None
            }
        };

        Ok(Response::new(vci_service_interface::IoCtlResponse {
            output_data,
        }))
    }

    /// Shared CLL resolution for [`Self::rpc_io_ctl_legacy`]'s 4 arms:
    /// resolves `cll_handle` to its LIVE `channel_id`, current as of `chans`
    /// (the caller's already-held `self.shared_channels` guard), not the
    /// possibly-stale `LogicalLinkState::channel_id` field. Mirrors
    /// `require_gm_uart_link`'s "proof parameter" pattern: `chans` forces
    /// every caller to already hold `self.shared_channels` before this
    /// resolves, per ADR-080's outermost-lock rule.
    ///
    /// `self.get_link_state` is called first (nesting a `self.logical_links`
    /// acquisition under the caller's already-held `chans`, correct ADR-080
    /// order) to get `link`, then the live channel id is resolved via
    /// `link.channel_key.and_then(|k| chans.get(&k)).map(|sc| sc.channel_id)`
    /// -- NOT `LogicalLinkState::channel_id` (`LinkView` no longer even
    /// carries that field, since this was its only remaining reader).
    /// Resolving through `chans` via `channel_key` is what makes this
    /// current: a disconnect clears `channel_key`/tears down the
    /// `SharedChannel` entry under a held `shared_channels` guard
    /// (`rpc_link.rs`), so a `chans` lookup
    /// while still holding that same guard can never observe a stale
    /// `channel_id` for an already-disconnected CLL.
    ///
    /// **Gated on `link.connected` first** (`edge-case-hunter` finding,
    /// this PR's own close-out pass): a hard channel error
    /// (`events::handle_channel_hard_error`) deliberately leaves
    /// `channel_key` set (so a later Disconnect/Destroy can still release
    /// the shared-channel ref) while clearing `connected`/`channel_id` and
    /// marking the `SharedChannel` entry `dead` -- it is NOT removed. A bare
    /// `channel_key` -> `chans.get()` resolution alone would still find that
    /// dead entry and return its `channel_id` as if the CLL were live,
    /// letting a legacy IOCTL reach `PassThruIoctl` on an offline channel
    /// instead of correctly rejecting `PDU_ERR_CLL_NOT_CONNECTED` -- so
    /// `channel_key`/`chans` are consulted only when `link.connected` is
    /// `true`, mirroring every other L-scoped RPC's own connected check
    /// (e.g. `rpc_start_com_primitive`'s `if !link.connected`) and matching
    /// this function's pre-fix behavior, where a disconnected/dead link's
    /// `LogicalLinkState::channel_id` field was already `None` either way.
    /// Returns `(channel_id, link)` on success -- callers need `link` for
    /// `hw_protocol_id`/`base_hw_protocol_id()`/`last_error` used later in
    /// their own arms. `pub(super)` (not private): also called from
    /// `rpc_misc.rs::rpc_io_ctl_vendor` (ADR-219 Decision item 2's
    /// `cll_handle` resolution) and `rpc_link.rs::unstaged_vendor_config_read`
    /// (Decision item 3's unstaged-`GetComParam` live read), both wanting
    /// the identical connected + live-`channel_id` resolution this function
    /// already provides.
    pub(super) async fn resolve_live_legacy_link(
        &self,
        chans: &MutexGuard<'_, HashMap<ChannelKey, SharedChannel>>,
        cll_handle: u32,
    ) -> Result<(ChannelId, LinkView), Status> {
        let link = self.get_link_state(cll_handle).await?;
        let channel_id = if link.connected {
            link.channel_key
                .and_then(|key| chans.get(&key))
                .map(|sc| sc.channel_id)
        } else {
            None
        };
        let Some(channel_id) = channel_id else {
            return Err(state_guard_status(
                Code::FailedPrecondition,
                "logical link is not connected",
                PduError::PduErrCllNotConnected,
                link.last_error,
            ));
        };
        Ok((channel_id, link))
    }

    /// The 4 pre-existing raw-J2534-ID IOCTL commands (`CLEAR_RX_BUFFER`,
    /// `CLEAR_TX_BUFFER`, `CLEAR_PERIODIC_MSGS`, `CLEAR_MSG_FILTERS`, all
    /// `<= 0x14`), recognized as legacy aliases alongside the 25 D-PDU-style
    /// adapter commands (17 from ADR-079, SW_CAN_HS/SW_CAN_NS from
    /// ADR-164 Decision 3/Phase 4, START/QUERY/STOP_REPEAT_MESSAGE from
    /// ADR-165/Phase 12, READ_J1962PIN_VOLTAGE from Phase 13, and
    /// GET/SET_DEVICE_CONFIG from ADR-176/Phase 14). These
    /// are channel-wide (not scoped to one CLL sharing a physical channel)
    /// and always require a connected `cll_handle`.
    async fn rpc_io_ctl_legacy(
        &self,
        handle: Option<vci_service_interface::io_ctl_request::Handle>,
        cmd_id: u32,
    ) -> Result<(), Status> {
        let cll_handle = Self::require_cll_handle_for_ioctl(handle)?;

        // TOCTOU fix (design-advisor-approved): `shared_channels` is now held from
        // before `channel_id` resolution through each arm's own native call below,
        // mirroring `ioctl_sw_can_mode`/`ioctl_get_ndis_adapter_info`'s established
        // fix for the identical race class (ADR-080 order: `shared_channels` ->
        // `logical_links`/`api`, nested, never reversed). Previously `channel_id`
        // was snapshotted once here via a momentary `get_link_state` lock/release,
        // and each arm separately re-acquired `self.api`/`self.logical_links` to
        // actually perform the native call -- leaving a window where a concurrent
        // `DisconnectComLogicalLink` (which clears this CLL's own `channel_key`
        // under a held `shared_channels` guard, `rpc_link.rs`) could land in the
        // gap and let a stale `channel_id` reach the native call instead of
        // correctly surfacing `PDU_ERR_CLL_NOT_CONNECTED`. Holding `chans`
        // continuously closes that window.
        let chans = self.shared_channels.lock().await;
        let (channel_id, link) = self.resolve_live_legacy_link(&chans, cll_handle).await?;
        // Each arm acquires the api lock individually so that CLEAR_PERIODIC_MSGS can
        // subsequently take the logical_links lock without holding both simultaneously.
        // `last_error` is read fresh in each arm's failure path, after the native call
        // returns, rather than snapshotted once up front: the poll task can acquire
        // `self.api` first, detect a hard channel error, and update
        // `LogicalLinkState::last_error` while this RPC is still waiting for that same
        // lock, and a pre-call snapshot would miss it (Codex review, ADR-105).
        //
        // Each arm also drops `chans` (`shared_channels`) as soon as its own native
        // call is done, before this `last_error` read (PR #105 round 2, closing a
        // real bug where an early `return` on the error path used to skip that drop
        // and keep `shared_channels` held, blocking a concurrent
        // `events::handle_channel_hard_error` -- which itself needs `shared_channels`
        // first, ADR-080 -- from making progress at all). That drop removes an
        // artificial block; it is NOT a fence. Tokio's `Mutex::lock().await` can
        // resolve immediately without yielding when uncontended, so there is still no
        // guarantee this RPC's `logical_links` read runs after a concurrently-racing
        // `handle_channel_hard_error`'s own `last_error` write completes (Codex
        // review, PR #105 round 3) -- exactly the same best-effort, no-ordering-
        // guarantee property `ErrorDetail.error_event_data` already has by design
        // (ADR-105 Decision item 2: "fetched or read best-effort"). This was already
        // true of this function before PR #105 (the pre-PR code never held
        // `shared_channels` around the native call at all, so the same last_error
        // race existed independent of any lock); PR #105 does not change it and is
        // not attempting to.
        match cmd_id {
            id if id == j2534_0404::CLEAR_RX_BUFFER => {
                // Bind the native call's Result before matching on it: an `if let`
                // scrutinee's own temporaries (here, the `self.api` MutexGuard) stay
                // alive for the whole `if let` block, which would hold `self.api`
                // locked across the `self.logical_links.lock().await` below --
                // deadlock-prone if any code elsewhere acquires them in the opposite
                // order.
                let result = self.api.lock().await.clear_rx_buffer(channel_id);
                drop(chans);
                if let Err(err) = result {
                    let last_error = self
                        .logical_links
                        .lock()
                        .await
                        .get(&cll_handle)
                        .and_then(|l| l.last_error.clone());
                    return Err(map_native_error_for_link(
                        "PassThruIoctl CLEAR_RX_BUFFER",
                        &err,
                        last_error,
                    ));
                }
            }
            id if id == j2534_0404::CLEAR_TX_BUFFER => {
                let result = self.api.lock().await.clear_tx_buffer(channel_id);
                drop(chans);
                if let Err(err) = result {
                    let last_error = self
                        .logical_links
                        .lock()
                        .await
                        .get(&cll_handle)
                        .and_then(|l| l.last_error.clone());
                    return Err(map_native_error_for_link(
                        "PassThruIoctl CLEAR_TX_BUFFER",
                        &err,
                        last_error,
                    ));
                }
            }
            id if id == j2534_0404::CLEAR_PERIODIC_MSGS => {
                // Periodic-clear epoch fix (Codex review round 5, PR #101,
                // ADR-192/Phase 7 Stage 7c): `self.api` stays locked across
                // BOTH the native call and the `periodic_clear_epoch` bump
                // immediately after success -- see that field's own doc
                // comment on `J2534Service` for why this ordering is what
                // makes the epoch meaningful against a racing
                // `StartComPrimitive`'s own native start. The lock is
                // scoped to this inner block only, dropped before the
                // `Err` arm's own async work below runs (that work locks
                // `self.logical_links`, a separate lock, but there is no
                // reason for this ioctl's error path to hold `self.api`
                // any longer than the native call itself needs it).
                let clear_result = {
                    let api = self.api.lock().await;
                    api.clear_periodic_messages(channel_id).map(|()| {
                        // The bump happens only on a successful clear -- an
                        // `Err` returns early below, matching the existing
                        // control flow exactly. `clear_generation` is this
                        // clear's own post-increment value: a committed
                        // `Tp20BroadcastPeriodic::started_epoch` (or leaked
                        // id's paired epoch) strictly less than it started
                        // before this clear's native call and is therefore
                        // provably already gone device-side.
                        self.periodic_clear_epoch
                            .fetch_add(1, portable_atomic::Ordering::Relaxed)
                            + 1
                    })
                };
                // TOCTOU fix: `chans` (the outer prologue's `shared_channels` guard,
                // held since before `channel_id` resolution) is no longer needed once
                // this arm's own native call above has completed -- drop it here,
                // BEFORE matching `clear_result`, not after: the poll task's own
                // `events::handle_channel_hard_error` also needs `shared_channels`
                // (ADR-080 outermost lock) to record a concurrent hard error's
                // `last_error`/event data, and the `Err` arm just below reads
                // `LogicalLinkState::last_error` fresh. Dropping `chans` only after
                // a match that the `Err` arm's own early `return` skips would keep
                // it held across that `logical_links` read, blocking the poll task
                // from recording the real error first and making this RPC observe a
                // stale/`None` `last_error` instead (Codex review, PR #105 round 2).
                // Mirrors the RX/TX arms' own "drop right after the native call,
                // before checking the Result" shape.
                drop(chans);
                let clear_generation = match clear_result {
                    Ok(clear_generation) => clear_generation,
                    Err(err) => {
                        let last_error = self
                            .logical_links
                            .lock()
                            .await
                            .get(&cll_handle)
                            .and_then(|l| l.last_error.clone());
                        return Err(map_native_error_for_link(
                            "PassThruIoctl CLEAR_PERIODIC_MSGS",
                            &err,
                            last_error,
                        ));
                    }
                };
                // All periodic messages on this physical channel are gone.  Clear the
                // tester-present state on EVERY CLL that shares the channel; this is
                // pure software state now (ADR-083/this diff), so nothing to fail on
                // a later CoptStopcomm/disconnect, but a stale armed CLL would keep
                // the poll task writing tester-present frames after the client asked
                // to clear periodic messages.
                // Collect the handles of CLLs that actually had an active TP message
                // so we can emit PduErrEvtTesterPresentError for each of them.
                // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c):
                // `CLEAR_PERIODIC_MSGS` is channel-wide -- it also
                // invalidates a live TP2.0 broadcast periodic message
                // device-side, as a side effect of a client's unrelated
                // request to clear periodic messages on this physical
                // channel. Collected alongside `affected_clls` (same loop,
                // same lock) so the owning COP's tracked state can be
                // reconciled to `PduCopstFinished` below rather than left
                // claiming a `PeriodicMessageId` this native call just
                // invalidated.
                let mut affected_broadcast_periodic: Vec<(u32, u32)> = Vec::new();
                let affected_clls: Vec<u32> = {
                    let mut links = self.logical_links.lock().await;
                    let mut affected = Vec::new();
                    for (&h, link) in links
                        .iter_mut()
                        .filter(|(_, l)| l.channel_id == Some(channel_id))
                    {
                        // Periodic-clear epoch fix (Codex review round 5,
                        // PR #101, ADR-192/Phase 7 Stage 7c), refined by the
                        // "sentinel deferral" fix (edge-case-hunter finding,
                        // design-advisor-approved): only take and finalize a
                        // COMMITTED entry (`message_id.is_some()`) whose
                        // native start provably preceded THIS clear's
                        // own native `clear_periodic_messages` call
                        // (`started_epoch < clear_generation`). An entry with
                        // `started_epoch >= clear_generation` started AT OR
                        // AFTER this clear's native call ran -- its message
                        // is still live device-side and must be left
                        // tracked, not finalized as "cleared".
                        //
                        // A `None`-sentinel reservation (meaning the native
                        // start is still in flight) is NOT taken here: at
                        // scan time its ordering against this clear is
                        // undecidable, since its native call hasn't returned
                        // yet and so no `started_epoch` exists to compare.
                        // Instead the scan stamps this clear's
                        // `clear_generation` onto the reservation's
                        // `pending_clear_generation` field (via `.max`, so a
                        // second racing clear can't regress an
                        // already-recorded higher generation from a first
                        // clear) and leaves it tracked;
                        // `finalize_or_orphan_broadcast_periodic_start_locked`
                        // (`rpc_primitive.rs`) resolves the reservation's
                        // true fate once its own native call completes and
                        // `started_epoch` becomes known. See that field's
                        // doc comment on `Tp20BroadcastPeriodic` for the
                        // full resolution rule, and the comment below (past
                        // this loop) for why the other four terminators
                        // don't need this deferral.
                        if link.tp20_broadcast_periodic.is_some_and(|p| {
                            p.message_id.is_some() && p.started_epoch < clear_generation
                        }) {
                            let periodic = link.tp20_broadcast_periodic.take().unwrap();
                            affected_broadcast_periodic.push((h, periodic.cop_handle));
                        }
                        if let Some(p) = link
                            .tp20_broadcast_periodic
                            .as_mut()
                            .filter(|p| p.message_id.is_none())
                        {
                            p.pending_clear_generation =
                                p.pending_clear_generation.max(clear_generation);
                        }
                        // Idle (ADR-083) has no hardware periodic message for
                        // this ioctl to have cleared, but the service-owned
                        // idle sender must still be disarmed here (Codex
                        // review): a bare `Periodic`-only check left the poll
                        // task writing tester-present frames for an
                        // idle-mode CLL after the client asked to clear
                        // periodic messages, even though the ioctl reported
                        // success.
                        // Only an `Armed` CLL actually had something to clear:
                        // `None` (never configured) and `Cleared` (already
                        // cleared by a prior call) are left untouched, and no
                        // error event is re-emitted for either (matching the
                        // existing "only emit for CLLs that actually had
                        // something to clear" behavior). `resolved` is carried
                        // forward into `Cleared` rather than dropped (a third
                        // Codex-review finding, distinct from the `None` this
                        // used to transition to): `handle_update_param`'s
                        // re-arm gate needs it as the baseline for
                        // `same_wire_behavior`, so an unrelated later
                        // `CoptUpdateparam` doesn't resurrect this cleared
                        // tester-present (see `TesterPresentState::Cleared`'s
                        // doc comment). As of ADR-137's fourth Codex-review
                        // fix (round-4 restructure), nothing needs to be
                        // carried forward for any still-open discard window
                        // -- it already lives independently in
                        // `link.open_tp_discards`, which this write does not
                        // touch.
                        if let TesterPresentState::Armed { resolved, .. } =
                            &link.tester_present_state
                        {
                            let resolved = resolved.clone();
                            link.tester_present_state = TesterPresentState::Cleared {
                                resolved,
                                cleared_at: tokio::time::Instant::now(),
                            };
                            affected.push(h);
                        }
                    }
                    affected
                };
                for cll_h in affected_clls {
                    events::send_error_event(
                        &self.subscriptions,
                        &self.logical_links,
                        cll_h,
                        vci_service_interface::PduErrorEvent::PduErrEvtTesterPresentError,
                        // PDU_IOCTL_CLEAR_PERIODIC_MSGS is a channel-wide
                        // administrative action, not a specific COP's
                        // failure (ISO 22900-2 §9.6.2) -- it clears
                        // tester-present for every CLL sharing the channel,
                        // not just one CoptStartcomm/CoptUpdateparam's own.
                        None,
                    )
                    .await;
                }
                // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c):
                // finalize each affected broadcast-periodic COP -- this
                // native call already invalidated its `PeriodicMessageId`
                // device-side (`link.tp20_broadcast_periodic` was already
                // cleared above, in the same critical section as the
                // successful `clear_periodic_messages` call), so no
                // corresponding `stop_periodic_message` is issued here.
                // `PduCopstFinished`, not `PduCopstCancelled`: this is a
                // side effect of an unrelated channel-wide administrative
                // action, not a client-initiated `CancelComPrimitive`.
                //
                // Fix 2 (Codex review, P1, PR #101), refined by the
                // "sentinel deferral" fix (edge-case-hunter finding,
                // design-advisor-approved): unlike
                // `rpc_cancel_com_primitive`/`Disconnect`/
                // `DestroyComLogicalLink`, this site never issues a
                // per-message `stop_periodic_message(id)` call at all (the
                // channel-wide `clear_periodic_messages` above already did
                // the equivalent device-side, for every periodic message on
                // the channel that existed when it ran) -- but a
                // `None`-sentinel `message_id`
                // (`rpc_start_com_primitive`'s in-flight-start
                // reservation) is NOT take-and-finalized here, unlike a
                // provably-pre-clear real entry. At scan time a reservation's
                // ordering against THIS clear's own native call is
                // undecidable -- its native start hasn't returned yet, so no
                // `started_epoch` exists to gate on. The scan above instead
                // records this clear's `clear_generation` onto the
                // reservation's `pending_clear_generation` field (via `.max`,
                // so a second racing clear can't regress an already-recorded
                // higher generation) and leaves it tracked.
                // `finalize_or_orphan_broadcast_periodic_start_locked`
                // (`rpc_primitive.rs`) resolves the reservation's true fate
                // once its own native call completes and `started_epoch` becomes
                // known: `started_epoch < pending_clear_generation` means at
                // least one recorded clear's native call ran after this
                // start's own native call returned, so the message is
                // already dead device-side -- taken and finalized there
                // (mirroring this exact reasoning: no per-id
                // `stop_periodic_message` needed, since a channel-wide clear
                // already handled it natively); `started_epoch >=
                // pending_clear_generation` means an ordinary live commit.
                //
                // This deferral is specific to `CLEAR_PERIODIC_MSGS`. The
                // other four terminators of a broadcast-periodic COP
                // (`CoptCancel`, suspension-termination, teardown, and a
                // hard channel error) intentionally remain unconditional
                // sentinel-takers: their termination isn't contingent on
                // native-call ordering against a channel-wide clear -- they
                // mean to kill the message regardless of timing -- so only
                // this site needs the epoch/generation gate at all.
                //
                // Periodic-clear epoch fix (Codex review round 5, PR #101,
                // ADR-192/Phase 7 Stage 7c, design-advisor consult): this
                // in-flight-start race used to be an accepted residual -- a
                // real, already-started message racing this clear's native
                // call could be wrongly finalized as "cleared" even though
                // it actually started AFTER the native clear ran and is
                // still live device-side, leaked with nothing left
                // tracking it. The `clear_generation`-gated scan above
                // closes that race: an entry is finalized here only when
                // its `started_epoch` provably precedes this clear's own
                // native call (`J2534Service::periodic_clear_epoch`'s own
                // doc comment has the full mechanism); a post-clear entry
                // is left live and tracked instead. A DIFFERENT, narrower
                // residual remains open -- an id-targeted terminator
                // (CoptCancel/suspension-termination/teardown) whose OWN
                // `stop_periodic_message(id)` call races AFTER this clear's
                // native call already removed the id natively -- see
                // the backlog.
                // Codex review (edge-case-hunter, PR #101 follow-up):
                // `events::emit_terminal_if_live`, not a hand-rolled
                // remove-then-send -- see `terminate_tp20_broadcast_periodic_
                // for_suspension`'s own comment (above) for the exact A2-23
                // race this closes and the `cancelled_cops` drain it also
                // performs.
                for (cll_h, cop_h) in affected_broadcast_periodic {
                    events::emit_terminal_if_live(
                        &self.primitives,
                        &self.logical_links,
                        &self.subscriptions,
                        &self.terminal_cops,
                        cll_h,
                        cop_h,
                        vci_service_interface::PduComPrimitiveStatus::PduCopstFinished,
                    )
                    .await;
                }
                // Fix B (design-advisor consult, Codex review round 3,
                // ADR-192 Decision item 2; retain logic epoch-gated by the
                // periodic-clear epoch fix, Codex review round 5): the
                // channel-wide native clear above already stopped every
                // periodic message that existed device-side when it ran,
                // live or leaked -- so any `SharedChannel::
                // leaked_periodic_message_ids` entry whose paired epoch
                // predates `clear_generation` is moot and dropped here. An
                // entry at or after `clear_generation` is a genuine
                // post-clear leak this native call never touched and must
                // stay tracked -- same reasoning as `tp20_broadcast_periodic`'s
                // own epoch gate above, one physical resource over. No
                // per-id retry-probe needed for the dropped entries (unlike
                // `retry_leaked_periodic_message_stops`'s opportunistic
                // retry elsewhere): this is the one channel-wide native
                // clear in this service. Neither `api` nor `logical_links`
                // is held at this point (both released above) -- nor is the
                // outer prologue's own `shared_channels` guard, dropped
                // earlier in this arm right after the native
                // `clear_periodic_messages` call itself, before matching its
                // `Result` (Codex review, PR #105 round 2 -- the drop used to
                // sit after that match, which the `Err` arm's own early
                // `return` skipped entirely) -- so acquiring `shared_channels`
                // here (a fresh binding, shadowing the already-dropped one)
                // does not violate ADR-080's ordering.
                let mut chans = self.shared_channels.lock().await;
                if let Some(sc) = chans.values_mut().find(|sc| sc.channel_id == channel_id) {
                    sc.leaked_periodic_message_ids
                        .retain(|&(_, epoch)| epoch >= clear_generation);
                }
            }
            id if id == j2534_0404::CLEAR_MSG_FILTERS => {
                // Hold shared_channels across the hardware clear and the
                // client_filters purge below (ADR-080: shared_channels is
                // the outermost lock) -- otherwise this can race
                // ioctl_start_msg_filter's own shared_channels-held
                // install-then-record sequence: this clear could run after
                // START_MSG_FILTER's hardware install but before its
                // client_filters write, so this purge finds nothing to
                // clear, and START_MSG_FILTER goes on to record a filter_id
                // that no longer exists on hardware (Codex-review fix).
                // `chans` here is the outer prologue's own `shared_channels`
                // guard (held since before `channel_id` resolution, TOCTOU
                // fix) -- this arm is the first to potentially need it after
                // the match, so no separate re-acquisition is needed.
                {
                    let api = self.api.lock().await;
                    if let Err(err) = api.clear_message_filters(channel_id) {
                        drop(api);
                        // Drop `chans` here, before the `logical_links` read
                        // below, on the failure path only (`edge-case-hunter`
                        // finding, PR #105 round 2 -- the identical class of
                        // bug Codex found a few lines up in the sibling
                        // CLEAR_PERIODIC_MSGS arm): the success path below
                        // still needs `chans` held across the native clear
                        // and the `client_filters` purge together (see this
                        // arm's own opening comment on why), but on failure
                        // neither of those runs, so there is no reason left
                        // to keep `shared_channels` locked while reading
                        // `last_error` fresh -- doing so would otherwise
                        // block a concurrent `events::handle_channel_hard_
                        // error` (which itself needs `shared_channels` first,
                        // ADR-080) from recording the real error before this
                        // RPC's own read.
                        drop(chans);
                        let last_error = self
                            .logical_links
                            .lock()
                            .await
                            .get(&cll_handle)
                            .and_then(|l| l.last_error.clone());
                        return Err(map_native_error_for_link(
                            "PassThruIoctl CLEAR_MSG_FILTERS",
                            &err,
                            last_error,
                        ));
                    }
                }
                // This wiped every hardware filter on the channel, including
                // any installed by PDU_IOCTL_START_MSG_FILTER for every CLL
                // sharing it -- not just this one. Purge their now-stale
                // `client_filters` entries (the filter_ids they reference no
                // longer exist on hardware), or a later STOP_MSG_FILTER fails
                // on the dead id and reusing the same FilterNumber is wrongly
                // rejected as already-installed.
                {
                    let mut links = self.logical_links.lock().await;
                    for (_, link) in links
                        .iter_mut()
                        .filter(|(_, l)| l.channel_id == Some(channel_id))
                    {
                        link.client_filters.clear();
                    }
                }
                drop(chans);
                // Re-install filters so the service can still receive frames after the
                // clear.  Without this the channel goes permanently deaf because J2534
                // adapters block all inbound traffic when no filters are active (ADR-008).
                //
                // Per the J2534 v04.04 spec, FLOW_CONTROL_FILTER is only valid on ISO15765
                // channels and PASS_FILTER/BLOCK_FILTER are only valid on non-ISO15765
                // channels (ADR-038), so exactly one family is re-installed.
                // `hw_protocol_id` (not the service protocol) decides the family: in
                // software-ISO-TP mode the channel is raw CAN and gets a PASS_FILTER
                // (ADR-046).
                let j2534_proto_id = link.hw_protocol_id;
                // ADR-157 Plane B: the filter-family gate below must see the
                // base protocol id even for a `_PS` link; the actual
                // `install_pass_all_filter` call further down still uses the
                // raw `j2534_proto_id` (Plane A, message building).
                let base_proto_id = link.base_hw_protocol_id();
                if base_proto_id == j2534_0404::ISO15765 {
                    // CLEAR_MSG_FILTERS wiped every point-to-point filter too:
                    // rebuild it from each CLL's UniqueRespIdTable (ADR-039).
                    // No pass-all fallback is re-installed (ADR-122).
                    //
                    // No `channel_key.is_some()` guard here: `channel_id` above was
                    // itself resolved via `link.channel_key` in
                    // `resolve_live_legacy_link`, so `channel_key` is provably `Some`
                    // whenever this arm runs.
                    self.reinstall_iso15765_channel_filters_after_clear(channel_id)
                        .await;
                } else if resources::is_analog_in_protocol_id(base_proto_id)
                    || resources::is_tp2_0_protocol_id(base_proto_id)
                    || base_proto_id == j2534_0404::PROTOCOL_ETHERNET_NDIS
                {
                    // Neither clause 10 Analog Inputs (ADR-177) nor clause 19
                    // TP2.0 (ADR-188 §1) ever gets a pass-all filter installed
                    // in the first place -- `connect_new_physical_channel`'s
                    // own connect-time gate already skips both for the same
                    // reason (clause 10 has no filter concept at all; clause
                    // 19's per-connection addressing is the RX model, not a
                    // pass-all baseline). CLEAR_MSG_FILTERS clearing zero
                    // filters on either protocol has nothing to reinstall --
                    // reinstalling a pass-all filter here unconditionally was
                    // a second, independent gap in the same bug class the
                    // connect-time gate's own TP2.0 exclusion fixes (Codex
                    // review finding via `edge-case-hunter`, round 26, PR
                    // #97): reachable any time a client issues
                    // `PDU_IOCTL_CLEAR_MSG_FILTERS` against an already-
                    // connected channel of either protocol, independent of
                    // whether connect time itself ever tried to install one.
                    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
                    // same exclusion, same reasoning as Analog Inputs above --
                    // clause 24.2.5.5 gives this protocol no filter concept at
                    // all, so `connect_new_physical_channel`'s own connect-time
                    // gate never installs a pass-all filter for it either; this
                    // arm is the identical gap `install_pass_all_filter`'s
                    // connect-time exclusion comment already anticipates.
                } else {
                    // Re-derive the per-ID-type filter set via the same helper used at
                    // connect time (ADR-065), using the flags this channel was actually
                    // connected with -- a single TxFlags-0 filter here would silently
                    // stop delivering 29-bit frames on a CAN_ID_BOTH channel.
                    let connect_flags = {
                        let chans = self.shared_channels.lock().await;
                        match link.channel_key.and_then(|key| chans.get(&key)) {
                            Some(sc) => sc.connect_flags,
                            None => {
                                warn!(
                                    channel_id = ?channel_id,
                                    "CLEAR_MSG_FILTERS: no shared_channels entry for channel_key; \
                                     falling back to a single TxFlags-0 pass-all filter",
                                );
                                0
                            }
                        }
                    };
                    let api = self.api.lock().await;
                    if let Err(err) =
                        install_pass_all_filter(&api, channel_id, j2534_proto_id, connect_flags)
                    {
                        warn!(channel_id = ?channel_id, %err, "CLEAR_MSG_FILTERS: failed to re-install pass-all filter");
                    }
                }
            }
            _ => {
                // No native call and no lock reacquisition below this arm, so
                // dropping `chans` here is not load-bearing -- but every other
                // arm above drops it explicitly once done with it, and doing
                // the same here keeps that convention consistent rather than
                // leaving this arm as the one silent exception
                // (`edge-case-hunter` finding, this PR's own close-out pass).
                drop(chans);
                return Err(state_guard_status(
                    Code::Unimplemented,
                    format!("IOCTL command {cmd_id:#010x} is not supported by j2534-0404-service"),
                    PduError::PduErrIdNotSupported,
                    link.last_error,
                ));
            }
        }

        Ok(())
    }

    /// ADR-219 Decision item 2: `IoCtl` dispatch for any `cmd_id` in the SAE
    /// J2534-1 §7.2.14.3 tool-manufacturer vendor range
    /// (`0x0001_0000..=0xFFFF_FFFF`), reached from `rpc_io_ctl`'s own
    /// fallback arm. A sibling to `rpc_io_ctl_legacy`, not a call site of
    /// it: that function is `cll_handle`-only and carries no payload, so a
    /// vendor IOCTL (which needs `module_handle`-or-`cll_handle` resolution
    /// plus an in/out `DataItem` payload) needs its own handle resolution
    /// and its own decode/encode.
    ///
    /// **Config validation happens BEFORE handle resolution/locking**
    /// (design-advisor decision, ADR-219 as amended): a rejected call must
    /// never take the `shared_channels`/`api` locks, so
    /// `validate_vendor_ioctl_buffers` runs against the decoded request and
    /// this service's startup-loaded `vendor_ioctls` map before the `match
    /// handle` below does any lock acquisition or native handle resolution.
    ///
    /// **Handle resolution** (ADR-219 Decision item 2): `module_handle`
    /// resolves to the native `DeviceID` via `require_connected_device_for`
    /// (after `require_module_handle` validates the handle itself, mirroring
    /// `require_module_handle_for_ioctl`'s own two-step shape); `cll_handle`
    /// resolves to the live channel id via `resolve_live_legacy_link`, under
    /// the same `shared_channels`-outermost lock discipline
    /// `rpc_io_ctl_legacy` already follows (ADR-080); `system_handle` is
    /// rejected `invalid_argument` -- a J2534 IOCTL targets a device or a
    /// channel natively, never this service's own top-level system handle.
    async fn rpc_io_ctl_vendor(
        &self,
        handle: Option<vci_service_interface::io_ctl_request::Handle>,
        cmd_id: u32,
        input_data: Option<vci_service_interface::DataItem>,
    ) -> Result<Option<vci_service_interface::DataItem>, Status> {
        use vci_service_interface::io_ctl_request::Handle;

        let payload = match input_data.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::BytearrayData(arr)) => arr.data,
            None => Vec::new(),
            Some(_) => {
                return Err(Status::invalid_argument(
                    "vendor IOCTL commands (cmd_id >= 0x10000) require bytearray_data input \
                     carrying the ADR-219 flags/output_capacity header, even when the input \
                     carries no payload bytes of its own",
                ));
            }
        };
        let decoded = unpack_vendor_ioctl_request(&payload)?;
        let contract = self.vendor_ioctls.get(&cmd_id).copied();
        let contract = validate_vendor_ioctl_buffers(contract, cmd_id, &decoded)?;

        match handle {
            Some(Handle::ModuleHandle(h)) => {
                Self::require_module_handle(Some(h), self.modules.len())?;
                let (_device_guard, device_id) =
                    self.require_connected_device_for(h.module_handle).await?;
                let result = self
                    .run_vendor_ioctl(device_id.0, cmd_id, decoded, contract)
                    .await;
                match result {
                    Ok(output) => Ok(output),
                    Err(err) => {
                        let last_error = self.module_state.lock().await.last_error.clone();
                        Err(map_native_error_for_link(
                            &format!("PassThruIoctl vendor cmd_id {cmd_id:#010x}"),
                            &err,
                            Some(last_error),
                        ))
                    }
                }
            }
            Some(Handle::CllHandle(h)) => {
                let cll_handle = h.cll_handle;
                let chans = self.shared_channels.lock().await;
                let (channel_id, _link) = self.resolve_live_legacy_link(&chans, cll_handle).await?;
                let result = self
                    .run_vendor_ioctl(channel_id.0, cmd_id, decoded, contract)
                    .await;
                drop(chans);
                match result {
                    Ok(output) => Ok(output),
                    Err(err) => {
                        let last_error = self
                            .logical_links
                            .lock()
                            .await
                            .get(&cll_handle)
                            .and_then(|l| l.last_error.clone());
                        Err(map_native_error_for_link(
                            &format!("PassThruIoctl vendor cmd_id {cmd_id:#010x}"),
                            &err,
                            last_error,
                        ))
                    }
                }
            }
            Some(Handle::SystemHandle(_)) => Err(Status::invalid_argument(
                "vendor IOCTL commands (cmd_id >= 0x10000) require a module_handle or \
                 cll_handle -- there is no native IOCTL target for system_handle (ADR-219)",
            )),
            None => Err(Status::invalid_argument("handle is required")),
        }
    }

    /// Issues the actual native `PassThruIoctl` call for a vendor IOCTL
    /// already resolved to a native `handle` (`DeviceID`/`ChannelID`, per
    /// the caller's own handle resolution) -- ADR-219 Decision item 2's
    /// raw-pointer-vs-`SBYTE_ARRAY` two-mode forwarding via the native
    /// wrapper's generic `J2534Api0404::ioctl` extension point. Builds the
    /// [`VendorIoCtl`] command (owning every buffer the native call is
    /// handed a pointer into), issues the call, then encodes the output
    /// back into `DataItem.bytearray_data` per Decision item 2's output
    /// rules.
    async fn run_vendor_ioctl(
        &self,
        handle: u32,
        cmd_id: u32,
        request: VendorIoCtlRequest,
        contract: crate::config::VendorIoctlContract,
    ) -> Result<Option<vci_service_interface::DataItem>, j2534_0404::Error> {
        let output_capacity = request.output_capacity as usize;
        // Raw mode's backing allocation sizes come from the already-validated
        // `vendor_ioctls` contract (`validate_vendor_ioctl_buffers`, called by
        // `rpc_io_ctl_vendor` before this function is ever reached); `0`/`0`
        // when wrapped, in which case `VendorIoCtl::new` never consults them.
        // `contract` is never absent here (as of ADR-219's seventh-round
        // amendment, every vendor `cmd_id` is allowlisted).
        let (raw_input_bytes, raw_output_bytes) = match contract {
            crate::config::VendorIoctlContract::Raw {
                input_bytes,
                output_bytes,
            } => (input_bytes as usize, output_bytes as usize),
            crate::config::VendorIoctlContract::SbyteArray { .. } => (0, 0),
        };
        // `command` (which owns raw pointers, via its `SBYTE_ARRAY` scratch
        // fields once `input_ptr`/`output_ptr` run) is deliberately
        // constructed AFTER the only `.await` in this function, not before:
        // a value alive across an `.await` point must be `Send`, and a raw
        // pointer is not. Acquiring the lock first keeps `command`'s entire
        // lifetime on this side of that await, so this function's generated
        // future stays `Send` with no `unsafe impl Send` needed.
        let api = self.api.lock().await;
        let mut command = VendorIoCtl::new(
            cmd_id,
            request.wrapped,
            request.input,
            output_capacity,
            raw_input_bytes,
            raw_output_bytes,
        );

        // Safety: `command` owns every buffer its `input_ptr`/`output_ptr`
        // point into (see `VendorIoCtl`'s own doc comment); none of those
        // buffers are resized for the rest of this function, so the
        // pointers handed to the native call stay valid for its entire
        // duration.
        unsafe { api.ioctl(handle, &mut command) }?;
        drop(api);

        let bytes = command.into_output_bytes();
        Ok(bytes.map(|data| vci_service_interface::DataItem {
            data: Some(vci_service_interface::data_item::Data::BytearrayData(
                vci_service_interface::IoBytearray { data },
            )),
        }))
    }

    /// **PDU_IOCTL_RESET (M).** Soft state reset only -- does NOT
    /// `PassThruClose`/reopen the device, which would drop live channels/
    /// connections (explicit product decision, ADR-079). Also rebases the
    /// synthetic module clock (`events::module_timestamp_us()`) to
    /// (approximately) zero via `events::reset_module_clock()` -- ISO
    /// 22900-2 §9.1.6.1 requires the time base to reset "within the
    /// `PDU_IOCTL_RESET` function," not just at boot (ADR-120 amendment).
    /// That call is self-contained (no lock coordination with anything
    /// below) and is made first, before any of the
    /// `logical_links`/`api`/`primitives` locking this function otherwise
    /// does, so it is never ordered relative to those locks. Resets
    /// `module_state` to ready/idle and, for every live `LogicalLinkState`:
    /// clears `rx_buf` and `client_filters` (stopping each client filter via
    /// the wrapper first). Also flushes hardware RX/TX per physical channel
    /// via `clear_rx_buffer`/`clear_tx_buffer` where a `channel_id` is
    /// present. Drains `tx_held` and resets `tx_suspended_by_ioctl` (leaving
    /// `tx_suspended_by_lock` untouched -- ADR-123, RESET must not let a
    /// client bypass another CLL's held `LOCK_PHYSICAL_TX_QUEUE`) via
    /// `events::cancel_held_tx_items`, which also emits `PduCopstCancelled`
    /// for each drained item's cop and removes it from `primitives` --
    /// otherwise a held COP's client would never learn it was cancelled.
    ///
    /// A first, brief `logical_links` pass snapshots per-CLL state
    /// (`connect_generation`, `channel_id`, client filter ids, the `rx_buf`
    /// Arc), then per target: the RX-buffer clear takes its own brief
    /// `logical_links` pass; the hardware teardown is regrouped per physical
    /// channel and holds `shared_channels` (outermost, ADR-080), then `api`,
    /// then `logical_links` nested briefly inside for the generation/
    /// channel/occupant recheck (ADR-161 -- see that ADR for why this phase
    /// needs all three, unlike the rest of this function); `cancel_held_tx_items`
    /// locks `logical_links` and `primitives` (never together with `api`);
    /// and a final, brief `logical_links` pass applies the remaining
    /// in-memory resets. Matches this file's own lock-acquisition convention
    /// (see the comment on `rpc_io_ctl_legacy`'s CLEAR_PERIODIC_MSGS
    /// handling) followed by every other call site (`rpc_link.rs`,
    /// `rpc_module.rs`, `events.rs::poll_rx_inner`). `device_id` (via the
    /// `_device_guard`
    /// returned by `require_connected_device_for`) is held across this
    /// entire function body as the outermost lock (ADR-107 addendum) -- it
    /// is acquired once, before any of the `logical_links`/`api`/`primitives`
    /// locking above, and is never re-acquired while one of those is held.
    ///
    /// `module_handle` is the validated handle the caller addressed this
    /// IOCTL to; rejects with `PDU_ERR_RESOURCE_BUSY`/`FailedPrecondition`
    /// if a DIFFERENT module's device is currently open -- otherwise this
    /// would reset whichever module actually happens to be open regardless
    /// of which one was requested (ADR-107 follow-up fix, Codex review on PR
    /// #110) -- and with `PDU_ERR_MODULE_NOT_CONNECTED` if `module_handle`
    /// has never been connected via `ModuleConnect` at all (ISO 22900-2
    /// §9.4.29.2 NOTE 1, Table 12; A2-8 -- RESET used to proceed as a silent
    /// no-op in that case instead of rejecting). `_device_guard` is held for
    /// the entire function body -- `device_id` is the outermost lock
    /// (ADR-107 addendum) -- closing the TOCTOU window in which a
    /// concurrent ModuleConnect/ModuleDisconnect could swap the open device
    /// between this check and the reset work below (edge-case-hunter
    /// finding on the prior peek-and-release fix).
    ///
    /// **ADR-161: linearized at the `targets` snapshot.** `PDU_IOCTL_RESET`
    /// is module-wide, but the three phases below run after that snapshot,
    /// separated by `.await` points where a concurrent disconnect+reconnect
    /// of the same `cll_handle` can complete. Every phase is gated on the
    /// `connect_generation` captured in the snapshot (plus a `channel_id`
    /// recheck for the hardware teardown), so a target whose live generation
    /// has since advanced is skipped entirely by that phase, as if RESET had
    /// completed an instant before the reconnect -- never reaching forward
    /// to mutate a session RESET itself never observed. The hardware
    /// teardown phase additionally has to serialize against a SIBLING CLL
    /// joining the target's already-open, shared physical channel -- a join
    /// completes entirely under `shared_channels` (ADR-023/ADR-156) and
    /// never touches `api`, so it is invisible to a per-target
    /// `connect_generation`/`channel_id` recheck alone. See ADR-161's Phase
    /// 1 mechanism.
    async fn ioctl_reset(&self, module_handle: u32) -> Result<(), Status> {
        let (_device_guard, _device_id) = self.require_connected_device_for(module_handle).await?;
        // ISO 22900-2 §9.1.6.1: the synthetic time base resets "within the
        // PDU_IOCTL_RESET function." Self-contained global, no lock
        // coordination with logical_links/api/primitives below (ADR-120
        // amendment) -- placed first, before any of those locks are taken.
        events::reset_module_clock();
        *self.module_state.lock().await = ModuleState::default();

        struct ResetTarget {
            cll_handle: u32,
            connect_generation: u64,
            channel_id: Option<ChannelId>,
            filters: Vec<(u32, Vec<MessageFilterId>)>,
            rx_buf: Arc<Mutex<CllEventQueue>>,
        }

        // ADR-161: `shared_channels` is acquired outermost (ADR-080), then
        // `logical_links`, both held together for this one snapshot moment
        // so `channel_epochs` below observes exactly the same instant as
        // `targets` -- not a separately-locked read that could see a join
        // land in between.
        let (targets, channel_epochs): (Vec<ResetTarget>, HashMap<ChannelId, u64>) = {
            let chans = self.shared_channels.lock().await;
            let links = self.logical_links.lock().await;
            let targets = links
                .iter()
                .map(|(&cll_handle, link)| ResetTarget {
                    cll_handle,
                    connect_generation: link.connect_generation,
                    channel_id: link.channel_id,
                    filters: link
                        .client_filters
                        .iter()
                        .map(|(&number, ids)| (number, ids.clone()))
                        .collect(),
                    rx_buf: Arc::clone(&link.rx_buf),
                })
                .collect();
            let channel_epochs = chans
                .values()
                .map(|sc| (sc.channel_id, sc.occupancy_epoch))
                .collect();
            (targets, channel_epochs)
        };

        // Per-CLL, per-FilterNumber ids that failed to stop, so the final
        // pass below keeps only those still tracked in `client_filters` --
        // mirrors PDU_IOCTL_CLEAR_MSG_FILTER: an id whose PassThruStopMsgFilter
        // call fails may still be active on hardware, so it must stay
        // tracked for the client to retry STOP_MSG_FILTER/CLEAR_MSG_FILTER,
        // rather than being silently forgotten by RESET.
        let mut failed_by_cll: HashMap<u32, HashMap<u32, Vec<MessageFilterId>>> = HashMap::new();
        for target in &targets {
            // RX-buffer clear (ADR-161): the `connect_generation` check and
            // the clear itself share ONE `logical_links` critical section,
            // with the `rx_buf` lock nested inside it -- matching this
            // crate's established `logical_links -> queue` lock order (see
            // `ioctl_set_event_queue_properties`'s identical shape, itself
            // fixed for the same reason by ADR-126 round 2: a check-then-
            // separately-relock split reopens the exact window the check
            // exists to close, since a concurrent reconnect can land in the
            // gap between dropping `logical_links` and re-acquiring the
            // queue lock). No `channel_id` check -- a plain disconnect with
            // no reconnect leaves the generation unchanged but `channel_id`
            // becomes `None`, and that queue is still cleared today in that
            // case. The `rx_buf` `Arc` persists unchanged across a
            // reconnect, so clearing it unconditionally would wipe a new
            // session's already-queued events; `api` is never held here, and
            // no event is emitted while `rx_buf`'s lock is held.
            {
                let links = self.logical_links.lock().await;
                let same_generation = links
                    .get(&target.cll_handle)
                    .is_some_and(|live| live.connect_generation == target.connect_generation);
                if same_generation {
                    target.rx_buf.lock().await.items.clear();
                }
            }
        }

        // Hardware teardown (ADR-161 Phase 1), grouped per physical channel
        // rather than per target: a per-target `connect_generation`/
        // `channel_id` recheck alone says nothing about a SIBLING CLL that
        // joined this target's already-open, shared physical channel
        // (ADR-023/ADR-156) after the snapshot -- that join runs entirely
        // under `shared_channels` and never touches `api`, so there is no
        // fresh `PassThruConnect` for an `api`-first ordering to serialize
        // against. The fix serializes against both hazards together:
        // `shared_channels` (outermost, ADR-080 -- the same lock
        // `disconnect_com_logical_link`'s filter-teardown/ref_count sequence
        // already holds, rpc_link.rs), then `api`, then a brief
        // `logical_links` critical section that rechecks each of this
        // channel's targets' own generation/channel identity as before.
        // Channel occupancy since the snapshot is checked separately, via
        // `channel_occupancy_matches_snapshot` against the live
        // `SharedChannel`'s `occupancy_epoch` (stamped on every `ref_count`
        // increment, including the UUDT-companion join, which never touches
        // `connect_generation` and so is invisible to a `LogicalLinkState`
        // scan -- see `SharedChannel::occupancy_epoch`'s doc comment and
        // ADR-161). Holding `shared_channels` across the recheck prevents a
        // join from completing during the window; the epoch comparison
        // catches one that already completed before this channel's turn.
        // Per-CLL filter stops still run for every target that individually
        // matches; the channel-wide RX/TX buffer clears run at most once per
        // channel, and only when the channel's occupancy is unchanged since
        // the snapshot -- an accepted residual leaves stale buffered frames
        // behind otherwise, rather than risk destroying a session RESET
        // never observed (ADR-161 Consequences).
        let mut by_channel: HashMap<ChannelId, Vec<&ResetTarget>> = HashMap::new();
        for target in &targets {
            if let Some(channel_id) = target.channel_id {
                by_channel.entry(channel_id).or_default().push(target);
            }
        }

        for (&channel_id, group) in &by_channel {
            let chans = self.shared_channels.lock().await;
            let api = self.api.lock().await;
            let matching: Vec<&ResetTarget> = {
                let links = self.logical_links.lock().await;
                group
                    .iter()
                    .copied()
                    .filter(|target| {
                        links.get(&target.cll_handle).is_some_and(|live| {
                            live.connect_generation == target.connect_generation
                                && live.channel_id == Some(channel_id)
                        })
                    })
                    .collect()
            };

            // ADR-161: compare this channel's live `occupancy_epoch` (read
            // from the same `shared_channels` guard held across this whole
            // per-channel critical section) against the value
            // `channel_epochs` snapshotted before any phase ran. A missing
            // live entry (channel torn down and not recreated) is a
            // mismatch, same as a numerically-recycled `channel_id` landing
            // on a fresh epoch.
            let live_epoch = chans
                .values()
                .find(|sc| sc.channel_id == channel_id)
                .map(|sc| sc.occupancy_epoch);
            let occupancy_unchanged = channel_occupancy_matches_snapshot(
                live_epoch,
                channel_epochs.get(&channel_id).copied(),
            );

            // Filter stops are unconditional on occupancy -- only conditional
            // on being in `matching` -- since stopping a matching target's
            // own filter never touches a sibling's state.
            for target in &matching {
                let mut failed: HashMap<u32, Vec<MessageFilterId>> = HashMap::new();
                for (filter_number, filter_ids) in &target.filters {
                    for &filter_id in filter_ids {
                        if let Err(err) = api.stop_message_filter(channel_id, filter_id) {
                            warn!(
                                cll_handle = target.cll_handle,
                                filter_number = *filter_number,
                                filter_id = filter_id.0,
                                %err,
                                "PDU_IOCTL_RESET: failed to remove client filter, keeping it \
                                 tracked for retry"
                            );
                            failed.entry(*filter_number).or_default().push(filter_id);
                        }
                    }
                }
                failed_by_cll.insert(target.cll_handle, failed);
            }

            // Channel-wide buffer clears run at most once per channel, and
            // only when at least one target still matches and the channel's
            // occupancy is unchanged since the snapshot -- skipping them
            // (never the filter stops above) when a sibling's traffic would
            // otherwise be destroyed.
            if !matching.is_empty() && occupancy_unchanged {
                let _ = api.clear_rx_buffer(channel_id);
                let _ = api.clear_tx_buffer(channel_id);
            }

            drop(api);
            drop(chans);
        }

        for target in &targets {
            events::cancel_held_tx_items(
                &self.primitives,
                &self.logical_links,
                &self.subscriptions,
                &self.terminal_cops,
                target.cll_handle,
                Some(target.connect_generation),
                true,
            )
            .await;
        }

        // Filter-tracking writeback (ADR-161): a target whose live
        // generation no longer matches the snapshot is skipped entirely (no
        // writeback at all) -- its `client_filters` originates from
        // `pending_client_filters` at the new connect, not from anything
        // this snapshot tracked, and the old session's filters were already
        // drained by its own disconnect path. For a target that still
        // matches, the writeback is set-difference-based rather than a
        // blanket insert-or-remove, so a filter installed by
        // `PDU_IOCTL_START_MSG_FILTER` between the snapshot and this phase
        // survives even if it reuses a filter number this snapshot also
        // covered.
        let mut links = self.logical_links.lock().await;
        for target in &targets {
            let Some(link) = links.get_mut(&target.cll_handle) else {
                continue;
            };
            if link.connect_generation != target.connect_generation {
                continue;
            }
            let mut failed = failed_by_cll.remove(&target.cll_handle).unwrap_or_default();
            for (filter_number, snapshot_ids) in &target.filters {
                let failed_ids = failed.remove(filter_number).unwrap_or_default();
                let retained: Vec<MessageFilterId> = link
                    .client_filters
                    .get(filter_number)
                    .map(|live_ids| {
                        live_ids
                            .iter()
                            .filter(|id| failed_ids.contains(id) || !snapshot_ids.contains(id))
                            .copied()
                            .collect()
                    })
                    .unwrap_or_default();
                if retained.is_empty() {
                    link.client_filters.remove(filter_number);
                } else {
                    link.client_filters.insert(*filter_number, retained);
                }
            }
        }
        Ok(())
    }

    /// **PDU_IOCTL_CLEAR_TX_QUEUE (L).** Drops this CLL's held TX queue
    /// (`tx_held`), emitting `PduCopstCancelled` for each drained item's cop
    /// and removing it from `primitives` via `events::cancel_held_tx_items`
    /// (`tx_suspended` is left untouched -- this command clears queued items,
    /// it does not resume dispatch). Also marks every still-queued COP
    /// belonging to this CLL (i.e. still in `primitives` after the `tx_held`
    /// drain -- not yet parked there) as cancelled via `cancelled_cops` (so an
    /// in-flight mpsc item still gets `PduCopstCancelled` via
    /// `should_skip_cancelled_item` in `events.rs` when the poll task
    /// dequeues it) -- EXCLUDING `executing_cop` (the item the poll task is
    /// currently dispatching is not "queued") and, as of ADR-100 S5, any
    /// cop_handle with a live tier-2 (`RegistrantTier::ReceiveOnly`)
    /// registrant on this CLL (a detached IS-CYCLIC COP: also not "queued",
    /// see `ioctl_clear_tx_queue`'s own body comment). Then clears the
    /// hardware TX buffer via `clear_tx_buffer` -- but ONLY when this CLL's
    /// `SharedChannel` has `ref_count == 1`: the channel-wide hardware clear
    /// cannot be scoped to one CLL when the physical channel is shared with
    /// another CLL, so it is skipped in that case rather than dropping a
    /// sibling CLL's traffic. Distinct from the legacy channel-wide
    /// `IOCTL_CLEAR_TX_BUFFER` raw ID, which this adapter-level command does
    /// not call directly.
    ///
    /// **Gated on `connected` first, re-read fresh under `shared_channels`**
    /// (`edge-case-hunter` finding #2, PR #105 close-out backlog entry;
    /// refined by Codex review, PR #106 round 2): the native hardware
    /// clear's `channel_key`/`chans` resolution re-reads both `channel_key`
    /// and `connected` under a freshly-acquired `shared_channels` guard,
    /// immediately before resolving `channel_id` -- NOT the function-entry
    /// snapshot above, which several await points (TX-item cancellation,
    /// the COP-bookkeeping lock cycles) separate from this point, any of
    /// which a concurrent Disconnect/hard-error could land in. A
    /// hard-errored CLL retains `channel_key` (and its `SharedChannel`
    /// entry stays in the map, merely marked `dead`) purely so a later
    /// Disconnect/Destroy can still release the shared-channel ref, so a
    /// bare `channel_key` lookup alone is not a liveness check -- and
    /// neither is a `connected` read taken before an intervening await
    /// point. See `resolve_live_legacy_link`'s own doc comment above for
    /// the same discipline applied there from the start.
    async fn ioctl_clear_tx_queue(&self, cll_handle: u32) -> Result<(), Status> {
        let (channel_key, hw_protocol_id, pin_select) = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            (link.channel_key, link.hw_protocol_id, link.pin_select)
        };

        // This is PDU_IOCTL_CLEAR_TX_QUEUE's own independent, deliberately-
        // retained LOCK_PHYSICAL_TX_QUEUE gate: clearing/cancelling this
        // CLL's queued TX items is a destructive (non-reversible) operation
        // on the shared physical resource, unlike CoptSendrecv/CoptStartcomm/
        // non-empty-data CoptStopcomm, whose own former hard-reject gate here
        // was removed in favor of TX-suspend-and-resume (ADR-123) -- those
        // now simply queue and wait for the lock to release, which is safe
        // because enqueuing has no side effect to undo. A destructive clear
        // has no such "just wait" option, so it stays a synchronous reject.
        // Checked BEFORE any state mutation below -- cancelling held items is
        // not reversible, so a caller that gets ResourceExhausted must see no
        // side effects at all.
        {
            let links = self.logical_links.lock().await;
            if find_physical_lock_holder(
                &links,
                cll_handle,
                hw_protocol_id,
                pin_select,
                channel_key,
                LOCK_PHYSICAL_TX_QUEUE,
            )
            .is_some()
            {
                let last_error = links.get(&cll_handle).and_then(|l| l.last_error.clone());
                return Err(state_guard_status(
                    Code::ResourceExhausted,
                    "physical TX queue lock is held by another ComLogicalLink on this resource",
                    PduError::PduErrRscLockedByOtherCll,
                    last_error,
                ));
            }
        }

        events::cancel_held_tx_items(
            &self.primitives,
            &self.logical_links,
            &self.subscriptions,
            &self.terminal_cops,
            cll_handle,
            None,
            false,
        )
        .await;

        // `primitives` tracks a COP from StartComPrimitive until it fully
        // completes -- both while merely queued in the shared mpsc channel
        // and while actively executing (ADR-021's `executing_cop`). Only
        // queued items belong to "the TX queue"; a COP already executing
        // (e.g. a long-running CoptSendrecv) must keep running -- CLEAR_TX_QUEUE
        // is not CancelComPrimitive. Exclude it before extending
        // `cancelled_cops`, whose consumers (`should_skip_cancelled_item`)
        // would otherwise treat this exactly like an explicit cancel.
        let executing_cop = match channel_key {
            Some(channel_key) => {
                let chans = self.shared_channels.lock().await;
                match chans.get(&channel_key) {
                    Some(sc) => *sc.executing_cop.lock().await,
                    None => None,
                }
            }
            None => None,
        };
        // ADR-100 Decision §2, "required companion fix": a detached
        // (migrated) IS-CYCLIC registrant sits in `primitives` but is NOT a
        // queued item waiting to be dispatched -- it is an active,
        // spec-compliant receive-only ComPrimitive that already finished
        // sending (S5). Exclude any cop_handle with a live tier-2
        // (`RegistrantTier::ReceiveOnly`) registrant on this CLL from "the TX
        // queue" the same way `executing_cop` is already excluded above, or
        // this IOCTL would wrongly cancel a compliant receive-only COP that
        // was never actually queued.
        let detached_tier2_cops: std::collections::HashSet<u32> = self
            .logical_links
            .lock()
            .await
            .get(&cll_handle)
            .map(|link| {
                link.registrants
                    .iter()
                    .filter(|r| r.tier == RegistrantTier::ReceiveOnly)
                    .map(|r| r.cop_handle)
                    .collect()
            })
            .unwrap_or_default();
        // ADR-192/Phase 7 Stage 7c edge-case-hunter fix: a TP2.0 broadcast
        // periodic COP (`link.tp20_broadcast_periodic`) has its own
        // out-of-band lifecycle -- a live native `PassThruStartPeriodicMsg`
        // message, stopped only by `CoptCancel`, CLL teardown, or
        // `CLEAR_PERIODIC_MSGS` -- exactly like `executing_cop`/
        // `detached_tier2_cops` above, it is not "queued" and must not be
        // marked `cancelled_cops` here. Without this exclusion, `GetStatus`
        // would report a live broadcast-periodic COP as `PduCopstCancelled`
        // while its native periodic message is still actually transmitting.
        let tp20_broadcast_periodic_cop: Option<u32> = self
            .logical_links
            .lock()
            .await
            .get(&cll_handle)
            .and_then(|link| link.tp20_broadcast_periodic.map(|p| p.cop_handle));
        let cop_handles: Vec<u32> = self
            .primitives
            .lock()
            .await
            .iter()
            .filter(|&(&cop, entry)| {
                entry.cll_handle == cll_handle
                    && Some(cop) != executing_cop
                    && !detached_tier2_cops.contains(&cop)
                    && Some(cop) != tp20_broadcast_periodic_cop
            })
            .map(|(&cop, _)| cop)
            .collect();
        {
            let mut links = self.logical_links.lock().await;
            if let Some(link) = links.get_mut(&cll_handle) {
                // ADR-100 Decision §2 correctness fix (PR #78 edge-case-hunter
                // follow-up): `detached_tier2_cops` above was snapshotted from
                // a strictly earlier `logical_links` lock acquisition than the
                // `primitives` read that built `cop_handles`. A COP that
                // migrates tier-1 -> tier-2 (its registrant tier flips to
                // `ReceiveOnly`, its `executing_cop` clears) in that gap is
                // invisible to the stale snapshot and would otherwise be
                // wrongly marked here, then wrongly cancelled by the S5 sweep.
                // Re-check against `link.registrants` fresh, right here, under
                // the same lock this block already holds.
                let tier2_now: std::collections::HashSet<u32> = link
                    .registrants
                    .iter()
                    .filter(|r| r.tier == RegistrantTier::ReceiveOnly)
                    .map(|r| r.cop_handle)
                    .collect();
                // Codex review fix (P1, PR #101, round 10): `tp20_broadcast_
                // periodic_cop` above was snapshotted from a strictly earlier
                // `logical_links` lock acquisition than this final extend. A
                // concurrent `StartComPrimitive` publishing a fresh
                // broadcast-periodic reservation into `link.
                // tp20_broadcast_periodic` in that gap would be invisible to
                // the stale snapshot and wrongly marked `cancelled_cops`
                // here. Re-check fresh, under the same lock this block
                // already holds, exactly like `tier2_now` above.
                let broadcast_periodic_cop_now: Option<u32> =
                    link.tp20_broadcast_periodic.map(|p| p.cop_handle);
                link.cancelled_cops.extend(
                    cop_handles.iter().copied().filter(|c| {
                        !tier2_now.contains(c) && Some(*c) != broadcast_periodic_cop_now
                    }),
                );
            }
        }
        // ADR-182 follow-up fix (Codex review round, PR #78): batch sibling
        // of `rpc_cancel_com_primitive`'s own self-check -- a COP the
        // maintenance reap (`events::reap_expired_cyclic_registrants`) fully
        // finished, including removing it from `primitives`, in the gap
        // between the read above and the `cancelled_cops.extend` just above
        // would otherwise leave its mark permanently stale. Looping the full
        // pre-filter `cop_handles` set (rather than just what was actually
        // marked) is harmless: draining a handle that was excluded from the
        // extend above and never marked is a no-op given the helper's own
        // `primitives`-absence gate.
        for &cop in &cop_handles {
            events::drain_cancelled_cop_if_finalized(
                &self.primitives,
                &self.logical_links,
                cll_handle,
                cop,
            )
            .await;
        }

        // `edge-case-hunter` finding #2, PR #105 close-out backlog entry,
        // REFINED per Codex review (PR #106 round 2): the initial gate above
        // used `connected` from the function-entry snapshot, taken before
        // several await points (the LOCK_PHYSICAL_TX_QUEUE check,
        // `cancel_held_tx_items`, and the multiple `logical_links`/
        // `primitives` lock cycles for COP bookkeeping above) -- any of
        // which a concurrent Disconnect or `handle_channel_hard_error` could
        // land in, changing `channel_key`/`connected` after that snapshot
        // was already taken. `channel_key`/`connected` are therefore
        // re-read HERE instead, fresh, under a freshly-acquired
        // `shared_channels` guard (`chans`, below) -- mirroring
        // `resolve_live_legacy_link`'s own discipline of resolving entirely
        // under one continuously-held guard. Both `handle_channel_hard_
        // error` and every production write to `channel_key`/`connected`
        // acquire `shared_channels` first (ADR-080's outermost-lock rule),
        // so nothing can change either field between this read and the
        // native call below, which keeps `chans` held throughout.
        let chans = self.shared_channels.lock().await;
        let live_channel_key = self
            .logical_links
            .lock()
            .await
            .get(&cll_handle)
            .and_then(|l| l.connected.then_some(l.channel_key).flatten());
        if let Some(channel_key) = live_channel_key {
            // ADR-164 Phase 4 fix (closing the accepted-residual entry in
            // the Prioritized Backlog): the `ref_count == 1` decision and the
            // native `clear_tx_buffer` call below are now ONE critical
            // section, `shared_channels` held across both with `api`
            // acquired nested inside it -- this crate's own established
            // `shared_channels` (outermost) -> `api` lock order (ADR-080),
            // mirroring `ioctl_sw_can_mode`'s own identical TOCTOU fix
            // above (see its "ADR-164/Bug 2 fix" doc comment for the fuller
            // rationale, which applies here unchanged). Previously this was
            // two separate critical sections with `shared_channels` dropped
            // in between, leaving a window where a concurrent
            // `ConnectComLogicalLink` joining this physical channel (which
            // bumps `SharedChannel::ref_count` under its own, separate
            // `shared_channels` acquisition, `rpc_link.rs`) could land in
            // the gap: the gate observes `ref_count == 1`, decides to
            // proceed, and then the native clear actually fires onto what
            // is, by then, a shared channel -- silently affecting the
            // joining sibling CLL's queued TX items even though it never
            // asked for a clear. Holding `shared_channels` for the native
            // call closes that window without a new deadlock risk, since
            // `shared_channels -> api` is already this crate's documented
            // nesting order and is never reversed elsewhere.
            let channel_id = chans
                .get(&channel_key)
                .and_then(|sc| (sc.ref_count == 1).then_some(sc.channel_id));
            if let Some(channel_id) = channel_id {
                // last_error is read fresh here, after the native call fails, not
                // snapshotted at function entry: several await points (TX-item
                // cancellation, lock cycles above) separate entry from this call,
                // any of which the poll task could race a hard-error update into
                // (Codex review, edge-case-hunter follow-up, ADR-105).
                let api = self.api.lock().await;
                let result = api.clear_tx_buffer(channel_id);
                drop(api);
                drop(chans);
                if let Err(err) = result {
                    let last_error = self
                        .logical_links
                        .lock()
                        .await
                        .get(&cll_handle)
                        .and_then(|l| l.last_error.clone());
                    return Err(map_native_error_for_link(
                        "PassThruIoctl CLEAR_TX_BUFFER",
                        &err,
                        last_error,
                    ));
                }
            }
        }
        Ok(())
    }

    /// **PDU_IOCTL_SW_CAN_HS (L, ADR-164 Decision 3/Phase 4).** SAE J2534-2
    /// clause 9's SW_CAN_HS command (switch this CLL's channel to Single
    /// Wire CAN high-speed mode) via `PassThruIoctl(SW_CAN_HS)` -- no
    /// input/output parameters, per clause 9's own command definition.
    /// Rejected (`PDU_ERR_ID_NOT_SUPPORTED`, matching this adapter's
    /// existing not-supported IOCTL style, e.g. `PDU_IOCTL_SEND_BREAK`
    /// above) when this CLL's link is not an SW_CAN_PS/SW_ISO15765_PS link.
    ///
    /// Gated on `SharedChannel::ref_count == 1` (ADR-164 Consequences,
    /// decided at implementation time): mirrors `PDU_IOCTL_CLEAR_TX_QUEUE`'s
    /// existing precedent (`ioctl_clear_tx_queue` above) for a
    /// channel-wide native IOCTL whose physical channel may be shared by
    /// more than one CLL -- a shared SWCAN bus has only one wire, so a
    /// per-CLL high-speed-mode switch would silently affect every sibling
    /// CLL's traffic too; skipped as a no-op (`Ok(())`, no native call, same
    /// as `ioctl_clear_tx_queue`'s own `ref_count > 1` skip) rather than
    /// surprising a sibling CLL with a mode change it never requested.
    ///
    /// **Gated on `link.connected` first** (`edge-case-hunter` finding #2,
    /// PR #105 close-out backlog entry, fixed here): `ioctl_sw_can_mode`'s
    /// `channel_key`/`chans` resolution only proceeds when `connected` is
    /// also `true`, for the same reason `resolve_live_legacy_link`'s own
    /// doc comment above describes -- a hard-errored CLL retains
    /// `channel_key` (and its `SharedChannel` entry stays in the map,
    /// merely marked `dead`) purely so a later Disconnect/Destroy can still
    /// release the shared-channel ref.
    async fn ioctl_sw_can_hs(&self, cll_handle: u32) -> Result<(), Status> {
        self.ioctl_sw_can_mode(cll_handle, true).await
    }

    /// **PDU_IOCTL_SW_CAN_NS (L, ADR-164 Decision 3/Phase 4).** Same shape as
    /// [`Self::ioctl_sw_can_hs`] for SAE J2534-2 clause 9's SW_CAN_NS command
    /// (switch to normal-speed mode).
    async fn ioctl_sw_can_ns(&self, cll_handle: u32) -> Result<(), Status> {
        self.ioctl_sw_can_mode(cll_handle, false).await
    }

    /// Shared implementation for [`Self::ioctl_sw_can_hs`]/
    /// [`Self::ioctl_sw_can_ns`] -- see those methods' own doc comments for
    /// the rejection/gating rationale. `high_speed` selects which native
    /// IOCTL to issue once every check has passed.
    async fn ioctl_sw_can_mode(&self, cll_handle: u32, high_speed: bool) -> Result<(), Status> {
        // Round 4 (Codex finding, PR #98, same shape as
        // `require_gm_uart_link`'s own fix): `shared_channels` is now
        // acquired BEFORE `logical_links` is even read, so the `channel_key`/
        // `hw_protocol_id`/`last_error` snapshot below happens while already
        // holding the lock the `ref_count == 1` gate further down also uses
        // -- one unbroken critical section, ADR-080's outermost-lock order
        // (`shared_channels` -> `logical_links`). Previously the two locks
        // were acquired in two separate critical sections, leaving a window
        // where `DisconnectComLogicalLink` (which clears this CLL's own
        // `channel_key` under a held `shared_channels` guard, `rpc_link.rs`)
        // could run between the snapshot and this function's own
        // `shared_channels` acquisition -- the exact race Codex found for
        // `ioctl_set_poll_response`/`ioctl_become_master`, which this
        // function shares the identical snapshot-then-relock shape with.
        let chans = self.shared_channels.lock().await;
        let (hw_protocol_id, connected, channel_key, last_error) = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            (
                link.hw_protocol_id,
                link.connected,
                link.channel_key,
                link.last_error.clone(),
            )
        };

        // ADR-212/Round 2: re-keyed from `is_sw_protocol_id` to the
        // family-wide `is_sw_family_protocol_id` so these two IOCTLs still
        // work on a `_CHx`-connected SW-CAN link's own `hw_protocol_id`
        // (Context item 1: "service-side pass-through" is this gate, the
        // mock's own `is_sw_protocol` -> `is_sw_family_protocol` re-key is
        // the other half of the identical fix).
        if !resources::is_sw_family_protocol_id(hw_protocol_id) {
            return Err(state_guard_status(
                Code::Unimplemented,
                format!(
                    "PDU_ERR_ID_NOT_SUPPORTED: {} requires a SAE J2534-2 clause 9 Single \
                     Wire CAN (SW_CAN_PS/SW_ISO15765_PS) ComLogicalLink",
                    if high_speed {
                        "PDU_IOCTL_SW_CAN_HS"
                    } else {
                        "PDU_IOCTL_SW_CAN_NS"
                    }
                ),
                PduError::PduErrIdNotSupported,
                last_error,
            ));
        }

        let Some(channel_key) = channel_key else {
            // Not yet connected -- nothing to switch mode on yet; matches
            // this adapter's established "no channel, no-op" precedent for
            // an L-scoped command issued before ConnectComLogicalLink.
            return Ok(());
        };

        // ADR-164/Bug 2 fix (edge-case-hunter audit, TOCTOU race): the
        // `ref_count == 1` decision and the native `sw_can_hs`/`sw_can_ns`
        // call below are now ONE critical section, `shared_channels` held
        // across both with `api` acquired nested inside it -- this crate's
        // own established `shared_channels` (outermost) -> `api` lock
        // order (ADR-080; see `connect_new_physical_channel`'s identical
        // nesting for `PassThruConnect`, or `ioctl_reset`'s hardware-
        // teardown phase, for precedent). Previously this was two separate
        // critical sections with `shared_channels` dropped in between,
        // leaving a window where a concurrent `ConnectComLogicalLink`
        // joining this physical channel (which bumps `SharedChannel::
        // ref_count` under its own, separate `shared_channels` acquisition,
        // `rpc_link.rs`) could land in the gap: the gate observes
        // `ref_count == 1`, decides to proceed, and then the native mode
        // switch actually fires onto what is, by then, a shared channel --
        // silently affecting the joining sibling CLL with a mode change it
        // never requested, exactly what this gate exists to prevent (ADR-164
        // Consequences). Holding `shared_channels` for the native call closes
        // that window without a new deadlock risk, since `shared_channels ->
        // api` is already this crate's documented nesting order and is never
        // reversed elsewhere (no call site acquires `api` first and then
        // `shared_channels`).
        //
        // `edge-case-hunter` finding #2, PR #105 close-out backlog entry:
        // this resolution is also gated on `connected` -- see
        // `resolve_live_legacy_link`'s own doc comment above for the fuller
        // rationale (a hard-errored CLL retains `channel_key`/its
        // `SharedChannel` entry purely so a later Disconnect/Destroy can
        // still release the shared-channel ref, so `channel_key` alone is
        // not a liveness signal).
        let channel_id = if connected {
            chans
                .get(&channel_key)
                .and_then(|sc| (sc.ref_count == 1).then_some(sc.channel_id))
        } else {
            None
        };
        let Some(channel_id) = channel_id else {
            // Either the channel is gone (race with a concurrent teardown),
            // it is shared with another CLL (`ref_count > 1`), or the link
            // is not currently connected -- skipped as a no-op in every
            // case, see this method's own doc comment.
            return Ok(());
        };

        let api = self.api.lock().await;
        let result = if high_speed {
            api.sw_can_hs(channel_id)
        } else {
            api.sw_can_ns(channel_id)
        };
        drop(api);
        drop(chans);
        result.map_err(|err| {
            map_native_error_for_link(
                if high_speed {
                    "PassThruIoctl SW_CAN_HS"
                } else {
                    "PassThruIoctl SW_CAN_NS"
                },
                &err,
                last_error,
            )
        })
    }

    /// **PDU_IOCTL_SET_POLL_RESPONSE (L, ADR-189/Phase 8).** SAE J2534-2
    /// clause 11's SET_POLL_RESPONSE command: defines the bytes of the
    /// poll-response message the interface should transmit once bus
    /// mastership is granted, via `PassThruIoctl(SET_POLL_RESPONSE)` -- no
    /// output. Input is Table 28's `PollResponseMsg[100]` (at most 100
    /// bytes) in `bytearray_data`; this handler does not pre-validate the
    /// length itself (a thin passthrough, ADR-189 Context/Decision item 4 --
    /// the native call is the authoritative check, matching every other
    /// IOCTL handler in this file). Rejected (`PDU_ERR_ID_NOT_SUPPORTED`,
    /// matching `PDU_IOCTL_SW_CAN_HS`'s own not-supported style) when this
    /// CLL's link is not a GM_UART_PS/GM_UART_CHx link. Gated on
    /// `SharedChannel::ref_count == 1`, the same shared-physical-channel
    /// precaution [`Self::ioctl_sw_can_mode`] applies -- a poll-response
    /// definition affects the whole physical GM UART bus, so a per-CLL call
    /// would otherwise silently affect a sibling CLL sharing the same
    /// channel.
    ///
    /// Codex review finding (P2, PR #98, closed alongside
    /// [`Self::ioctl_become_master`]'s own in-flight fix): a shared channel
    /// used to be a silent `Ok(())` no-op here, which lied to the caller --
    /// it looks exactly like a successful SET_POLL_RESPONSE, but nothing was
    /// ever sent to the adapter. Now rejected with
    /// `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL`/`ResourceExhausted`, the same shape
    /// a sibling CLL's own join attempt gets from `rpc_link.rs`'s
    /// existing-channel-join branches. This IOCTL's own native call is
    /// fast/non-blocking (unlike `BECOME_MASTER`'s ~2s worst case), so no
    /// `become_master_in_flight`-style reservation flag is needed here --
    /// the plain `ref_count == 1` check, now rejecting instead of silently
    /// succeeding, is sufficient on its own.
    ///
    /// Round 4 (Codex finding, PR #98): `shared_channels` is now acquired
    /// BEFORE calling [`Self::require_gm_uart_link`] (passed in as that
    /// function's proof parameter), so `channel_key` resolution and the
    /// `ref_count == 1` check below are one unbroken critical section
    /// instead of two separate ones with a gap in between. Previously the
    /// snapshot taken by `require_gm_uart_link` could go stale in that gap:
    /// `DisconnectComLogicalLink` (which itself requires `shared_channels`,
    /// held first per ADR-080) could clear this CLL's own `channel_key`
    /// between the snapshot and the `ref_count` check, leaving a
    /// now-disconnected CLL's stale `channel_key` still resolving to a
    /// sibling CLL's now-sole-owned `SharedChannel` entry -- wrongly firing
    /// SET_POLL_RESPONSE on a channel this CLL no longer has any claim to.
    async fn ioctl_set_poll_response(
        &self,
        cll_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<(), Status> {
        let poll_response = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::BytearrayData(io_bytearray)) => {
                io_bytearray.data
            }
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_SET_POLL_RESPONSE requires bytearray_data (IOBytearray) input_data",
                ));
            }
        };
        let chans = self.shared_channels.lock().await;
        let (channel_key, last_error) = self
            .require_gm_uart_link(&chans, cll_handle, "PDU_IOCTL_SET_POLL_RESPONSE")
            .await?;
        let Some(channel_key) = channel_key else {
            return Ok(());
        };
        let channel_id = match chans.get(&channel_key) {
            Some(sc) if sc.ref_count == 1 => sc.channel_id,
            Some(_) => {
                return Err(gm_uart_shared_channel_locked_status(
                    "PDU_IOCTL_SET_POLL_RESPONSE",
                    "this physical channel is shared with another ComLogicalLink (ref_count > 1)",
                    last_error,
                ));
            }
            None => return Ok(()),
        };
        let api = self.api.lock().await;
        let result = api.set_poll_response(channel_id, &poll_response);
        drop(api);
        drop(chans);
        result.map_err(|err| {
            map_native_error_for_link("PassThruIoctl SET_POLL_RESPONSE", &err, last_error)
        })
    }

    /// **PDU_IOCTL_BECOME_MASTER (L, ADR-189/Phase 8).** SAE J2534-2 clause
    /// 11's BECOME_MASTER command: requests bus mastership via
    /// `PassThruIoctl(BECOME_MASTER)` -- a single blocking native call with
    /// an ~2 second native-side timeout (clause 11.3.3.2), not a state
    /// machine this service drives (ADR-189 Decision item 4/Consequences).
    /// Input is Table 32's single `Poll_ID` byte in `unum32_value`
    /// (0..=255); no output. Rejected the same way
    /// [`Self::ioctl_set_poll_response`] is when this CLL's link is not a
    /// GM_UART_PS/GM_UART_CHx link, and gated on the same
    /// `SharedChannel::ref_count == 1` shared-physical-channel precaution.
    /// `ERR_FAILED` (the documented "no poll message within 2s" outcome) is
    /// left to `map_native_error_for_link`'s existing generic mapping -- no
    /// protocol-specific error-code handling, per ADR-189.
    ///
    /// Codex review finding (P2, PR #98): unlike every other native call in
    /// this service, whose duration is near-instant, this one has a
    /// documented multi-second worst case -- issuing it the ordinary way
    /// (`self.api.lock().await`, held across the call) would tie up a tokio
    /// worker thread for up to ~2s AND (since `shared_channels` was still
    /// held too) block every other RPC needing either lock, including
    /// unrelated channels' connects/disconnects. Fixed by resolving
    /// `channel_id` and releasing `shared_channels` first, then running the
    /// native call itself inside `spawn_blocking` (off the async runtime's
    /// worker pool), acquiring `self.api` via `Mutex::blocking_lock` from
    /// within that blocking closure rather than holding an async guard
    /// across it -- the first native call in this codebase needing this
    /// treatment (see ADR-189's own Consequences for why the pre-existing
    /// global `self.api` mutex itself still serializes this call against
    /// every other native call for its own ~2s duration, same as it always
    /// has for every native call; only the artificially-added extra cost
    /// this function itself introduced -- the worker-thread block and the
    /// unnecessary `shared_channels` hold -- is what this fix removes).
    ///
    /// design-advisor consult (Codex P2 finding, PR #98 round 2): the
    /// gate-then-release-then-call shape above still left a window between
    /// releasing `shared_channels` and the native call actually running,
    /// during which a sibling CLL's `ConnectComLogicalLink` could join this
    /// SAME channel (bumping `ref_count` under its own, separate
    /// `shared_channels` acquisition, `rpc_link.rs`, which never touches
    /// `self.api`) while the mastership bid was still outstanding --
    /// defeating the sole-owner precaution the gate exists for. Closed by
    /// `SharedChannel::become_master_in_flight`: set under the SAME
    /// `shared_channels` critical section as the `ref_count == 1` gate
    /// itself (so the two are checked and armed atomically), cleared by an
    /// RAII guard ([`BecomeMasterInFlightGuard`]) constructed as the first
    /// statement inside the `spawn_blocking` closure -- not in this async
    /// fn -- so it clears on normal return, on panic/unwind, and even if
    /// this RPC's own future is later dropped/cancelled by the caller
    /// before `spawn_blocking` resolves. Both join branches
    /// (`rpc_connect_com_logical_link`; `ensure_uudt_companion_channel` is
    /// confirmed unreachable for a GM_UART_PS channel, see that function's
    /// own `channel_key` -- always CAN-keyed) reject a join while the flag
    /// is set, the same shape as the pre-existing `dead`-channel rejection.
    /// A channel already shared (`ref_count != 1`) OR already carrying an
    /// in-flight bid is rejected here too (`PDU_ERR_RSC_LOCKED_BY_OTHER_CLL`/
    /// `ResourceExhausted`) instead of the previous silent `Ok(())` no-op,
    /// which lied to the caller about having attempted the IOCTL at all.
    ///
    /// Round 4 (Codex finding, PR #98): `shared_channels` is now acquired
    /// BEFORE calling [`Self::require_gm_uart_link`] (passed in as that
    /// function's proof parameter), so `channel_key` resolution and the
    /// `ref_count == 1`/`become_master_in_flight` gate+arm above are one
    /// unbroken critical section instead of two separate ones with a gap in
    /// between. Previously the snapshot `require_gm_uart_link` returned
    /// could go stale in that gap: `DisconnectComLogicalLink` (which itself
    /// requires `shared_channels`, held first per ADR-080) could clear this
    /// CLL's own `channel_key` between the snapshot and the gate check,
    /// leaving a now-disconnected CLL's stale `channel_key` still resolving
    /// to a sibling CLL's now-sole-owned `SharedChannel` entry -- wrongly
    /// firing BECOME_MASTER on a channel this CLL no longer has any claim
    /// to. This closes cleanly with `become_master_in_flight`'s own arm
    /// step above unchanged: both now happen under the same lock as the
    /// `channel_key` resolution, atomically.
    async fn ioctl_become_master(
        &self,
        cll_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<(), Status> {
        let poll_id = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::Unum32Value(v)) if v <= 0xFF => v as u8,
            Some(vci_service_interface::data_item::Data::Unum32Value(_)) => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_BECOME_MASTER's Poll_ID input_data must fit in a single byte \
                     (0..=255)",
                ));
            }
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_BECOME_MASTER requires PDU_IT_IO_UNUM32 input_data",
                ));
            }
        };
        // Round 4 (Codex finding, PR #98): `shared_channels` is now
        // acquired FIRST, before `require_gm_uart_link` resolves this CLL's
        // `channel_key` -- so that resolution and the `ref_count == 1`/
        // `become_master_in_flight` gate+arm below are one unbroken
        // critical section instead of two separate ones with a gap in
        // between. Previously the snapshot `require_gm_uart_link` returned
        // could go stale in that gap: `DisconnectComLogicalLink` (which
        // itself requires `shared_channels`, held first per ADR-080) could
        // clear this CLL's own `channel_key` between the snapshot and the
        // gate check, leaving a now-disconnected CLL's stale `channel_key`
        // still resolving to a sibling CLL's now-sole-owned `SharedChannel`
        // entry -- wrongly firing BECOME_MASTER on a channel this CLL no
        // longer has any claim to.
        let (channel_id, in_flight_flag, last_error) = {
            let chans = self.shared_channels.lock().await;
            let (channel_key, last_error) = self
                .require_gm_uart_link(&chans, cll_handle, "PDU_IOCTL_BECOME_MASTER")
                .await?;
            let Some(channel_key) = channel_key else {
                return Ok(());
            };
            match chans.get(&channel_key) {
                Some(sc)
                    if sc.ref_count == 1 && !sc.become_master_in_flight.load(Ordering::SeqCst) =>
                {
                    sc.become_master_in_flight.store(true, Ordering::SeqCst);
                    (
                        sc.channel_id,
                        Arc::clone(&sc.become_master_in_flight),
                        last_error,
                    )
                }
                Some(_) => {
                    return Err(gm_uart_shared_channel_locked_status(
                        "PDU_IOCTL_BECOME_MASTER",
                        "this physical channel is either shared with another ComLogicalLink \
                         (ref_count > 1) or already has a BECOME_MASTER bid in flight -- \
                         BECOME_MASTER is bounded to ~2s per SAE J2534-2 clause 11.3.3.2 and \
                         retryable once it completes",
                        last_error,
                    ));
                }
                None => return Ok(()),
            }
        };
        let api = self.api.clone();
        let result = tokio::task::spawn_blocking(move || {
            // Constructed FIRST, before anything else in this blocking
            // closure: guarantees `become_master_in_flight` clears on every
            // exit path (normal return, native-call panic/unwind, or the
            // closure simply never being cancelled once spawn_blocking has
            // started it -- unlike an async-side guard, which tonic RPC
            // cancellation could drop before this closure even runs).
            let _guard = BecomeMasterInFlightGuard(in_flight_flag);
            let api = api.blocking_lock();
            api.become_master(channel_id, poll_id)
        })
        .await
        .map_err(|err| {
            Status::internal(format!(
                "PDU_IOCTL_BECOME_MASTER's blocking native call panicked: {err}"
            ))
        })?;
        result.map_err(|err| {
            map_native_error_for_link("PassThruIoctl BECOME_MASTER", &err, last_error)
        })
    }

    /// **PDU_IOCTL_GET_NDIS_ADAPTER_INFO (L, ADR-194/Phase 16).** SAE
    /// J2534-2 clause 24's GET_NDIS_ADAPTER_INFO command: reads adapter
    /// identity/status via `PassThruIoctl(GET_NDIS_ADAPTER_INFO)` -- a
    /// single non-blocking native call, no input, channel-scoped, the same
    /// thin-forwarder shape [`Self::ioctl_sw_can_mode`]/
    /// [`Self::ioctl_become_master`]/the `*_REPEAT_MESSAGE` family use.
    /// Rejected (`PDU_ERR_ID_NOT_SUPPORTED`) when this CLL's link is not an
    /// Ethernet_NDIS link, and (`PDU_ERR_CLL_NOT_CONNECTED`) when this CLL
    /// has no live channel yet -- unlike the fire-and-forget IOCTLs above,
    /// this one returns real data, so (unlike e.g. `ioctl_sw_can_mode`'s
    /// "not yet connected, no-op" precedent) there is no sensible response
    /// to fabricate for an unconnected CLL; clause 24's own IOCTL
    /// definition requires a live `ChannelID` too. Output is hand-packed
    /// into `bytearray_data` by [`pack_ndis_adapter_info`] at the RPC
    /// dispatch call site.
    ///
    /// Codex review finding (PR #102): `shared_channels` is now acquired
    /// BEFORE `logical_links`, and the actual `channel_id` used for the
    /// native call is re-derived from `SharedChannel` under that same held
    /// guard -- not taken from the `LinkView` snapshot directly. The
    /// snapshot-then-relock shape this replaces left a window where
    /// `DisconnectComLogicalLink` (which clears this CLL's own
    /// `channel_key`/tears down the `SharedChannel` under a held
    /// `shared_channels` guard, `rpc_link.rs`) could run between the
    /// snapshot and this function's own lock acquisition, letting a stale
    /// `channel_id` reach the native call -- the same race class
    /// [`Self::ioctl_sw_can_mode`]'s own doc comment describes for
    /// `ioctl_set_poll_response`/`ioctl_become_master`. Holding
    /// `shared_channels` through the native call closes it, mirroring
    /// `ioctl_sw_can_mode`'s identical fix; unlike that IOCTL, no
    /// `ref_count == 1` exclusivity gate applies here -- reading adapter
    /// info is safe on a channel shared by multiple CLLs with matching
    /// `CP_NdisPinOption`s (ADR-194's Shared-channel join guard), since
    /// nothing about this read affects the physical channel's
    /// configuration.
    ///
    /// **Gated on `link.connected` first** (`edge-case-hunter` finding #2,
    /// PR #105 close-out backlog entry, fixed here): the `channel_key`/
    /// `chans` resolution below only proceeds when `connected` is also
    /// `true`, for the same reason `resolve_live_legacy_link`'s own doc
    /// comment above describes -- a hard-errored CLL retains `channel_key`
    /// (and its `SharedChannel` entry stays in the map, merely marked
    /// `dead`) purely so a later Disconnect/Destroy can still release the
    /// shared-channel ref, so `channel_key` alone is not a liveness signal
    /// and a disconnected/dead link falls through to the same
    /// `PDU_ERR_CLL_NOT_CONNECTED` rejection as an unconnected one.
    async fn ioctl_get_ndis_adapter_info(
        &self,
        cll_handle: u32,
    ) -> Result<j2534_0404::NdisAdapterInfo, Status> {
        let chans = self.shared_channels.lock().await;
        let (hw_protocol_id, connected, channel_key, last_error) = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            (
                link.hw_protocol_id,
                link.connected,
                link.channel_key,
                link.last_error.clone(),
            )
        };

        if hw_protocol_id != j2534_0404::PROTOCOL_ETHERNET_NDIS {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_GET_NDIS_ADAPTER_INFO requires a SAE \
                 J2534-2 clause 24 Ethernet_NDIS ComLogicalLink",
                PduError::PduErrIdNotSupported,
                last_error,
            ));
        }

        let channel_id = if connected {
            channel_key
                .and_then(|key| chans.get(&key))
                .map(|sc| sc.channel_id)
        } else {
            None
        };
        let Some(channel_id) = channel_id else {
            return Err(state_guard_status(
                Code::FailedPrecondition,
                "logical link is not connected",
                PduError::PduErrCllNotConnected,
                last_error,
            ));
        };

        let api = self.api.lock().await;
        let result = api.get_ndis_adapter_info(channel_id);
        drop(api);
        drop(chans);
        result.map_err(|err| {
            map_native_error_for_link("PassThruIoctl GET_NDIS_ADAPTER_INFO", &err, last_error)
        })
    }

    /// Shared CLL resolution + protocol gate for
    /// [`Self::ioctl_set_poll_response`]/[`Self::ioctl_become_master`]:
    /// looks up `cll_handle`'s `channel_key`/`last_error`, rejecting
    /// (`PDU_ERR_ID_NOT_SUPPORTED`) unless the link's hardware protocol id
    /// is GM_UART_PS/GM_UART_CHx ([`resources::is_gm_uart_protocol_id`]).
    /// `channel_key` is `None` when the CLL is not yet connected -- callers
    /// treat that as a no-op, matching every other L-scoped IOCTL's
    /// established "no channel, no-op" precedent.
    ///
    /// `_chans` (round 4, Codex finding, PR #98) is a proof parameter: it
    /// forces every caller to already hold `self.shared_channels` before
    /// this function resolves `cll_handle`'s `channel_key`, per ADR-080's
    /// outermost-lock rule (`shared_channels` -> `logical_links` nested,
    /// which this function's own body still does internally). Every
    /// production write to `LogicalLinkState::channel_key` (connect
    /// finalization, disconnect, destroy) happens under a held
    /// `shared_channels` guard too, so a `channel_key` read taken while
    /// `shared_channels` is already held is guaranteed current -- not a
    /// stale snapshot that a concurrent `DisconnectComLogicalLink` (which
    /// acquires `shared_channels` first, then clears the disconnecting
    /// CLL's own `channel_key` under that held guard, `rpc_link.rs`) could
    /// invalidate between this function returning and the caller's own
    /// `ref_count`/`become_master_in_flight` check. Previously this
    /// function locked and released `logical_links` on its own, returning
    /// a snapshot the caller then re-validated against a separately (and
    /// later) acquired `shared_channels` -- leaving exactly that gap: a
    /// disconnecting CLL's own stale `channel_key` could still resolve to
    /// a sibling's now-sole-owned `SharedChannel` entry, wrongly firing
    /// `SET_POLL_RESPONSE`/`BECOME_MASTER` on a channel the requesting CLL
    /// no longer has any claim to.
    ///
    /// **Gated on `link.connected` first** (`edge-case-hunter` finding #2,
    /// PR #105 close-out backlog entry, fixed here): the returned
    /// `channel_key` is folded through `connected` before being handed
    /// back, rather than duplicated in each of the two callers, since both
    /// already treat a `None` `channel_key` as their existing "not yet
    /// connected, no-op" path -- gating it here fixes both at once. See
    /// `resolve_live_legacy_link`'s own doc comment above for the fuller
    /// rationale (a hard-errored CLL retains `channel_key`, and its
    /// `SharedChannel` entry stays in the map merely marked `dead`, purely
    /// so a later Disconnect/Destroy can still release the shared-channel
    /// ref, so `channel_key` alone is not a liveness signal).
    async fn require_gm_uart_link(
        &self,
        _chans: &MutexGuard<'_, HashMap<ChannelKey, SharedChannel>>,
        cll_handle: u32,
        ioctl_name: &str,
    ) -> Result<(Option<ChannelKey>, Option<TrackedError>), Status> {
        let (hw_protocol_id, connected, channel_key, last_error) = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            (
                link.hw_protocol_id,
                link.connected,
                link.channel_key,
                link.last_error.clone(),
            )
        };

        if !resources::is_gm_uart_protocol_id(hw_protocol_id) {
            return Err(state_guard_status(
                Code::Unimplemented,
                format!(
                    "PDU_ERR_ID_NOT_SUPPORTED: {ioctl_name} requires a SAE J2534-2 clause 11 GM \
                     UART (GM_UART_PS/GM_UART_CHx) ComLogicalLink"
                ),
                PduError::PduErrIdNotSupported,
                last_error,
            ));
        }

        let channel_key = if connected { channel_key } else { None };

        Ok((channel_key, last_error))
    }

    /// SAE J2534-2 clause 14 Repeat Messaging opportunistic-retry point
    /// (ADR-165 Decision 6, Codex review PR #42 round 2 Finding A): retries
    /// STOP for every `MsgId` `channel_key`'s `SharedChannel::
    /// leaked_repeat_message_ids` still carries from an earlier sibling
    /// CLL's teardown that could not stop it while the channel stayed open
    /// (see that field's doc comment). Called from the top of every
    /// START/QUERY/STOP_REPEAT_MESSAGE handler, with `shared_channels`
    /// already locked by the caller across its own native call -- passing
    /// the same guard here means this cannot race a concurrent leak (a
    /// teardown) or another retry for the same channel. Best effort and
    /// self-pruning: a `MsgId` that still fails to stop is left in the list
    /// for the next opportunity; this never surfaces an error to the caller,
    /// since a slow/stuck vendor DLL retrying someone else's stale slot must
    /// not block an unrelated repeat-message IOCTL.
    ///
    /// As of ADR-180 Decision 20, `leaked_repeat_message_ids` entries can
    /// also arrive from a still-live CLL's own relinquishment-triggered
    /// `stop_repeat_slots_for_cll` STOP failure (`events_j1939_claim.rs`'s
    /// `push_leaked_repeat_slots`/`record_leaked_repeat_slots`), not only
    /// from CLL teardown/hard-error -- this retry point picks those up
    /// exactly the same way, with no changes of its own needed.
    async fn retry_leaked_repeat_message_stops(
        &self,
        chans: &mut MutexGuard<'_, HashMap<ChannelKey, SharedChannel>>,
        channel_key: ChannelKey,
    ) {
        let Some((channel_id, pending)) = chans.get(&channel_key).and_then(|sc| {
            if sc.leaked_repeat_message_ids.is_empty() {
                None
            } else {
                Some((sc.channel_id, sc.leaked_repeat_message_ids.clone()))
            }
        }) else {
            return;
        };
        let api = self.api.lock().await;
        let mut still_leaked = Vec::new();
        for msg_id in pending {
            if let Err(err) = api.stop_repeat_message(channel_id, msg_id) {
                // Finding F (Codex review, ADR-165 PR #42 round 3):
                // ERR_INVALID_MSG_ID specifically means the device has
                // already forgotten this slot (e.g. a `Condition == 1` slot
                // that self-completed while sitting in the leaked list) --
                // that is cleanup-already-complete, not still-leaked. Only a
                // non-ERR_INVALID_MSG_ID failure is genuinely "still can't
                // stop it, retry later"; treating ERR_INVALID_MSG_ID the
                // same way would grow this list unboundedly on a busy
                // channel and risk the same recycled-MsgId collision as
                // Finding E if the device later reassigns the numeric ID to
                // an unrelated slot.
                let is_invalid_msg_id = matches!(
                    &err,
                    j2534_0404::Error::ApiStatus { code, .. }
                        if code.as_u32() == j2534_0404::ERR_INVALID_MSG_ID
                );
                if is_invalid_msg_id {
                    debug!(
                        channel_id = channel_id.0,
                        msg_id,
                        "leaked repeat-message STOP retry got ERR_INVALID_MSG_ID -- device \
                         already forgot this slot, treating as cleaned up"
                    );
                } else {
                    debug!(
                        channel_id = channel_id.0,
                        msg_id,
                        %err,
                        "opportunistic retry of a leaked repeat-message STOP failed, left tracked"
                    );
                    still_leaked.push(msg_id);
                }
            }
        }
        drop(api);
        if let Some(sc) = chans.get_mut(&channel_key) {
            sc.leaked_repeat_message_ids = still_leaked;
        }
    }

    /// **PDU_IOCTL_START_REPEAT_MESSAGE (L, ADR-165/Phase 12).** SAE J2534-2
    /// clause 14's START command: begins autonomous device-side
    /// retransmission of a client-supplied repeat message via
    /// `PassThruIoctl(START_REPEAT_MESSAGE)`. A thin forwarder (this
    /// adapter's core ADR-165 Decision) -- the device owns the
    /// retransmission timing and mask/pattern evaluation; this service only
    /// composes the native `REPEAT_MSG_SETUP` and tracks the returned
    /// `MsgId` for this CLL's own `QUERY`/`STOP`/teardown scoping
    /// (`LogicalLinkState::repeat_message_ids`).
    ///
    /// Requires this CLL connected (`PDU_ERR_CLL_NOT_CONNECTED` otherwise,
    /// matching `rpc_io_ctl_legacy`'s own gate) and the connecting module
    /// opted into SAE J2534-2 (clause 5) -- clause 14 applies channel-wide,
    /// not per-protocol, so (unlike `PDU_IOCTL_SW_CAN_HS`/`_NS`, which lean
    /// on `is_sw_family_protocol_id`'s implicit connect-time gate) there is no
    /// protocol-family check to piggyback on; checked directly against the
    /// open device's `pname`, mirroring `apply_fd_mode`'s identical gate
    /// (`rpc_link.rs`).
    ///
    /// `input_data` must carry `bytearray_data` (`IOBytearray`), a
    /// hand-packed byte payload decoded by `unpack_repeat_message_setup`
    /// (ADR-178; the removed `IORepeatMessageSetup` proto message's field
    /// shape lives on unchanged as this function's local `RepeatMessageSetup`
    /// struct). `mask_data`/`pattern_data` must be equal
    /// length (Codex-review Finding 3, PR #42) -- `RepeatMsgData[1]`/`[2]` is
    /// a matched mask/pattern pair, so a mismatch is rejected up front. The
    /// repeat message's own D-PDU payload (`repeat_msg_data`) is composed
    /// into a full native `PASSTHRU_MSG` (`RepeatMsgData[0]`) the same way an
    /// ordinary `CoptSendrecv` TX is (`tx_header::build_tx_message`,
    /// ADR-050/ADR-165 Decision 3) -- header/ID bytes are never
    /// client-supplied. The client's `mask_data`/`pattern_data`
    /// (payload-scoped, ADR-051) are widened with this CLL's own
    /// expected-response header/ID bytes prepended
    /// (`tx_header::response_header_bytes`, whose own returned mask is
    /// all-ones over those header positions except a KWP/ISO14230/ISO9141
    /// separate-length-byte position, wildcarded -- Codex-review Finding B,
    /// PR #42 round 2) before being placed in `RepeatMsgData[1]`/`[2]`,
    /// scoping the repeat's device-side stop condition to the addressed
    /// ECU's own responses (ADR-165 Decision 3). `response_header_bytes`
    /// also returns the RESPONSE-side `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE`
    /// flags for `RepeatMsgData[1]`/`[2]`'s own `TxFlags` (Codex-review
    /// Finding 2, PR #42) -- these can differ from `RepeatMsgData[0]`'s
    /// request-side `addressing_tx_flags` whenever the request and response
    /// use different CAN ID widths or addressing modes.
    ///
    /// Holds `shared_channels` locked (edge-case-hunter fix, Bug 1) from
    /// before the `logical_links` read all the way through the native
    /// `start_repeat_message` call and the final `repeat_message_ids` push,
    /// exactly the pattern `ioctl_start_msg_filter` already uses for the
    /// identical "resolve CLL state -> drop lock -> native call -> re-acquire
    /// to record" shape. Without this, a concurrent
    /// `DestroyComLogicalLink`/`DisconnectComLogicalLink` on the same
    /// `cll_handle` could remove the CLL from `logical_links` while the
    /// native call is in flight, so the returned `MsgId` would find no CLL to
    /// record it against when this function re-acquired the lock -- silently
    /// orphaning a live, unstoppable device-side repeat slot (its owning
    /// CLL's best-effort teardown loop already ran over an empty
    /// `repeat_message_ids`, and no other CLL is ever tracked as owning it).
    /// Serializing on `shared_channels` first closes this: both
    /// `DestroyComLogicalLink` and `DisconnectComLogicalLink` acquire this
    /// same lock before removing/disconnecting the CLL and hold it through
    /// their own repeat-slot teardown loop (`rpc_link.rs`, ADR-080's
    /// "shared_channels is outermost" convention), so the two can never
    /// interleave. `logical_links`/`api` are acquired and released
    /// underneath it, never before it, matching this crate's lock hierarchy.
    ///
    /// Rejects with `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL`/`ResourceExhausted`
    /// (a hard, synchronous check, mirroring `ioctl_clear_tx_queue`'s own
    /// `LOCK_PHYSICAL_TX_QUEUE` gate) before any state mutation if a sibling
    /// CLL on this physical resource holds `LOCK_PHYSICAL_TX_QUEUE`
    /// (Codex-review Finding L, PR #42 round 5) -- Repeat Messaging is a
    /// genuinely autonomous, device-driven TX stream with no
    /// "pause/resume with this service's own TX-suspend machinery" option
    /// the way an ordinary queued COP has under ADR-123. ADR-186 removed the
    /// raw CAN FD filler-padding step that used to live here (padding a
    /// `repeat_msg_data` payload up to 64 bytes was itself nonconformant
    /// with SAE J2534-1 §7.2.7's periodic-message cap, which this function
    /// now enforces before composition instead). The request-side
    /// `addressing_tx_flags` composed for the actually-TRANSMITTED
    /// `RepeatMsgData[0]` message also masks out `ISO15765_ADDR_TYPE`
    /// unless this link's protocol is actually ISO15765 (Codex-review
    /// Finding K, PR #42 round 5) -- mirrors round 3's Finding D fix in
    /// `tx_header::response_header_bytes`, applied only to this function's
    /// own inlined composition; the same gap in the shared
    /// `tx_header::can_addressing_tx_flags`/`rpc_primitive::
    /// apply_resolved_tx_flags` helper it calls is a separate, broader,
    /// pre-existing issue this fix deliberately does not touch (see
    /// PR #42 round 5's Finding K discussion).
    async fn ioctl_start_repeat_message(
        &self,
        cll_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<u32, Status> {
        let setup = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::BytearrayData(io_bytearray)) => {
                unpack_repeat_message_setup(&io_bytearray.data)?
            }
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_START_REPEAT_MESSAGE requires bytearray_data (IOBytearray) \
                     input_data",
                ));
            }
        };

        // Finding 3 (Codex review, ADR-165 PR #42; the round-8 condition-0
        // carve-out this check used to have was reversed to unconditional by
        // ADR-173 Decision 4): SAE J2534-2 clause 14's REPEAT_MSG_SETUP
        // treats RepeatMsgData[1]/[2] (mask/pattern) as a matched pair --
        // rejecting a length mismatch up front guarantees the
        // header-prepended mask_data/pattern_data this function composes
        // below are always equal length, closing the mock-side truncation
        // hazard `repeat_mask_pattern_matches` otherwise has to defend
        // against on its own (defense in depth,
        // j2534-0404-mock/src/lib.rs). Unconditional as of ADR-173: clause
        // 14.2.2.1's `Condition == 0` (`REPEAT_MESSAGE_UNTIL_MATCH`) DOES
        // evaluate mask/pattern against incoming traffic -- it is the match
        // that stops it -- so both conditions need a well-formed mask/
        // pattern pair equally; ADR-165's original gate rested on the
        // inverted paraphrase of `Condition` this ADR corrects.
        if setup.mask_data.len() != setup.pattern_data.len() {
            return Err(Status::invalid_argument(format!(
                "PDU_IOCTL_START_REPEAT_MESSAGE requires mask_data and pattern_data to be the \
                 same length (got {} and {} bytes)",
                setup.mask_data.len(),
                setup.pattern_data.len()
            )));
        }

        // ADR-185 Stage 2 / lock-order fix: `device_id` must be the outermost
        // lock (`service.rs`'s `require_connected_device_for` doc comment, ADR-107
        // addendum) -- held here, BEFORE `shared_channels` is acquired below, so
        // the Discovery capacity precheck further down can resolve via
        // `DeviceAccess::AlreadyOpen` instead of `OpenIfNeeded` (which would
        // re-lock `device_id` while `shared_channels` is already held, inverting
        // the documented order and risking an AB-BA deadlock against
        // `ConnectComLogicalLink`'s own device_id-outer/shared_channels-inner
        // order). Held through the Discovery check and dropped right after (see
        // below) -- nothing downstream in this function touches `device_id`.
        let device_guard = self.device_id.lock().await;
        let open_device = *device_guard;
        let opted_in = open_device.is_some_and(|(module_handle, _)| {
            discovery::is_j2534_2_opted_in(
                self.modules[(module_handle - 1) as usize].pname.as_deref(),
            )
        });

        // See this function's doc comment: `shared_channels` is acquired
        // before `logical_links` and held through the native call and the
        // final `repeat_message_ids` write below (Bug 1 fix).
        let mut chans = self.shared_channels.lock().await;
        let (
            channel_id,
            channel_key,
            hw_protocol_id,
            pin_select,
            active,
            entries,
            last_error,
            software_isotp,
            tp20_established_tx_id,
            tp20_established_rx_id,
            raw_mode,
            checksum_mode,
        ) = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            if !opted_in {
                return Err(state_guard_status(
                    Code::InvalidArgument,
                    "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_START_REPEAT_MESSAGE is a SAE J2534-2 \
                     clause 14 feature -- this module has not opted into J2534-2 (its pname \
                     lacks the \"J2534-2:\" prefix, clause 5)",
                    PduError::PduErrIdNotSupported,
                    link.last_error.clone(),
                ));
            }
            let channel_id = link.channel_id.ok_or_else(|| {
                state_guard_status(
                    Code::FailedPrecondition,
                    "logical link is not connected",
                    PduError::PduErrCllNotConnected,
                    link.last_error.clone(),
                )
            })?;
            // ADR-180 Decision 13 (design-advisor consult, PR #72 round 11):
            // SAE J2534-2 clause 14's device-autonomous retransmission model
            // freezes a repeat slot's source-address byte at START time
            // (`tx_header::build_tx_message`'s J1939 arm reads Active
            // `NODE_ADDRESS`, only meaningful once a claim has resolved) and
            // never revisits it -- this function never checked J1939 claim
            // state at all, so a START during ANY unclaimed window (before
            // this CLL's first successful claim, during a pending reclaim
            // after a spontaneous loss, after a claim attempt's candidate
            // list is exhausted) created a permanent slot transmitting under
            // a source address this CLL does not own (the `0xF1` pre-claim
            // default, or a just-relinquished address). Scoped to a
            // negotiation-ENABLED CLL only (`j1939_claim_requested`,
            // `CP_J1939AddressNegotiationRule` bit 1 clear): a non-negotiated
            // CLL (bit 1 set) never runs the claim loop at all and
            // `j1939_claimed_address` stays `None` for its entire life by
            // design, with `CP_TesterSourceAddress` client-managed instead
            // (the same distinction ADR-180 Decision 12's own
            // `CoptUpdateparam` guard already draws) -- gating on
            // `j1939_claimed_address` alone would permanently and wrongly
            // block Repeat Messaging on every such CLL. This is a genuine
            // client-visible behavior change: a START on a negotiation-
            // enabled J1939 CLL that never successfully claimed an address
            // previously succeeded silently (composing under the `0xF1`
            // default); it is now rejected, matching SAE J1939's own
            // claim-before-transmit precondition (clause 16 -- see the
            // native `ERR_ADDRESS_NOT_CLAIMED` this same precondition
            // already surfaces elsewhere in this file for other J1939 sends).
            //
            // Refactored (ADR-180 Decisions 14/15/16, PR #72 round 12) to
            // call the shared `j1939_negotiated_unclaimed` predicate --
            // behavior-neutral, the identical three-way `AND` this gate
            // always used, now factored out so Findings 1 and 3's own
            // gates share it instead of re-deriving the condition inline.
            if events::j1939_negotiated_unclaimed(link) {
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "PDU_IOCTL_START_REPEAT_MESSAGE requires this SAE J1939 ComLogicalLink to \
                     have successfully claimed a source address first \
                     (CP_J1939AddressNegotiationRule requests negotiation, ADR-179 Decision 3) \
                     -- issue and complete a successful CoptStartcomm, or wait for a spontaneous \
                     reclaim to finish, before starting a repeat slot",
                    PduError::PduErrCllNotStarted,
                    link.last_error.clone(),
                ));
            }
            // Codex review fix (P2, PR #101, round 9, ADR-192/Phase 7 Stage
            // 7c): SAE J2534-2 clause 14 Repeat Messaging and clause
            // 19.3.2.2/19.3.2.3's TP2.0 broadcast mechanisms are two
            // unrelated features this service never designed to compose --
            // `tx_header::build_tx_message`'s TP2.0 arm unconditionally
            // composes `[address] ++ payload` broadcast framing whenever
            // `CP_TP20BroadcastAddress` is staged in Active, regardless of
            // caller intent, but this function's own TxFlags composition
            // (below) has no broadcast awareness at all and never adds
            // `TX_FLAG_TP2_0_BROADCAST_MSG` -- so a repeat slot started while
            // a broadcast address happens to be staged (a client can set
            // `CP_TP20BroadcastAddress` via plain `SetComParam`, entirely
            // independent of ever issuing a broadcast COP) would compose a
            // broadcast-address-prefixed frame but transmit it with ordinary
            // connection-bound flags, which a real adapter will reject or
            // misinterpret. Rejected explicitly here, in the same critical
            // section as the J1939 claim check above, before any state
            // mutation -- mirroring `validate_tp20_broadcast_address_range`'s
            // own "explicitly reject an unsupported combination" precedent in
            // `rpc_primitive.rs`, rather than expanding Repeat Messaging to
            // also support broadcast composition (a new feature, out of
            // scope here).
            //
            // Codex review round 19 (P2, PR #101): this must check the RAW
            // staged `CP_TP20BroadcastAddress` value, not the normalized
            // `tp20_broadcast_address()` accessor -- that accessor's own doc
            // comment (`service.rs`) says plainly that it folds an
            // out-of-range nonzero value to `None`, on the documented
            // assumption (ADR-192 Decision item 1) that the RPC layer
            // (`rpc_primitive.rs`'s `validate_tp20_broadcast_address_range`)
            // already rejected any such value before this accessor is ever
            // consulted. That assumption does not hold for Repeat Messaging:
            // `SetComParam`/`CoptUpdateparam` can stage an out-of-range
            // nonzero value into Active without ever routing through a COP
            // send path, so consulting the normalized accessor here let an
            // invalid staged address (e.g. `0x01`) silently resolve `None`
            // and slip past this guard entirely -- unlike the COP send
            // paths, Repeat Messaging has no legitimate broadcast use for
            // ANY nonzero staged address (valid range or not), so the raw
            // value is checked directly against zero, not range-validated.
            let raw_broadcast_address = link
                .active
                .unum32
                .get(&PARAM_TP20_BROADCAST_ADDRESS)
                .copied()
                .unwrap_or(0);
            // ADR-210 Decision item 10: re-keyed from the narrow
            // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id` --
            // `link.hw_protocol_id` is a live link's raw id, so a
            // `_CHx`-connected TP2.0 link would otherwise silently skip this
            // conditional Repeat Messaging rejection.
            if resources::is_tp2_0_family_protocol_id(link.hw_protocol_id)
                && raw_broadcast_address != 0
            {
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "PDU_IOCTL_START_REPEAT_MESSAGE is not supported on a TP2.0 ComLogicalLink \
                     while CP_TP20BroadcastAddress is staged in Active -- Repeat Messaging and \
                     TP2.0 broadcast framing (SAE J2534-2 clause 19.3.2.2/19.3.2.3) are not a \
                     supported combination; clear CP_TP20BroadcastAddress first",
                    PduError::PduErrIdNotSupported,
                    link.last_error.clone(),
                ));
            }
            // Codex review fix (PR #97, ADR-188): the same
            // `logical_links`-tracked-phase discipline `rpc_primitive.rs`'s
            // shared CoptSendrecv/CoptStartcomm/CoptStopcomm snapshot block
            // uses -- captured here, under this SAME critical section, not
            // from a client-writable ComParam.
            let tp20_established_tx_id = link.tp20_connection.and_then(|c| {
                (c.phase == Tp20ConnectionPhase::Established)
                    .then_some(c.established_tx_id)
                    .flatten()
            });
            // Codex review fix (PR #97, ADR-188, Fix H): the RX-ID
            // counterpart of `tp20_established_tx_id` just above, resolved
            // the identical way -- from this CLL's real, `logical_links`-
            // tracked `tp20_connection` phase, never a ComParam -- for
            // `response_header_bytes`'s new PROTOCOL_TP2_0_PS arm below.
            let tp20_established_rx_id = link
                .tp20_connection
                .filter(|c| c.phase == Tp20ConnectionPhase::Established)
                .map(|c| c.requested_rx_id);
            (
                channel_id,
                link.channel_key,
                link.hw_protocol_id,
                link.pin_select,
                link.active.clone(),
                link.active_unique_resp_id_table.clone(),
                link.last_error.clone(),
                link.software_isotp,
                tp20_established_tx_id,
                tp20_established_rx_id,
                link.raw_mode,
                link.checksum_mode,
            )
        };

        // Finding H (Codex review, ADR-165 PR #42 round 4): SAE J2534-2
        // clause 14's whole design is the device autonomously retransmitting
        // one fixed native frame with no software involvement per
        // retransmission, but software-ISO-TP mode (`can_channel_mode =
        // "software-isotp"`, ADR-046) requires THIS SERVICE to actively drive
        // segmentation and flow-control handshaking for every transmission
        // (`rpc_primitive::SoftIsoTpTx`/`resolve_send_recv_tx`) -- there is no
        // way for the device to autonomously replay a multi-frame ISO-TP
        // exchange it has no awareness of. This is the same fundamental
        // incompatibility already rejected for FD + software-ISO-TP at
        // CONNECT time (`fd_can.rs`'s
        // `fd_mode_rejected_when_link_is_software_isotp`, `rpc_primitive.rs`'s
        // `!software_isotp` guards around line 209-224) -- unlike that case,
        // there is no reason to reject software-ISO-TP itself at connect
        // time, only Repeat Messaging on such a link, so the rejection lives
        // here at START time instead. Without this guard,
        // `hw_protocol_id`/`build_tx_message` below would silently resolve to
        // the underlying raw CAN channel and compose a raw-CAN frame with no
        // ISO-TP PCI/Single-Frame header -- malformed traffic, not merely
        // unimplemented framing.
        if software_isotp {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_START_REPEAT_MESSAGE is not supported on a \
                 software-ISO-TP link (can_channel_mode = \"software-isotp\", ADR-046) -- SAE \
                 J2534-2 clause 14's device-autonomous retransmission model has no way to drive \
                 the per-retransmission ISO-TP segmentation/flow-control this link mode requires, \
                 so this link mode cannot support Repeat Messaging at all, not just \"not yet \
                 implemented\"",
                PduError::PduErrIdNotSupported,
                last_error,
            ));
        }

        // ADR-170/Phase 9: SAE J2534-2 clause 12 UART Echo Byte Protocol
        // explicitly excludes Repeat Messaging -- the interface cannot
        // autonomously maintain the clause 12.4.2 message-counter/echo-byte
        // handshake state a repeat retransmission would need to replay,
        // mirroring the software-ISO-TP rejection just above (a device that
        // can autonomously retransmit one fixed frame has no way to drive
        // per-retransmission protocol-level state either mode requires).
        // Unlike the software-ISO-TP case, this is closed at all three of
        // START/QUERY/STOP (`Self::require_owned_repeat_message`'s own check
        // covers QUERY/STOP) -- clause 12.3.3.1's exclusion is unconditional,
        // not scoped to the initiating call alone. ADR-207 Decision item 10:
        // checked via the range-inclusive `is_uart_echo_byte_family_protocol_id`,
        // not the arm-gate-only `is_uart_echo_byte_protocol_id`, so a `_CHx`
        // Additional Channels link (ADR-207) is excluded the same way its
        // `_PS` sibling is -- the clause 12.4.2 handshake state a device
        // cannot autonomously replay is unavailable on a `_CHx` link too.
        if resources::is_uart_echo_byte_family_protocol_id(hw_protocol_id) {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_START_REPEAT_MESSAGE is not supported on a \
                 SAE J2534-2 clause 12 UART Echo Byte Protocol link -- the interface cannot \
                 autonomously maintain the clause 12.4.2 message-counter/echo-byte handshake a \
                 repeat retransmission would require",
                PduError::PduErrIdNotSupported,
                last_error,
            ));
        }

        // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15, Codex
        // review finding on PR #66): Repeat Messaging is a device-autonomous
        // TRANSMIT mechanism -- it does not route through
        // `rpc_start_com_primitive`'s own write rejection (`rpc_primitive.rs`,
        // gated on `transmits`), since this is a separate IOCTL dispatch
        // path, not a COP. Without this check, a client could start an
        // autonomous repeat transmit on a link clause 10 defines as strictly
        // read-only -- the same class of gap the write/filter rejections
        // above exist to close, just reached through a different call site.
        if resources::is_analog_in_protocol_id(hw_protocol_id) {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_START_REPEAT_MESSAGE is not supported on a \
                 SAE J2534-2 clause 10 Analog Input link -- clause 10 defines this protocol as \
                 read-only; only PassThruReadMsgs is valid",
                PduError::PduErrIdNotSupported,
                last_error,
            ));
        }

        // ADR-194/Phase 16 (Codex review, PR #102): SAE J2534-2 clause 24
        // Ethernet_NDIS rejects every message-transmit path -- ordinary
        // writes (`PassThruWriteMsgs`) and every COP type
        // (`rpc_start_com_primitive`'s gate). Repeat Messaging is a
        // device-autonomous TRANSMIT mechanism reached through this
        // separate IOCTL dispatch path, not through either of those, so it
        // needs its own exclusion here -- the same class of gap the Analog
        // Inputs check just above exists to close for its own protocol.
        // Clause 24 defines no autonomous-retransmission capability for an
        // NDIS adapter binding at all; closed at START only, same as the
        // UART Echo Byte/Analog Inputs checks above, since a slot can never
        // exist to reach QUERY/STOP if START always rejects it here.
        if hw_protocol_id == j2534_0404::PROTOCOL_ETHERNET_NDIS {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_START_REPEAT_MESSAGE is not supported on a \
                 SAE J2534-2 clause 24 Ethernet_NDIS link -- clause 24 routes all Ethernet \
                 payload traffic outside the J2534 API entirely, so no autonomous device-side \
                 retransmission capability exists for this protocol",
                PduError::PduErrIdNotSupported,
                last_error,
            ));
        }

        // ADR-185 Stage 2: SAE J2534-2 clause 14/25.3.2.3 Repeat Messaging
        // Discovery-cache capacity precheck, run only once the protocol
        // itself is known to support Repeat Messaging at all (the four
        // exclusion rejections just above). `needed` is this physical
        // channel's current live repeat-message slot count, summed across
        // every sibling CLL sharing `channel_id` (ADR-165 Decision 4's
        // shared-slot-budget design -- the budget is per physical channel,
        // not per CLL), plus 1 for the slot this call is about to start.
        // `open_device` is guaranteed `Some` here: `opted_in` (derived from
        // it above) has already gated every earlier return in this
        // function. Resolved via `AlreadyOpen` against the still-held
        // `device_guard` (see this function's own comment on it, above) --
        // NOT `OpenIfNeeded`, which would re-lock `device_id` here while
        // `shared_channels` is already held, inverting this codebase's
        // documented outermost-lock order (ADR-107 addendum).
        {
            let (module_handle, device_id) =
                open_device.expect("opted_in was true, so a device must be open (see above)");
            let live_slots: u32 = {
                let links = self.logical_links.lock().await;
                links
                    .values()
                    .filter(|l| l.channel_id == Some(channel_id))
                    .map(|l| l.repeat_message_ids.len() as u32)
                    .sum()
            };
            self.enforce_discovery_capability(
                module_handle,
                discovery::DeviceAccess::AlreadyOpen(device_id),
                discovery::DiscoveryCheck::ProtocolCapacity {
                    protocol_id: hw_protocol_id,
                    parameter: j2534_0404::PROTOCOL_INFO_MAX_REPEAT_MESSAGING,
                    needed: live_slots + 1,
                },
                "PDU_IOCTL_START_REPEAT_MESSAGE",
                PduError::PduErrResourceError,
                last_error.clone(),
            )
            .await?;
        }
        // `device_id` is unused past this point in this function -- dropped
        // here rather than held through the native `start_repeat_message`
        // call and the bookkeeping after it, which don't need it (mirrors
        // `rpc_link.rs`'s identical explicit-drop precedent for its own
        // `device_guard`). Note this drop point is NOT earlier than
        // `chans`/the `enforce_discovery_capability` call above it: the
        // ADR-185 lock-order fix holds `device_guard` across `shared_channels`
        // acquisition, the `logical_links` read, and the Discovery precheck's
        // own native `GET_PROTOCOL_INFO` call (awaiting `self.api`) -- all of
        // which now run before this point, so `device_guard` is held
        // strictly longer than it was before that fix, not shorter.
        drop(device_guard);

        // Finding L (Codex review, ADR-165 PR #42 round 5, ADR-123): a
        // sibling CLL holding LOCK_PHYSICAL_TX_QUEUE on this physical
        // resource must block starting a repeat slot -- Repeat Messaging is
        // a genuinely autonomous, device-driven TX stream that never goes
        // through this service's own TX queue/COP machinery, so unlike an
        // ordinary queued COP (which simply queues and waits for the lock to
        // release, ADR-123) it has no "pause and resume later" semantics to
        // fall back on. Mirrors `ioctl_clear_tx_queue`'s own hard,
        // synchronous reject for the identical reason (see that function's
        // own comment): starting a device-autonomous retransmission is not
        // something this service can undo once the native call succeeds.
        // Checked here, before `retry_leaked_repeat_message_stops`/the
        // native call/any state mutation below -- a caller that gets
        // ResourceExhausted must see no side effects at all.
        {
            let links = self.logical_links.lock().await;
            if find_physical_lock_holder(
                &links,
                cll_handle,
                hw_protocol_id,
                pin_select,
                channel_key,
                LOCK_PHYSICAL_TX_QUEUE,
            )
            .is_some()
            {
                let holder_last_error = links.get(&cll_handle).and_then(|l| l.last_error.clone());
                return Err(state_guard_status(
                    Code::ResourceExhausted,
                    "physical TX queue lock is held by another ComLogicalLink on this resource",
                    PduError::PduErrRscLockedByOtherCll,
                    holder_last_error,
                ));
            }
        }

        // ADR-180 Decision 24's sibling gate (round-27 correction to
        // Decision 22, Codex review, PR #72; `design-advisor` consult): the
        // `events::j1939_negotiated_unclaimed` gate a few dozen lines above
        // only covers THIS CLL's own claim posture -- a CLL that opted out of
        // negotiation, or manages its own source address, sails straight
        // past it. But `SharedChannel::leaked_j1939_claims` (Decision 22) can
        // hold an address a DIFFERENT, already-torn-down sibling CLL failed
        // to natively cancel, and the leak belongs to the physical channel,
        // not to any one CLL's own negotiation posture -- without this check,
        // such a CLL could start an autonomous device-side repeat slot while
        // the adapter may still be defending a leaked address on the same
        // physical channel. Reconciles the whole channel's leaked set via the
        // same shared helper Decisions 22/23's round-23/24 corrections
        // already established (`events_j1939_claim::
        // reconcile_leaked_j1939_claims`), rather than scoping to this CLL's
        // own candidates -- `leaked_j1939_claims` carries no NAME/owner
        // attribution, so a narrower check would miss a same-NAME reconnect
        // under a different `cll_handle`, the identical reasoning those
        // corrections already recorded. `chans` is already held (this
        // function's own documented lock order, above) and `logical_links` is
        // not (it was locked-and-dropped for the `LOCK_PHYSICAL_TX_QUEUE`
        // check just above), so this runs here, after that check and before
        // the leaked-repeat-message-stop retry below. If the channel is no
        // longer found in `chans`, skip silently -- teardown already owns
        // cleanup for a physical channel that vanished mid-op, mirroring
        // `events.rs`'s own identical-shaped opt-out-branch call site.
        if let Some(sc) = chans.values_mut().find(|sc| sc.channel_id == channel_id)
            && !sc.leaked_j1939_claims.is_empty()
        {
            let api = self.api.lock().await;
            let reconciled =
                events::reconcile_leaked_j1939_claims(sc, &api, channel_id, cll_handle);
            drop(api);
            if !reconciled {
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "PDU_IOCTL_START_REPEAT_MESSAGE cannot start until a leaked SAE J1939 claim on \
                     this physical channel finishes reconciling -- a prior ComLogicalLink's \
                     relinquished claim failed to natively cancel and the adapter may still be \
                     defending that address; retry once it clears",
                    PduError::PduErrCllNotStarted,
                    last_error,
                ));
            }
        }

        // SAE J2534-2 clause 14 Repeat Messaging opportunistic retry (ADR-165
        // Decision 6, Codex review PR #42 round 2 Finding A): this CLL is
        // about to touch repeat messaging on this channel anyway, so flush
        // any MsgIds a sibling CLL's teardown left leaked on it first -- see
        // `retry_leaked_repeat_message_stops`'s doc comment.
        if let Some(channel_key) = channel_key {
            self.retry_leaked_repeat_message_stops(&mut chans, channel_key)
                .await;
        }

        let base_protocol_id = resources::base_protocol_id(hw_protocol_id);
        let protocol = ChannelProtocol::from_raw(base_protocol_id);
        // ADR-199/ADR-214: under RawMode, `setup.tx_flag_bits` is the source
        // of `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE` for the message this slot
        // actually TRANSMITS (`repeat_msg_data`) -- this fold is reused below
        // both for that message's own TxFlags composition and (ADR-199,
        // pre-ADR-214) for the periodic-cap size-range check on it. Mirrors
        // `rpc_primitive::compute_j2534_tx_flags`'s identical RawMode
        // `TxFlagBits` fold, factored out as
        // `rpc_primitive::raw_mode_tx_flag_bit_to_j2534`.
        //
        // ADR-214 closes ADR-199's own accepted residual: the mask/pattern
        // response template's OWN addressing basis (`response_tx_flags`,
        // below) no longer unconditionally reuses this request-side fold --
        // it now derives from `setup.response_tx_flag_bits` (the packed
        // `REPEAT_MSG_SETUP` wire format's optional v2 trailing section)
        // when present, independently gated through the identical
        // `raw_mode_can_family`/`raw_mode_iso15765` booleans, falling back to
        // this request-side fold only when the client omits the v2 section
        // (`None`, e.g. a v1 client) -- see `response_addressing_flags`
        // below.
        let raw_mode_can_family =
            raw_mode && matches!(base_protocol_id, j2534_0404::CAN | j2534_0404::ISO15765);
        let raw_mode_iso15765 = raw_mode && base_protocol_id == j2534_0404::ISO15765;
        let raw_mode_addressing_flags = setup.tx_flag_bits.iter().fold(0u32, |acc, &bit_val| {
            acc | rpc_primitive::raw_mode_tx_flag_bit_to_j2534(
                raw_mode_can_family,
                raw_mode_iso15765,
                bit_val,
            )
        });
        // ADR-214 Decision items 1-2: the response template's own,
        // independent addressing basis. `None` (no v2 section on the wire)
        // inherits `raw_mode_addressing_flags` byte-for-byte, preserving
        // today's behavior for every v1 client; `Some(bits)` (possibly
        // empty) is folded through the identical gate, entirely independent
        // of `setup.tx_flag_bits`.
        let response_addressing_flags = match &setup.response_tx_flag_bits {
            Some(bits) => bits.iter().fold(0u32, |acc, &bit_val| {
                acc | rpc_primitive::raw_mode_tx_flag_bit_to_j2534(
                    raw_mode_can_family,
                    raw_mode_iso15765,
                    bit_val,
                )
            }),
            None => raw_mode_addressing_flags,
        };
        let full_message = tx_header::build_tx_message(
            protocol,
            tx_header::AddrModeSource::Request,
            &active,
            &entries,
            &setup.repeat_msg_data,
            false,
            tp20_established_tx_id,
            // ADR-199: RawMode now extends to Repeat Messaging's own message
            // composition -- a RawMode client's `repeat_msg_data` is its own
            // complete, header-inclusive frame, so this passes the real
            // `raw_mode` value instead of hardcoding `false`.
            raw_mode,
        )
        .map_err(Status::invalid_argument)?;

        // ADR-062's "objective facts this service already resolved" flags
        // (extended CAN ID width, SCI mode/voltage, SW_CAN high-voltage) --
        // the same building blocks `rpc_primitive::apply_resolved_tx_flags`
        // composes for an ordinary CoptSendrecv TX, applied directly here
        // since that helper itself is private to `rpc_primitive`. Hoisted
        // above the size-range check below (Codex review, ADR-165 PR #42
        // round 6) so `can_addressing`/`fd_link` are resolved once and
        // shared by both that check and the addressing_tx_flags composition
        // further down, instead of being derived twice.
        let can_addressing = tx_header::resolve_can_addressing(
            tx_header::AddrModeSource::Request,
            &active,
            &entries,
        );
        let fd_link = resources::is_fd_protocol_id(hw_protocol_id);

        // ADR-186 (supersedes this check's round-6 mechanism, ADR-165 PR #42):
        // `RepeatMsgData[0]` (the message this Repeat Messaging slot actually
        // TRANSMITS) is a periodic message -- SAE J2534-1 v04.04 §7.2.7 caps
        // every periodic message, of any protocol, at a single frame
        // (<DataSize> <= 12 bytes including the message header/ID), and SAE
        // J2534-2 clause 14.2.1 ties Repeat Messaging to that same cap.
        // Unlike an ordinary CoptSendrecv TX message (SAE J2534-2 21.4.4),
        // this cap is NOT relaxed by TX_FD_CAN_FORMAT -- clauses 21.2.2(h)/
        // 22.2.2(h) restate the flat 12/11-byte periodic ceiling for CAN FD
        // and FD-ISO15765 specifically. The round-6 check this replaces
        // reused the wider ordinary-TX ranges
        // (`fd_can_tx_message_size_range`/`fd_iso15765_tx_message_size_range`/
        // `protocol.tx_message_size_range`) for this periodic message, which
        // let an oversized repeat-message start compose and send a payload a
        // conforming device would reject with ERR_INVALID_MSG (clause
        // 14.2.2.1) -- most visibly the raw FD_CAN_PS padding block below,
        // now deleted, which grew a payload up to 64 bytes. `protocol` is
        // already base-family-resolved (CAN/ISO15765 regardless of FD-ness,
        // see `resources::base_protocol_id`), so a single match on
        // `protocol.j2534_protocol_id()` covers both the classic and FD
        // variant of each family identically -- the periodic cap does not
        // distinguish them. This check does NOT apply to `mask_data`/
        // `pattern_data` (`RepeatMsgData[1]`/`[2]`) -- those are
        // never-transmitted comparison TEMPLATES, deliberately left on the
        // wider ranges further below (round 17/18's own widening to support
        // matching deep into a long ECU response for discrimination
        // purposes; capping them here would silently revert that fix).
        // ADR-199: under RawMode, `can_addressing` (ComParam-derived) is not
        // authoritative -- a RawMode client has no reason to configure
        // `CP_Can*Format` at all, so this defers to the same
        // `raw_mode_addressing_flags` fold computed above, mirroring
        // `rpc_primitive::resolve_send_recv_tx`'s identical RawMode
        // `extended_addressing` derivation (there via
        // `compute_tx_prefix`/`ISO15765_ADDR_TYPE`).
        let extended_addressing = if raw_mode {
            raw_mode_addressing_flags & j2534_0404::ISO15765_ADDR_TYPE != 0
        } else {
            matches!(
                can_addressing.map(|a| a.tx_addressing),
                Some(isotp::Addressing::Extended(_))
            )
        };
        let fd_base_family = fd_link.then_some(base_protocol_id);
        let size_range = match protocol.j2534_protocol_id() {
            // Table 91's own DataSize accounting: 4-byte CAN ID + up to
            // 8 bytes payload. Already the natural upper bound of
            // `protocol.tx_message_size_range` for classic CAN (4..=12), so
            // this arm is really only needed to fix the FD case, which
            // previously tracked the wider staged CP_CANFDTxMaxDataLength
            // range instead.
            j2534_0404::CAN => 4..=12,
            // Clause 22.2.2(h)'s 11-byte FD-ISO15765 cap; SAE J2534-1
            // §7.2.7's flat, protocol-independent periodic cap applies the
            // same 11-byte ceiling to classic ISO15765 too. This is a real
            // narrowing for classic, not a no-op: classic ISO15765's own
            // natural `tx_message_size_range` is `4..=4099`/`5..=4100`
            // (`protocol.rs`), far wider than 11 -- coincidentally, 11
            // bytes also happens to be a single ISO-TP Single Frame's own
            // natural size (1 PCI byte leaves 7 payload bytes in an 8-byte
            // classic CAN frame), but that coincidence is not why this cap
            // applies here; it applies because §7.2.7 caps every periodic
            // message this way regardless of a protocol's own wider
            // ordinary-TX range.
            j2534_0404::ISO15765 => {
                if extended_addressing {
                    5..=11
                } else {
                    4..=11
                }
            }
            // Every other protocol this function supports (KWP/ISO14230,
            // ISO9141, J1850PWM/VPW, SCI, J1939, and any SAE J2534-2
            // protocol reaching this point, e.g. J1708_PS/HONDA_DIAGH_PS):
            // §7.2.7's cap is protocol-independent, so simply cap the
            // EXISTING range's own upper bound at 12, preserving whatever
            // lower bound already applies per protocol. ADR-186 Decision
            // item 1 (design-advisor consult, third round): J1939
            // deliberately falls through to this generic arm rather than
            // getting a dedicated one -- SAE J2534's <DataSize> field
            // never carries a "wire bytes transmitted" carve-out anywhere
            // in the spec family (even clause 21.2.2(h)/22.2.2(h)'s own
            // CAN-FD periodic cap counts the 4-byte CAN ID toward
            // <DataSize>, and clause 16's Table 62 gives a 0-data-byte
            // J1939 message a minimum <DataSize> of 5 despite that byte
            // being a clause 16.4.4 don't-care on the wire), so J1939
            // inherits the flat SAE J2534-1 §7.2.7 12-byte cap like every
            // other protocol without its own clause 21/22-style periodic
            // restatement. This produces `5..=12` here (J1939's own
            // `tx_message_size_range` lower bound of 5, capped at 12).
            _ => {
                let existing_range = protocol.tx_message_size_range(extended_addressing);
                *existing_range.start()..=(*existing_range.end()).min(12)
            }
        };
        if !size_range.contains(&full_message.len()) {
            return Err(Status::invalid_argument(format!(
                "PDU_IOCTL_START_REPEAT_MESSAGE repeat_msg_data length {} produces a {}-byte \
                 J2534 message, outside the valid periodic-message TX size range ({}..={} \
                 bytes) -- SAE J2534-1 v04.04 §7.2.7 caps every periodic message at a single \
                 frame (SAE J2534-2 clause 14.2.1 ties Repeat Messaging to this same cap), not \
                 relaxed by TX_FD_CAN_FORMAT (clause 21.2.2(h)/22.2.2(h))",
                setup.repeat_msg_data.len(),
                full_message.len(),
                size_range.start(),
                size_range.end(),
            )));
        }

        // ADR-055/ADR-169's functionally addressed ISO15765 Single Frame
        // check (a second call site mirroring `rpc_primitive.rs`'s
        // `CoptSendrecv` enforcement) used to be inlined here too, but
        // ADR-186 Decision item 3 deleted it: the periodic cap just above
        // admits at most 7 payload bytes (normal addressing) or 6
        // (extended), which never exceeds the Single Frame limit -- exactly
        // 7/6 for classic ISO15765 and for FD-ISO15765 at the minimum
        // staged `CP_CANFDTxMaxDataLength` of 8, and strictly less than the
        // FD-widened limit at any larger staged value -- and the periodic
        // cap runs first, so this inlined check could never fire for any
        // ISO15765 flavor. Any FUTURE widening of the periodic cap above
        // must reinstate an equivalent check here. The shared
        // `isotp::Addressing::max_sf_payload`/`fd_max_sf_payload` helpers
        // remain live for `rpc_primitive.rs`'s own ordinary `CoptSendrecv`
        // TX path, which still enforces the Single Frame limit there.

        // ADR-173 Decision 4 (reverses the round-7 condition-0 carve-out,
        // Codex review ADR-165 PR #42 round 7 Finding 1): both `Condition`
        // values now resolve the response header unconditionally. SAE
        // J2534-2 clause 14.2.2.1's `Condition == 0`
        // (`REPEAT_MESSAGE_UNTIL_MATCH`) DOES have the device evaluate
        // `RepeatMsgData[1]`/`[2]` (mask/pattern) against incoming traffic --
        // a matching frame is precisely what stops it -- so a `Condition ==
        // 0` link needs a resolvable response header exactly as much as
        // `Condition == 1` already correctly required. **Client-visible
        // behavior change** (ADR-173 Decision 4): a `Condition == 0` link
        // with no resolvable response header, previously silently accepted
        // (since the old code never composed a header for that condition),
        // is now rejected the same way `Condition == 1` already was --
        // a configuration whose own stop criterion could never actually
        // evaluate correctly against wire frames now fails fast instead of
        // silently never stopping.
        // ADR-199: under RawMode, ISO 22900-2:2022 §10.1.4.19.5/Table 80's
        // RawMode expected-response template shape is header-inclusive --
        // the client supplies the CAN-ID/header bytes directly in
        // `setup.mask_data`/`setup.pattern_data`, the same way it already
        // does for `repeat_msg_data` (ADR-196 Decision item 3's identical
        // rule for ordinary expected-response matching, extended here to
        // Repeat Messaging's device-side mask/pattern). `response_header_bytes`'s
        // ComParam-derived construction is therefore skipped entirely --
        // Annex B documents `CP_HeaderFormatJ1850`/`CP_HeaderFormatKW` and
        // the CAN-ID/UniqueRespIdTable-driven ComParams that function
        // consults as inert under RawMode, so calling it would resolve a
        // header the client's own template does not want prepended (and, for
        // a CLL with no UniqueRespIdTable entry configured at all -- entirely
        // plausible for a RawMode client -- would simply error). Empty
        // `response_header`/`response_header_mask` make the prepend below a
        // no-op, so `mask_data`/`pattern_data` end up exactly
        // `setup.mask_data`/`setup.pattern_data`, unprefixed.
        // ADR-214: `response_tx_flags` now uses `response_addressing_flags`
        // -- the response template's own, independent addressing basis
        // (`setup.response_tx_flag_bits` when the client's payload includes
        // ADR-214's optional v2 trailing section, else falling back to the
        // request-side `raw_mode_addressing_flags` fold for a v1 client) --
        // rather than unconditionally reusing the request-side fold. This
        // closes ADR-199's own accepted residual: a RawMode client whose
        // real ECU transmits on one CAN-ID width and responds on a
        // different one can now express that asymmetry explicitly.
        let (response_header, response_header_mask, response_tx_flags) = if raw_mode {
            (Vec::new(), Vec::new(), response_addressing_flags)
        } else {
            tx_header::response_header_bytes(protocol, &active, &entries, tp20_established_rx_id)
                .map_err(Status::invalid_argument)?
        };
        // Codex review, ADR-165 PR #42 round 15: SAE J2534-2 21.4.4 lets a device
        // cap a PASSTHRU_MSG's DataSize at 12 bytes when TX_FD_CAN_FORMAT is unset
        // -- a long mask/pattern template on an FD-connected link needs this flag
        // set for template VALIDITY, even though clause 21.2.2(g)/22.2.2(d) require
        // FD-capable-channel filtering/matching to ignore CAN message format
        // entirely (matching is on address+data only, never on classic-vs-FD wire
        // encoding). Deliberately NOT TX_FD_CAN_BRS: Table 93 defines BRS as a
        // transmission bit-timing property within an FD frame (always 0 for a
        // classic CAN 2.0 frame), not a classic/FD discriminator, and it carries no
        // analogous DataSize-validity coupling on a never-transmitted template.
        // Applied to BOTH condition branches above (including condition == 0's
        // `(vec![], vec![], 0)` case) -- 21.4.4's validity concern applies to
        // whatever template ends up composed below, regardless of condition.
        let response_tx_flags = if fd_link {
            response_tx_flags | j2534_0404::TX_FD_CAN_FORMAT
        } else {
            response_tx_flags
        };
        // ADR-200 close-out (`edge-case-hunter` finding on this PR's own
        // diff): a SAE J1939 frame's CAN identifier is always 29-bit
        // (clause 16.4.3), but under RawMode `response_tx_flags` comes
        // entirely from `response_addressing_flags` (ADR-214: either
        // `setup.response_tx_flag_bits`'s own fold or, absent a v2 section,
        // the request-side `raw_mode_addressing_flags` fold -- both gated to
        // CAN/ISO15765 only, see those folds' own comments) -- so a RawMode
        // J1939 CLL's mask/pattern template never carried `TX_EXTENDED_ID`
        // at all via either fold, regardless of what the client requested,
        // and the device-side matcher (keyed on this bit agreeing with each
        // incoming frame's own `CAN_29BIT_ID_STATUS`, mirroring the exact
        // mechanism ADR-199's own close-out review caught for CAN/ISO15765)
        // rejected every genuinely-matching, honestly-29-bit-flagged real
        // response as a non-match, unconditionally, for every RawMode J1939
        // repeat slot -- not a narrow asymmetric-addressing corner case
        // (ADR-199's own accepted residual, closed by ADR-214), since J1939
        // has no 11-bit form to be asymmetric with in the first place. The
        // non-RawMode path never had this gap: `tx_header::
        // response_header_bytes`'s own J1939 arm already force-sets this bit
        // unconditionally (see this function's own `tx_flags` J1939 comment
        // two blocks below, which already forces the identical bit for the
        // TRANSMITTED message and explicitly notes the response side "was
        // unaffected" -- true only for the non-RawMode/`response_header_
        // bytes` path this ADR-200 RawMode branch bypasses entirely). Forced
        // unconditionally here, mirroring the transmitted message's own
        // unconditional force, so both RawMode and non-RawMode J1939
        // templates carry it consistently regardless of what either the
        // client's `tx_flag_bits` or `response_tx_flag_bits` requested
        // (RawMode's client-authoritative-addressing premise does not
        // extend to a bit no J1939 frame can ever legitimately clear) --
        // ADR-214 Decision item 4: neither fold ever contributes this bit
        // for J1939 in the first place (`raw_mode_can_family`/
        // `raw_mode_iso15765` are both already `false`), so this force
        // remains the sole source unchanged.
        let response_tx_flags = if resources::is_j1939_protocol_id(hw_protocol_id) {
            response_tx_flags | j2534_0404::TX_EXTENDED_ID
        } else {
            response_tx_flags
        };

        // Finding K (Codex review, ADR-165 PR #42 round 5): mirrors round
        // 3's Finding D fix in `tx_header::response_header_bytes` --
        // ISO15765_ADDR_TYPE is an ISO15765-only extended-addressing
        // indicator (SAE J2534-1 Table B.13), so a plain raw-CAN link with
        // extended addressing configured must not carry it. `can_addressing_
        // tx_flags` -- the shared helper this composition calls for the
        // actually-TRANSMITTED `RepeatMsgData[0]` message -- now gates this
        // bit on `hw_protocol_id` itself (backlog fix, closing the gap this
        // round's own comment used to flag as "explicitly out of scope"), so
        // this call site no longer needs its own local gate. TX_EXTENDED_ID
        // (CAN ID width) is unaffected either way -- it applies regardless of
        // protocol.
        //
        // ADR-199: under RawMode, `can_addressing_tx_flags`'s ComParam-derived
        // contribution for these same two bits is suppressed -- the client is
        // now authoritative for them (`raw_mode_addressing_flags`, computed
        // above from `setup.tx_flag_bits` via `raw_mode_tx_flag_bit_to_j2534`)
        // -- mirroring exactly how `rpc_primitive::apply_resolved_tx_flags`
        // already does this for an ordinary `CoptSendrecv` TX under RawMode
        // (see that function's own RawMode doc comment).
        let addressing_tx_flags = if raw_mode {
            raw_mode_addressing_flags
        } else {
            tx_header::can_addressing_tx_flags(can_addressing, hw_protocol_id)
        };
        let mut tx_flags = addressing_tx_flags | active.sci_tx_flags();
        // ADR-212 Decision item 4's own class of fix, mirroring
        // `rpc_primitive::apply_resolved_tx_flags`'s identical re-key just
        // above this file's own reference to it -- without this, a
        // `_CHx`-connected SW-CAN link's `CP_SwCan_HighVoltage` bit would
        // silently never reach a repeat message's own TX flags.
        if resources::is_sw_family_protocol_id(hw_protocol_id) {
            tx_flags |= active.sw_can_tx_flags();
        }
        // ADR-175/Phase 11 (Codex review, PR #64), re-keyed by ADR-209
        // Decision item 8: mirrors `rpc_primitive::apply_resolved_tx_flags`'s
        // own J1708-gated `msg_priority_tx_flags()` OR -- this composition
        // site builds the actually-transmitted `RepeatMsgData[0]` message's
        // TxFlags independently of that shared helper (the same reason it
        // has its own local SW-CAN gate just above), so a J1708 link's
        // `CP_MessagePriority` value would otherwise never reach
        // `MSG_PRIORITY_VALUE` on a repeat-messaging transmission, even
        // though clause 17 places no Repeat Messaging exclusion on this
        // protocol. Unlike UART Echo Byte's own `rpc_misc.rs` fixes (ADR-207
        // Decision item 10, which widened a Repeat Messaging *rejection*
        // since UART Echo Byte's clause 12 excludes it entirely), this
        // widens a flag *application*: clause 17 places no Repeat Messaging
        // exclusion on J1708 at all, so a `_CHx`-connected link doing Repeat
        // Messaging must still receive its `MSG_PRIORITY_VALUE` flag the
        // same as its `_PS` sibling -- gated on the family-wide
        // `is_j1708_family_protocol_id` (ADR-209) rather than the narrower,
        // arm-gate-only `is_j1708_protocol_id`, the same reasoning as
        // `rpc_primitive::apply_resolved_tx_flags`'s own re-keying.
        if resources::is_j1708_family_protocol_id(hw_protocol_id) {
            tx_flags |= active.msg_priority_tx_flags();
        }
        // SAE J2534-2 clause 16.4.3 (Codex review, `edge-case-hunter` pass on
        // PR #72 round 11's own diff): a SAE J1939 frame's CAN identifier is
        // always 29-bit, but this composition's own `can_addressing_tx_flags`
        // call above resolves `TX_EXTENDED_ID` from `can_addressing`, which
        // is always `None` for J1939 (`resolve_can_addressing` is keyed on
        // `CP_CanPhysReqId`/`CP_CanFuncReqId`, which J1939 never configures)
        // -- so the actually-transmitted `RepeatMsgData[0]` message went out
        // as an 11-bit frame despite round 11's own `rpc_primitive::
        // apply_resolved_tx_flags` fix forcing this same bit for every OTHER
        // J1939 TX path (`CoptSendrecv`, the optional `CoptStartcomm`
        // message, tester-present) -- this composition builds its flags
        // independently of that shared helper (the same reason the SW-CAN/
        // J1708 gates just above are each duplicated here rather than
        // shared), so it needed the identical fix at this second site.
        // `tx_header::response_header_bytes`'s own J1939 arm (the STOP-
        // condition mask/pattern, not this TX message) already force-sets
        // this bit unconditionally for a non-RawMode CLL -- but a RawMode
        // J1939 CLL bypasses that function entirely (ADR-200) and needed
        // the identical force applied to `response_tx_flags` separately;
        // see this file's own `response_tx_flags` J1939 comment above
        // (ADR-200 close-out) for that fix.
        if resources::is_j1939_protocol_id(hw_protocol_id) {
            tx_flags |= j2534_0404::TX_EXTENDED_ID;
        }
        // Codex review finding (P1, PR #97, round 18): mirrors the identical
        // fix `rpc_primitive::apply_resolved_tx_flags` needed for this exact
        // gap -- see this composition's own J1939 comment just above for why
        // this duplicates that helper's logic instead of sharing it. Unlike
        // J1939, a TP2.0 connection's TX-ID is not unconditionally 29-bit
        // (clause 19 permits either); derived from the established TX-ID's
        // own value instead.
        if tp20_established_tx_id.is_some_and(|tx_id| tx_id > 0x7FF) {
            tx_flags |= j2534_0404::TX_EXTENDED_ID;
        }

        // Codex review, ADR-165 PR #42 round 7 (Finding 2): `IORepeatMessageSetup`
        // had no way for a client to request pass-through TX flags (e.g.
        // ISO-TP frame padding) on the transmitted `RepeatMsgData[0]`
        // message, unlike an ordinary CoptSendrecv TX
        // (`rpc_primitive::compute_j2534_tx_flags`'s `TxFlagBits` handling).
        // Reuses that same per-bit mapping (`tx_flag_bit_to_j2534`) via the
        // new `setup.tx_flag_bits` field. Applied only to this actually-
        // transmitted message's `tx_flags`, never to the mask/pattern
        // `PassThruMessage`s below (those use `response_tx_flags`, unrelated
        // -- established precedent per Finding 2, round 2: mask/pattern
        // messages don't carry TX-composition flags, since they're
        // comparison templates, not transmitted frames).
        for &bit_val in &setup.tx_flag_bits {
            tx_flags |= tx_flag_bit_to_j2534(bit_val);
        }

        // Bug 3 (edge-case-hunter): `rpc_primitive::resolve_send_recv_tx`
        // always adds these two FD-specific flags for an FD-connected link's
        // TX message (SAE J2534-2 21.4.4/Tables 99-100: a conformant device
        // rejects/misinterprets an oversized-for-Classic message with
        // TX_FD_CAN_FORMAT unset), and this inlined composition (copied from
        // that helper because it is private to `rpc_primitive`) had omitted
        // them. A repeat slot's `RepeatMsgData[0]` is always one raw native
        // frame -- never software-ISOTP-segmented -- so unlike
        // `resolve_send_recv_tx`'s `fd_link` (which also gates on
        // `!software_isotp`), that gate does not apply here; `fd_link` is
        // simply whether the connected hw_protocol_id is FD (computed above,
        // alongside the ADR-186 periodic-message size-range check, and
        // reused here).
        if fd_link {
            tx_flags |= j2534_0404::TX_FD_CAN_FORMAT;
            if active
                .unum32
                .get(&PARAM_CANFD_BAUDRATE)
                .copied()
                .unwrap_or(0)
                != 0
            {
                tx_flags |= j2534_0404::TX_FD_CAN_BRS;
            }

            // ADR-186 (removes the round-4/5 FD_CAN_PS padding-to-64-bytes
            // step this comment used to describe -- see this fix's own
            // comment above the round-6-superseding size-range check
            // earlier in this function): the periodic-message cap enforced
            // there restricts every raw CAN family `repeat_msg_data` (FD or
            // classic) to a 4..=12-byte `full_message`, i.e. a payload of at
            // most 8 bytes -- every legal CAN FD DLC in that 0-8 range is
            // already wire-legal with no filler padding needed, so the
            // `data_len > 8` case this padding step used to handle can no
            // longer be reached for this family. Kept as a `debug_assert!`
            // rather than deleted outright, so a future change to the
            // size-range check above that reopens this path fails loudly in
            // tests instead of silently sending an unpadded, wire-illegal
            // CAN FD frame.
            if resources::base_protocol_id(hw_protocol_id) == j2534_0404::CAN {
                let data_len = full_message.len() - 4;
                debug_assert!(
                    data_len <= 8,
                    "ADR-186's periodic-message cap (4..=12 bytes for the raw CAN family) \
                     should make an FD_CAN_PS repeat_msg_data payload longer than 8 bytes \
                     unreachable here -- got {data_len} bytes"
                );
            }
        }

        // Finding B (Codex review, ADR-165 PR #42 round 2): the header mask
        // is no longer implicitly all-ones -- `response_header_bytes` now
        // marks a KWP/ISO14230/ISO9141 separate-length-byte position (when
        // present) as `0x00` (don't-care), since its real wire value is
        // payload-length-dependent and cannot be predicted here. Using that
        // mask verbatim (instead of a fresh all-ones vec sized off
        // `response_header.len()`) keeps this composition correct for that
        // position.
        let mut mask_data = response_header_mask;
        mask_data.extend_from_slice(&setup.mask_data);
        let mut pattern_data = response_header;
        pattern_data.extend_from_slice(&setup.pattern_data);

        // ADR-200 (Phase 3): a RawMode SAE J1939 CLL's device-side
        // stop-condition template (`mask_data`/`pattern_data`, just composed
        // above -- empty `response_header`/`response_header_mask` under
        // RawMode make this exactly `setup.mask_data`/`setup.pattern_data`
        // unprefixed) is compared by the DEVICE against native
        // 5-byte-CAN-ID-plus-DA frames (SAE J2534-2 §16.4.3/Table 62), but
        // the client's own raw template is the D-PDU 4-byte-CAN-ID-only
        // shape (ISO 22900-2:2022 line 774/Table 80) -- insert a single
        // zeroed (`0x00`, don't-care) byte at position 4 in both so the
        // device's own byte-for-byte comparison stays aligned from index 4
        // onward. A DERIVED DA (mirroring `tx_header::raw_j1939_tx_message`'s
        // TX-side insertion) is deliberately NOT used here: the device's own
        // incoming DA on a RECEIVED frame is not predictable the way a
        // TRANSMITTED DA is, and clause 14.2.2.1's own don't-care-beyond-
        // DataSize mask mechanism is exactly the tool for this -- mirrors the
        // K-line ChecksumMode=OFF don't-care technique
        // `docs/rpc-api-guide.md` already documents for a different protocol
        // (ADR-199). A template shorter than the 4-byte CAN-ID span is left
        // alone -- the size-range check just below rejects it on its own
        // terms.
        if raw_mode && base_protocol_id == j2534_0404::PROTOCOL_J1939_PS {
            if mask_data.len() >= 4 {
                mask_data.insert(4, 0x00);
            }
            if pattern_data.len() >= 4 {
                pattern_data.insert(4, 0x00);
            }
        }

        // Codex review, ADR-165 PR #42 round 17 (still applies post-ADR-186):
        // the ADR-186 periodic-message size-range check above only
        // validates `full_message`
        // (`RepeatMsgData[0]`, the actually-transmitted message) --
        // `mask_data`/`pattern_data` (`RepeatMsgData[1]`/`[2]`), just composed
        // above by prepending the response header to the client's own
        // `setup.mask_data`/`setup.pattern_data`, were never checked against
        // this protocol's real TX message size range at all;
        // `PassThruMessage::new` below only enforces the struct-wide
        // capacity, not this per-protocol range -- e.g. a 9-byte client
        // `mask_data` on a classic CAN link becomes a 13-byte composed
        // template (4-byte CAN-ID header + 9 bytes), exceeding CAN's
        // `4..=12` range, and was silently accepted. Gated on
        // `setup.condition != 0`, mirroring this function's own established
        // precedent (the length-match check above, and
        // `response_header_bytes`'s round-7 skip): a `condition == 0` slot
        // never has its mask/pattern evaluated by the device at all, so
        // validating an irrelevant template's size is unnecessary.
        //
        // Deliberately NOT reusing either value `full_message`'s own check
        // computed above. Two distinct traps, post-ADR-186:
        // (1) NOT the periodic-cap `size_range` itself (`4..=11`/`5..=11`
        // for ISO15765) -- that cap is specific to `full_message`, the
        // message actually TRANSMITTED; `mask_data`/`pattern_data` are
        // never-transmitted comparison templates deliberately left on the
        // wider ordinary `tx_message_size_range` (see the note above this
        // block), so reusing the periodic cap here would wrongly reject a
        // legitimately long template.
        // (2) NOT even just the request-side `extended_addressing` bool as
        // input to `protocol.tx_message_size_range`: `mask_data`/
        // `pattern_data` are built from the RESPONSE's own addressing
        // (`response_header`/`response_header_mask`/`response_tx_flags`,
        // resolved by `tx_header::response_header_bytes` above), which this
        // function has already established twice (see "Finding 2" below and
        // "Finding D" in `tx_header.rs`) can genuinely differ from the
        // request-side addressing `extended_addressing` was computed from --
        // reusing that bool here could validate this template against the
        // wrong one of ISO15765's own two ordinary ranges (`4..=4099`
        // normal / `5..=4100` extended, `protocol.rs`). The correct basis
        // is derived from `response_tx_flags & j2534_0404::ISO15765_ADDR_TYPE`
        // instead, the response-side extended-addressing indicator
        // `response_header_bytes` already resolved. For classic CAN this
        // makes no functional difference (`ChannelProtocol::CAN`'s range is a
        // fixed `4..=12` regardless of addressing), but getting it right for
        // ISO15765 costs nothing extra and avoids a latent bug on a
        // differently-addressed ISO15765 link.
        //
        // ADR-173 Decision 4 (reverses the round-17 condition-0 carve-out,
        // Codex review ADR-165 PR #42 round 17): this size-range validation
        // now applies unconditionally to both `Condition` values, mirroring
        // the response-header-resolution and length-equality corrections
        // above -- `Condition == 0` evaluates mask/pattern against incoming
        // traffic exactly as `Condition == 1` does (clause 14.2.2.1), so an
        // oversized template is equally meaningless-but-dangerous under
        // either condition.
        {
            let response_extended_addressing =
                response_tx_flags & j2534_0404::ISO15765_ADDR_TYPE != 0;
            let mask_pattern_size_range = match fd_base_family {
                // Codex review, ADR-165 PR #42 round 18: round 17 (just
                // above in git history) used `fd_can_tx_message_size_range`
                // here, the SAME function `full_message`'s check above uses
                // -- but that function's own doc comment establishes its
                // upper bound is derived from `CP_CANFDTxMaxDataLength`,
                // ISO 22900-2's tester-declared max TX length (ISO 15765-2's
                // TX_DL). `mask_data`/`pattern_data` are never transmitted --
                // they're a comparison template evaluated against a
                // RECEIVED ECU response -- and an ECU's RX capability on a
                // CAN FD bus is independent of the tester's own configured
                // TX_DL, so a legitimately longer ECU response must not be
                // rejected merely because the tester's own `TX_DL` (or its
                // `.max(8)` floor when unset) is narrower. Round 15's fix
                // already sets `TX_FD_CAN_FORMAT` on this exact template
                // specifically to make it valid up to the full CAN FD frame
                // limit, so the correct upper bound here is the CAN FD
                // frame's structural maximum payload
                // (`CANFD_TX_MAX_DATA_LENGTH_ACCEPTED`'s max, 64 bytes),
                // not the staged `CP_CANFDTxMaxDataLength`.
                Some(j2534_0404::CAN) => {
                    4..=(4 + *CANFD_TX_MAX_DATA_LENGTH_ACCEPTED
                        .last()
                        .expect("CANFD_TX_MAX_DATA_LENGTH_ACCEPTED is non-empty")
                        as usize)
                }
                Some(j2534_0404::ISO15765) => {
                    protocol::fd_iso15765_tx_message_size_range(response_extended_addressing)
                }
                // ADR-199: mirrors `rpc_primitive::resolve_send_recv_tx`'s
                // (and the `CoptStartcomm` fast-init check's) identical
                // `kline_manual_checksum_iso14230`/`raw_mode && !checksum_mode`
                // ADR-198 widening -- under RawMode with ChecksumMode=OFF on
                // an ISO14230 link, the client's own mask/pattern template
                // includes its own manually-managed trailing checksum byte,
                // one byte wider than the interface-managed row (SAE
                // J2534-1 §8.3 Figure 42). Unlike the periodic-cap check
                // above (already flat-capped at 12 bytes, making this
                // widening moot there), this template check is not capped
                // that way, so the widening matters here.
                _ => {
                    let base_range = protocol.tx_message_size_range(response_extended_addressing);
                    if raw_mode && !checksum_mode && protocol == ChannelProtocol::ISO14230 {
                        *base_range.start()..=(*base_range.end() + 1)
                    } else {
                        base_range
                    }
                }
            };
            if !mask_pattern_size_range.contains(&mask_data.len()) {
                return Err(Status::invalid_argument(format!(
                    "PDU_IOCTL_START_REPEAT_MESSAGE mask_data length {} produces a {}-byte \
                     J2534 mask template, outside the valid TX message size range ({}..={} \
                     bytes)",
                    setup.mask_data.len(),
                    mask_data.len(),
                    mask_pattern_size_range.start(),
                    mask_pattern_size_range.end(),
                )));
            }
            if !mask_pattern_size_range.contains(&pattern_data.len()) {
                return Err(Status::invalid_argument(format!(
                    "PDU_IOCTL_START_REPEAT_MESSAGE pattern_data length {} produces a {}-byte \
                     J2534 pattern template, outside the valid TX message size range ({}..={} \
                     bytes)",
                    setup.pattern_data.len(),
                    pattern_data.len(),
                    mask_pattern_size_range.start(),
                    mask_pattern_size_range.end(),
                )));
            }
        }

        let message =
            j2534_0404::PassThruMessage::new(hw_protocol_id, 0, tx_flags, 0, 0, &full_message)
                .map_err(|err| Status::invalid_argument(err.to_string()))?;
        // Finding 2 (Codex review, ADR-165 PR #42): the mask/pattern
        // messages' bytes (`response_header` above) were built from the
        // RESPONSE's own CAN ID width/addressing (`tx_header::
        // response_header_bytes`'s returned `response_tx_flags`), not the
        // request's -- `addressing_tx_flags` (request-side, used for
        // `message` above) would misdescribe them whenever request and
        // response differ in CAN ID width (11 vs 29-bit) or addressing mode
        // (normal vs extended).
        // TX_FD_CAN_FORMAT IS now included above (round 15) -- not for matching
        // purposes (SAE J2534-2 21.2.2(g)/22.2.2(d) require FD-capable-channel
        // filtering to ignore CAN message format; two frames with identical
        // address+data are the same message whether sent as classic or FD), but
        // because 21.4.4 lets a device cap a template's DataSize at 12 bytes
        // without it -- template validity, not comparison semantics. TX_FD_CAN_BRS
        // is still deliberately excluded (Table 93: a bit-timing property within an
        // FD frame, not a format discriminator, with no equivalent validity
        // coupling). This narrows, rather than overturns, the original "mask/
        // pattern is a comparison template, not something transmitted" reasoning:
        // `install_client_message_filters`'s own mask/pattern composition
        // (`rpc_link.rs`) staying format-blind is independently correct per the
        // same clause -- see the backlog for whether
        // IT also needs the same >12-byte template-validity treatment (out of
        // scope here).
        let mask = j2534_0404::PassThruMessage::new(
            hw_protocol_id,
            0,
            response_tx_flags,
            0,
            0,
            &mask_data,
        )
        .map_err(|err| Status::invalid_argument(err.to_string()))?;
        let pattern = j2534_0404::PassThruMessage::new(
            hw_protocol_id,
            0,
            response_tx_flags,
            0,
            0,
            &pattern_data,
        )
        .map_err(|err| Status::invalid_argument(err.to_string()))?;

        let mut native_setup = j2534_0404::RepeatMsgSetup::new(
            setup.time_interval,
            setup.condition,
            message,
            mask,
            pattern,
        );

        let api = self.api.lock().await;
        let result = api.start_repeat_message(channel_id, &mut native_setup);
        drop(api);
        let msg_id = result.map_err(|err| {
            map_native_error_for_link("PassThruIoctl START_REPEAT_MESSAGE", &err, last_error)
        })?;

        // `shared_channels` (held since before the `logical_links` read
        // above) guarantees this CLL is still here to record against -- see
        // this function's doc comment (Bug 1 fix).
        let mut links = self.logical_links.lock().await;

        // Finding E (Codex review, ADR-165 PR #42 round 3): the device
        // returning `msg_id` from a successful START is authoritative proof
        // it currently considers that numeric MsgId free-and-now-assigned to
        // THIS CLL. A sibling CLL sharing the same physical channel
        // (`channel_key`, ADR-165 Decision 4's shared-slot-budget design)
        // may still be carrying a stale claim on this same numeric MsgId --
        // e.g. it started a `Condition == 1` slot that later self-completed
        // device-side with no notification back to this service, and never
        // itself issued a QUERY/STOP that would have pruned it (the round-2
        // Finding C prune only fires on that CLL's OWN next QUERY/STOP).
        // `require_owned_repeat_message` only checks set membership, not
        // current device-side assignment, so an unrevoked stale claim could
        // let that sibling STOP/QUERY this brand-new, unrelated slot. Revoke
        // any such stale claim from every OTHER CLL on this channel before
        // recording it here, under the same `shared_channels` critical
        // section this call already holds (no new lock-acquisition window).
        if let Some(channel_key) = channel_key {
            for (&other_handle, other_link) in links.iter_mut() {
                if other_handle != cll_handle && other_link.channel_key == Some(channel_key) {
                    other_link.repeat_message_ids.retain(|&id| id != msg_id);
                }
            }
        }
        // Codex review, ADR-165 PR #42 round 12: closes a gap in round 3
        // Finding E's own fix above -- that fix only revoked stale claims
        // from OTHER CLLs sharing this channel, missing the case where THIS
        // SAME CLL is the one carrying the stale claim (e.g. it started a
        // `Condition == 1` slot that self-completed device-side, and never
        // itself issued the QUERY/STOP that would have pruned it, per round
        // 2 Finding C's prune-on-own-next-QUERY/STOP trigger). Without this,
        // a later START_REPEAT_MESSAGE reassigned the same numeric MsgId to
        // this CLL would push a duplicate entry into its own list.
        if let Some(link) = links.get_mut(&cll_handle) {
            link.repeat_message_ids.retain(|&id| id != msg_id);
            link.repeat_message_ids.push(msg_id);
        }
        drop(links);

        // A leaked-and-now-reassigned MsgId must stop being retried as
        // leaked -- retrying a STOP against a live slot that now belongs to
        // this CLL would be its own bug (Finding E).
        if let Some(channel_key) = channel_key
            && let Some(sc) = chans.get_mut(&channel_key)
        {
            sc.leaked_repeat_message_ids.retain(|&id| id != msg_id);
        }
        drop(chans);
        Ok(msg_id)
    }

    /// Probes each tracked MsgId via QUERY_REPEAT_MESSAGE against the device
    /// -- the single source of truth for repeat-slot liveness, since
    /// ADR-165 has no proactive completion signal for a self-completed
    /// slot. Per ADR-173 Decision 3, "should stay tracked" and "should
    /// count as actively transmitting" are now two independent questions,
    /// not one: a status-0 (terminated-but-unstopped) `MsgId` remains a
    /// valid claim until an explicit STOP (clause 14.2.2.3's `MsgId`-
    /// retention rule) and so is NEVER pruned from `ids`, but it is also
    /// not actively transmitting and so must not count toward the returned
    /// bool. Retain criterion (unchanged from before ADR-173): drop an id
    /// only on `Err(ERR_INVALID_MSG_ID)` (device says it is genuinely
    /// gone); keep every other id, including a status-0 one. Returned bool
    /// (ADR-173 polarity correction): `true` iff any REMAINING id is either
    /// status-1 (live, Table 53) or an inconclusive `Err` (fail closed,
    /// since a wrong grant during autonomous device TX is the asymmetric
    /// harm) -- a status-0 id contributes `false`.
    pub(super) fn prune_stale_repeat_message_ids(
        api: &J2534Api0404,
        channel_id: ChannelId,
        ids: &mut Vec<u32>,
    ) -> bool {
        let mut any_blocking = false;
        ids.retain(
            |&msg_id| match api.query_repeat_message(channel_id, msg_id) {
                Ok(status) => {
                    if status != 0 {
                        any_blocking = true;
                    }
                    true
                }
                Err(j2534_0404::Error::ApiStatus { code, .. })
                    if code.as_u32() == j2534_0404::ERR_INVALID_MSG_ID =>
                {
                    false
                }
                Err(_) => {
                    any_blocking = true;
                    true
                }
            },
        );
        any_blocking
    }

    /// SAE J2534-2 clause 19.3.2.3 TP2.0 broadcast periodic re-trigger
    /// opportunistic retry/prune (ADR-192/Phase 7 Stage 7c Fix B, design-
    /// advisor consult, Codex review round 3): the structural sibling of
    /// `retry_leaked_repeat_message_stops`/`prune_stale_repeat_message_ids`
    /// just above, but ONE function instead of a split retry/prune pair --
    /// design-advisor's own reasoning: the native periodic-message API has
    /// no QUERY primitive, so the retry's own `stop_periodic_message`
    /// attempt IS the liveness probe; there is nothing separate to query.
    ///
    /// For each id in `ids`: attempts `stop_periodic_message`. On `Ok`, or on
    /// `ERR_INVALID_MSG_ID` (the device no longer recognizes this id --
    /// mirrors `retry_leaked_repeat_message_stops`'s own Finding F
    /// self-pruning for the repeat-message case), the id is removed -- it is
    /// gone either way, whether this call just stopped it or it was already
    /// gone before this retry. On any other error, the id is kept -- still
    /// genuinely live, still blocking. Returns whether any id remains in
    /// `ids` after this pass (mirrors `prune_stale_repeat_message_ids`'s own
    /// returned-bool shape).
    ///
    /// `ids`' epoch half (the periodic-clear epoch fix, Codex review round
    /// 5, PR #101, ADR-192/Phase 7 Stage 7c) is not otherwise used inside
    /// this retry -- it is preserved in the tuple purely so a re-tracked
    /// failed-stop keeps its original `started_epoch` for the NEXT
    /// `CLEAR_PERIODIC_MSGS`' own `clear_generation` gate to judge
    /// correctly, rather than losing that provenance here.
    pub(super) fn retry_leaked_periodic_message_stops(
        api: &J2534Api0404,
        channel_id: ChannelId,
        ids: &mut Vec<(j2534_0404::PeriodicMessageId, u64)>,
    ) -> bool {
        ids.retain(
            |&(id, _epoch)| match api.stop_periodic_message(channel_id, id) {
                Ok(()) => false,
                Err(j2534_0404::Error::ApiStatus { code, .. })
                    if code.as_u32() == j2534_0404::ERR_INVALID_MSG_ID =>
                {
                    debug!(
                        channel_id = channel_id.0,
                        message_id = id.0,
                        "leaked TP2.0 broadcast periodic STOP retry got ERR_INVALID_MSG_ID -- \
                         device already forgot this message, treating as cleaned up"
                    );
                    false
                }
                Err(err) => {
                    debug!(
                        channel_id = channel_id.0,
                        message_id = id.0,
                        %err,
                        "opportunistic retry of a leaked TP2.0 broadcast periodic STOP failed, \
                         left tracked"
                    );
                    true
                }
            },
        );
        !ids.is_empty()
    }

    /// Resolves and validates a caller-supplied `MsgId` against this CLL's
    /// own `repeat_message_ids` (ADR-165 Decision 4) for
    /// `PDU_IOCTL_QUERY_REPEAT_MESSAGE`/`_STOP_REPEAT_MESSAGE`: a `MsgId` not
    /// in this set -- whether never issued, or issued to a sibling CLL
    /// sharing the same physical channel -- is rejected as
    /// `PDU_ERR_INVALID_MSG_ID` (mapped the same way
    /// `error::pdu_error_for` already maps the native `ERR_INVALID_MSG_ID`
    /// code, so a rejection looks identical to the client whether this
    /// service's own check or the device's caught it) before ever reaching
    /// the native call. Also requires this CLL connected, same as `START` --
    /// checked BEFORE the ownership check (Codex review, PR #42 round 22),
    /// not after: `DisconnectComLogicalLink`'s teardown (`rpc_link.rs`)
    /// clears `channel_id` to `None` and drains `repeat_message_ids` to
    /// empty, but -- unlike `DestroyComLogicalLink`, which removes the
    /// CLL's entry from `logical_links` outright, hitting this function's
    /// `unknown_handle_status` guard above before either check below runs,
    /// on both orderings -- leaves the CLL's own entry in `logical_links`.
    /// With the ownership check first, a disconnected-but-not-destroyed
    /// CLL would therefore *always* fail that check -- regardless of
    /// whether `msg_id` had genuinely been owned immediately before
    /// disconnect -- making this function's own documented connection
    /// requirement structurally unreachable for that CLL and reporting a
    /// misleading PDU_ERR_INVALID_MSG_ID instead of
    /// PDU_ERR_CLL_NOT_CONNECTED. Returns the connected `channel_id`, this
    /// CLL's `channel_key` (for the
    /// leaked-repeat-message opportunistic retry, Codex review PR #42 round
    /// 2 Finding A), and the fresh `last_error` snapshot on success.
    fn require_owned_repeat_message(
        cll_handle: u32,
        msg_id: u32,
        links: &std::collections::HashMap<u32, LogicalLinkState>,
    ) -> Result<(ChannelId, Option<ChannelKey>, Option<TrackedError>), Status> {
        let link = links
            .get(&cll_handle)
            .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
        let channel_id = link.channel_id.ok_or_else(|| {
            state_guard_status(
                Code::FailedPrecondition,
                "logical link is not connected",
                PduError::PduErrCllNotConnected,
                link.last_error.clone(),
            )
        })?;
        // ADR-170/Phase 9: mirrors `ioctl_start_repeat_message`'s own
        // rejection -- QUERY/STOP would otherwise only ever fail this
        // function's ownership check below with a generic
        // PDU_ERR_INVALID_MSG_ID (a UART Echo Byte link can never actually
        // own a `msg_id`, since START rejects it before ever registering
        // one), which is a correct but misleading answer: clause 12.3.3.1's
        // Repeat Messaging exclusion applies to all three commands
        // unconditionally, not merely "this MsgId happens to not exist" --
        // so QUERY/STOP get the same explicit, named rejection START does.
        // ADR-207 Decision item 10: checked via the range-inclusive
        // `is_uart_echo_byte_family_protocol_id`, mirroring
        // `ioctl_start_repeat_message`'s own fix, so a `_CHx` Additional
        // Channels link is excluded the same way its `_PS` sibling is.
        if resources::is_uart_echo_byte_family_protocol_id(link.hw_protocol_id) {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: Repeat Messaging is not supported on a SAE J2534-2 \
                 clause 12 UART Echo Byte Protocol link -- the interface cannot autonomously \
                 maintain the clause 12.4.2 message-counter/echo-byte handshake a repeat \
                 retransmission would require",
                PduError::PduErrIdNotSupported,
                link.last_error.clone(),
            ));
        }
        if !link.repeat_message_ids.contains(&msg_id) {
            return Err(state_guard_status(
                Code::NotFound,
                format!(
                    "PDU_ERR_INVALID_MSG_ID: MsgId {msg_id} was not started by this \
                     ComLogicalLink"
                ),
                PduError::PduErrInvalidHandle,
                link.last_error.clone(),
            ));
        }
        Ok((channel_id, link.channel_key, link.last_error.clone()))
    }

    /// **PDU_IOCTL_QUERY_REPEAT_MESSAGE (L, ADR-165/Phase 12).** SAE J2534-2
    /// clause 14's QUERY command: polls a running repeat slot's status via
    /// `PassThruIoctl(QUERY_REPEAT_MESSAGE)`. `input_data` must carry the
    /// `MsgId` (`PDU_IT_IO_UNUM32`) a prior `PDU_IOCTL_START_REPEAT_MESSAGE`
    /// on this same CLL returned -- see
    /// [`Self::require_owned_repeat_message`]. Returns the device-reported
    /// status as `unum32_value`.
    ///
    /// Holds `shared_channels` locked across the ownership check and the
    /// native call, the same `ioctl_start_repeat_message`/Bug 1 pattern:
    /// without it, this call's own ownership check could pass and then race
    /// a concurrent teardown's best-effort STOP for the same `MsgId`,
    /// surfacing a confusing `ERR_INVALID_MSG_ID` from the device for a slot
    /// that is, in fact, already gone. Serializing on `shared_channels`
    /// against `DestroyComLogicalLink`/`DisconnectComLogicalLink`'s own
    /// teardown loop (which holds the same lock, ADR-080) closes that
    /// window.
    async fn ioctl_query_repeat_message(
        &self,
        cll_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<u32, Status> {
        let msg_id = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::Unum32Value(v)) => v,
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_QUERY_REPEAT_MESSAGE requires a PDU_IT_IO_UNUM32 input_data \
                     (the MsgId returned by PDU_IOCTL_START_REPEAT_MESSAGE)",
                ));
            }
        };

        let mut chans = self.shared_channels.lock().await;
        let (channel_id, channel_key, last_error) = {
            let links = self.logical_links.lock().await;
            Self::require_owned_repeat_message(cll_handle, msg_id, &links)?
        };

        // SAE J2534-2 clause 14 Repeat Messaging opportunistic retry
        // (ADR-165 Decision 6, Codex review PR #42 round 2 Finding A): see
        // `retry_leaked_repeat_message_stops`'s doc comment.
        if let Some(channel_key) = channel_key {
            self.retry_leaked_repeat_message_stops(&mut chans, channel_key)
                .await;
        }

        let api = self.api.lock().await;
        let result = api.query_repeat_message(channel_id, msg_id);
        drop(api);

        // Codex review PR #42 round 2 Finding C: a `Condition == 1` slot can
        // self-complete device-side (a matching response, or `time_interval`
        // expiry, ADR-165 Decision 6) with no notification back to this
        // service, leaving its `MsgId` stuck in `repeat_message_ids` forever
        // unless a client happens to STOP it explicitly. The device's own
        // authoritative "this MsgId doesn't exist" answer -- ERR_INVALID_MSG_ID
        // on a QUERY, exactly as on a STOP -- is the pruning trigger: prune
        // it from tracking here too, not just on a successful STOP, so a
        // stale entry never lingers indefinitely nor risks authorizing a
        // later, unrelated slot that happens to reuse the same numeric
        // `MsgId`.
        //
        // Finding G (Codex review, ADR-165 PR #42 round 3): this pruning
        // must happen BEFORE `chans` (`shared_channels`) is released, not
        // after -- releasing it first (the pre-round-3 ordering) reopened
        // the exact race Bug 1/round 1's Finding 1 fix closed: a concurrent
        // Destroy/DisconnectComLogicalLink teardown (which also acquires
        // `shared_channels` before its own `repeat_message_ids` walk,
        // ADR-080) could interleave in the gap and see this `MsgId` still
        // present, issuing a redundant native STOP for an already-gone slot.
        // `logical_links` is acquired nested inside `shared_channels` here,
        // matching this crate's lock hierarchy (ADR-080) and
        // `ioctl_start_repeat_message`'s own ordering.
        if let Err(j2534_0404::Error::ApiStatus { code, .. }) = &result
            && code.as_u32() == j2534_0404::ERR_INVALID_MSG_ID
        {
            let mut links = self.logical_links.lock().await;
            if let Some(link) = links.get_mut(&cll_handle) {
                link.repeat_message_ids.retain(|&id| id != msg_id);
            }
        }
        drop(chans);

        result.map_err(|err| {
            map_native_error_for_link("PassThruIoctl QUERY_REPEAT_MESSAGE", &err, last_error)
        })
    }

    /// **PDU_IOCTL_STOP_REPEAT_MESSAGE (L, ADR-165/Phase 12).** SAE J2534-2
    /// clause 14's STOP command: cancels a running repeat slot via
    /// `PassThruIoctl(STOP_REPEAT_MESSAGE)` and drops its `MsgId` from
    /// `LogicalLinkState::repeat_message_ids`. `input_data` must carry the
    /// `MsgId` (`PDU_IT_IO_UNUM32`) a prior `PDU_IOCTL_START_REPEAT_MESSAGE`
    /// on this same CLL returned -- see
    /// [`Self::require_owned_repeat_message`].
    ///
    /// Holds `shared_channels` locked from before the ownership check through
    /// the native call and the final `repeat_message_ids` removal, same
    /// rationale and pattern as `ioctl_query_repeat_message`/
    /// `ioctl_start_repeat_message` (Bug 1): this closes the window where a
    /// client's own STOP could race a concurrent teardown's best-effort STOP
    /// for the same slot.
    async fn ioctl_stop_repeat_message(
        &self,
        cll_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<(), Status> {
        let msg_id = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::Unum32Value(v)) => v,
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_STOP_REPEAT_MESSAGE requires a PDU_IT_IO_UNUM32 input_data (the \
                     MsgId returned by PDU_IOCTL_START_REPEAT_MESSAGE)",
                ));
            }
        };

        let mut chans = self.shared_channels.lock().await;
        let (channel_id, channel_key, last_error) = {
            let links = self.logical_links.lock().await;
            Self::require_owned_repeat_message(cll_handle, msg_id, &links)?
        };

        // SAE J2534-2 clause 14 Repeat Messaging opportunistic retry
        // (ADR-165 Decision 6, Codex review PR #42 round 2 Finding A): see
        // `retry_leaked_repeat_message_stops`'s doc comment.
        if let Some(channel_key) = channel_key {
            self.retry_leaked_repeat_message_stops(&mut chans, channel_key)
                .await;
        }

        let api = self.api.lock().await;
        let result = api.stop_repeat_message(channel_id, msg_id);
        drop(api);

        // Codex review PR #42 round 2 Finding C: prune this MsgId from
        // tracking not only on a successful STOP (below) but also when the
        // device rejects it with ERR_INVALID_MSG_ID -- a self-completed
        // `Condition == 1` slot (ADR-165 Decision 6) is already gone
        // device-side by the time a client's explicit STOP reaches it, and
        // without this the stale entry would linger in
        // `repeat_message_ids` forever. See `ioctl_query_repeat_message`'s
        // identical fix for the full rationale.
        //
        // Finding G (Codex review, ADR-165 PR #42 round 3): as in
        // `ioctl_query_repeat_message`, this pruning must happen BEFORE
        // `chans` is released -- see that function's comment for the full
        // race this closes.
        let is_invalid_msg_id = matches!(
            &result,
            Err(j2534_0404::Error::ApiStatus { code, .. })
                if code.as_u32() == j2534_0404::ERR_INVALID_MSG_ID
        );
        if result.is_ok() || is_invalid_msg_id {
            let mut links = self.logical_links.lock().await;
            if let Some(link) = links.get_mut(&cll_handle) {
                link.repeat_message_ids.retain(|&id| id != msg_id);
            }
        }
        drop(chans);

        result.map_err(|err| {
            map_native_error_for_link("PassThruIoctl STOP_REPEAT_MESSAGE", &err, last_error)
        })?;
        Ok(())
    }

    /// SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c Fix A, design-
    /// advisor consult, Codex review round 3): TX-dispatch suspension taking
    /// effect must terminate an already-running TP2.0 broadcast periodic, not
    /// merely block a future re-arm -- the governing rule (ADR-192 Decision
    /// item 2) is that every COP-level mechanism this service already has
    /// terminates or tracks a broadcast periodic exactly as it would any
    /// other executing COP, and leaving one running under the identical
    /// suspension sources this ADR's own start-time synchronous rejection
    /// already treats as fatal to a *new* one was the incoherent middle
    /// state that review round found.
    ///
    /// Shared by all four suspension-authority call sites
    /// (`ioctl_suspend_tx_queue` here, and `events.rs`'s three
    /// `CP_SuspendQueueOnError` transition sites -- the batch-final
    /// reconciliation and the two receive-phase timeout hooks) so a future
    /// site can't silently drift from the other three's behavior. Callers
    /// take `periodic` from `link.tp20_broadcast_periodic` in the SAME
    /// critical section that sets their own suspension flag, then call this
    /// AFTER releasing `logical_links` -- the native stop needs `self.api`,
    /// which must never be held together with `logical_links` (ADR-080).
    ///
    /// Codex review fix (P1, PR #101, round 6): `channel_id` is a parameter,
    /// not re-derived here from a fresh `self.logical_links` lookup. Every
    /// caller captures it from the SAME `link` reference it takes `periodic`
    /// from, in that same critical section -- the channel the message was
    /// actually started on. A fresh lookup at call time raced a
    /// disconnect/reconnect on `cll_handle` in between: `None` (a plain
    /// disconnect) was silently treated as "nothing to stop," finalizing the
    /// COP while the old channel's native periodic transmitter stayed live
    /// and untracked; a NEW `channel_id` (a reconnect) meant issuing the
    /// native stop against the WRONG channel with the OLD `message_id`,
    /// risking a collision with an unrelated live message on that new
    /// channel. Using the captured, original channel identity unconditionally
    /// is correct regardless of what happened to `cll_handle` afterward --
    /// if that physical channel has since closed (this CLL disconnected and
    /// was the sole owner), the native call simply fails harmlessly, handled
    /// by the same restore-on-failure/leak logic below either way.
    ///
    /// A `None`-sentinel `message_id` (`rpc_start_com_primitive`'s own
    /// in-flight-start reservation, see `Tp20BroadcastPeriodic`'s doc
    /// comment) means no real native message exists yet -- no native stop is
    /// issued for it, same treatment every other site gives it. As of round
    /// 15's serialization fix (Codex review, P1, PR #101, ADR-193) that skip
    /// is no longer an accepted residual: `self.api` is acquired here
    /// UNCONDITIONALLY, sentinel included, so this function cannot report the
    /// COP terminal until any in-flight `PassThruStartPeriodicMsg` bracket
    /// has finished -- and that bracket, finding its reservation taken by
    /// this function's caller, either never starts the message or
    /// orphan-stops it under that same guard. See the `stop_result` block's
    /// own comment below and
    /// `rpc_primitive.rs::revalidate_tp20_broadcast_periodic_reservation` for
    /// the full design. On success (or the sentinel skip), the COP is finalized
    /// with `PduCopstFinished` -- NOT `PduCopstCancelled` -- mirroring
    /// `CLEAR_PERIODIC_MSGS`'s own reconciliation shape, since like that
    /// ioctl this is a side effect of an administrative action, not a
    /// client-initiated `CancelComPrimitive`. On a real native-call failure,
    /// the entry is restored onto the CLL (mirroring
    /// `rpc_cancel_com_primitive`'s own restore-on-failure) rather than
    /// leak-tracked -- the CLL itself is not going away here, unlike a
    /// shared-channel teardown/hard-error (Fix B). The suspension flag's own
    /// lifecycle is untouched here either way; it belongs entirely to its own
    /// trigger (`PDU_IOCTL_SUSPEND_TX_QUEUE` / the error-suspension
    /// mechanism), not to this stop's outcome.
    ///
    /// Codex review fix (P2, PR #101, round 10): `connect_generation` and
    /// `channel_key` are two more parameters captured, not re-derived, from
    /// the SAME `link`/lock scope every caller already takes `periodic`/
    /// `channel_id` from (same "captured, not re-derived" principle as the
    /// round-6 `channel_id` fix above). The restore-onto-the-CLL behavior
    /// described in the paragraph just above is now gated on `cll_handle`
    /// still being the SAME connect session `periodic` was captured
    /// against (`link.connect_generation == connect_generation`): a
    /// concurrent disconnect (or disconnect+reconnect) racing this stop
    /// failure can make `link.tp20_broadcast_periodic` read as empty for an
    /// unrelated reason (the disconnect's own teardown, or a fresh
    /// reconnect's clean slate), and restoring onto that mismatched session
    /// would wrongly graft an old channel's failed transmitter onto a new
    /// one, with no guarantee anything ever notices it again if that CLL
    /// disconnects a second time. When the generation no longer matches (or
    /// the link is gone entirely), the failed stop is leak-tracked against
    /// `channel_key`'s `SharedChannel::leaked_periodic_message_ids` instead
    /// -- unconditionally, not gated on a sibling ref count, mirroring
    /// `DisconnectComLogicalLink`'s own Fix B shape -- or, if that physical
    /// channel has also fully closed by the time this runs, logged as an
    /// accepted-residual double-fault with nothing left to track. Per
    /// ADR-080, `shared_channels` is acquired before `logical_links` in
    /// this branch even though the same-session path never touches it, to
    /// keep the lock ordering uniform across both outcomes.
    ///
    /// Codex review fix (P2, PR #101, round 11): the restore-vs-leak-track
    /// decision described in the paragraphs above now lives in the shared
    /// `restore_or_leak_track_broadcast_periodic` helper (defined just
    /// below this function), corrected there for finding 1 (the
    /// `same_session` gate now also requires `link.connected`, not just a
    /// matching `connect_generation` -- a plain disconnect with no
    /// reconnect never bumps the generation) and shared with
    /// `rpc_cancel_com_primitive`'s structurally identical `CoptCancel`
    /// failure branch (finding 2), which never got round 10's protection
    /// at all. See that helper's own doc comment for the corrected
    /// reasoning in full.
    ///
    /// Codex review fix (P2, PR #101, round 15): a `stop_periodic_message`
    /// failure of exactly `ERR_INVALID_MSG_ID` no longer takes the
    /// restore-or-leak-track path above at all -- it is an AUTHORITATIVE
    /// "the device already forgot this message" signal, not a genuine
    /// failure, produced when `CLEAR_PERIODIC_MSGS` wins `self.api`'s lock
    /// after this function's caller already took the entry off
    /// `link.tp20_broadcast_periodic` but before this call's own native
    /// stop runs. Before this fix, that race unconditionally restored (or
    /// leak-tracked) the now-stale entry, wrongly perpetuating tracking for
    /// a message that no longer exists device-side and leaving the COP
    /// reported `Executing` until an explicit cancellation cleaned it up.
    /// Mirrors round 14's identical fix to `rpc_cancel_com_primitive`'s
    /// `CoptCancel` handling (`rpc_primitive.rs`) and this crate's two
    /// earlier precedents for the same idiom,
    /// `retry_leaked_periodic_message_stops` and
    /// `retry_leaked_repeat_message_stops` (both above in this file): on
    /// `ERR_INVALID_MSG_ID`, this function falls through to the same clean
    /// `PduCopstFinished` finalization the success arm already performs.
    pub(super) async fn terminate_tp20_broadcast_periodic_for_suspension(
        &self,
        cll_handle: u32,
        periodic: Tp20BroadcastPeriodic,
        channel_id: Option<ChannelId>,
        connect_generation: u64,
        channel_key: Option<ChannelKey>,
    ) {
        let stop_result = {
            // Codex review round 15 Fix 2 (P1, PR #101, ADR-193): `self.api`
            // is acquired UNCONDITIONALLY here, including for a
            // `None`-sentinel entry that issues no native call at all --
            // acquiring it is the point. `self.api` is the serialization
            // fence for an in-flight `PassThruStartPeriodicMsg`
            // (`revalidate_tp20_broadcast_periodic_reservation` and
            // `take_broadcast_periodic_under_api_locked` describe the whole
            // design); this function's four callers take the entry off the
            // link under `logical_links` alone, so without this acquisition
            // the `PduCopstFinished` finalization below could report the COP
            // terminal while the start's native call was still in flight and
            // about to emit SAE J2534-2 clause 19.3.2.3's five-frame burst.
            // Waiting for the fence guarantees the opposite ordering: either
            // the start has not yet revalidated (and will now find its
            // reservation gone -- taken by our caller -- and abort without
            // transmitting), or it already committed inside its own `api`
            // bracket and that bracket's own resolution has already
            // orphan-stopped the message it started. The guard is released
            // again before the restore-or-leak-track/finalization work below,
            // which needs no fence and must not hold `self.api` while
            // acquiring `shared_channels` (ADR-080's outermost lock).
            let api = self.api.lock().await;
            match (periodic.message_id, channel_id) {
                (Some(id), Some(channel_id)) => {
                    Some((id, api.stop_periodic_message(channel_id, id)))
                }
                _ => None,
            }
        };
        // Codex review fix (P2, PR #101, round 15): `ERR_INVALID_MSG_ID` from
        // this stop is an AUTHORITATIVE "the device already forgot this
        // message" signal, not a genuine failure -- a `CLEAR_PERIODIC_MSGS`
        // that wins `self.api`'s lock after this function's caller already
        // took the tracking entry off the link (mirroring the race round
        // 14's `CoptCancel` fix in `rpc_primitive.rs` documents in full) but
        // before this call's own native stop runs has already cleared the
        // message device-side by the time we get here. Treat it exactly the
        // way round 14's `CoptCancel` fix, and this crate's two earlier
        // precedents for the same idiom --
        // `retry_leaked_periodic_message_stops` and
        // `retry_leaked_repeat_message_stops` (both above in this file) --
        // already treat it: fall through to the same clean finalization the
        // success arm below performs, instead of restoring or leak-tracking
        // an entry for a message that no longer exists.
        let is_invalid_msg_id = matches!(
            &stop_result,
            Some((_, Err(j2534_0404::Error::ApiStatus { code, .. })))
                if code.as_u32() == j2534_0404::ERR_INVALID_MSG_ID
        );
        match stop_result {
            Some((id, Err(err))) if !is_invalid_msg_id => {
                warn!(
                    cll_handle,
                    cop_handle = periodic.cop_handle,
                    message_id = id.0,
                    %err,
                    "PassThruStopPeriodicMsg failed while terminating a TP2.0 broadcast \
                     periodic for TX-dispatch suspension; retaining tracking for a later retry"
                );
                self.restore_or_leak_track_broadcast_periodic(
                    cll_handle,
                    periodic,
                    id,
                    CapturedBroadcastPeriodicSession {
                        connect_generation,
                        channel_key,
                        channel_id,
                    },
                    "TX-dispatch suspension termination",
                )
                .await;
            }
            _ => {
                if let Some((id, Err(_))) = &stop_result {
                    debug!(
                        cll_handle,
                        cop_handle = periodic.cop_handle,
                        message_id = id.0,
                        "TX-dispatch suspension TP2.0 broadcast periodic STOP got \
                         ERR_INVALID_MSG_ID -- device already forgot this message, treating as \
                         cleaned up"
                    );
                }
                // Codex review (edge-case-hunter, PR #101 follow-up):
                // `events::emit_terminal_if_live`, not a hand-rolled
                // remove-then-send -- the hand-rolled version's `drop(prims)`
                // before the `send_cop_status` await reopens the exact A2-23
                // race `emit_terminal_if_live` exists to close (a concurrent
                // `CancelComPrimitive`/`GetStatus` landing in the gap sees a
                // miss on both `primitives` and `terminal_cops`, and reports
                // this cop_handle as never having existed), and never drains
                // this cop_handle's own `cancelled_cops` mark the way
                // `emit_terminal_if_live` also does. See that function's own
                // doc comment (`events_event_senders.rs`) for the full
                // rationale.
                events::emit_terminal_if_live(
                    &self.primitives,
                    &self.logical_links,
                    &self.subscriptions,
                    &self.terminal_cops,
                    cll_handle,
                    periodic.cop_handle,
                    vci_service_interface::PduComPrimitiveStatus::PduCopstFinished,
                )
                .await;
            }
        }
    }

    /// Shared restore-vs-leak-track decision for a failed native TP2.0
    /// broadcast-periodic `PassThruStopPeriodicMsg` call (Codex review,
    /// round 11, PR #101). Both call sites that can terminate an
    /// already-running TP2.0 broadcast periodic and hit a real native
    /// stop failure --
    /// `terminate_tp20_broadcast_periodic_for_suspension` (TX-dispatch
    /// suspension) and `rpc_cancel_com_primitive`'s `CoptCancel` handling
    /// (`rpc_primitive.rs`) -- call this AFTER the native
    /// `stop_periodic_message` call has already failed, passing the
    /// `periodic`/`connect_generation`/`channel_key`/`channel_id` they
    /// captured from the SAME `logical_links` critical section `periodic`
    /// itself came from, before releasing that lock to make the (now
    /// failed) native call. `context` is a short caller-identifying
    /// string used only in this function's own `warn!` messages, so each
    /// call site's own log lines stay distinguishable.
    ///
    /// Before round 11, this decision was duplicated: written once inline
    /// in `terminate_tp20_broadcast_periodic_for_suspension` (round 10),
    /// and never applied at all to `rpc_cancel_com_primitive`'s
    /// structurally identical `CoptCancel` failure branch -- exactly the
    /// kind of drift a fix applied to only one of two identical branches
    /// produces. Both call sites now share this one implementation.
    ///
    /// Codex review fix (P2, PR #101, round 11 finding 1): `same_session`
    /// requires `link.connected`, not just a matching `connect_generation`
    /// -- a PLAIN disconnect (no reconnect) does not bump
    /// `connect_generation` (`rpc_link.rs`'s disconnect path only clears
    /// `connected`/`channel_id`), so generation alone would still read
    /// `true` after a plain disconnect and wrongly restore the failed
    /// entry onto the now-disconnected link. Mirrors
    /// `reserve_tp20_broadcast_periodic`'s own identical
    /// `l.connect_generation != snapshot_connect_generation || !l.connected`
    /// gate (`rpc_primitive.rs`; see that function's own doc comment for
    /// the same rationale in more detail).
    ///
    /// When `same_session` holds, the entry is restored onto
    /// `link.tp20_broadcast_periodic` only if that slot is still
    /// genuinely empty -- a concurrent `StartComPrimitive` could have
    /// legitimately written a fresh `None`-sentinel reservation for a
    /// DIFFERENT cop_handle into this now-empty slot while the caller's
    /// own `logical_links` lock was released for the native call;
    /// resurrecting the old (now-erroring) entry unconditionally would
    /// clobber that fresh, unrelated reservation.
    ///
    /// Codex review fix (P2, PR #101, round 13): if the slot WAS
    /// reclaimed, this now falls through to the same leak-tracking path
    /// used for a session mismatch, instead of dead-ending in a `warn!`
    /// with nothing tracked. Before round 11 unified the two call sites
    /// into this shared helper, `shared_channels` was not yet locked in
    /// scope at this point at all, so there was no lock to leak-track
    /// through -- the reclaimed-slot case was an accepted residual for
    /// that reason (previously documented in
    /// the backlog). That justification went
    /// stale the moment round 11 moved this logic into
    /// `restore_or_leak_track_broadcast_periodic`: `chans` (the
    /// `shared_channels` guard) is already acquired and held for this
    /// function's entire body, below, before `same_session` is even
    /// computed -- so the reclaimed-slot case can, and now does, share
    /// the exact same fallback as a session mismatch. Without this, the
    /// old transmitter stays active on the device but ends up tracked in
    /// neither `tp20_broadcast_periodic` (reclaimed by the fresh
    /// reservation) nor `leaked_periodic_message_ids` (only the
    /// session-mismatch path used to leak-track), so nothing --
    /// including `LOCK_PHYSICAL_TX_QUEUE`'s active-transmission scan,
    /// which checks both those places -- could ever detect or retry
    /// stopping it.
    ///
    /// Otherwise (generation mismatch, not connected, the link is gone
    /// entirely, or the slot was reclaimed as above), the failed stop is
    /// leak-tracked against `channel_key`'s
    /// `SharedChannel::leaked_periodic_message_ids` instead --
    /// unconditionally, not gated on a sibling ref count, mirroring
    /// `DisconnectComLogicalLink`'s own Fix B shape -- but ONLY if the
    /// live `SharedChannel::channel_id` still equals the captured
    /// `channel_id` too (edge-case-hunter finding, PR #101 round 10
    /// verification): `ChannelKey` alone (protocol/baud/flags/pin) has no
    /// uniqueness-over-time guarantee -- a channel close followed by an
    /// unrelated fresh `ConnectComLogicalLink` at the same params installs
    /// a brand-new `SharedChannel` at the identical key, and leak-tracking
    /// onto it would corrupt an unrelated session's tracking. Accepted
    /// residual, NOT airtight: per `next_connect_generation`'s own doc
    /// comment (`service.rs`), a `channel_id` CAN coincidentally repeat on
    /// a shared channel even without this exact race; a fully airtight
    /// check would compare `SharedChannel::occupancy_epoch` (ADR-161) the
    /// way `ioctl_reset`'s Phase 1 does, but that requires capturing it at
    /// the SAME time as `periodic`/`channel_id`/`connect_generation` at
    /// every call site -- judged out of scope; tracked in
    /// the backlog. If no matching channel is
    /// found at all, this is logged as an accepted-residual double-fault
    /// with nothing left to restore or leak-track -- the log line
    /// distinguishes session-mismatch from slot-reclaimed so a future
    /// reader debugging a leak can tell the two causes apart.
    ///
    /// Per ADR-080, `shared_channels` is acquired before `logical_links`
    /// here even though the restore path never touches it, to keep the
    /// lock ordering uniform across all outcomes.
    pub(super) async fn restore_or_leak_track_broadcast_periodic(
        &self,
        cll_handle: u32,
        periodic: Tp20BroadcastPeriodic,
        message_id: j2534_0404::PeriodicMessageId,
        session: CapturedBroadcastPeriodicSession,
        context: &str,
    ) {
        let mut chans = self.shared_channels.lock().await;
        let mut links = self.logical_links.lock().await;
        let same_session = links.get(&cll_handle).is_some_and(|link| {
            link.connect_generation == session.connect_generation && link.connected
        });
        let slot_reclaimed = same_session
            && links
                .get(&cll_handle)
                .is_some_and(|link| link.tp20_broadcast_periodic.is_some());
        if same_session && !slot_reclaimed {
            if let Some(link) = links.get_mut(&cll_handle) {
                link.tp20_broadcast_periodic = Some(periodic);
            }
            return;
        }
        // Either the session no longer matches (disconnect/reconnect raced
        // this failure), or it does but `slot_reclaimed` shows a fresh
        // reservation already occupies `tp20_broadcast_periodic` -- either
        // way, writing onto `link.tp20_broadcast_periodic` here would be
        // unsafe, so fall through to leak-tracking against the shared
        // channel instead (Codex review fix, P2, PR #101, round 13).
        if let Some(sc) = session
            .channel_key
            .and_then(|key| chans.get_mut(&key))
            .filter(|sc| Some(sc.channel_id) == session.channel_id)
        {
            if !sc
                .leaked_periodic_message_ids
                .iter()
                .any(|&(existing_id, _)| existing_id == message_id)
            {
                sc.leaked_periodic_message_ids
                    .push((message_id, periodic.started_epoch));
            }
        } else {
            let reason = if slot_reclaimed {
                "this CLL's tracking slot was reclaimed by a fresh reservation and the \
                 original physical channel has also fully closed or been replaced"
            } else {
                "the original connect session is gone and its physical channel has also \
                 fully closed or been replaced"
            };
            warn!(
                cll_handle,
                cop_handle = periodic.cop_handle,
                message_id = message_id.0,
                slot_reclaimed,
                "TP2.0 broadcast periodic STOP failed during {context}, but {reason} -- \
                 accepted-residual double-fault, nothing left to restore or leak-track"
            );
        }
    }

    /// The shared "take a TP2.0 broadcast-periodic tracking entry safely"
    /// primitive every id-targeted terminator now goes through (Codex review
    /// round 15 Fix 2, P1, PR #101, ADR-193 partially superseding ADR-192
    /// Decision item 2's in-flight-reservation mechanism).
    ///
    /// **The bug this closes.** `reserve_tp20_broadcast_periodic`
    /// (`rpc_primitive.rs`) writes its `Tp20BroadcastPeriodic { message_id:
    /// None, .. }` reservation under `logical_links` alone and releases that
    /// lock; `rpc_start_com_primitive` only acquires `self.api` afterward,
    /// and the native `PassThruStartPeriodicMsg` call happens later still.
    /// Every terminator used to take that `None`-sentinel entry -- and report
    /// the owning COP terminal to the client -- holding nothing but
    /// `logical_links`, with no synchronization at all against that in-flight
    /// native call. Since SAE J2534-2 clause 19.3.2.3's five-frame burst is
    /// emitted synchronously inside the native call, the burst still went out
    /// AFTER the client had been told the COP was cancelled (or after a
    /// `PDU_IOCTL_SUSPEND_TX_QUEUE` had reported success), and the
    /// after-the-fact orphan stop could not retract frames already on the
    /// wire.
    ///
    /// **The fence.** `self.api`'s mutex is now the serialization point for
    /// in-flight starts: the start bracket holds it across the temp-param
    /// apply, the native start, AND the resolution that commits the real
    /// `message_id`
    /// (`finalize_or_orphan_broadcast_periodic_start_locked`), and it
    /// re-validates its own reservation under that same guard before starting
    /// anything (`revalidate_tp20_broadcast_periodic_reservation`). A
    /// terminator that inspects the entry only while holding `self.api`
    /// therefore cannot observe a half-resolved state; exactly one of two
    /// orderings happens, and this function's three outcomes are exactly
    /// those two plus "somebody else already did it":
    ///
    /// - **Still this `cop_handle`'s `None`-sentinel** (`Some`, with
    ///   `periodic.message_id == None`): the terminator won the fence. The
    ///   entry is taken here, so the start's own revalidation -- which cannot
    ///   run until this caller releases `self.api` -- will find its
    ///   reservation gone and abort without ever calling the native start.
    ///   No native stop is needed or possible (there is no id); the caller is
    ///   free to report its COP terminal.
    /// - **Now `Some(real id)`**: the start bracket won the fence and
    ///   committed before this caller acquired `self.api`. The entry is taken
    ///   here too, and the caller runs its ordinary live-stop machinery for
    ///   that real id, exactly as it always has for a committed entry.
    /// - **Absent** (`None`): the slot is empty, the CLL is gone, or a
    ///   DIFFERENT `cop_handle` owns the slot now -- another resolver already
    ///   acted on it, so this is a no-op.
    ///
    /// **Id-targeted callers only.** `cop_handle` names the exact COP whose
    /// entry this take is for (`rpc_cancel_com_primitive`'s `CoptCancel` is
    /// the only such caller today); the entry is left alone if a DIFFERENT
    /// COP owns the slot. A CLL-wide teardown deliberately does NOT go
    /// through this helper: `DisconnectComLogicalLink` takes its entry
    /// atomically with its own `channel_id`/`channel_key` clear, under
    /// `logical_links` alone, and fences only the native-stop DECISION (see
    /// ADR-193 Decision item 3 and that function's own comment) --
    /// `ioctl_suspend_tx_queue`/
    /// `terminate_tp20_broadcast_periodic_for_suspension` use the same shape.
    /// Taking late, from inside the fence, would leave the CLL observable
    /// with a live entry but an already-cleared `channel_id`, which is the
    /// very inconsistency ADR-193 exists to prevent.
    ///
    /// `api` is the caller's own guard deref, taken purely as proof the fence
    /// is held -- the `_locked` shape this crate already uses for
    /// `apply_params_to_hardware_locked`/`revert_hardware_to_live_active_locked`.
    /// Passing the guard rather than acquiring `self.api` internally is what
    /// lets the caller issue its native `stop_periodic_message` for the
    /// committed-id case under the SAME guard this take happened under.
    /// `logical_links` is nested inside it, the ADR-110-sanctioned order.
    pub(super) async fn take_broadcast_periodic_under_api_locked(
        &self,
        _api: &J2534Api0404,
        cll_handle: u32,
        cop_handle: u32,
    ) -> Option<TakenBroadcastPeriodic> {
        let mut links = self.logical_links.lock().await;
        let link = links.get_mut(&cll_handle)?;
        if link
            .tp20_broadcast_periodic
            .is_none_or(|p| p.cop_handle != cop_handle)
        {
            return None;
        }
        let periodic = link.tp20_broadcast_periodic.take()?;
        // Defensive cleanup (ADR-192/Phase 7 Stage 7c edge-case-hunter fix,
        // moved here from `rpc_cancel_com_primitive` by round 15's shared-
        // helper extraction): a stale `cancelled_cops` mark for this COP
        // could have been left over from a `CoptStopcomm`/`CLEAR_TX_QUEUE`
        // sweep that ran before those sites' own `tp20_broadcast_periodic`
        // exclusion existed, or from any future gap in that exclusion -- this
        // COP's real teardown, right here, must not leave a stale mark
        // outliving it.
        link.cancelled_cops.remove(&periodic.cop_handle);
        Some(TakenBroadcastPeriodic {
            periodic,
            channel_id: link.channel_id,
            queue_target: Some(events::CllQueueTarget::from_link(link)),
            connect_generation: link.connect_generation,
            channel_key: link.channel_key,
        })
    }

    /// **PDU_IOCTL_SUSPEND_TX_QUEUE (L).** Sets `tx_suspended_by_ioctl = true`
    /// (leaving `tx_suspended_by_lock` untouched, ADR-123). `events::dispatch_tx_item`
    /// checks `tx_suspended()` (the OR of both flags) for every item belonging
    /// to this CLL -- including cyclic/periodic follow-up cycles -- and holds
    /// it in `tx_held` instead of executing it.
    ///
    /// SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c Fix A): also
    /// terminates an already-running TP2.0 broadcast periodic on this CLL, in
    /// the same critical section that sets the flag -- see
    /// `terminate_tp20_broadcast_periodic_for_suspension`'s own doc comment.
    async fn ioctl_suspend_tx_queue(&self, cll_handle: u32) -> Result<(), Status> {
        let periodic = {
            let mut links = self.logical_links.lock().await;
            let link = links
                .get_mut(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            link.tx_suspended_by_ioctl = true;
            link.tp20_broadcast_periodic.take().map(|periodic| {
                (
                    periodic,
                    link.channel_id,
                    link.connect_generation,
                    link.channel_key,
                )
            })
        };
        if let Some((periodic, channel_id, connect_generation, channel_key)) = periodic {
            self.terminate_tp20_broadcast_periodic_for_suspension(
                cll_handle,
                periodic,
                channel_id,
                connect_generation,
                channel_key,
            )
            .await;
        }
        Ok(())
    }

    /// **PDU_IOCTL_RESUME_TX_QUEUE (L).** Sets `tx_suspended_by_ioctl = false`
    /// and, per ADR-147, `tx_suspended_by_error = false` too -- a client's
    /// explicit `PDU_IOCTL_RESUME_TX_QUEUE` is documented as a full manual
    /// escape from any suspend source that protects no sibling CLL's
    /// privilege, including an error-triggered suspension (leaving
    /// `tx_suspended_by_lock` untouched, ADR-123 -- a sibling CLL's
    /// held `LOCK_PHYSICAL_TX_QUEUE` must keep suspending this CLL regardless
    /// of this client-issued resume). Routes through
    /// `LogicalLinkState::clear_error_suspension`, in this same critical
    /// section, which bumps `error_clear_seq` UNCONDITIONALLY (ADR-147
    /// third amendment, capture-at-fold sequencing) -- this explicit resume
    /// must invalidate any `CP_SuspendQueueOnError` `Suspend` classification
    /// a concurrent poll pass computed before this call but has not yet
    /// applied, so that pass's end-of-pass writeback cannot silently
    /// re-suspend the queue behind this resume. Unconditional is safe here
    /// because the staleness check moved to the CAPTURE side
    /// (`CllRxEntry::suspend_seq`, captured at the exact moment a frame's
    /// classification folds `Suspend` into the entry -- fifth amendment,
    /// split direction-specific anchors; a `Positive` classification is
    /// never compared against this counter at all, see `error_clear_seq`'s
    /// own doc comment), not at pass-snapshot time -- this bump can only
    /// ever invalidate a fold that already happened before it, never a
    /// fresh one whose fold happens after (which captures the bumped seq
    /// itself and so still applies). Then sends a content-free
    /// `TxItem::ResumeWake` onto the owning `SharedChannel`'s `tx_queue` --
    /// NOT the held items themselves (Codex-review fix: re-injecting real
    /// items into the shared queue's tail can reorder them behind this CLL's
    /// own items still in flight in that same queue). The wake merely
    /// guarantees the poll loop dequeues *something* for this CLL soon, which
    /// triggers `drain_tx_held_backlog` (events.rs) to flush the actual
    /// `tx_held` backlog in FIFO order -- provably correct because everything
    /// held is older than anything still queued for this CLL. A spurious wake
    /// while still suspended by `tx_suspended_by_lock` is harmless: the
    /// siphon re-checks `tx_suspended()` and re-holds the item. When the CLL
    /// is not currently connected to a physical channel, nothing is sent --
    /// `tx_held` drains on the next `PDU_IOCTL_RESUME_TX_QUEUE` call after
    /// this CLL reconnects (unchanged behavior).
    ///
    /// **`connect_generation`-gated (Codex review finding r3680291021,
    /// PR #20).** `channel_key`/`tx_queue` are captured under an EARLIER,
    /// separate `logical_links` acquisition than the one that actually
    /// clears the suspend flags (the two are separate critical sections
    /// because `tx_queue` is read from `shared_channels`, which must never
    /// be nested inside `logical_links`, ADR-080). A same-handle disconnect
    /// and reconnect completing in the gap between them would otherwise let
    /// this call clear a brand-new session's suspension using a `tx_queue`
    /// still pointing at the OLD session's channel -- the wake would reach
    /// the wrong (or already-torn-down) poll task, stranding anything the
    /// new session already parked. `connect_generation` is captured
    /// alongside `channel_key` in the first acquisition and re-checked
    /// against the live value in the second; both the flag clears and the
    /// wake send are skipped entirely on a mismatch, matching the same
    /// freshness-gate pattern every other explicit-clear site in this
    /// mechanism already uses (`bind_frame`'s `CoptUpdateparam` promotion
    /// path checks `connect_generation` before its own `clear_error_suspension`
    /// call for the identical reason). A skipped clear is not a stranded
    /// resume: the new session's own next explicit action, or its own COP's
    /// eventual receive-phase timeout/positive response, resolves it —
    /// this call simply declines to act on a connection it no longer
    /// matches, rather than acting on the wrong one.
    async fn ioctl_resume_tx_queue(&self, cll_handle: u32) -> Result<(), Status> {
        let (channel_key, connect_generation) = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            (link.channel_key, link.connect_generation)
        };
        let tx_queue = match channel_key {
            Some(channel_key) => {
                let chans = self.shared_channels.lock().await;
                chans.get(&channel_key).map(|sc| sc.tx_queue.clone())
            }
            None => None,
        };

        // Codex PR review finding r3680291021 (PR #20): a same-handle
        // disconnect+reconnect completing between the `channel_key` capture
        // above and this second `logical_links` acquisition must not let
        // this RPC clear a brand-new session's suspension state using a
        // `tx_queue` captured from the OLD session's channel -- the wake
        // would reach the wrong (or a torn-down) channel's poll task,
        // stranding anything the new session has already parked.
        // `connect_generation` is re-checked here (the same freshness
        // signal `finalize_connected_link`/ADR-086 stamps on every connect,
        // including a reconnect of the same `cll_handle`) so both this
        // RPC's own explicit-suspension clear and the wake are scoped to
        // the exact connection this call captured, not whatever session
        // happens to be live by the time the second lock is acquired.
        let mut links = self.logical_links.lock().await;
        let link = links
            .get_mut(&cll_handle)
            .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
        let same_connection = link.connect_generation == connect_generation;
        if same_connection {
            link.tx_suspended_by_ioctl = false;
            link.clear_error_suspension();
        }
        drop(links);

        if same_connection && let Some(tx_queue) = tx_queue {
            let _ = tx_queue.send(TxItem::ResumeWake { cll_handle });
        }
        Ok(())
    }

    /// **PDU_IOCTL_CLEAR_RX_QUEUE (L).** Clears this CLL's RX ring buffer
    /// (`rx_buf`).
    async fn ioctl_clear_rx_queue(&self, cll_handle: u32) -> Result<(), Status> {
        let rx_buf = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            Arc::clone(&link.rx_buf)
        };
        rx_buf.lock().await.items.clear();
        Ok(())
    }

    /// **PDU_IOCTL_READ_VBATT (M).** Reads battery voltage in millivolts via
    /// `PassThruIoctl(READ_VBATT)`. Rejects with `PDU_ERR_MODULE_NOT_CONNECTED`
    /// rather than lazily opening the device if `module_handle` has not been
    /// connected via `ModuleConnect` yet (A2-8).
    async fn ioctl_read_vbatt(&self, module_handle: u32) -> Result<u32, Status> {
        let (_device_guard, device_id) = self.require_connected_device_for(module_handle).await?;
        // Bind the native call's Result before matching on it, and read
        // `last_error` only on failure, after the call: a pre-call snapshot (or
        // an `if let`/`match` directly on the api-lock expression, which would
        // keep the guard alive for the whole arm) can miss a hard-channel-error
        // update the poll task makes while this RPC is still waiting for the
        // same `self.api` lock (Codex review, ADR-105).
        let result = self.api.lock().await.read_vbatt(device_id);
        match result {
            Ok(mv) => Ok(mv),
            Err(err) => {
                let last_error = self.module_state.lock().await.last_error.clone();
                Err(map_native_error_for_link(
                    "PassThruIoctl READ_VBATT",
                    &err,
                    Some(last_error),
                ))
            }
        }
    }

    /// **PDU_IOCTL_SET_PROG_VOLTAGE (M).** Input `PDU_IT_IO_PROG_VOLTAGE`
    /// (`ProgVoltage_mv`, `PinOnDLC`). Calls
    /// `PassThruSetProgrammingVoltage` and, on success, mirrors the value
    /// into `J2534Service::prog_voltage` (pin -> mV). Rejects with
    /// `PDU_ERR_MODULE_NOT_CONNECTED` rather than lazily opening the device
    /// if `module_handle` has not been connected via `ModuleConnect` yet
    /// (A2-8).
    ///
    /// SAE J2534-2 clause 15 adds Short-to-Ground support on pin 9
    /// (alongside J2534-1's existing pin 15), not mutually exclusive with
    /// other programming pins, though Short-to-Ground itself is exclusive to
    /// one pin at a time; no new pin validation is needed here since this
    /// call already forwards `pin_on_dlc`/`prog_voltage_mv` to the native
    /// call with no pin allowlist. The native call reports three distinct
    /// failure conditions, mapped 3-way below (Phase 13):
    /// `ERR_PIN_INVALID` (invalid pin/resource) -> `PDU_ERR_MUX_RSC_NOT_SUPPORTED`
    /// (A2-21, unchanged); `ERR_PIN_IN_USE` (setting voltage on pin 9 while
    /// it is grounded, or vice versa) / `ERR_VOLTAGE_IN_USE` (grounding pin 9
    /// while pin 15 is grounded, or vice versa) -> `PDU_ERR_RESOURCE_BUSY`,
    /// matching the `ERR_CHANNEL_IN_USE` precedent (`error.rs`); everything
    /// else (including a device that does not support pin 9
    /// Short-to-Ground, reported as `ERR_NOT_SUPPORTED`) -> the generic
    /// `PDU_ERR_VOLTAGE_NOT_SUPPORTED` catch-all.
    async fn ioctl_set_prog_voltage(
        &self,
        module_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<(), Status> {
        let (_device_guard, device_id) = self.require_connected_device_for(module_handle).await?;
        let prog_voltage = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::ProgVoltage(pv)) => pv,
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_SET_PROG_VOLTAGE requires PDU_IT_IO_PROG_VOLTAGE input_data",
                ));
            }
        };
        // ADR-185 Stage 2: SAE J2534-2 clause 15's pin-9 Short-to-Ground case
        // only -- pin 15's J2534-1-era Short-to-Ground and every other pin
        // stay native-call-only, matching Decision 2's "pin-9 short-to-ground
        // case only" scope. `prog_voltage_mv` carries three distinct
        // semantics per SAE J2534-1 v04.04 §7.2.11.3's "Voltage Values"
        // table: a real millivolt value, the `SHORT_TO_GROUND` sentinel
        // (0xFFFFFFFE), or the `VOLTAGE_OFF` sentinel (0xFFFFFFFF). Only the
        // short-to-ground sentinel on pin 9 gets a Discovery precheck here,
        // against `DEVICE_INFO_SHORT_TO_GND_J1962` -- a real voltage value on
        // pin 9 is not a short-to-ground request at all, and `VOLTAGE_OFF` is
        // deliberately out of Stage 2's scope entirely (Decision 2); neither
        // should be checked against `SHORT_TO_GND_J1962` (the wrong
        // capability) or any other Discovery flag yet. `PGM_VOLTAGE_J1962`
        // (the separate clause-15 programming-voltage-on-pin-9 capability)
        // remains unwired for the same reason -- see the implementation notes
        // backlog entry.
        if prog_voltage.pin_on_dlc == 9 && prog_voltage.prog_voltage_mv == SHORT_TO_GROUND {
            let last_error = self.module_state.lock().await.last_error.clone();
            self.enforce_discovery_capability(
                module_handle,
                discovery::DeviceAccess::AlreadyOpen(device_id),
                discovery::DiscoveryCheck::DeviceFlag {
                    parameter: j2534_0404::DEVICE_INFO_SHORT_TO_GND_J1962,
                    input_value: 1u32 << 8, // bit 8 = pin 9 (Table 111, clause 25.3.2.2)
                },
                "PDU_IOCTL_SET_PROG_VOLTAGE",
                PduError::PduErrVoltageNotSupported,
                Some(last_error),
            )
            .await?;
        }
        // See ioctl_read_vbatt above: last_error is read after the native call
        // fails, not before waiting for the api lock (Codex review, ADR-105).
        let result = self.api.lock().await.set_programming_voltage(
            device_id,
            prog_voltage.pin_on_dlc,
            prog_voltage.prog_voltage_mv,
        );
        if let Err(err) = result {
            let last_error = self.module_state.lock().await.last_error.clone();
            // ISO 22900-2:2009 Table 49 reserves PDU_ERR_MUX_RSC_NOT_SUPPORTED
            // for an invalid pin/resource, distinct from
            // PDU_ERR_VOLTAGE_NOT_SUPPORTED for an unsupported voltage value;
            // ERR_PIN_INVALID is the only native status that maps to the
            // former here (A2-21). SAE J2534-2 clause 15 adds two further
            // pin-9 Short-to-Ground state-conflict codes, ERR_PIN_IN_USE
            // (same-pin conflict) and ERR_VOLTAGE_IN_USE (cross-pin
            // Short-to-Ground exclusivity conflict against pin 15), which map
            // to PDU_ERR_RESOURCE_BUSY instead of falling through to the
            // generic PDU_ERR_VOLTAGE_NOT_SUPPORTED catch-all (Phase 13).
            let native_code = match &err {
                j2534_0404::Error::ApiStatus { code, .. } => Some(code.as_u32()),
                _ => None,
            };
            let (context, pdu_error) = match native_code {
                Some(j2534_0404::ERR_PIN_INVALID) => (
                    "PDU_ERR_MUX_RSC_NOT_SUPPORTED: PassThruSetProgrammingVoltage",
                    PduError::PduErrMuxRscNotSupported,
                ),
                Some(j2534_0404::ERR_PIN_IN_USE) | Some(j2534_0404::ERR_VOLTAGE_IN_USE) => (
                    "PDU_ERR_RESOURCE_BUSY: PassThruSetProgrammingVoltage",
                    PduError::PduErrResourceBusy,
                ),
                _ => (
                    "PDU_ERR_VOLTAGE_NOT_SUPPORTED: PassThruSetProgrammingVoltage",
                    PduError::PduErrVoltageNotSupported,
                ),
            };
            return Err(map_native_error_as(
                context,
                &err,
                pdu_error,
                Some(last_error),
            ));
        }
        self.prog_voltage
            .lock()
            .await
            .insert(prog_voltage.pin_on_dlc, prog_voltage.prog_voltage_mv);
        Ok(())
    }

    /// **PDU_IOCTL_READ_PROG_VOLTAGE (M).** Reads J1962 programming voltage
    /// in millivolts via `PassThruIoctl(READ_PROG_VOLTAGE)`. Rejects with
    /// `PDU_ERR_MODULE_NOT_CONNECTED` rather than lazily opening the device
    /// if `module_handle` has not been connected via `ModuleConnect` yet
    /// (A2-8).
    async fn ioctl_read_prog_voltage(&self, module_handle: u32) -> Result<u32, Status> {
        let (_device_guard, device_id) = self.require_connected_device_for(module_handle).await?;
        // See ioctl_read_vbatt above: last_error is read after the native call
        // fails, not before waiting for the api lock (Codex review, ADR-105).
        let result = self.api.lock().await.read_prog_voltage(device_id);
        match result {
            Ok(mv) => Ok(mv),
            Err(err) => {
                let last_error = self.module_state.lock().await.last_error.clone();
                Err(map_native_error_for_link(
                    "PassThruIoctl READ_PROG_VOLTAGE",
                    &err,
                    Some(last_error),
                ))
            }
        }
    }

    /// **PDU_IOCTL_READ_J1962PIN_VOLTAGE (M).** Input `PDU_IT_IO_UNUM32`: the
    /// J1962 pin number (1-16) to read. Calls
    /// `PassThruIoctl(READ_J1962PIN_VOLTAGE)` (SAE J2534-2 clause 23) and
    /// returns the voltage in millivolts. Rejects with
    /// `PDU_ERR_MODULE_NOT_CONNECTED` rather than lazily opening the device
    /// if `module_handle` has not been connected via `ModuleConnect` yet
    /// (A2-8), and with `PDU_ERR_ID_NOT_SUPPORTED` if the connected module
    /// has not opted into SAE J2534-2 (clause 5's `"J2534-2:"` `pname`
    /// prefix) -- gated the same way `ioctl_start_repeat_message` gates
    /// clause 14 (Codex review, PR #48: this check was originally missing,
    /// letting a base J2534-1 module reach a clause-23-only extension).
    /// `ERR_PIN_INVALID` (unsupported pin, e.g. pin 4/5 or a pin outside
    /// 1-16) maps to `PDU_ERR_MUX_RSC_NOT_SUPPORTED` (ISO 22900-2 Table 49,
    /// consistent with `ioctl_set_prog_voltage`'s A2-21 mapping); any other
    /// native failure maps through the generic `map_native_error_for_link`
    /// -- no IN_USE conflict concept applies to this read-only IOCTL.
    async fn ioctl_read_j1962_pin_voltage(
        &self,
        module_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<u32, Status> {
        let (_device_guard, device_id) = self.require_connected_device_for(module_handle).await?;
        // SAE J2534-2 clause 23 is an opt-in feature (clause 5) -- gated the
        // same way `ioctl_start_repeat_message` gates clause 14, so a base
        // J2534-1 module (no "J2534-2:" pname prefix) cannot reach an
        // extension it never opted into (Codex review, PR #48).
        if !discovery::is_j2534_2_opted_in(
            self.modules[(module_handle - 1) as usize].pname.as_deref(),
        ) {
            let last_error = self.module_state.lock().await.last_error.clone();
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_READ_J1962PIN_VOLTAGE is a SAE J2534-2 \
                 clause 23 feature -- this module has not opted into J2534-2 (its pname lacks \
                 the \"J2534-2:\" prefix, clause 5)",
                PduError::PduErrIdNotSupported,
                Some(last_error),
            ));
        }
        let pin_number = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::Unum32Value(v)) => v,
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_READ_J1962PIN_VOLTAGE requires PDU_IT_IO_UNUM32 input_data",
                ));
            }
        };
        // ADR-185 Stage 2: SAE J2534-2 clause 25.3.2.2/Table 111 defines
        // `DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED`'s `Value` identically
        // to the two per-pin parameters (`SHORT_TO_GND_J1962`/
        // `PGM_VOLTAGE_J1962`) -- a bit-mapped pin selector, not a flat
        // capability flag (spec-accuracy correction to ADR-185 Decision 3's
        // original table row, made in this same change -- see the ADR's
        // "Correction" note). Only run the Discovery check for a `pin_number`
        // in the valid 1..=16 range; an out-of-range value is left to the
        // existing native-call `ERR_PIN_INVALID` path below, unchanged, to
        // avoid a shift overflow/underflow here.
        if let Ok(bit) = u8::try_from(pin_number.wrapping_sub(1))
            && (1..=16).contains(&pin_number)
        {
            let last_error = self.module_state.lock().await.last_error.clone();
            self.enforce_discovery_capability(
                module_handle,
                discovery::DeviceAccess::AlreadyOpen(device_id),
                discovery::DiscoveryCheck::DeviceFlag {
                    parameter: j2534_0404::DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED,
                    input_value: 1u32 << bit,
                },
                "PDU_IOCTL_READ_J1962PIN_VOLTAGE",
                PduError::PduErrMuxRscNotSupported,
                Some(last_error),
            )
            .await?;
        }
        // See ioctl_read_vbatt above: last_error is read after the native call
        // fails, not before waiting for the api lock (Codex review, ADR-105).
        let result = self
            .api
            .lock()
            .await
            .read_j1962_pin_voltage(device_id, pin_number);
        match result {
            Ok(mv) => Ok(mv),
            Err(err) => {
                let last_error = self.module_state.lock().await.last_error.clone();
                let is_pin_invalid = matches!(
                    err,
                    j2534_0404::Error::ApiStatus { code, .. }
                        if code.as_u32() == j2534_0404::ERR_PIN_INVALID
                );
                if is_pin_invalid {
                    Err(map_native_error_as(
                        "PDU_ERR_MUX_RSC_NOT_SUPPORTED: PassThruIoctl READ_J1962PIN_VOLTAGE",
                        &err,
                        PduError::PduErrMuxRscNotSupported,
                        Some(last_error),
                    ))
                } else {
                    Err(map_native_error_for_link(
                        "PassThruIoctl READ_J1962PIN_VOLTAGE",
                        &err,
                        Some(last_error),
                    ))
                }
            }
        }
    }

    /// **PDU_IOCTL_GET_DEVICE_CONFIG (M).** Input `bytearray_data`
    /// (`IOBytearray`, ADR-178): a hand-packed little-endian byte payload
    /// (`u32 entry_count`, then `entry_count` × `{u32 parameter_id, u32
    /// value}`) naming the `parameter_id`s (`NON_VOLATILE_STORE_1`.._10`) to
    /// read (`value` ignored on input). Calls
    /// `PassThruIoctl(GET_DEVICE_CONFIG)` (SAE J2534-2 clause 18, ADR-176/
    /// Phase 14) and returns the same entries with `value` populated. Rejects
    /// with `PDU_ERR_MODULE_NOT_CONNECTED` rather than lazily opening the
    /// device if `module_handle` has not been connected via `ModuleConnect`
    /// yet (A2-8), and with `PDU_ERR_ID_NOT_SUPPORTED` if the connected
    /// module has not opted into SAE J2534-2 (clause 5's `"J2534-2:"` `pname`
    /// prefix) -- gated the same way `ioctl_read_j1962_pin_voltage` gates
    /// clause 23.
    ///
    /// Per ADR-176, `parameter_id` is forwarded to the native call with no
    /// service-side range check (no allowlist): an out-of-range id is
    /// rejected by the native/mock layer with `ERR_INVALID_IOCTL_PARAM_ID`,
    /// which `map_native_error_for_link` below maps to
    /// `PDU_ERR_ID_NOT_SUPPORTED` via its own `pdu_error_for` arm (this
    /// phase; alongside `ERR_INVALID_IOCTL_ID`/`ERR_INVALID_PROTOCOL_ID`'s
    /// existing one) -- the same "forward unvalidated, let native/mock
    /// enforce the range" shape `ioctl_read_j1962_pin_voltage` uses for its
    /// unvalidated `pin_number`, though that handler's own native rejection
    /// (`ERR_PIN_INVALID`) is mapped via an explicit call-site override
    /// instead of a generic-table arm.
    async fn ioctl_get_device_config(
        &self,
        module_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<Vec<(u32, u32)>, Status> {
        let (_device_guard, device_id) = self.require_connected_device_for(module_handle).await?;
        // SAE J2534-2 clause 18 is an opt-in feature (clause 5) -- gated the
        // same way `ioctl_read_j1962_pin_voltage` gates clause 23, so a base
        // J2534-1 module (no "J2534-2:" pname prefix) cannot reach an
        // extension it never opted into (Codex review, PR #48).
        if !discovery::is_j2534_2_opted_in(
            self.modules[(module_handle - 1) as usize].pname.as_deref(),
        ) {
            let last_error = self.module_state.lock().await.last_error.clone();
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_GET_DEVICE_CONFIG is a SAE J2534-2 clause \
                 18 feature -- this module has not opted into J2534-2 (its pname lacks the \
                 \"J2534-2:\" prefix, clause 5)",
                PduError::PduErrIdNotSupported,
                Some(last_error),
            ));
        }
        let entries = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::BytearrayData(io_bytearray)) => {
                unpack_device_config_entries(&io_bytearray.data)?
            }
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_GET_DEVICE_CONFIG requires bytearray_data (IOBytearray) input_data",
                ));
            }
        };
        let parameter_ids: Vec<u32> = entries
            .into_iter()
            .map(|(parameter_id, _)| parameter_id)
            .collect();
        // ADR-185 Stage 2: SAE J2534-2 clause 18 Device Configuration
        // Discovery-cache precheck -- the device must report at least one
        // non-volatile storage slot before a Get/Set Device Config IOCTL is
        // even attempted.
        {
            let last_error = self.module_state.lock().await.last_error.clone();
            self.enforce_discovery_capability(
                module_handle,
                discovery::DeviceAccess::AlreadyOpen(device_id),
                discovery::DiscoveryCheck::DeviceCapacity {
                    parameter: j2534_0404::DEVICE_INFO_MAX_NON_VOLATILE_STORAGE,
                    extract: |v| v, // plain unsigned-long count (Table 111), not packed
                    needed: 1,
                },
                "PDU_IOCTL_GET_DEVICE_CONFIG",
                PduError::PduErrIdNotSupported,
                Some(last_error),
            )
            .await?;
        }
        // See ioctl_read_vbatt above: last_error is read after the native call
        // fails, not before waiting for the api lock (Codex review, ADR-105).
        let result = self
            .api
            .lock()
            .await
            .get_device_config(device_id, &parameter_ids);
        match result {
            Ok(values) => Ok(parameter_ids.into_iter().zip(values).collect()),
            Err(err) => {
                let last_error = self.module_state.lock().await.last_error.clone();
                Err(map_native_error_for_link(
                    "PassThruIoctl GET_DEVICE_CONFIG",
                    &err,
                    Some(last_error),
                ))
            }
        }
    }

    /// **PDU_IOCTL_SET_DEVICE_CONFIG (M).** Input `bytearray_data`
    /// (`IOBytearray`, ADR-178): a hand-packed little-endian byte payload
    /// (`u32 entry_count`, then `entry_count` × `{u32 parameter_id, u32
    /// value}`) of `parameter_id`/`value` pairs to write. Calls
    /// `PassThruIoctl(SET_DEVICE_CONFIG)` (SAE J2534-2 clause 18, ADR-176/
    /// Phase 14); no output. Rejects with `PDU_ERR_MODULE_NOT_CONNECTED`
    /// rather than lazily opening the device if `module_handle` has not been
    /// connected via `ModuleConnect` yet (A2-8), and with
    /// `PDU_ERR_ID_NOT_SUPPORTED` if the connected module has not opted into
    /// SAE J2534-2 (clause 5's `"J2534-2:"` `pname` prefix) -- gated the same
    /// way [`ioctl_get_device_config`] gates it.
    ///
    /// Per ADR-176, `parameter_id` is forwarded to the native call with no
    /// service-side range check, the same shape [`ioctl_get_device_config`]
    /// uses -- see that function's doc comment.
    ///
    /// [`ioctl_get_device_config`]: Self::ioctl_get_device_config
    async fn ioctl_set_device_config(
        &self,
        module_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<(), Status> {
        let (_device_guard, device_id) = self.require_connected_device_for(module_handle).await?;
        // See ioctl_get_device_config above: same clause 5 opt-in gate.
        if !discovery::is_j2534_2_opted_in(
            self.modules[(module_handle - 1) as usize].pname.as_deref(),
        ) {
            let last_error = self.module_state.lock().await.last_error.clone();
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_SET_DEVICE_CONFIG is a SAE J2534-2 clause \
                 18 feature -- this module has not opted into J2534-2 (its pname lacks the \
                 \"J2534-2:\" prefix, clause 5)",
                PduError::PduErrIdNotSupported,
                Some(last_error),
            ));
        }
        let configs = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::BytearrayData(io_bytearray)) => {
                unpack_device_config_entries(&io_bytearray.data)?
            }
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_SET_DEVICE_CONFIG requires bytearray_data (IOBytearray) input_data",
                ));
            }
        };
        // See ioctl_get_device_config above: same ADR-185 Stage 2 Discovery
        // precheck.
        {
            let last_error = self.module_state.lock().await.last_error.clone();
            self.enforce_discovery_capability(
                module_handle,
                discovery::DeviceAccess::AlreadyOpen(device_id),
                discovery::DiscoveryCheck::DeviceCapacity {
                    parameter: j2534_0404::DEVICE_INFO_MAX_NON_VOLATILE_STORAGE,
                    extract: |v| v, // plain unsigned-long count (Table 111), not packed
                    needed: 1,
                },
                "PDU_IOCTL_SET_DEVICE_CONFIG",
                PduError::PduErrIdNotSupported,
                Some(last_error),
            )
            .await?;
        }
        // See ioctl_read_vbatt above: last_error is read after the native call
        // fails, not before waiting for the api lock (Codex review, ADR-105).
        let result = self.api.lock().await.set_device_config(device_id, &configs);
        if let Err(err) = result {
            let last_error = self.module_state.lock().await.last_error.clone();
            return Err(map_native_error_for_link(
                "PassThruIoctl SET_DEVICE_CONFIG",
                &err,
                Some(last_error),
            ));
        }
        Ok(())
    }

    /// **PDU_IOCTL_SET_BUFFER_SIZE (L).** Input `PDU_IT_IO_UNUM32`: caps the
    /// per-item byte size of `GetComPrimitiveData` result items for this CLL
    /// -- distinct from the event queue size
    /// (`PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`). Stored on
    /// `CllEventQueue::result_buffer_limit` (moved off `LogicalLinkState`,
    /// Codex review on PR #3 / ADR-140 follow-up -- see that struct's own doc
    /// comment for why), under the queue's own lock rather than
    /// `logical_links`, so every producer reads it fresh at the point of use
    /// instead of from a value some caller captured ahead of time. Consulted
    /// by `events::cll_queue_item_to_event_item`, the conversion shared by
    /// `rpc_get_event_item` (`rpc_primitive.rs`, Codex-review fix) and the
    /// `SubscribeEvent` live-delivery path (`events::deliver_or_enqueue`,
    /// ADR-115 single-consumer correction) -- both truncate
    /// `ResultData.data_bytes` to at most this many bytes for a buffered or
    /// live-delivered frame on a CLL handle. `header_bytes`/`footer_bytes`
    /// (protocol framing, ADR-051) are left untouched since they are not
    /// "the result." Superseded note: this used to say the `SubscribeEvent`
    /// live fan-out sites were intentionally left unaffected by this limit
    /// -- ADR-115's single-consumer correction made a live subscriber this
    /// queue's drain, so it now applies the same limit `GetEventItem`
    /// always has.
    async fn ioctl_set_buffer_size(
        &self,
        cll_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<(), Status> {
        let limit = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::Unum32Value(v)) => v,
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_SET_BUFFER_SIZE requires PDU_IT_IO_UNUM32 input_data",
                ));
            }
        };
        let rx_buf = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            link.rx_buf.clone()
        };
        rx_buf.lock().await.result_buffer_limit = Some(limit);
        Ok(())
    }

    /// **PDU_IOCTL_START_MSG_FILTER (L).** Input `PDU_IT_IO_FILTER`
    /// (`PDU_IO_FILTER_LIST` of one or more `PDU_IO_FILTER_DATA`). Rejects the
    /// request up front (no hardware calls made) if it contains a duplicate
    /// `FilterNumber`, or if any `FilterNumber` is already installed on this
    /// CLL's `client_filters` or already pending in `pending_client_filters`
    /// -- overwriting any of those would orphan a previously-installed
    /// hardware filter or silently discard an earlier pre-connect request,
    /// unreachable from any handle for a later STOP/CLEAR. **`PDU_FLT_PASS`/
    /// `PDU_FLT_PASS_UUDT` are also rejected outright, on any channel or
    /// connection state** (ADR-079 amendment): `connect_new_physical_channel`
    /// already installs a wide-open zero-mask `PASS_FILTER` on every
    /// non-ISO15765 channel (ADR-008) so this service's own response matching
    /// sees every frame, and `poll_rx_inner` never consults `client_filters`
    /// in software -- so a narrower client `PASS_FILTER` would be
    /// indistinguishable from a no-op. `PDU_FLT_BLOCK`/`_BLOCK_UUDT` are
    /// unaffected (J2534 v04.04 BLOCK-wins precedence means they genuinely
    /// restrict traffic).
    ///
    /// ADR-038: `FLOW_CONTROL_FILTER` is the only J2534 filter type this
    /// service installs on an ISO15765 channel, and `PASS_FILTER`/
    /// `BLOCK_FILTER` -- the only two hardware filter types `PDU_FLT_PASS`/
    /// `_BLOCK`/`_PASS_UUDT`/`_BLOCK_UUDT` can become -- are rejected there.
    /// `PDU_IO_FILTER_DATA` carries no flow-control-message field at all
    /// (unlike its mask/pattern pair), so there is no way to construct a
    /// spec-conformant `FLOW_CONTROL_FILTER` from client-supplied filter
    /// data -- every `PDU_FLT` value is therefore rejected on an ISO15765
    /// link (design decision, see ADR-079; ADR-162 confirms this also
    /// covers a native-mixed-mode link -- clause 8.2.2.4 permits a properly
    /// paired-`ProtocolID` client PASS_FILTER/BLOCK_FILTER there, but this
    /// service defers supporting it, since nothing downstream yet routes a
    /// delivery for one). `base_hw_protocol_id()` normalizes every ISO15765
    /// family variant (`_PS`, FD, `_CHx`), so this rejection runs
    /// unconditionally, before branching on connection state (unlike
    /// request-shape validation, which the not-yet-connected branch below
    /// runs for its whole batch up front, while the connected path keeps
    /// validating and installing one filter at a time -- see that branch's
    /// own comment for why the two paths deliberately differ here).
    ///
    /// **A2-7 (conformance audit) / ADR-129:** ISO 22900-2 §9.5.13's Table 54
    /// return-value table has no `CLL_NOT_CONNECTED`-equivalent code, and
    /// §9.4.11.2 d) explicitly allows a client to pre-configure filters via
    /// this IOCTL before `PDUConnect` -- they become active once the CLL
    /// reaches `PDU_CLLST_ONLINE`. So when this CLL is not yet connected
    /// (`channel_id` is `None`), a request that passes the checks above is
    /// stored verbatim in `LogicalLinkState::pending_client_filters` (keyed by
    /// `FilterNumber`, no hardware call made) instead of being rejected.
    /// `rpc_connect_com_logical_link` drains it into `client_filters` once
    /// the CLL actually has a `channel_id` -- or fails the connect outright
    /// if honoring it would mean joining an already-shared channel (ADR-082's
    /// sole-channel-ownership invariant forbids that regardless of which side
    /// of the join brought the filters).
    ///
    /// When already connected, this otherwise installs each filter via
    /// `PassThruStartMsgFilter` (`install_client_message_filters`) and stores
    /// the returned `MessageFilterId`s keyed by `FilterNumber` in
    /// `client_filters` (kept separate from `unique_resp_filter_ids` so a
    /// client STOP/CLEAR never touches this service's own ADR-005/008/039
    /// filters). It is also rejected outright when this CLL's physical
    /// channel is shared with another CLL (`ref_count > 1`): a real
    /// `PASS_FILTER`/`BLOCK_FILTER` is installed on the shared `channel_id`,
    /// not something scoped to one CLL, so a `BLOCK_FILTER` requested by this
    /// CLL would silently drop frames a sibling CLL still needs (and, once
    /// installed, a second CLL is barred from later joining that same channel
    /// too -- see `rpc_connect_com_logical_link`'s reciprocal check).
    ///
    /// Holds `shared_channels` locked (Codex-review fix, lock-hierarchy ADR)
    /// from before the `channel_id` read all the way through either the
    /// not-yet-connected branch's `pending_client_filters` write or the
    /// connected branch's hardware install and final `client_filters` write,
    /// so a concurrent `ConnectComLogicalLink` can neither join this channel
    /// in the window between this call reading `ref_count` and recording its
    /// filter, nor snapshot-install-finalize a pre-connect filter this call
    /// is simultaneously trying to stop/clear/replace out from under it (a
    /// second Codex-review fix, PR #139, on top of the first -- see ADR-129's
    /// amendment). `api`/`logical_links` are acquired and released
    /// underneath it, never before it, matching this crate's lock hierarchy.
    async fn ioctl_start_msg_filter(
        &self,
        cll_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<(), Status> {
        let filters = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::FilterData(list))
                if !list.filters.is_empty() =>
            {
                list.filters
            }
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_START_MSG_FILTER requires a non-empty PDU_IT_IO_FILTER \
                     (PDU_IO_FILTER_LIST) input_data",
                ));
            }
        };

        {
            let mut seen = std::collections::HashSet::with_capacity(filters.len());
            for filter in &filters {
                if !seen.insert(filter.filter_number) {
                    let last_error = self
                        .logical_links
                        .lock()
                        .await
                        .get(&cll_handle)
                        .and_then(|l| l.last_error.clone());
                    return Err(state_guard_status(
                        Code::InvalidArgument,
                        format!(
                            "PDU_ERR_INVALID_PARAMETERS: duplicate FilterNumber {} in the same \
                             PDU_IOCTL_START_MSG_FILTER request",
                            filter.filter_number
                        ),
                        PduError::PduErrInvalidParameters,
                        last_error,
                    ));
                }
            }
        }

        // PDU_FLT_PASS/_PASS_UUDT cannot be honored: `connect_new_physical_channel`
        // installs a wide-open zero-mask PASS_FILTER on every non-ISO15765
        // channel (ADR-008) so this service's own software response-matching
        // (`poll_rx_inner`) sees every frame -- narrowing that baseline in
        // hardware would starve it. `poll_rx_inner` does not consult
        // `client_filters` in software either, so a client PASS filter would
        // be a silent no-op: reject the whole request up front instead
        // (ADR-079 amendment). PDU_FLT_BLOCK/_BLOCK_UUDT are unaffected --
        // J2534 v04.04 BLOCK-wins precedence means they genuinely restrict
        // traffic even with the pass-all baseline in place.
        for filter in &filters {
            if matches!(
                vci_service_interface::PduFilter::try_from(filter.filter_type),
                Ok(vci_service_interface::PduFilter::PduFltPass
                    | vci_service_interface::PduFilter::PduFltPassUudt)
            ) {
                // No PDUError variant means "function not supported" specifically
                // (that name never existed in the ISO 22900-2 PDUError enum this
                // service exposes); PDU_ERR_FCT_FAILED is the closest real code
                // for an adapter-level structural limitation like this one.
                let last_error = self
                    .logical_links
                    .lock()
                    .await
                    .get(&cll_handle)
                    .and_then(|l| l.last_error.clone());
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "PDU_ERR_FCT_FAILED: PDU_FLT_PASS/PDU_FLT_PASS_UUDT cannot be \
                     enforced -- this adapter keeps a pass-all hardware baseline so its own \
                     response matching sees every frame; use PDU_FLT_BLOCK/PDU_FLT_BLOCK_UUDT \
                     to restrict traffic",
                    PduError::PduErrFctFailed,
                    last_error,
                ));
            }
        }

        // Codex review (PR #139), fixing a race the original ADR-129 PR
        // missed: `shared_channels` is acquired here, before the `channel_id`
        // read below, and held continuously through either the not-yet-
        // connected branch's `pending_client_filters` read+write or the
        // connected branch's hardware install and `client_filters` write.
        // `rpc_connect_com_logical_link` holds this same lock continuously
        // from before it snapshots `pending_client_filters` through
        // `finalize_connected_link`'s clear-and-fold of that snapshot --
        // without also serializing this function on it, a concurrent
        // `ConnectComLogicalLink` could snapshot `pending_client_filters`,
        // release `logical_links` momentarily (between the snapshot and the
        // native install, and again before `finalize_connected_link`
        // re-acquires it), and let this function's read-then-mutate slip
        // into that gap: a `STOP`/`CLEAR` there would report success on a
        // filter that then still gets installed from the stale snapshot, and
        // a `START` there would report success on a filter that
        // `finalize_connected_link`'s unconditional `clear()` then discards
        // before it ever reaches hardware. See ADR-129's amendment.
        let chans = self.shared_channels.lock().await;
        let (channel_id, hw_protocol_id, base_hw_protocol_id, channel_key, pin_select, last_error) = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            (
                link.channel_id,
                // Raw (possibly `_PS`) id -- still needed below for
                // `PassThruMessage::new` (Plane A) and
                // `find_physical_lock_holder` (Plane C).
                link.hw_protocol_id,
                // ADR-157 Plane B: normalized -- the ADR-038 ban must also
                // apply to an ISO15765_PS link.
                link.base_hw_protocol_id(),
                link.channel_key,
                // Plane C, ADR-157 Bug D fix -- `find_physical_lock_holder`'s
                // pre-connect fallback discriminant.
                link.pin_select,
                link.last_error.clone(),
            )
        };

        // ADR-038: `base_hw_protocol_id()` normalizes `_PS`/FD/`_CHx` variants
        // to `ISO15765` and `hw_protocol_id`'s only post-create mutations
        // (FD/pin-select substitution) never move a link into the ISO15765
        // family from outside it, so this runs unconditionally -- before
        // branching on connection state -- rather than only once connected.
        //
        // ADR-162: this also covers a native-mixed-mode CLL. Clause 8.2.2.4
        // makes a correctly-tagged client PASS_FILTER/BLOCK_FILTER spec-
        // legitimate on such a channel, so "the only filter type valid on an
        // ISO15765 link" is no longer strictly true for every case -- but
        // supporting it needs matching RX delivery-routing work (ADR-160
        // Decision 4 only forwards a CAN-tagged frame that already matches a
        // `CP_CanRespUUDTId` table entry) that doesn't exist yet, so this
        // remains a deliberate deferral, not a spec-conformance guarantee.
        // See ADR-162 Decision 1 and `install_client_message_filters`'s own
        // invariant note (`rpc_link.rs`) for the full rationale.
        if base_hw_protocol_id == j2534_0404::ISO15765 {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_VALUE_NOT_SUPPORTED: PDU_IOCTL_START_MSG_FILTER's PASS_FILTER/ \
                 BLOCK_FILTER-based filter types are not supported on an ISO15765 link -- \
                 FLOW_CONTROL_FILTER is the only filter type this service installs there; a \
                 clause-8.2.2.4-tagged client filter under native-mixed mode is not yet \
                 supported either (ADR-162)",
                PduError::PduErrValueNotSupported,
                last_error,
            ));
        }

        // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): clause 10
        // has no filter concept at all -- `PDU_IOCTL_START_MSG_FILTER` is
        // rejected the same way `rpc_primitive.rs`'s write rejection is,
        // mirroring `rpc_misc.rs`'s own UART Echo Byte + Repeat Messaging
        // precedent above (this file's `ioctl_start_repeat_message`). Runs
        // unconditionally here too, gated only on the link's own
        // `hw_protocol_id` -- like the ISO15765 check just above, not scoped
        // to "already connected", so a pre-connect filter request is
        // rejected just as early as a post-connect one.
        if resources::is_analog_in_protocol_id(hw_protocol_id) {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_START_MSG_FILTER is not supported on a SAE \
                 J2534-2 clause 10 Analog Input link -- clause 10 defines no filter concept at \
                 all",
                PduError::PduErrIdNotSupported,
                last_error,
            ));
        }

        // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): the same
        // "no filter concept at all" reasoning as Analog Inputs above --
        // clause 24.2.5.5 always rejects `PassThruStartMsgFilter` on this
        // protocol. Rejected here too, unconditionally on `hw_protocol_id`
        // and before any connection-state branching, so a pre-connect
        // `PDU_IOCTL_START_MSG_FILTER` fails immediately with this code
        // instead of surfacing later as a generic connect failure.
        if hw_protocol_id == j2534_0404::PROTOCOL_ETHERNET_NDIS {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: PDU_IOCTL_START_MSG_FILTER is not supported on a SAE \
                 J2534-2 clause 24 Ethernet_NDIS link -- clause 24 defines no filter concept at \
                 all",
                PduError::PduErrIdNotSupported,
                last_error,
            ));
        }

        // Reject before any hardware call or pending-store: silently
        // overwriting an already-installed OR already-pending FilterNumber
        // would orphan a live hardware filter or discard an earlier
        // pre-connect request, with no client-visible handle left to ever
        // STOP/CLEAR it. The client must STOP_MSG_FILTER/CLEAR_MSG_FILTER the
        // existing entry first, whichever state it's in.
        {
            let links = self.logical_links.lock().await;
            if let Some(link) = links.get(&cll_handle) {
                for filter in &filters {
                    if link.client_filters.contains_key(&filter.filter_number)
                        || link
                            .pending_client_filters
                            .contains_key(&filter.filter_number)
                    {
                        return Err(state_guard_status(
                            Code::AlreadyExists,
                            format!(
                                "PDU_ERR_INVALID_PARAMETERS: FilterNumber {} is already installed \
                                 or pending on this ComLogicalLink -- stop or clear it first",
                                filter.filter_number
                            ),
                            PduError::PduErrInvalidParameters,
                            link.last_error.clone(),
                        ));
                    }
                }
            }
        }

        let Some(channel_id) = channel_id else {
            // Not yet connected (ISO 22900-2 §9.4.11.2 d, A2-7/ADR-129): no
            // channel to install onto yet. Request-shape validation
            // (recognized, non-`PDU_FLT_UNSPECIFIED` `filter_type`;
            // well-formed mask/pattern) runs here, up front for the whole
            // batch, so this path gets the same `PDU_ERR_INVALID_PARAMETERS`-
            // shaped rejection the connected path below would give -- rather
            // than silently deferring a malformed request to
            // `ConnectComLogicalLink`, whose own return-value table (Table
            // 19) has no such code, only `FCT_FAILED`. `TxFlags` is
            // irrelevant to `PassThruMessage::new`'s validation (only payload
            // length is checked), so `flags = 0` here is representative of
            // every ID-width variant the connected path would actually
            // build. Deliberately NOT hoisted into the connected path below:
            // that path's own per-filter interleaved validate-then-install
            // loop (`install_client_message_filters`) is what makes an
            // earlier filter's rollback-on-later-failure behavior observable
            // (`start_msg_filter_rolls_back_earlier_filters_on_a_later_failure`)
            // -- validating the whole batch first would install nothing at
            // all instead.
            for filter in &filters {
                let filter_type = vci_service_interface::PduFilter::try_from(filter.filter_type)
                    .map_err(|_| {
                        Status::invalid_argument(format!(
                            "unrecognized PDU_FLT filter_type {}",
                            filter.filter_type
                        ))
                    })?;
                if matches!(
                    filter_type,
                    vci_service_interface::PduFilter::PduFltUnspecified
                ) {
                    return Err(Status::invalid_argument(
                        "PDU_ERR_INVALID_PARAMETERS: filter_type is required",
                    ));
                }
                j2534_0404::PassThruMessage::new(
                    hw_protocol_id,
                    0,
                    0,
                    0,
                    0,
                    &filter.filter_mask_message,
                )
                .map_err(|err| {
                    Status::invalid_argument(format!("invalid filter_mask_message: {err}"))
                })?;
                j2534_0404::PassThruMessage::new(
                    hw_protocol_id,
                    0,
                    0,
                    0,
                    0,
                    &filter.filter_pattern_message,
                )
                .map_err(|err| {
                    Status::invalid_argument(format!("invalid filter_pattern_message: {err}"))
                })?;
            }

            // Store the validated definitions; `rpc_connect_com_logical_link`
            // installs them once this CLL actually has a `channel_id`, or
            // fails the connect outright rather than partially honor them
            // (ADR-082).
            let mut links = self.logical_links.lock().await;
            if let Some(link) = links.get_mut(&cll_handle) {
                for filter in filters {
                    link.pending_client_filters
                        .insert(filter.filter_number, filter);
                }
            }
            return Ok(());
        };

        // A `PDU_IO_FILTER_DATA` filter is a real J2534 PASS_FILTER/BLOCK_FILTER
        // installed on the shared `channel_id`, not something `poll_rx_inner`
        // can scope to one CLL after the fact -- a BLOCK_FILTER installed for
        // this CLL would silently drop frames a sibling CLL on the same
        // physical channel still needs. Reject rather than let one CLL's
        // filter request affect another CLL's traffic.
        //
        // `chans` (acquired above, before the not-yet-connected branch) is
        // held all the way through the hardware install and the
        // `client_filters` write below (function-scoped, not dropped early)
        // -- otherwise a concurrent `ConnectComLogicalLink` could slip a
        // second CLL onto this channel between the ref_count read and the
        // `client_filters` write, since its own reciprocal check
        // (`client_filters.is_empty()`) would still see this filter as
        // not-yet-installed. `shared_channels` is the established outermost
        // lock of the three (`shared_channels` → `logical_links`/`api`) used
        // by `rpc_connect_com_logical_link` for the same reason -- see the
        // lock-hierarchy ADR.
        if let Some(channel_key) = channel_key {
            let shared = chans.get(&channel_key).map(|sc| sc.ref_count).unwrap_or(1) > 1;
            if shared {
                // Read `last_error` fresh at this guard decision itself, per
                // `state_guard_status`'s same-lock-acquisition contract --
                // the FilterNumber-check block above dropped and re-acquired
                // `logical_links` (a real intervening `.await`) since the
                // `last_error` snapshot was taken, so reusing that snapshot
                // here could return a stale value if another RPC updated
                // this CLL's tracked error in between.
                let last_error = self
                    .logical_links
                    .lock()
                    .await
                    .get(&cll_handle)
                    .and_then(|l| l.last_error.clone());
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "PDU_ERR_FCT_FAILED: PDU_IOCTL_START_MSG_FILTER cannot be scoped \
                     to one ComLogicalLink when its physical channel is shared with another \
                     ComLogicalLink",
                    PduError::PduErrFctFailed,
                    last_error,
                ));
            }
        }

        {
            let links = self.logical_links.lock().await;
            if find_physical_lock_holder(
                &links,
                cll_handle,
                hw_protocol_id,
                pin_select,
                channel_key,
                LOCK_PHYSICAL_COM_PARAMS,
            )
            .is_some()
            {
                let last_error = links.get(&cll_handle).and_then(|l| l.last_error.clone());
                return Err(state_guard_status(
                    Code::ResourceExhausted,
                    "physical ComParam lock is held by another ComLogicalLink on this resource",
                    PduError::PduErrRscLockedByOtherCll,
                    last_error,
                ));
            }
        }

        // A TxFlags-0 filter only matches 11-bit CAN Ids, so on a CAN channel
        // connected with CAN_ID_BOTH/CAN_29BIT_ID a client filter must be
        // installed once per applicable ID-width variant or it silently never
        // matches 29-bit traffic (ADR-065; Codex-review fix). `chans`/
        // `channel_key` are already in scope and held for the duration of
        // this function (see the comment above).
        let connect_flags = channel_key
            .and_then(|key| chans.get(&key))
            .map_or(0, |sc| sc.connect_flags);

        let api = self.api.lock().await;
        let by_filter_number = install_client_message_filters(
            &api,
            cll_handle,
            channel_id,
            hw_protocol_id,
            connect_flags,
            &filters,
        )
        .await;
        drop(api);

        let by_filter_number = match by_filter_number {
            Ok(map) => map,
            Err(InstallFilterFailure::Status(status)) => return Err(status),
            Err(InstallFilterFailure::Native(err)) => {
                let last_error = self
                    .logical_links
                    .lock()
                    .await
                    .get(&cll_handle)
                    .and_then(|l| l.last_error.clone());
                return Err(map_native_error_for_link(
                    "PassThruStartMsgFilter",
                    &err,
                    last_error,
                ));
            }
        };

        let mut links = self.logical_links.lock().await;
        if let Some(link) = links.get_mut(&cll_handle) {
            for (filter_number, filter_ids) in by_filter_number {
                link.client_filters.insert(filter_number, filter_ids);
            }
        }
        Ok(())
    }

    /// **PDU_IOCTL_STOP_MSG_FILTER (L).** Input `PDU_IT_IO_UNUM32` =
    /// `FilterNumber`. Looks it up in `client_filters` -- which may hold more
    /// than one `MessageFilterId` for this `FilterNumber` if it was installed
    /// under multiple `TxFlags`/ID-width variants (see
    /// `ioctl_start_msg_filter`) -- and calls `PassThruStopMsgFilter` for
    /// each, attempting every id (no short-circuiting). Mirrors
    /// `ioctl_clear_msg_filter`/`ioctl_reset`'s best-effort retry-tracking
    /// philosophy: a failed stop's id stays tracked under `filter_number` for
    /// retry rather than being forgotten, since its hardware filter may still
    /// be active; the entry is only removed from `client_filters` once every
    /// id for it has stopped successfully. Per ISO 22900-2:2009(E) Table 55,
    /// unlike the internal/teardown callers of `stop_message_filter`
    /// (`ioctl_reset`, `DestroyComLogicalLink`, flow-control filter cleanup),
    /// this client-facing IOCTL reports a native failure back to the caller
    /// as `PDU_ERR_FCT_FAILED` rather than silently returning `Ok(())`
    /// (ADR-114, superseding ADR-079 items 12-13's silent-success behavior;
    /// the retry-tracking itself is unchanged).
    ///
    /// **A2-7/ADR-129:** ISO 22900-2 §9.4.11.2 d) names this IOCTL, alongside
    /// START/CLEAR_MSG_FILTER, as usable before `PDUConnect`. When this CLL
    /// is not yet connected, the `FilterNumber` (if it exists at all) can only
    /// be in `pending_client_filters` -- never in `client_filters`, which is
    /// empty until a connect drains it -- so this removes it there directly,
    /// with no native call and no lock-holder check. `shared_channels` is
    /// held across the `channel_id` read and that removal (Codex review, PR
    /// #139) so a concurrent `ConnectComLogicalLink` cannot snapshot-install-
    /// finalize this exact `pending_client_filters` entry in the gap between
    /// this call reading `channel_id` and removing it -- which would
    /// otherwise let this call report success on a stop that still gets
    /// installed from the stale snapshot. See `ioctl_start_msg_filter`'s doc
    /// comment and ADR-129's amendment for the full race. Dropped immediately
    /// once this CLL is confirmed connected -- the connected path below
    /// never needed it.
    async fn ioctl_stop_msg_filter(
        &self,
        cll_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<(), Status> {
        let filter_number = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::Unum32Value(v)) => v,
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_STOP_MSG_FILTER requires PDU_IT_IO_UNUM32 input_data (FilterNumber)",
                ));
            }
        };

        let chans = self.shared_channels.lock().await;
        let (channel_id, hw_protocol_id, channel_key, pin_select, filter_ids) = {
            let mut links = self.logical_links.lock().await;
            let link = links
                .get_mut(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            let Some(channel_id) = link.channel_id else {
                return if link.pending_client_filters.remove(&filter_number).is_some() {
                    Ok(())
                } else {
                    // ISO 22900-2:2009 Table 55: an unrecognized FilterNumber
                    // is PDU_ERR_INVALID_PARAMETERS, not PDU_ERR_INVALID_HANDLE
                    // (reserved for an invalid ComLogicalLink handle) (A2-19).
                    Err(state_guard_status(
                        Code::InvalidArgument,
                        format!(
                            "PDU_ERR_INVALID_PARAMETERS: no client filter installed or pending \
                             with FilterNumber {filter_number}"
                        ),
                        PduError::PduErrInvalidParameters,
                        link.last_error.clone(),
                    ))
                };
            };
            let last_error = link.last_error.clone();
            let filter_ids = link
                .client_filters
                .get(&filter_number)
                .cloned()
                .ok_or_else(|| {
                    // Same A2-19 rationale as above.
                    state_guard_status(
                        Code::InvalidArgument,
                        format!(
                            "PDU_ERR_INVALID_PARAMETERS: no client filter installed with \
                             FilterNumber {filter_number}"
                        ),
                        PduError::PduErrInvalidParameters,
                        last_error,
                    )
                })?;
            (
                channel_id,
                link.hw_protocol_id,
                link.channel_key,
                link.pin_select,
                filter_ids,
            )
        };
        drop(chans);

        {
            let links = self.logical_links.lock().await;
            if find_physical_lock_holder(
                &links,
                cll_handle,
                hw_protocol_id,
                pin_select,
                channel_key,
                LOCK_PHYSICAL_COM_PARAMS,
            )
            .is_some()
            {
                let last_error = links.get(&cll_handle).and_then(|l| l.last_error.clone());
                return Err(state_guard_status(
                    Code::ResourceExhausted,
                    "physical ComParam lock is held by another ComLogicalLink on this resource",
                    PduError::PduErrRscLockedByOtherCll,
                    last_error,
                ));
            }
        }

        let api = self.api.lock().await;
        let total_ids = filter_ids.len();
        let mut failed = Vec::with_capacity(total_ids);
        let mut first_error: Option<j2534_0404::Error> = None;
        for filter_id in filter_ids {
            if let Err(err) = api.stop_message_filter(channel_id, filter_id) {
                warn!(
                    cll_handle,
                    filter_number,
                    filter_id = filter_id.0,
                    %err,
                    "PDU_IOCTL_STOP_MSG_FILTER: failed to stop client filter, keeping it \
                     tracked for retry"
                );
                if first_error.is_none() {
                    first_error = Some(err);
                }
                failed.push(filter_id);
            }
        }
        drop(api);

        let mut links = self.logical_links.lock().await;
        let failed_count = failed.len();
        let last_error = links.get(&cll_handle).and_then(|l| l.last_error.clone());
        if let Some(link) = links.get_mut(&cll_handle) {
            if failed.is_empty() {
                link.client_filters.remove(&filter_number);
            } else {
                link.client_filters.insert(filter_number, failed);
            }
        }
        drop(links);

        if let Some(first_error) = first_error {
            return Err(map_native_error_as(
                &format!(
                    "PDU_IOCTL_STOP_MSG_FILTER: FilterNumber {filter_number} ({failed_count}/{total_ids} \
                     underlying filter ids failed to stop, kept tracked for retry)",
                ),
                &first_error,
                PduError::PduErrFctFailed,
                last_error,
            ));
        }
        Ok(())
    }

    /// **PDU_IOCTL_CLEAR_MSG_FILTER (L).** Stops every filter in this CLL's
    /// `client_filters` map via `PassThruStopMsgFilter` (attempting every id
    /// across every `FilterNumber`, no short-circuiting), removing only the
    /// entries that actually stopped -- a `FilterNumber` whose
    /// `PassThruStopMsgFilter` call fails stays in `client_filters` (best
    /// effort, logged) rather than being silently forgotten, since its
    /// hardware filter may still be active and blocking traffic; the client
    /// can still see and retry `STOP_MSG_FILTER`/`CLEAR_MSG_FILTER` on it.
    /// Does NOT touch `unique_resp_filter_ids` and does NOT call the legacy
    /// channel-wide `IOCTL_CLEAR_MSG_FILTERS` raw ID. Per ISO 22900-2:2009(E)
    /// Table 56, if any underlying native stop call fails this reports
    /// `PDU_ERR_FCT_FAILED` back to the caller (naming the affected
    /// `FilterNumber`s) instead of silently returning `Ok(())` (ADR-114,
    /// superseding ADR-079 items 12-13's silent-success behavior; the
    /// retry-tracking itself is unchanged).
    ///
    /// **A2-7/ADR-129:** ISO 22900-2 §9.4.11.2 d) names this IOCTL, alongside
    /// START/STOP_MSG_FILTER, as usable before `PDUConnect`. When this CLL is
    /// not yet connected, `client_filters` is always empty (nothing has been
    /// installed yet), so this simply empties `pending_client_filters`
    /// instead -- no native calls, no lock-holder check. `shared_channels` is
    /// held across the `channel_id` read and that clear (Codex review, PR
    /// #139), for the same reason as `ioctl_stop_msg_filter`'s identical
    /// guard: it closes a race against a concurrent `ConnectComLogicalLink`
    /// snapshotting `pending_client_filters` in the gap this call would
    /// otherwise be able to clear it out from under (ADR-129's amendment).
    /// Dropped immediately once this CLL is confirmed connected -- the
    /// connected path below never needed it.
    async fn ioctl_clear_msg_filter(&self, cll_handle: u32) -> Result<(), Status> {
        let chans = self.shared_channels.lock().await;
        let (channel_id, hw_protocol_id, channel_key, pin_select, filters) = {
            let mut links = self.logical_links.lock().await;
            let link = links
                .get_mut(&cll_handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
            let Some(channel_id) = link.channel_id else {
                link.pending_client_filters.clear();
                return Ok(());
            };
            (
                channel_id,
                link.hw_protocol_id,
                link.channel_key,
                link.pin_select,
                link.client_filters
                    .iter()
                    .map(|(&number, ids)| (number, ids.clone()))
                    .collect::<Vec<_>>(),
            )
        };
        drop(chans);

        {
            let links = self.logical_links.lock().await;
            if find_physical_lock_holder(
                &links,
                cll_handle,
                hw_protocol_id,
                pin_select,
                channel_key,
                LOCK_PHYSICAL_COM_PARAMS,
            )
            .is_some()
            {
                let last_error = links.get(&cll_handle).and_then(|l| l.last_error.clone());
                return Err(state_guard_status(
                    Code::ResourceExhausted,
                    "physical ComParam lock is held by another ComLogicalLink on this resource",
                    PduError::PduErrRscLockedByOtherCll,
                    last_error,
                ));
            }
        }

        let api = self.api.lock().await;
        // Per FilterNumber, any ids that fail to stop stay behind (best
        // effort, logged) -- the hardware filter may still be active, so the
        // client must still be able to see and retry
        // STOP_MSG_FILTER/CLEAR_MSG_FILTER on it rather than losing track of
        // a filter that could still be blocking traffic on this channel. A
        // FilterNumber whose ids all stopped is dropped from `client_filters`
        // entirely below.
        let mut failed_by_filter_number: HashMap<u32, Vec<MessageFilterId>> = HashMap::new();
        let mut first_error: Option<j2534_0404::Error> = None;
        for (filter_number, filter_ids) in &filters {
            for &filter_id in filter_ids {
                if let Err(err) = api.stop_message_filter(channel_id, filter_id) {
                    warn!(
                        cll_handle,
                        filter_number = *filter_number,
                        filter_id = filter_id.0,
                        %err,
                        "PDU_IOCTL_CLEAR_MSG_FILTER: failed to remove client filter, keeping it \
                         tracked for retry"
                    );
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                    failed_by_filter_number
                        .entry(*filter_number)
                        .or_default()
                        .push(filter_id);
                }
            }
        }
        drop(api);

        // Collected before the bookkeeping loop below drains
        // `failed_by_filter_number` via `.remove()`.
        let mut sorted_failed: Vec<u32> = failed_by_filter_number.keys().copied().collect();
        sorted_failed.sort_unstable();

        let mut links = self.logical_links.lock().await;
        let last_error = links.get(&cll_handle).and_then(|l| l.last_error.clone());
        if let Some(link) = links.get_mut(&cll_handle) {
            for (filter_number, _) in filters {
                match failed_by_filter_number.remove(&filter_number) {
                    Some(failed_ids) => {
                        link.client_filters.insert(filter_number, failed_ids);
                    }
                    None => {
                        link.client_filters.remove(&filter_number);
                    }
                }
            }
        }
        drop(links);

        if let Some(first_error) = first_error {
            return Err(map_native_error_as(
                &format!(
                    "PDU_IOCTL_CLEAR_MSG_FILTER: FilterNumbers {sorted_failed:?} failed to stop, \
                     kept tracked for retry"
                ),
                &first_error,
                PduError::PduErrFctFailed,
                last_error,
            ));
        }
        Ok(())
    }

    /// **PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES (L).** Input
    /// `PDU_IT_IO_EVENT_QUEUE_PROPERTY` (`QueueSize`, `QueueMode`). Sets
    /// `CllEventQueue::event_queue_cap`/`event_queue_mode` (moved off
    /// `LogicalLinkState`, Codex review on PR #3 / ADR-140 follow-up -- see
    /// that struct's own doc comment for why), consulted by the per-CLL event
    /// queue insert sites in `events.rs` (`push_cll_event`). `QueueSize` is
    /// clamped to `MAX_EVENT_QUEUE_CAP` so a client cannot grow `rx_buf`
    /// unboundedly. Maps `T_PDU_QUEUE_MODE` to the two eviction policies this
    /// adapter's bounded ring buffer can implement: `PDU_QUE_CIRCULAR` ->
    /// `OverwriteOldest` (the pre-existing default behavior), `PDU_QUE_LIMITED`
    /// -> `DiscardNewest`, and `PDU_QUE_UNLIMITED` -> `OverwriteOldest` as
    /// well, since this adapter has no unbounded-queue mode to map it to
    /// (design decision, ADR-079). Per ISO 22900-2 §9.5.16, only usable prior
    /// to `PDUConnect`: a `connected` CLL, or one with a
    /// `ConnectComLogicalLink` call already in flight for it
    /// (`LogicalLinkState::pdu_connect_begun`), is rejected with
    /// `PDU_ERR_CLL_CONNECTED` before anything is mutated (ADR-126, round 2 for
    /// the in-flight-Connect window).
    ///
    /// The gate check and the queue mutation (policy write + cap-trim loop)
    /// share ONE `logical_links` critical section, with the `rx_buf` queue
    /// lock acquired nested inside it -- not a gate checked under
    /// `logical_links` that is then dropped before a separately-locked
    /// write. That two-lock split is exactly the gap a second Codex review
    /// round on PR #3 flagged: a concurrent `ConnectComLogicalLink` could
    /// claim `connect_in_flight` in the window between dropping
    /// `logical_links` and acquiring the queue lock, letting this IOCTL's
    /// write land after PDUConnect had effectively begun (violating
    /// ADR-126's "reconfiguration only before PDUConnect" invariant). Holding
    /// `logical_links` continuously from the gate check through the trim
    /// loop restores that atomicity -- both against `ConnectComLogicalLink`
    /// racing the gate, and against a concurrent producer
    /// (`deliver_or_enqueue`/`rpc_get_event_item`) observing the new cap
    /// applied before the trim actually ran.
    async fn ioctl_set_event_queue_properties(
        &self,
        cll_handle: u32,
        input: Option<vci_service_interface::DataItem>,
    ) -> Result<(), Status> {
        let props = match input.and_then(|d| d.data) {
            Some(vci_service_interface::data_item::Data::EventQueueProperty(p)) => p,
            _ => {
                return Err(Status::invalid_argument(
                    "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES requires \
                     PDU_IT_IO_EVENT_QUEUE_PROPERTY input_data",
                ));
            }
        };
        if props.queue_size == 0 {
            let last_error = self
                .logical_links
                .lock()
                .await
                .get(&cll_handle)
                .and_then(|l| l.last_error.clone());
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_INVALID_PARAMETERS: QueueSize must be nonzero",
                PduError::PduErrInvalidParameters,
                last_error,
            ));
        }
        let mode = match vci_service_interface::PduQueueMode::try_from(props.queue_mode) {
            Ok(
                vci_service_interface::PduQueueMode::PduQueCircular
                | vci_service_interface::PduQueueMode::PduQueUnlimited,
            ) => EventQueueMode::OverwriteOldest,
            Ok(vci_service_interface::PduQueueMode::PduQueLimited) => EventQueueMode::DiscardNewest,
            Err(_) => {
                return Err(Status::invalid_argument(format!(
                    "unrecognized T_PDU_QUEUE_MODE value {}",
                    props.queue_mode
                )));
            }
        };

        // Clamp to MAX_EVENT_QUEUE_CAP: see its doc comment for why an
        // unbounded client-supplied QueueSize is not applied as-is.
        let new_cap = (props.queue_size as usize).min(MAX_EVENT_QUEUE_CAP);
        // Crux of the fix (Codex review on PR #3, second round / ADR-140
        // follow-up): the `pdu_connect_begun()` gate check and the queue
        // mutation (policy write + cap-trim loop) now share ONE
        // `logical_links` critical section, with the queue lock nested
        // inside it, rather than the gate being checked under
        // `logical_links` and then separately re-locked under `rx_buf`. The
        // prior two-lock split reopened exactly the window ADR-126 round 2
        // closed: a concurrent `ConnectComLogicalLink` could claim
        // `connect_in_flight` in the gap between dropping `logical_links`
        // and acquiring the queue lock, letting this IOCTL's write land
        // after PDUConnect had effectively begun. Holding `logical_links`
        // continuously from the gate check through the trim loop restores
        // that atomicity; see `logical_links`'s own doc comment in
        // `service.rs` for the resulting `logical_links -> queue` lock-order
        // edge.
        let links = self.logical_links.lock().await;
        let link = links
            .get(&cll_handle)
            .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {cll_handle}")))?;
        // `pdu_connect_begun()` gate (ADR-126 round 2): a
        // `ConnectComLogicalLink` RPC that has been accepted but not yet
        // finalized also counts as "PDUConnect already called" per ISO
        // 22900-2 §9.5.16 -- `connected` alone would let this IOCTL slip
        // through during that in-flight window.
        if link.pdu_connect_begun() {
            return Err(state_guard_status(
                Code::FailedPrecondition,
                "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES must be issued before \
                 PDUConnect is called; disconnect the ComLogicalLink (or wait for the \
                 in-progress Connect to finish and disconnect) before reconfiguring \
                 its event queue",
                PduError::PduErrCllConnected,
                link.last_error.clone(),
            ));
        }
        // The policy write AND the cap-trim loop below happen in ONE
        // `rx_buf` lock acquisition, nested inside the still-held
        // `logical_links` guard above. A lowered cap must take effect
        // immediately, not just gate future `push_cll_event` calls (which
        // evict at most one entry per inbound frame/error): otherwise a
        // buffer already holding more than the new cap stays over-cap
        // indefinitely under continued traffic, and in DiscardNewest mode no
        // new frames could be delivered until the client drains it back
        // under the cap on its own.
        let mut queue = link.rx_buf.lock().await;
        queue.event_queue_cap = new_cap;
        queue.event_queue_mode = mode;
        while queue.items.len() > new_cap {
            queue.items.pop_front();
        }
        Ok(())
    }

    pub(super) async fn rpc_get_object_id(
        &self,
        request: Request<vci_service_interface::GetObjectIdRequest>,
    ) -> Result<Response<vci_service_interface::ObjectIdResponse>, Status> {
        let request = request.into_inner();
        let object_type = vci_service_interface::ObjectType::try_from(request.object_type)
            .map_err(|_| Status::invalid_argument("object_type is invalid"))?;
        let object_id = Self::resolve_object_id(object_type, &request.shortname)?;

        Ok(Response::new(vci_service_interface::ObjectIdResponse {
            pdu_object_id: object_id,
        }))
    }

    pub(super) async fn rpc_get_unique_resp_id_table(
        &self,
        request: Request<vci_service_interface::GetUniqueRespIdTableRequest>,
    ) -> Result<Response<vci_service_interface::UniqueRespIdTableResponse>, Status> {
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;

        let links = self.logical_links.lock().await;
        let link = links
            .get(&handle)
            .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;

        let unique_data: Vec<vci_service_interface::EcuUniqueRespData> = if link
            .working_unique_resp_id_table
            .is_empty()
        {
            // No table has been configured yet (ISO 22900-2 §9.3.3.6):
            // return a single template entry with UniqueRespIdentifier = PDU_ID_UNDEF.
            // The params list enumerates all PDU_PC_UNIQUE_ID class params for this
            // protocol with their current Working Set values (0 / empty when unset).
            // Clients use this template to discover which params to fill in per ECU
            // before calling SetUniqueRespIdTable.
            let (unum32_ids, bytes_ids) = comparam_support::unique_id_params(link.protocol);
            let mut params = Vec::with_capacity(unum32_ids.len() + bytes_ids.len());
            for &param_id in unum32_ids {
                params.push(vci_service_interface::ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(param_id.0)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcUniqueId as i32,
                    param_data: Some(vci_service_interface::param_item::ParamData::Unum32(
                        link.working.unum32.get(&param_id).copied().unwrap_or(0),
                    )),
                });
            }
            for &param_id in bytes_ids {
                params.push(vci_service_interface::ParamItem {
                    id: Some(vci_service_interface::param_item::Id::ParamId(param_id.0)),
                    com_param_class: vci_service_interface::PduParamClass::PduPcUniqueId as i32,
                    param_data: Some(vci_service_interface::param_item::ParamData::Bytefield(
                        link.working
                            .bytes
                            .get(&param_id)
                            .cloned()
                            .unwrap_or_default(),
                    )),
                });
            }
            vec![vci_service_interface::EcuUniqueRespData {
                unique_resp_identifier: PDU_ID_UNDEF,
                params,
            }]
        } else {
            link.working_unique_resp_id_table
                .iter()
                .map(|entry| {
                    let mut params =
                        Vec::with_capacity(entry.params.unum32.len() + entry.params.bytes.len());
                    for (&param_id, &value) in &entry.params.unum32 {
                        params.push(vci_service_interface::ParamItem {
                            id: Some(vci_service_interface::param_item::Id::ParamId(param_id.0)),
                            com_param_class: vci_service_interface::PduParamClass::PduPcUniqueId
                                as i32,
                            param_data: Some(vci_service_interface::param_item::ParamData::Unum32(
                                value,
                            )),
                        });
                    }
                    for (&param_id, bytes) in &entry.params.bytes {
                        params.push(vci_service_interface::ParamItem {
                            id: Some(vci_service_interface::param_item::Id::ParamId(param_id.0)),
                            com_param_class: vci_service_interface::PduParamClass::PduPcUniqueId
                                as i32,
                            param_data: Some(
                                vci_service_interface::param_item::ParamData::Bytefield(
                                    bytes.clone(),
                                ),
                            ),
                        });
                    }
                    vci_service_interface::EcuUniqueRespData {
                        unique_resp_identifier: entry.unique_resp_identifier,
                        params,
                    }
                })
                .collect()
        };

        Ok(Response::new(
            vci_service_interface::UniqueRespIdTableResponse {
                unique_resp_id_table: Some(vci_service_interface::UniqueRespIdTableItem {
                    unique_data,
                }),
            },
        ))
    }

    /// Stages `request`'s table into `working_unique_resp_id_table` only --
    /// `SetUniqueRespIdTable` never touches hardware itself (ADR-068). It is
    /// promoted to `active_unique_resp_id_table` -- along with the
    /// ISO15765 `FLOW_CONTROL_FILTER`/pass-all-fallback/UUDT-companion
    /// reconciliation this RPC used to perform inline -- at this CLL's own
    /// `ConnectComLogicalLink` or at `CoptUpdateparam` execution (see
    /// `J2534Service::promote_unique_resp_id_table`).
    pub(super) async fn rpc_set_unique_resp_id_table(
        &self,
        request: Request<vci_service_interface::SetUniqueRespIdTableRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;

        let table_item = request
            .unique_resp_id_table
            .ok_or_else(|| Status::invalid_argument("unique_resp_id_table is required"))?;

        // Snapshot the connection state needed to enforce LOCK_PHYSICAL_COM_PARAMS
        // below, before storing the new table.
        let (protocol, hw_protocol_id, base_hw_protocol_id, channel_key, pin_select, _last_error) = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
            (
                link.protocol,
                // Raw (possibly `_PS`) id -- still needed below for
                // `find_physical_lock_holder` (Plane C).
                link.hw_protocol_id,
                // ADR-157 Plane B: normalized for the LOCK_PHYSICAL_COM_PARAMS
                // gate below.
                link.base_hw_protocol_id(),
                link.channel_key,
                // Plane C, ADR-157 Bug D fix -- `find_physical_lock_holder`'s
                // pre-connect fallback discriminant.
                link.pin_select,
                link.last_error.clone(),
            )
        };

        // On an ISO15765 hardware channel, this table eventually drives real
        // hardware I/O once promoted (PassThruStartMsgFilter / PassThruStopMsgFilter,
        // ADR-039) — treat staging it in Working as a physical-ComParam write for
        // LOCK_PHYSICAL_COM_PARAMS purposes, same as SetComParam does for
        // physical-layer params (ADR-043; amended by ADR-068: the check stays at
        // Set/stage time even though the hardware I/O itself now happens at
        // promotion). Other hardware protocols never touch hardware from this
        // table, so they are exempt, same as SetComParam exempts service-level
        // (non-physical) params. In software-ISO-TP mode the channel is raw CAN
        // and routing is applied in the poll task, so no hardware I/O happens
        // here either (ADR-046).
        if base_hw_protocol_id == j2534_0404::ISO15765 {
            let links = self.logical_links.lock().await;
            if find_physical_lock_holder(
                &links,
                handle,
                hw_protocol_id,
                pin_select,
                channel_key,
                LOCK_PHYSICAL_COM_PARAMS,
            )
            .is_some()
            {
                let last_error = links.get(&handle).and_then(|l| l.last_error.clone());
                return Err(state_guard_status(
                    Code::ResourceExhausted,
                    "physical ComParam lock is held by another ComLogicalLink on this resource",
                    PduError::PduErrRscLockedByOtherCll,
                    last_error,
                ));
            }
        }

        let (unique_unum32, unique_bytes) = comparam_support::unique_id_params(protocol);

        let mut entries = Vec::with_capacity(table_item.unique_data.len());
        for ecu_data in table_item.unique_data {
            let mut params = ComParamSet::default();
            for item in ecu_data.params {
                let param_id = match item
                    .id
                    .ok_or_else(|| Status::invalid_argument("param_item.id is required"))?
                {
                    vci_service_interface::param_item::Id::ParamId(id) => ComParamId(id),
                    vci_service_interface::param_item::Id::ParamName(name) => {
                        Self::resolve_comparam_name(&name)?
                    }
                };
                match item.param_data {
                    Some(vci_service_interface::param_item::ParamData::Unum32(v)) => {
                        if !unique_unum32.contains(&param_id) {
                            // Read `last_error` fresh at the guard decision itself,
                            // per `state_guard_status`'s same-lock-acquisition
                            // contract -- this loop iterates a client-controlled,
                            // unbounded table with no lock held across iterations,
                            // so a snapshot taken once before the loop can go stale
                            // (another RPC updating this CLL's tracked error on this
                            // service's multi-threaded #[tokio::main] runtime)
                            // before a later entry triggers this rejection.
                            let last_error = self
                                .logical_links
                                .lock()
                                .await
                                .get(&handle)
                                .and_then(|l| l.last_error.clone());
                            return Err(state_guard_status(
                                Code::InvalidArgument,
                                format!(
                                    "PDU_ERR_COMPARAM_NOT_SUPPORTED: ComParam {:#010x} is not \
                                     PDU_PC_UNIQUE_ID class for protocol {:#010x}; \
                                     UniqueRespIdTable only accepts PDU_PC_UNIQUE_ID class \
                                     params (ISO 22900-2 §9.3.3.6)",
                                    param_id.0,
                                    protocol.value(),
                                ),
                                PduError::PduErrComparamNotSupported,
                                last_error,
                            ));
                        }
                        params.unum32.insert(param_id, v);
                    }
                    Some(vci_service_interface::param_item::ParamData::Bytefield(b)) => {
                        if !unique_bytes.contains(&param_id) {
                            let last_error = self
                                .logical_links
                                .lock()
                                .await
                                .get(&handle)
                                .and_then(|l| l.last_error.clone());
                            return Err(state_guard_status(
                                Code::InvalidArgument,
                                format!(
                                    "PDU_ERR_COMPARAM_NOT_SUPPORTED: ComParam {:#010x} is not \
                                     PDU_PC_UNIQUE_ID class for protocol {:#010x}; \
                                     UniqueRespIdTable only accepts PDU_PC_UNIQUE_ID class \
                                     params (ISO 22900-2 §9.3.3.6)",
                                    param_id.0,
                                    protocol.value(),
                                ),
                                PduError::PduErrComparamNotSupported,
                                last_error,
                            ));
                        }
                        params.bytes.insert(param_id, b);
                    }
                    _ => {
                        return Err(Status::unimplemented(
                            "only unum32 and bytefield params are supported in UniqueRespIdTable",
                        ));
                    }
                }
            }
            entries.push(EcuUniqueRespEntry {
                unique_resp_identifier: ecu_data.unique_resp_identifier,
                params,
            });
        }

        // Stage into Working only -- no hardware I/O here (ADR-068). The
        // ISO15765 FLOW_CONTROL_FILTER install/remove, the pass-all-fallback
        // sync, and the dual-channel-mode UUDT companion-channel open/close
        // this RPC used to perform inline all move to promotion time (this
        // CLL's own ConnectComLogicalLink, or CoptUpdateparam execution via
        // `J2534Service::promote_unique_resp_id_table`).
        {
            let mut links = self.logical_links.lock().await;
            let link = links
                .get_mut(&handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
            link.working_unique_resp_id_table = entries;
        }

        Ok(Self::empty_response())
    }
}

/// Codex review on PR #3 (ADR-140 follow-up), design-advisor-specified fix:
/// `ioctl_set_event_queue_properties` must set `CllEventQueue::event_queue_cap`/
/// `event_queue_mode` AND run the cap-trim loop in ONE `rx_buf` lock
/// acquisition, not the write followed by a separately-locked trim -- the
/// crux of the fix (see that handler's own doc comment).
///
/// **Correction (edge-case-hunter review):** the sequential test below
/// (`policy_change_and_trim_are_atomic_so_a_push_right_after_never_exceeds_the_new_cap`)
/// does NOT actually prove the one-critical-section property despite its
/// name -- it calls the IOCTL handler to completion, then checks state
/// afterward, so nothing ever races during the test. It passes identically
/// whether the write and the trim share one `rx_buf` lock acquisition or two
/// separate ones (confirmed by temporarily re-splitting the real handler and
/// re-running it: still passed). It is kept as a basic end-state sanity
/// check (e.g. it would still catch the trim being dropped entirely), but
/// the genuine concurrency proof is
/// `concurrent_push_racing_the_gap_between_write_and_trim_never_leaks_pre_trim_backlog_live`
/// below -- see its own doc comment for the mechanism and for why a bare
/// final-state check can never distinguish split from atomic here, even
/// under a forced race.
#[cfg(test)]
mod ioctl_set_event_queue_properties_atomic_trim_tests {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Arc;

    use tokio::sync::{Mutex, mpsc};

    use super::*;

    const TEST_HANDLE: u32 = 1;

    /// Builds a minimal `J2534Service` backed by the mock J2534 cdylib, with
    /// a single not-yet-connected `LogicalLinkState` at `TEST_HANDLE` whose
    /// `rx_buf` is pre-seeded with `backlog` `CllStatus` items at
    /// `initial_cap`. Mirrors `rpc_link.rs::tests::
    /// service_with_auto_can_mode_and_no_links`'s construction shape.
    async fn service_with_seeded_queue(initial_cap: usize, backlog: usize) -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");

        let mut items = VecDeque::new();
        for ts in 0..backlog {
            items.push_back(CllQueueItem::Status(TrackedStatus {
                event: StatusEvent::Cll(
                    vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline,
                ),
                timestamp: ts as u32,
            }));
        }

        let mut logical_links = HashMap::new();
        logical_links.insert(
            TEST_HANDLE,
            LogicalLinkState {
                channel_id: None,
                protocol: ChannelProtocol::CAN,
                hw_protocol_id: 0,
                software_isotp: false,
                uudt_channel_id: None,
                uudt_channel_key: None,
                isotp_rx: Arc::new(Mutex::new(HashMap::new())),
                connect_in_flight: std::sync::Weak::new(),
                connected: false,
                comm_started: false,
                raw_mode: false,
                checksum_mode: false,
                connect_generation: 0,
                stop_comm_pending: false,
                channel_key: None,
                pin_select: None,
                channel_index: None,
                base_hw_protocol_override: None,
                rx_buf: Arc::new(Mutex::new(CllEventQueue {
                    items,
                    event_queue_cap: initial_cap,
                    ..CllEventQueue::default()
                })),
                working: ComParamSet::default(),
                active: ComParamSet::default(),
                tester_present_state: TesterPresentState::None,
                tester_present_base_tx_flags: 0,
                open_tp_discards: Vec::new(),
                working_unique_resp_id_table: Vec::new(),
                active_unique_resp_id_table: Vec::new(),
                unique_resp_filter_ids: Vec::new(),
                cancelled_cops: HashSet::new(),
                held_lock_mask: 0,
                last_error: None,
                tx_held: VecDeque::new(),
                tx_suspended_by_ioctl: false,
                tx_suspended_by_lock: false,
                tx_suspended_by_error: false,
                error_clear_seq: 0,
                error_set_seq: 0,
                client_filters: HashMap::new(),
                repeat_message_ids: Vec::new(),
                pending_client_filters: HashMap::new(),
                registrants: Vec::new(),
                next_registrant_seq: 0,
                j1939_claimed_address: None,
                j1939_claim_cursor: 0,
                j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
                tp20_connection: None,
                tp20_broadcast_periodic: None,
            },
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::default(),
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(logical_links)),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(HashMap::new())),
            primitives: Arc::new(Mutex::new(HashMap::new())),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[tokio::test]
    async fn policy_change_and_trim_are_atomic_so_a_push_right_after_never_exceeds_the_new_cap() {
        // Seeded well over the new cap, at the pre-existing default cap
        // (RX_BUF_CAPACITY), same shape as a CLL that has been quietly
        // accumulating backlog under the old, larger policy.
        let service = service_with_seeded_queue(RX_BUF_CAPACITY, 5).await;
        let new_cap = 2usize;

        service
            .ioctl_set_event_queue_properties(
                TEST_HANDLE,
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::EventQueueProperty(
                        vci_service_interface::IoEventQueueProperty {
                            queue_size: new_cap as u32,
                            queue_mode: vci_service_interface::PduQueueMode::PduQueCircular as i32,
                        },
                    )),
                }),
            )
            .await
            .expect("SET_EVENT_QUEUE_PROPERTIES should succeed on a not-yet-connected CLL");

        let rx_buf = {
            let links = service.logical_links.lock().await;
            links.get(&TEST_HANDLE).unwrap().rx_buf.clone()
        };
        assert_eq!(
            rx_buf.lock().await.items.len(),
            new_cap,
            "the trim must already have applied by the time the IOCTL call returns -- the \
             write and the trim are one critical section, not write-then-separately-trim"
        );

        // One more push through a REAL `deliver_or_enqueue` producer: no
        // timing race needed to make this deterministic, because the policy
        // write and the trim above already happened atomically -- this push
        // reads the already-lowered cap fresh under the queue's own lock, the
        // same lock the IOCTL handler just released.
        events::send_cll_status(
            &service.subscriptions,
            &service.logical_links,
            TEST_HANDLE,
            vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline,
        )
        .await;

        assert_eq!(
            rx_buf.lock().await.items.len(),
            new_cap,
            "a push right after SET_EVENT_QUEUE_PROPERTIES must never exceed the new cap"
        );
    }

    /// Genuine concurrent regression test for the one-critical-section
    /// property the sequential test above cannot actually exercise (see
    /// this module's own doc comment for edge-case-hunter's finding).
    ///
    /// **Mechanism**: `tokio::sync::Mutex` grants its lock to queued waiters
    /// strictly FIFO. The test holds `rx_buf` itself first, spawns the IOCTL
    /// call (which blocks trying to acquire `rx_buf`, becoming FIFO waiter
    /// #1), yields until that registration has actually happened, then
    /// spawns a concurrent push through a REAL producer (`events::
    /// send_cll_status`, which also blocks on `rx_buf`, becoming FIFO waiter
    /// #2 -- *behind* the IOCTL call), yields again, then drops the held
    /// guard and lets both run.
    ///
    /// For the real, ONE-critical-section handler, the IOCTL task's single
    /// `rx_buf.lock().await` is not released until BOTH the policy write and
    /// the trim are done, so the push (already queued as waiter #2 before
    /// that release) always observes the fully-updated, already-trimmed
    /// queue.
    ///
    /// For a manually re-split handler (two separate `rx_buf.lock().await`
    /// calls, write then trim -- edge-case-hunter's exact repro), the IOCTL
    /// task's SECOND lock attempt is a brand-new acquisition that has to
    /// queue behind the ALREADY-WAITING push (waiter #2 registered before
    /// the IOCTL task's first lock was even released) -- so the push's
    /// `deliver_or_enqueue` deterministically runs in the gap between the
    /// write and the trim, against the STILL-untrimmed backlog.
    ///
    /// **Why a bare final-state check cannot distinguish these two cases,
    /// even under this forced race** (reasoned through by hand, both
    /// orderings): `push_cll_event`'s evict-oldest-append-newest and the
    /// trim loop's pop-from-front are both just "remove from the front" --
    /// interleaving a push's conditional evict with the trim's unconditional
    /// pops converges to the identical final "last `cap` items, in order"
    /// content regardless of exactly when, relative to each other, they run,
    /// for both `OverwriteOldest` and `DiscardNewest`. So this test does NOT
    /// assert on final queue length/content (see the sequential test above
    /// for that, which is necessary but not sufficient). The genuinely
    /// observable difference is what a LIVE `SubscribeEvent` subscriber
    /// sees: `deliver_or_enqueue` opportunistically drains the ENTIRE queue
    /// out to any live subscriber, in the same lock, right after its own
    /// push. If that push (and drain) happens BEFORE the trim (split
    /// handler), the still-untrimmed backlog -- which the trim would
    /// otherwise have silently discarded, never exposing it anywhere -- gets
    /// drained out live instead: real, observable data reaches a subscriber
    /// that the atomic handler guarantees can never see it.
    ///
    /// **Empirical confirmation (manual, per this repo's
    /// `codex-pr-review-loop` convention):** against the real handler, this
    /// test passes (`status_count == 2`: `timestamp` 4 -- the one backlog
    /// item the trim correctly leaves behind -- plus the racing push's own
    /// new item). Manually re-splitting `ioctl_set_event_queue_properties`'s
    /// single `rx_buf.lock().await` into two separate acquisitions (write,
    /// then a second `rx_buf.lock().await` for the trim loop) and re-running
    /// this test in isolation (`cargo test -p j2534-0404-service
    /// concurrent_push_racing_the_gap_between_write_and_trim_never_leaks_pre_trim_backlog_live
    /// -- --exact`) makes it fail: `status_count` becomes 5 --
    /// `timestamp` 1, 2, and 3, which the atomic handler always discards
    /// without ever exposing them to a subscriber, leak out live too
    /// (empirically confirmed: `left: 5, right: 2`).
    #[tokio::test]
    async fn concurrent_push_racing_the_gap_between_write_and_trim_never_leaks_pre_trim_backlog_live()
     {
        let service = service_with_seeded_queue(RX_BUF_CAPACITY, 5).await;
        let new_cap = 2usize;

        let rx_buf = {
            let links = service.logical_links.lock().await;
            links.get(&TEST_HANDLE).unwrap().rx_buf.clone()
        };

        // A live subscriber, attached directly to the queue -- mirrors
        // `events::deliver_or_enqueue_live_sender_tests`'s own pattern of
        // setting `live_sender` directly rather than going through
        // `rpc_subscribe_event`, since only `deliver_or_enqueue`'s
        // drain-on-push behavior (not the subscription bookkeeping) matters
        // here.
        let (tx, mut rx) = mpsc::unbounded_channel();
        rx_buf.lock().await.live_sender = Some(tx);

        // Test holds `rx_buf` first so both spawned tasks below block on it,
        // in the order they attempt to acquire it.
        let hold = rx_buf.lock().await;

        let svc_ioctl = service.clone();
        let ioctl_task = tokio::spawn(async move {
            svc_ioctl
                .ioctl_set_event_queue_properties(
                    TEST_HANDLE,
                    Some(vci_service_interface::DataItem {
                        data: Some(vci_service_interface::data_item::Data::EventQueueProperty(
                            vci_service_interface::IoEventQueueProperty {
                                queue_size: new_cap as u32,
                                queue_mode: vci_service_interface::PduQueueMode::PduQueCircular
                                    as i32,
                            },
                        )),
                    }),
                )
                .await
        });

        // Let the IOCTL task run up to (and register as FIFO waiter #1 on)
        // `rx_buf`: it has only one prior await point (`logical_links`,
        // uncontended), so a handful of yields is ample.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let svc_push = service.clone();
        let push_task = tokio::spawn(async move {
            events::send_cll_status(
                &svc_push.subscriptions,
                &svc_push.logical_links,
                TEST_HANDLE,
                vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline,
            )
            .await;
        });

        // Let the push task run up to (and register as FIFO waiter #2,
        // BEHIND the IOCTL task, on) `rx_buf`.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        drop(hold);

        let (ioctl_res, push_res) = tokio::join!(ioctl_task, push_task);
        ioctl_res
            .expect("IOCTL task should not panic")
            .expect("SET_EVENT_QUEUE_PROPERTIES should succeed on a not-yet-connected CLL");
        push_res.expect("push task should not panic");

        let mut lost_count = 0usize;
        let mut status_count = 0usize;
        while let Ok(notification) = rx.try_recv() {
            let notification = notification.expect("Ok notification");
            match notification.event_data {
                Some(vci_service_interface::event_notification::EventData::Lost(_)) => {
                    lost_count += 1;
                }
                Some(vci_service_interface::event_notification::EventData::Item(
                    vci_service_interface::EventItem {
                        data: Some(vci_service_interface::event_item::Data::CllStatus(_)),
                        ..
                    },
                )) => {
                    status_count += 1;
                }
                other => panic!("unexpected notification: {other:?}"),
            }
        }

        assert_eq!(
            lost_count, 1,
            "exactly one item is ever evicted in this scenario, however the write/trim/push \
             race interleaves"
        );
        assert_eq!(
            status_count, 2,
            "only the item the trim correctly leaves behind (timestamp 4) and the racing \
             push's own new item should ever reach a live subscriber -- a split write-then-trim \
             lets the still-untrimmed backlog (timestamps 1, 2, and 3) leak out live instead, \
             which would push this count to 5"
        );
    }
}

/// ADR-161 Phase 1's occupancy-epoch predicate (`channel_occupancy_matches_snapshot`)
/// exercised directly against a bare `shared_channels`-shaped map, per the
/// ADR's own claim that this is unit-testable without any interleaving
/// hooks -- unlike the lock-ordering/critical-section-boundary properties
/// around it, which the ADR leaves to code inspection plus an
/// `edge-case-hunter` pass.
#[cfg(test)]
mod ioctl_reset_occupancy_epoch_tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;

    /// Builds a `SharedChannel` for direct insertion into a test-local
    /// `shared_channels`-shaped map -- mirrors `rpc_link.rs`'s own
    /// `dead_shared_channel_for_test` construction shape, with a
    /// caller-supplied `ref_count`/`occupancy_epoch`.
    fn test_shared_channel(
        channel_id: ChannelId,
        ref_count: u32,
        occupancy_epoch: u64,
    ) -> SharedChannel {
        SharedChannel {
            channel_id,
            ref_count,
            tx_queue: tokio::sync::mpsc::unbounded_channel().0,
            executing_cop: Arc::new(Mutex::new(None)),
            _poll_cancel: tokio::sync::oneshot::channel().0,
            connect_flags: 0,
            dead: false,
            occupancy_epoch,
            leaked_repeat_message_ids: Vec::new(),
            leaked_periodic_message_ids: Vec::new(),
            applied_analog_sample_rate: None,
            applied_analog_samples_per_reading: None,
            applied_analog_readings_per_msg: None,
            j1939_claims: HashMap::new(),
            j1939_claim_results: HashMap::new(),
            j1939_reclaim_pending: HashMap::new(),
            leaked_j1939_claims: Vec::new(),
            tp20_connections: HashMap::new(),
            tp20_connection_results: HashMap::new(),
            become_master_in_flight: Arc::new(portable_atomic::AtomicBool::new(false)),
            tp20_passive: None,
        }
    }

    /// Direct truth-table proof of `channel_occupancy_matches_snapshot`: a
    /// settled epoch matches itself; any numeric mismatch, or either side
    /// missing entirely (channel torn down and not recreated, or -- should
    /// not happen in practice -- absent from the snapshot), is reported as
    /// a mismatch, never a match.
    #[test]
    fn matches_only_when_both_sides_are_present_and_equal() {
        assert!(channel_occupancy_matches_snapshot(Some(5), Some(5)));
        assert!(!channel_occupancy_matches_snapshot(Some(6), Some(5)));
        assert!(!channel_occupancy_matches_snapshot(None, Some(5)));
        assert!(!channel_occupancy_matches_snapshot(Some(5), None));
        assert!(!channel_occupancy_matches_snapshot(None, None));
    }

    /// ADR-161's own claim under test: a sibling CLL joining an
    /// already-open shared channel -- bumping `ref_count` AND re-stamping
    /// `occupancy_epoch` together, exactly as both `rpc_link.rs` join sites
    /// (the primary-connect join and the UUDT-companion join) do -- between
    /// RESET's snapshot and Phase 1 reaching this channel must be detected
    /// as a mismatch against the OLD snapshotted epoch. This is the exact
    /// hazard a `connect_generation` scan alone misses for a UUDT-companion
    /// join, which never touches that field.
    #[test]
    fn a_ref_count_and_epoch_bump_together_is_flagged_as_a_mismatch() {
        let channel_id = ChannelId(42);
        let key: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);
        let mut chans: HashMap<ChannelKey, SharedChannel> = HashMap::new();
        chans.insert(key, test_shared_channel(channel_id, 1, 7));

        // RESET's snapshot: taken while the channel is sole-owned, at epoch 7.
        let snapshot_epoch = chans.get(&key).map(|sc| sc.occupancy_epoch);
        assert_eq!(snapshot_epoch, Some(7));

        // A sibling join lands before Phase 1 reaches this channel.
        {
            let sc = chans.get_mut(&key).expect("seeded above");
            sc.ref_count += 1;
            sc.occupancy_epoch = 8;
        }

        let live_epoch = chans
            .values()
            .find(|sc| sc.channel_id == channel_id)
            .map(|sc| sc.occupancy_epoch);
        assert_eq!(live_epoch, Some(8));

        assert!(
            !channel_occupancy_matches_snapshot(live_epoch, snapshot_epoch),
            "a join completing between RESET's snapshot and Phase 1 reaching this channel must \
             be flagged as a mismatch, blocking the channel-wide buffer clears"
        );
    }

    /// The mirror-image case: a `ref_count` DECREMENT alone (a sibling
    /// disconnecting, not joining) must NEVER bump `occupancy_epoch`
    /// (ADR-161 is explicit about this), so it must NOT be flagged as a
    /// mismatch -- releasing an occupant cannot introduce a session RESET
    /// never observed.
    #[test]
    fn a_ref_count_decrement_alone_is_not_flagged_as_a_mismatch() {
        let channel_id = ChannelId(42);
        let key: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);
        let mut chans: HashMap<ChannelKey, SharedChannel> = HashMap::new();
        chans.insert(key, test_shared_channel(channel_id, 2, 7));

        let snapshot_epoch = chans.get(&key).map(|sc| sc.occupancy_epoch);
        assert_eq!(snapshot_epoch, Some(7));

        // A sibling disconnects: `ref_count` drops, `occupancy_epoch` is
        // left untouched.
        {
            let sc = chans.get_mut(&key).expect("seeded above");
            sc.ref_count -= 1;
        }

        let live_epoch = chans
            .values()
            .find(|sc| sc.channel_id == channel_id)
            .map(|sc| sc.occupancy_epoch);
        assert_eq!(live_epoch, Some(7));

        assert!(
            channel_occupancy_matches_snapshot(live_epoch, snapshot_epoch),
            "a plain decrement (sibling disconnect, no join) must not be flagged as a mismatch"
        );
    }

    /// ADR-161's rollback sub-rule, base case: a join that stamps E1 over E0
    /// then rolls back (destroyed before it published) must restore the
    /// pre-join epoch E0 -- exactly matching the pre-join snapshot again, so
    /// a RESET snapshotting the channel BEFORE the failed join sees no
    /// spurious mismatch.
    #[test]
    fn rollback_restores_the_pre_join_epoch_when_no_sibling_joined_since() {
        let channel_id = ChannelId(42);
        let mut sc = test_shared_channel(channel_id, 1, 0 /* E0 */);

        // The join: bump ref_count, stamp E1, capturing (stamped, prev).
        sc.ref_count += 1;
        let prev_epoch = sc.occupancy_epoch;
        let stamped_epoch = 1; // E1
        sc.occupancy_epoch = stamped_epoch;
        assert_eq!(sc.occupancy_epoch, 1);

        // The join rolls back: compare-and-restore.
        J2534Service::restore_occupancy_epoch_on_rollback(
            &mut sc,
            Some((stamped_epoch, prev_epoch)),
        );

        assert_eq!(
            sc.occupancy_epoch, 0,
            "a rolled-back join with no surviving sibling must restore the exact pre-join epoch"
        );
    }

    /// ADR-161's rollback sub-rule, the load-bearing case: a second, LATER
    /// join (E2) lands after the first join's own stamp (E1) but before the
    /// first join's rollback runs. Rolling back the FIRST join must NOT
    /// erase the second join's newer stamp -- the live epoch is E2, not E1,
    /// so the compare fails and the epoch is left at E2. Rolling back the
    /// SECOND join afterward correctly restores E1 (its own immediate
    /// predecessor), since by then its own stamp (E2) is once again the live
    /// value -- see the inline comment below for why this does not (and, by
    /// this mechanism's design, cannot) unwind all the way back to E0.
    #[test]
    fn rollback_does_not_erase_a_surviving_siblings_newer_epoch() {
        let channel_id = ChannelId(42);
        let mut sc = test_shared_channel(channel_id, 1, 0 /* E0 */);

        // First join: stamps E1.
        sc.ref_count += 1;
        let first_prev_epoch = sc.occupancy_epoch; // E0
        let first_stamped_epoch = 1; // E1
        sc.occupancy_epoch = first_stamped_epoch;

        // Second, later join lands before the first join's rollback: stamps E2.
        sc.ref_count += 1;
        let second_prev_epoch = sc.occupancy_epoch; // E1
        let second_stamped_epoch = 2; // E2
        sc.occupancy_epoch = second_stamped_epoch;
        assert_eq!(sc.occupancy_epoch, 2);

        // Roll back the FIRST join. The live epoch (E2) no longer equals the
        // first join's own stamp (E1), so this must be a no-op.
        J2534Service::restore_occupancy_epoch_on_rollback(
            &mut sc,
            Some((first_stamped_epoch, first_prev_epoch)),
        );
        assert_eq!(
            sc.occupancy_epoch, 2,
            "rolling back the first join must NOT erase the second join's still-live, newer \
             epoch stamp -- this is the exact 'don't erase a surviving sibling's evidence' case"
        );

        // Now roll back the SECOND join too: its own stamp (E2) is still
        // live, so this restores the epoch to whatever WAS live right
        // before the second join's own stamp -- E1, the first join's
        // stamp -- not all the way back to the original E0. Each rollback
        // only knows its own immediate predecessor, never the full join
        // history; unwinding all the way back to E0 here would additionally
        // require re-attempting the first join's rollback now that its own
        // stamp (E1) is live again, which is not a valid real-world
        // sequence (a rollback fires at most once, exactly when its own CLL
        // is destroyed).
        J2534Service::restore_occupancy_epoch_on_rollback(
            &mut sc,
            Some((second_stamped_epoch, second_prev_epoch)),
        );
        assert_eq!(
            sc.occupancy_epoch, 1,
            "rolling back the second join, once its own stamp is confirmed still live, must \
             restore to the epoch that was live immediately before ITS OWN stamp (E1), not the \
             original pre-any-join epoch (E0)"
        );
    }
}

/// SAE J2534-2 clause 14 Repeat Messaging leaked-STOP bookkeeping (ADR-165
/// Decision 6, Codex review PR #42 round 2 Finding A) -- direct unit
/// coverage of `SharedChannel::leaked_repeat_message_ids` and
/// `retry_leaked_repeat_message_stops`, both private to this module, so
/// these tests live here rather than in `rpc_link.rs`'s own test module
/// (Rust privacy: a plain `fn`/`async fn` inside an `impl` block is only
/// visible to the module it is written in and that module's descendants).
///
/// At the time these unit tests were first written, no mock
/// `__mock_set_*_error`-style hook existed for `STOP_REPEAT_MESSAGE`
/// specifically (unlike `stop_message_filter`'s
/// `__mock_set_stop_filter_error`), so they drove this leaked-STOP
/// bookkeeping path using a `MsgId` that was never started -- reliably
/// failing the same way `ERR_INVALID_MSG_ID` would for a genuinely-orphaned
/// slot, exercising the bookkeeping mechanism itself deterministically
/// without a real-time wait for self-completion or a genuinely-connected
/// channel/device. **ADR-180 Decision 20** (round 20) later added
/// `__mock_set_stop_repeat_message_error` (mirroring `stop_message_filter`'s
/// own hook) for the SIBLING case these unit tests do not cover -- a
/// genuinely-live, still-retransmitting `MsgId` whose native STOP fails --
/// with its own end-to-end regression test in
/// `tests/grpc_mock/j1939.rs::spontaneous_loss_leaked_repeat_stop_is_drained_by_a_later_sibling_ioctl`.
/// These unit tests remain valid and are not superseded by that addition --
/// they cover the private `SharedChannel`/`retry_leaked_repeat_message_ids`
/// bookkeeping directly, which the end-to-end test cannot reach from the
/// live RPC surface alone.
#[cfg(test)]
mod repeat_message_leak_tests {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Arc;

    use tokio::sync::{Mutex, mpsc};
    use tonic::Request;

    use super::*;

    const CLL_A: u32 = 1;
    const CLL_B: u32 = 2;

    /// A minimal, already-`connected: true` `LogicalLinkState` sharing
    /// `channel_key`/`channel_id` with whatever sibling the caller also
    /// inserts -- mirrors `rpc_primitive.rs::tests::service_with_one_link`'s
    /// field-by-field construction shape (this crate's established pattern
    /// for a hand-built `LogicalLinkState` in a unit test), adjusted for a
    /// connected link with a specific `repeat_message_ids` set.
    fn connected_link(
        channel_id: ChannelId,
        channel_key: ChannelKey,
        repeat_message_ids: Vec<u32>,
    ) -> LogicalLinkState {
        LogicalLinkState {
            channel_id: Some(channel_id),
            protocol: ChannelProtocol::CAN,
            hw_protocol_id: j2534_0404::CAN,
            software_isotp: false,
            uudt_channel_id: None,
            uudt_channel_key: None,
            isotp_rx: Arc::new(Mutex::new(HashMap::new())),
            connect_in_flight: std::sync::Weak::new(),
            connected: true,
            comm_started: false,
            raw_mode: false,
            checksum_mode: false,
            connect_generation: 0,
            stop_comm_pending: false,
            channel_key: Some(channel_key),
            pin_select: None,
            channel_index: None,
            base_hw_protocol_override: None,
            rx_buf: Arc::new(Mutex::new(CllEventQueue {
                event_queue_cap: 16,
                ..CllEventQueue::default()
            })),
            working: ComParamSet::default(),
            active: ComParamSet::default(),
            tester_present_state: TesterPresentState::None,
            tester_present_base_tx_flags: 0,
            open_tp_discards: Vec::new(),
            working_unique_resp_id_table: Vec::new(),
            active_unique_resp_id_table: Vec::new(),
            unique_resp_filter_ids: Vec::new(),
            cancelled_cops: HashSet::new(),
            held_lock_mask: 0,
            last_error: None,
            tx_held: VecDeque::new(),
            tx_suspended_by_ioctl: false,
            tx_suspended_by_lock: false,
            tx_suspended_by_error: false,
            error_clear_seq: 0,
            error_set_seq: 0,
            client_filters: HashMap::new(),
            repeat_message_ids,
            pending_client_filters: HashMap::new(),
            registrants: Vec::new(),
            next_registrant_seq: 0,
            j1939_claimed_address: None,
            j1939_claim_cursor: 0,
            j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
            tp20_connection: None,
            tp20_broadcast_periodic: None,
        }
    }

    /// Builds a `J2534Service` with a real device open and a real physical
    /// CAN channel connected through the mock cdylib, plus `CLL_A`/`CLL_B`
    /// already `connected: true` and sharing that channel (`SharedChannel::
    /// ref_count == 2`). Mirrors `rpc_link.rs::tests::
    /// service_with_auto_can_mode_and_no_links`'s construction shape, with
    /// the create/connect RPC handshake done directly against the real
    /// mock API instead, since these tests only need the post-connect
    /// state, not RPC-level coverage of the handshake itself (already
    /// covered by `rpc_link.rs`'s own connect tests).
    async fn service_with_two_sibling_clls_sharing_a_channel(
        cll_a_repeat_message_ids: Vec<u32>,
    ) -> (J2534Service, ChannelKey) {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
        let device_id = api.open(None).expect("mock open");
        let channel_id = api
            .connect(device_id, j2534_0404::CAN, 0, 500_000)
            .expect("mock connect");
        let channel_key: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);

        let mut logical_links = HashMap::new();
        logical_links.insert(
            CLL_A,
            connected_link(channel_id, channel_key, cll_a_repeat_message_ids),
        );
        logical_links.insert(CLL_B, connected_link(channel_id, channel_key, Vec::new()));

        let mut shared_channels = HashMap::new();
        let (tx_tx, _tx_rx) = mpsc::unbounded_channel();
        let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel();
        shared_channels.insert(
            channel_key,
            SharedChannel {
                channel_id,
                ref_count: 2,
                tx_queue: tx_tx,
                executing_cop: Arc::new(Mutex::new(None)),
                _poll_cancel: cancel_tx,
                connect_flags: 0,
                dead: false,
                occupancy_epoch: 0,
                leaked_repeat_message_ids: Vec::new(),
                leaked_periodic_message_ids: Vec::new(),
                applied_analog_sample_rate: None,
                applied_analog_samples_per_reading: None,
                applied_analog_readings_per_msg: None,
                j1939_claims: HashMap::new(),
                j1939_claim_results: HashMap::new(),
                j1939_reclaim_pending: HashMap::new(),
                leaked_j1939_claims: Vec::new(),
                tp20_connections: HashMap::new(),
                tp20_connection_results: HashMap::new(),
                become_master_in_flight: Arc::new(portable_atomic::AtomicBool::new(false)),
                tp20_passive: None,
            },
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        let service = J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::default(),
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                // Opted into SAE J2534-2 (clause 5, "J2534-2:" pname prefix)
                // -- this whole test module exercises clause 14 Repeat
                // Messaging, which `ioctl_start_repeat_message` gates on
                // this (Finding E, Codex review, ADR-165 PR #42 round 3,
                // needs this fixture to reach the native call at all).
                pname: Some(std::ffi::CString::new("J2534-2:mock").unwrap()),
            }]),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_id: Arc::new(Mutex::new(Some((DEFAULT_MODULE_HANDLE, device_id)))),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(logical_links)),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(shared_channels)),
            primitives: Arc::new(Mutex::new(HashMap::new())),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(2)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        };
        (service, channel_key)
    }

    /// Finding A: `DisconnectComLogicalLink` tearing down `CLL_A` (which
    /// owns a stale `MsgId` its best-effort native STOP fails for) while
    /// `CLL_B` stays connected to the same physical channel must NOT just
    /// log and drop the failure -- the channel is not about to close
    /// (`ref_count` will only drop from 2 to 1), so the failed `MsgId` must
    /// land in `SharedChannel::leaked_repeat_message_ids` instead of being
    /// silently forgotten on a channel with no more owner for it.
    #[tokio::test]
    async fn disconnect_tracks_a_failed_repeat_message_stop_when_the_channel_stays_open() {
        const STALE_MSG_ID: u32 = 999;
        let (service, channel_key) =
            service_with_two_sibling_clls_sharing_a_channel(vec![STALE_MSG_ID]).await;

        service
            .rpc_disconnect_com_logical_link(Request::new(
                vci_service_interface::DisconnectComLogicalLinkRequest {
                    cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle: CLL_A,
                    }),
                },
            ))
            .await
            .expect(
                "DisconnectComLogicalLink must still succeed despite the best-effort \
                 stop_repeat_message failure (mirrors the pre-existing client_filters \
                 best-effort teardown)",
            );

        let chans = service.shared_channels.lock().await;
        let sc = chans
            .get(&channel_key)
            .expect("the shared channel must still exist -- CLL_B still holds a reference");
        assert_eq!(
            sc.ref_count, 1,
            "sanity: CLL_B's own reference must be the only one left"
        );
        assert_eq!(
            sc.leaked_repeat_message_ids,
            vec![STALE_MSG_ID],
            "the MsgId whose best-effort STOP failed while the channel stayed open must be \
             tracked, not silently dropped"
        );
    }

    /// Finding A backstop: the same scenario as above, but `CLL_B` is ALSO
    /// disconnected afterward (`ref_count` reaches 0, tearing the channel
    /// down) -- the leaked `MsgId` must not block or fail that teardown; it
    /// is simply dropped along with the rest of the channel's bookkeeping
    /// (`PassThruDisconnect` forgets every repeat slot along with the
    /// channel itself).
    #[tokio::test]
    async fn disconnect_drops_a_leaked_repeat_message_id_once_the_channel_itself_closes() {
        const STALE_MSG_ID: u32 = 999;
        let (service, channel_key) =
            service_with_two_sibling_clls_sharing_a_channel(vec![STALE_MSG_ID]).await;

        service
            .rpc_disconnect_com_logical_link(Request::new(
                vci_service_interface::DisconnectComLogicalLinkRequest {
                    cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle: CLL_A,
                    }),
                },
            ))
            .await
            .expect("DisconnectComLogicalLink(CLL_A) should succeed");

        service
            .rpc_disconnect_com_logical_link(Request::new(
                vci_service_interface::DisconnectComLogicalLinkRequest {
                    cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle: CLL_B,
                    }),
                },
            ))
            .await
            .expect(
                "DisconnectComLogicalLink(CLL_B) must still succeed even though the channel \
                 it is about to close still carries a leaked MsgId in its bookkeeping",
            );

        assert!(
            !service
                .shared_channels
                .lock()
                .await
                .contains_key(&channel_key),
            "the shared channel entry (and its leaked_repeat_message_ids with it) must be \
             gone once ref_count reaches 0"
        );
    }

    /// `retry_leaked_repeat_message_stops` (Finding A's opportunistic retry
    /// point): given a channel with two leaked `MsgId`s -- one that a fresh
    /// native STOP genuinely succeeds for (started for real via a direct API
    /// call, `Condition == 0` so it never self-completes), and one that
    /// never existed at all -- a retry must prune both: the first because it
    /// was actually stopped, the second because `ERR_INVALID_MSG_ID` means
    /// the device already forgot it (Finding F, Codex review, ADR-165 PR
    /// #42 round 3 -- prior to that fix, this test asserted the
    /// never-existed one stayed tracked as "still failing"; that was itself
    /// the round-3 bug: this module's own doc comment above notes the mock
    /// has no way to fail a STOP with anything other than
    /// `ERR_INVALID_MSG_ID`, so a "genuinely still can't stop it" retained
    /// case is not reproducible against this mock, and treating
    /// `ERR_INVALID_MSG_ID` as still-leaked was never correct regardless).
    #[tokio::test]
    async fn retry_leaked_repeat_message_stops_prunes_both_stopped_and_already_gone_ids() {
        const NEVER_EXISTED_MSG_ID: u32 = 999;
        let (service, channel_key) =
            service_with_two_sibling_clls_sharing_a_channel(Vec::new()).await;

        let channel_id = {
            let chans = service.shared_channels.lock().await;
            chans.get(&channel_key).unwrap().channel_id
        };

        // Start a genuine, still-alive repeat slot directly against the
        // mock API -- Condition == 0 (ADR-173 Decision 1: retransmit
        // through silence, stop only on a matching received frame) never
        // self-completes here since this test never injects any RX frame
        // for it, so its native STOP is guaranteed to succeed regardless
        // of timing.
        let alive_msg_id = {
            let api = service.api.lock().await;
            let message = j2534_0404::PassThruMessage::new(j2534_0404::CAN, 0, 0, 0, 0, &[0x01])
                .expect("message should build");
            let mask = j2534_0404::PassThruMessage::new(j2534_0404::CAN, 0, 0, 0, 0, &[0x00])
                .expect("mask should build");
            let pattern = j2534_0404::PassThruMessage::new(j2534_0404::CAN, 0, 0, 0, 0, &[0x00])
                .expect("pattern should build");
            let mut setup = j2534_0404::RepeatMsgSetup::new(1000, 0, message, mask, pattern);
            api.start_repeat_message(channel_id, &mut setup)
                .expect("start_repeat_message should succeed against a real connected channel")
        };

        {
            let mut chans = service.shared_channels.lock().await;
            let sc = chans.get_mut(&channel_key).unwrap();
            sc.leaked_repeat_message_ids = vec![alive_msg_id, NEVER_EXISTED_MSG_ID];
        }

        {
            let mut chans = service.shared_channels.lock().await;
            service
                .retry_leaked_repeat_message_stops(&mut chans, channel_key)
                .await;
        }

        let chans = service.shared_channels.lock().await;
        let sc = chans.get(&channel_key).unwrap();
        assert!(
            sc.leaked_repeat_message_ids.is_empty(),
            "both the genuinely-stopped MsgId and the never-existed (ERR_INVALID_MSG_ID) one \
             must be pruned -- neither is still-leaked"
        );
    }

    /// Finding F (Codex review, ADR-165 PR #42 round 3): a leaked `MsgId`
    /// whose retry STOP fails with `ERR_INVALID_MSG_ID` (the mock returns
    /// this for any unknown `MsgId`, exactly as it would for a real
    /// self-completed slot the device has already forgotten) must be
    /// dropped from `leaked_repeat_message_ids`, not retained for another
    /// retry -- distinct from `NEVER_EXISTED_MSG_ID`'s sibling case above,
    /// which was written before this fix and (incidentally, since the mock
    /// has only one failure mode for STOP) already exercised the same
    /// `ERR_INVALID_MSG_ID` path but asserted the pre-fix "always retained"
    /// behavior. This test asserts the corrected behavior explicitly.
    #[tokio::test]
    async fn retry_leaked_repeat_message_stops_drops_an_invalid_msg_id_instead_of_retaining_it() {
        const NEVER_EXISTED_MSG_ID: u32 = 999;
        let (service, channel_key) =
            service_with_two_sibling_clls_sharing_a_channel(Vec::new()).await;

        {
            let mut chans = service.shared_channels.lock().await;
            let sc = chans.get_mut(&channel_key).unwrap();
            sc.leaked_repeat_message_ids = vec![NEVER_EXISTED_MSG_ID];
        }

        {
            let mut chans = service.shared_channels.lock().await;
            service
                .retry_leaked_repeat_message_stops(&mut chans, channel_key)
                .await;
        }

        let chans = service.shared_channels.lock().await;
        let sc = chans.get(&channel_key).unwrap();
        assert!(
            sc.leaked_repeat_message_ids.is_empty(),
            "an ERR_INVALID_MSG_ID retry result means the device already forgot this slot -- \
             it must be dropped as cleaned up, not retained for another retry"
        );
    }

    /// Finding E (Codex review, ADR-165 PR #42 round 3): a successful
    /// `START_REPEAT_MESSAGE` returning a `MsgId` that a SIBLING CLL still
    /// carries a stale claim on (e.g. that sibling's own `Condition == 1`
    /// slot self-completed device-side without ever being pruned) must
    /// revoke the sibling's stale claim, not leave it able to
    /// `QUERY`/`STOP` this brand-new, unrelated slot. Driven end-to-end
    /// through the real service/mock: the mock's `next_repeat_msg_id`
    /// counter is fresh (starts at 1) for this test's newly-connected
    /// channel, so CLL_B's very first START is guaranteed to return the
    /// exact numeric MsgId this test seeds as CLL_A's stale entry --
    /// reproducing the recycled-MsgId collision directly, without needing
    /// to bypass the mock's own counter.
    #[tokio::test]
    async fn start_repeat_message_revokes_a_siblings_stale_claim_on_the_reused_msg_id() {
        const STALE_MSG_ID: u32 = 1;
        let (service, channel_key) =
            service_with_two_sibling_clls_sharing_a_channel(vec![STALE_MSG_ID]).await;

        // Seed the same MsgId into the shared channel's leaked list too, as
        // if a prior teardown had already flagged it leaked before it got
        // reassigned -- must also be revoked, not retried as leaked against
        // CLL_B's new slot.
        {
            let mut chans = service.shared_channels.lock().await;
            let sc = chans.get_mut(&channel_key).unwrap();
            sc.leaked_repeat_message_ids = vec![STALE_MSG_ID];
        }

        // CLL_B needs a UniqueRespIdTable entry for both `build_tx_message`
        // (CP_CanPhysReqId) and `response_header_bytes`'s plain-CAN branch
        // (CP_CanRespUUDTId, connected_link's protocol) to succeed.
        {
            let mut links = service.logical_links.lock().await;
            let link_b = links.get_mut(&CLL_B).unwrap();
            let mut params = ComParamSet::default();
            params.unum32.insert(PARAM_CAN_PHYS_REQ_ID, 0x7E0);
            params.unum32.insert(PARAM_CAN_RESP_UUDT_ID, 0x7E8);
            link_b.active_unique_resp_id_table = vec![EcuUniqueRespEntry {
                unique_resp_identifier: 1,
                params,
            }];
        }

        let setup = RepeatMessageSetup {
            time_interval: 1000,
            condition: 0, // ADR-173: retransmits until a match -- never injected here, so it stays alive
            repeat_msg_data: vec![0x01],
            mask_data: vec![0x00],
            pattern_data: vec![0x00],
            tx_flag_bits: vec![],
            response_tx_flag_bits: None,
        };
        let msg_id = service
            .ioctl_start_repeat_message(
                CLL_B,
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::BytearrayData(
                        vci_service_interface::IoBytearray {
                            data: pack_repeat_message_setup(&setup),
                        },
                    )),
                }),
            )
            .await
            .expect("START_REPEAT_MESSAGE for CLL_B should succeed");
        assert_eq!(
            msg_id, STALE_MSG_ID,
            "sanity: the mock's fresh per-channel counter must hand back the same numeric \
             MsgId this test seeded as CLL_A's stale entry, or this test is not exercising the \
             collision it claims to"
        );

        // CLL_A's stale claim must be gone: a QUERY/STOP from CLL_A on this
        // MsgId must now be rejected as not-owned by CLL_A.
        {
            let links = service.logical_links.lock().await;
            let link_a = links.get(&CLL_A).unwrap();
            assert!(
                !link_a.repeat_message_ids.contains(&STALE_MSG_ID),
                "CLL_A's stale claim on the reused MsgId must be revoked once CLL_B is \
                 authoritatively assigned it"
            );
        }
        let err = J2534Service::require_owned_repeat_message(
            CLL_A,
            STALE_MSG_ID,
            &*service.logical_links.lock().await,
        )
        .expect_err("CLL_A must no longer be recognized as owning the reused MsgId");
        assert_eq!(err.code(), Code::NotFound);

        // CLL_B's own QUERY must succeed -- it is now the legitimate owner.
        let (channel_id, _, _) = J2534Service::require_owned_repeat_message(
            CLL_B,
            STALE_MSG_ID,
            &*service.logical_links.lock().await,
        )
        .expect("CLL_B must be recognized as owning the MsgId it was just assigned");
        let chans = service.shared_channels.lock().await;
        assert_eq!(chans.get(&channel_key).unwrap().channel_id, channel_id);
        assert!(
            chans
                .get(&channel_key)
                .unwrap()
                .leaked_repeat_message_ids
                .is_empty(),
            "the reused MsgId must also be revoked from leaked_repeat_message_ids -- retrying \
             a STOP against it would now target CLL_B's live, unrelated slot"
        );
    }

    /// Codex review, ADR-165 PR #42 round 12: closes a gap in round 3 Finding
    /// E's own fix above -- that fix's sibling-revocation loop explicitly
    /// excludes `cll_handle` itself (it is scoped to OTHER CLLs' stale
    /// claims), but nothing deduplicated a stale claim THIS SAME CLL still
    /// carries on its own `repeat_message_ids` before pushing the new claim.
    /// If CLL_A previously started a `Condition == 1` slot that self-
    /// completed device-side (leaving a stale entry CLL_A itself never
    /// pruned via its own QUERY/STOP, round 2 Finding C's prune trigger only
    /// firing on that CLL's OWN next QUERY/STOP), and a later
    /// START_REPEAT_MESSAGE call on CLL_A gets the device to reassign that
    /// exact same numeric MsgId, the unconditional push used to create a
    /// duplicate entry in CLL_A's own `Vec<u32>`.
    ///
    /// Driven end-to-end through the real service/mock, mirroring
    /// `start_repeat_message_revokes_a_siblings_stale_claim_on_the_reused_msg_id`'s
    /// own reused-ID-forcing technique: the mock's `next_repeat_msg_id`
    /// counter is fresh (starts at 1) for this test's newly-connected
    /// channel, so CLL_A's very first real START is guaranteed to return the
    /// exact numeric MsgId this test seeds as CLL_A's OWN stale entry --
    /// reproducing the same-CLL reused-MsgId collision directly.
    #[tokio::test]
    async fn start_repeat_message_deduplicates_its_own_stale_claim_on_the_reused_msg_id() {
        const STALE_MSG_ID: u32 = 1;
        let (service, _channel_key) =
            service_with_two_sibling_clls_sharing_a_channel(vec![STALE_MSG_ID]).await;

        // CLL_A needs a UniqueRespIdTable entry for both `build_tx_message`
        // (CP_CanPhysReqId) and `response_header_bytes`'s plain-CAN branch
        // (CP_CanRespUUDTId, connected_link's protocol) to succeed.
        {
            let mut links = service.logical_links.lock().await;
            let link_a = links.get_mut(&CLL_A).unwrap();
            let mut params = ComParamSet::default();
            params.unum32.insert(PARAM_CAN_PHYS_REQ_ID, 0x7E0);
            params.unum32.insert(PARAM_CAN_RESP_UUDT_ID, 0x7E8);
            link_a.active_unique_resp_id_table = vec![EcuUniqueRespEntry {
                unique_resp_identifier: 1,
                params,
            }];
        }

        let setup = RepeatMessageSetup {
            time_interval: 1000,
            condition: 0, // ADR-173: retransmits until a match -- never injected here, so it stays alive
            repeat_msg_data: vec![0x01],
            mask_data: vec![0x00],
            pattern_data: vec![0x00],
            tx_flag_bits: vec![],
            response_tx_flag_bits: None,
        };
        let msg_id = service
            .ioctl_start_repeat_message(
                CLL_A,
                Some(vci_service_interface::DataItem {
                    data: Some(vci_service_interface::data_item::Data::BytearrayData(
                        vci_service_interface::IoBytearray {
                            data: pack_repeat_message_setup(&setup),
                        },
                    )),
                }),
            )
            .await
            .expect("START_REPEAT_MESSAGE for CLL_A should succeed");
        assert_eq!(
            msg_id, STALE_MSG_ID,
            "sanity: the mock's fresh per-channel counter must hand back the same numeric \
             MsgId this test seeded as CLL_A's own stale entry, or this test is not exercising \
             the collision it claims to"
        );

        let links = service.logical_links.lock().await;
        let link_a = links.get(&CLL_A).unwrap();
        assert_eq!(
            link_a.repeat_message_ids,
            vec![STALE_MSG_ID],
            "CLL_A's own stale claim on the reused MsgId must be deduplicated, not left \
             alongside a duplicate newly-pushed entry for the same numeric id"
        );
    }

    /// Codex review, ADR-165 PR #42 round 11: `rpc_lock_resource`'s
    /// `LOCK_PHYSICAL_TX_QUEUE` grant must also reject on a leaked repeat-
    /// message slot (`SharedChannel::leaked_repeat_message_ids`), not just a
    /// sibling CLL's own `repeat_message_ids` (Finding L, round 5/6) -- a
    /// leaked slot has no owning `LogicalLinkState` left to scan via `links`
    /// at all (it is created exactly when the owning CLL has already been
    /// torn down), so this test lives here rather than in
    /// `tests/grpc_mock/repeat_message.rs` (this file's originating brief):
    /// this module's own doc comment above already establishes that the
    /// mock has no way to fail a native STOP for a `MsgId` that is
    /// genuinely still alive, so a black-box test cannot naturally produce a
    /// leaked-and-still-transmitting slot without new mock machinery (out of
    /// scope). Mirrors round 3's
    /// `retry_leaked_repeat_message_stops_prunes_both_stopped_and_already_gone_ids`'s
    /// own technique for the same limitation: start a genuine, still-alive
    /// slot directly against the mock API, then seed it into
    /// `leaked_repeat_message_ids` by hand rather than via a real failed
    /// STOP.
    #[tokio::test]
    async fn lock_resource_is_rejected_by_a_leaked_repeat_message_slot_still_transmitting() {
        let (service, channel_key) =
            service_with_two_sibling_clls_sharing_a_channel(Vec::new()).await;

        let channel_id = {
            let chans = service.shared_channels.lock().await;
            chans.get(&channel_key).unwrap().channel_id
        };

        // Condition == 0 (ADR-173 Decision 1: retransmit through silence,
        // stop only on a matching received frame) never self-completes
        // here since this test never injects any RX frame for it, so this
        // slot is guaranteed still alive when the grant below probes it.
        let alive_msg_id = {
            let api = service.api.lock().await;
            let message = j2534_0404::PassThruMessage::new(j2534_0404::CAN, 0, 0, 0, 0, &[0x01])
                .expect("message should build");
            let mask = j2534_0404::PassThruMessage::new(j2534_0404::CAN, 0, 0, 0, 0, &[0x00])
                .expect("mask should build");
            let pattern = j2534_0404::PassThruMessage::new(j2534_0404::CAN, 0, 0, 0, 0, &[0x00])
                .expect("pattern should build");
            let mut setup = j2534_0404::RepeatMsgSetup::new(1000, 0, message, mask, pattern);
            api.start_repeat_message(channel_id, &mut setup)
                .expect("start_repeat_message should succeed against a real connected channel")
        };

        {
            let mut chans = service.shared_channels.lock().await;
            chans
                .get_mut(&channel_key)
                .unwrap()
                .leaked_repeat_message_ids = vec![alive_msg_id];
        }

        let status = service
            .rpc_lock_resource(Request::new(vci_service_interface::LockResourceRequest {
                cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle: CLL_B,
                }),
                lock_mask: LOCK_PHYSICAL_TX_QUEUE,
            }))
            .await
            .expect_err(
                "LockResource(LOCK_PHYSICAL_TX_QUEUE) must be rejected while a leaked \
                 repeat-message slot on this physical resource's shared channel is still \
                 actively transmitting",
            );
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(
            status.message().contains("leaked repeat-message slot"),
            "unexpected rejection message: {}",
            status.message()
        );

        let chans = service.shared_channels.lock().await;
        assert_eq!(
            chans.get(&channel_key).unwrap().leaked_repeat_message_ids,
            vec![alive_msg_id],
            "a genuinely-still-alive leaked MsgId must not be pruned by the probe"
        );
    }

    /// Codex review, ADR-165 PR #42 round 11 regression guard (mirrors round
    /// 6's own `prune_stale_repeat_message_ids`-before-rejecting test for
    /// `repeat_message_ids`): a leaked slot the device has already forgotten
    /// (`ERR_INVALID_MSG_ID`) must not block the grant forever -- the same
    /// staleness probe applied to `repeat_message_ids` must also be applied
    /// to `leaked_repeat_message_ids`, pruning it before the reject check.
    #[tokio::test]
    async fn lock_resource_is_granted_once_a_leaked_repeat_message_slot_is_pruned() {
        const NEVER_EXISTED_MSG_ID: u32 = 999;
        let (service, channel_key) =
            service_with_two_sibling_clls_sharing_a_channel(Vec::new()).await;

        {
            let mut chans = service.shared_channels.lock().await;
            chans
                .get_mut(&channel_key)
                .unwrap()
                .leaked_repeat_message_ids = vec![NEVER_EXISTED_MSG_ID];
        }

        service
            .rpc_lock_resource(Request::new(vci_service_interface::LockResourceRequest {
                cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle: CLL_B,
                }),
                lock_mask: LOCK_PHYSICAL_TX_QUEUE,
            }))
            .await
            .expect(
                "LockResource(LOCK_PHYSICAL_TX_QUEUE) must be granted once the only leaked \
                 repeat-message slot on this physical resource has been pruned as stale",
            );

        let chans = service.shared_channels.lock().await;
        assert!(
            chans
                .get(&channel_key)
                .unwrap()
                .leaked_repeat_message_ids
                .is_empty(),
            "the never-existed MsgId must have been pruned from leaked_repeat_message_ids by \
             the LockResource grant's probe"
        );
    }
}

/// PR #78 edge-case-hunter follow-up: `ioctl_clear_tx_queue`'s own
/// `cancelled_cops.extend` (ADR-100 S5's batch-cancel counterpart to
/// `rpc_cancel_com_primitive`) gained two fixes, covered here directly
/// against the real handler rather than the extracted-helper-only coverage
/// in `events_drain_cancelled_cop_if_finalized_tests.rs`: a fresh,
/// call-time `link.registrants` recheck (closing the gap where the earlier
/// `detached_tier2_cops` snapshot could be stale relative to a tier-1 ->
/// tier-2 migration landing before the extend), and the same
/// `events::drain_cancelled_cop_if_finalized` batch drain loop
/// `rpc_primitive.rs`'s `CoptStopcomm` cancel-all block also gained (see
/// `docs/adr/ADR-182-cp-cyclic-resp-timeout-finite-receive-only-scope.md`'s
/// Consequences section). The genuine reap race the drain loop guards
/// against is covered at the loop-shape level in
/// `events_drain_cancelled_cop_if_finalized_tests.rs`'s
/// `batch_drain_loop_drains_only_the_stranded_marks_in_a_mixed_batch` --
/// see that test's own doc comment for why reproducing it end-to-end
/// through this handler itself is infeasible without a test-only pause
/// hook.
#[cfg(test)]
mod ioctl_clear_tx_queue_cancelled_cops_tests {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Arc;

    use tokio::sync::Mutex;
    use tonic::Request;

    use super::*;

    const TEST_CLL: u32 = 1;
    const QUEUED_COP: u32 = 100;

    /// A minimal, unconnected `LogicalLinkState` (`channel_key: None`) --
    /// `ioctl_clear_tx_queue` returns early after its `cancelled_cops`
    /// bookkeeping when `channel_key` is `None` (no shared physical channel
    /// to also clear), so this is sufficient to exercise the fix without
    /// needing a real mock-cdylib-backed `SharedChannel`. Mirrors
    /// `rpc_primitive.rs::rollback_stop_comm_pending_tests::
    /// service_with_one_link`'s field-by-field construction shape, this
    /// crate's established pattern for a hand-built `LogicalLinkState`.
    fn minimal_link(registrants: Vec<CopRegistrant>) -> LogicalLinkState {
        LogicalLinkState {
            channel_id: None,
            protocol: ChannelProtocol::CAN,
            hw_protocol_id: 0,
            software_isotp: false,
            uudt_channel_id: None,
            uudt_channel_key: None,
            isotp_rx: Arc::new(Mutex::new(HashMap::new())),
            connect_in_flight: std::sync::Weak::new(),
            connected: false,
            comm_started: false,
            raw_mode: false,
            checksum_mode: false,
            connect_generation: 0,
            stop_comm_pending: false,
            channel_key: None,
            pin_select: None,
            channel_index: None,
            base_hw_protocol_override: None,
            rx_buf: Arc::new(Mutex::new(CllEventQueue {
                event_queue_cap: 16,
                ..CllEventQueue::default()
            })),
            working: ComParamSet::default(),
            active: ComParamSet::default(),
            tester_present_state: TesterPresentState::None,
            tester_present_base_tx_flags: 0,
            open_tp_discards: Vec::new(),
            working_unique_resp_id_table: Vec::new(),
            active_unique_resp_id_table: Vec::new(),
            unique_resp_filter_ids: Vec::new(),
            cancelled_cops: HashSet::new(),
            held_lock_mask: 0,
            last_error: None,
            tx_held: VecDeque::new(),
            tx_suspended_by_ioctl: false,
            tx_suspended_by_lock: false,
            tx_suspended_by_error: false,
            error_clear_seq: 0,
            error_set_seq: 0,
            client_filters: HashMap::new(),
            repeat_message_ids: Vec::new(),
            pending_client_filters: HashMap::new(),
            registrants,
            next_registrant_seq: 0,
            j1939_claimed_address: None,
            j1939_claim_cursor: 0,
            j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
            tp20_connection: None,
            tp20_broadcast_periodic: None,
        }
    }

    fn cop_registrant(cop_handle: u32, tier: RegistrantTier) -> CopRegistrant {
        CopRegistrant {
            cop_handle,
            registration_seq: 0,
            tier,
            expected: Vec::new(),
            rc_cfg: None,
            request_sid: None,
            matches_needed: None,
            matches_got: 0,
            pending_rc: None,
            connect_generation: 0,
            cyclic_deadline: None,
            cyclic_timeout_ms: None,
            migrate_on_first_match: false,
            timing_cfg: None,
            timing_accumulator: None,
            pending_timing_change: None,
            concat_enabled: false,
            concat: Vec::new(),
            concat_segments_got: 0,
        }
    }

    fn cop_entry(cll_handle: u32) -> CopEntry {
        CopEntry {
            cll_handle,
            dispatched: false,
            transmits: false,
            is_send_recv: false,
            cop_tag: None,
        }
    }

    /// Builds a minimal `J2534Service` backed by the mock J2534 cdylib, with
    /// a single `LogicalLinkState` at `TEST_CLL` and `self.primitives`
    /// seeded from `primitives`. Never opens a device or connects a
    /// channel; only used to call `ioctl_clear_tx_queue` directly.
    async fn service_with_one_link(
        link: LogicalLinkState,
        primitives: HashMap<u32, CopEntry>,
    ) -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::default(),
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)]))),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(HashMap::new())),
            primitives: Arc::new(Mutex::new(primitives)),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn clear_tx_queue_request() -> Request<vci_service_interface::IoCtlRequest> {
        Request::new(vci_service_interface::IoCtlRequest {
            handle: Some(vci_service_interface::io_ctl_request::Handle::CllHandle(
                vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle: TEST_CLL,
                },
            )),
            io_ctrl_command: Some(
                vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                    PDU_IOCTL_CLEAR_TX_QUEUE,
                ),
            ),
            input_data: None,
            has_output: false,
        })
    }

    /// (b) A cop that stays live in `primitives` throughout the call: the
    /// extend marks it, and the new drain loop's per-cop
    /// `drain_cancelled_cop_if_finalized` check sees it still present in
    /// `primitives` and leaves the mark alone.
    #[tokio::test]
    async fn leaves_a_still_live_cops_mark_in_place() {
        let link = minimal_link(Vec::new());
        let primitives = HashMap::from([(QUEUED_COP, cop_entry(TEST_CLL))]);
        let service = service_with_one_link(link, primitives).await;

        service
            .rpc_io_ctl(clear_tx_queue_request())
            .await
            .expect("CLEAR_TX_QUEUE should succeed");

        assert!(
            service.primitives.lock().await.contains_key(&QUEUED_COP),
            "CLEAR_TX_QUEUE must not remove a merely-queued (not tx_held) cop from primitives"
        );
        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .cancelled_cops
                .contains(&QUEUED_COP),
            "a still-live queued cop must be marked cancelled and its mark left in place"
        );
    }

    /// (c) The correctness-bug half of the fix: a cop with a genuine live
    /// tier-2 (`RegistrantTier::ReceiveOnly`) registrant on this CLL must be
    /// excluded from being newly marked cancelled at all -- it is a
    /// detached, already-sending-complete IS-CYCLIC COP (ADR-100 S5), not a
    /// queued TX-queue item, regardless of whether the earlier
    /// `detached_tier2_cops` snapshot alone would have caught it (this
    /// steady-state case is; the fix's own value is the fresh recheck
    /// closing a narrow snapshot-staleness race that is otherwise
    /// infeasible to reproduce deterministically through this call path --
    /// see this module's own doc comment).
    #[tokio::test]
    async fn excludes_a_live_tier2_registrant_from_being_marked() {
        let link = minimal_link(vec![cop_registrant(
            QUEUED_COP,
            RegistrantTier::ReceiveOnly,
        )]);
        let primitives = HashMap::from([(QUEUED_COP, cop_entry(TEST_CLL))]);
        let service = service_with_one_link(link, primitives).await;

        service
            .rpc_io_ctl(clear_tx_queue_request())
            .await
            .expect("CLEAR_TX_QUEUE should succeed");

        let links = service.logical_links.lock().await;
        assert!(
            !links
                .get(&TEST_CLL)
                .unwrap()
                .cancelled_cops
                .contains(&QUEUED_COP),
            "a live tier-2 registrant's cop must never be marked cancelled by CLEAR_TX_QUEUE"
        );
    }
}

/// Direct unit-level coverage of `CLEAR_PERIODIC_MSGS`'s periodic-clear
/// epoch gating (Codex review round 5, PR #101, ADR-192/Phase 7 Stage 7c,
/// design-advisor consult): a REAL, already-committed broadcast-periodic
/// entry whose native start actually raced AFTER this ioctl's own native
/// `clear_periodic_messages` call returned must be left live and tracked,
/// not wrongly finalized as "cleared" -- see `J2534Service::
/// periodic_clear_epoch`'s own doc comment for the full mechanism.
///
/// **Why direct unit tests, not `tests/grpc_mock`:** the actual bug this
/// closes requires the real native `start_periodic_message` call to
/// execute AFTER the real native `clear_periodic_messages` call while the
/// two calls' own `logical_links`/`shared_channels` reconciliation runs in
/// the opposite order -- genuine two-task interleaving this crate has no
/// test-only synchronization hook to force deterministically (the same
/// class of gap `rpc_primitive.rs`'s own `reserve_tp20_broadcast_periodic_
/// tests`/`finalize_or_orphan_broadcast_periodic_start_tests` doc comments
/// document for this exact mechanism). These tests instead hand-seed
/// `LogicalLinkState`/`SharedChannel` state with a chosen `started_epoch`/
/// `periodic_clear_epoch` combination and drive `CLEAR_PERIODIC_MSGS`
/// directly, exercising the same epoch-comparison logic that would fire if
/// the race actually happened. `TEST_CHANNEL_ID` is never actually opened/
/// connected via the mock's own `PassThruConnect` -- `clear_periodic_
/// messages`/`stop_periodic_message` against an unregistered channel_id are
/// both no-ops (`no_error()`) on the mock side, which is sufficient since
/// these tests only exercise this SERVICE's own epoch-comparison
/// bookkeeping, not the mock's own periodic-message state (mirroring
/// `events_hard_error_broadcast_periodic_tests.rs`'s identical `ctx.api`
/// shape).
#[cfg(test)]
mod clear_periodic_msgs_epoch_gating_tests {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Arc;

    use j2534_0404_sys::libloading::{Library, Symbol};
    use serial_test::serial;
    use tokio::sync::Mutex;
    use tonic::Request;

    use super::*;

    const TEST_CLL: u32 = 1;
    const COP_HANDLE: u32 = 100;
    const TEST_CHANNEL_ID: ChannelId = ChannelId(9);
    const TEST_CHANNEL_KEY: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);

    /// A connected `LogicalLinkState` on `TEST_CHANNEL_ID`/`TEST_CHANNEL_KEY`
    /// with a live broadcast-periodic entry at the given `started_epoch`.
    /// Mirrors `events_hard_error_broadcast_periodic_tests.rs`'s own
    /// `link_with_a_live_broadcast_periodic`'s construction shape.
    fn link_with_periodic(started_epoch: u64) -> LogicalLinkState {
        LogicalLinkState {
            channel_id: Some(TEST_CHANNEL_ID),
            protocol: ChannelProtocol::CAN,
            hw_protocol_id: j2534_0404::CAN,
            software_isotp: false,
            uudt_channel_id: None,
            uudt_channel_key: None,
            isotp_rx: Arc::new(Mutex::new(HashMap::new())),
            connect_in_flight: std::sync::Weak::new(),
            connected: true,
            comm_started: true,
            raw_mode: false,
            checksum_mode: false,
            connect_generation: 1,
            stop_comm_pending: false,
            channel_key: Some(TEST_CHANNEL_KEY),
            pin_select: None,
            channel_index: None,
            base_hw_protocol_override: None,
            rx_buf: Arc::new(Mutex::new(CllEventQueue {
                event_queue_cap: 16,
                ..CllEventQueue::default()
            })),
            working: ComParamSet::default(),
            active: ComParamSet::default(),
            tester_present_state: TesterPresentState::None,
            tester_present_base_tx_flags: 0,
            open_tp_discards: Vec::new(),
            working_unique_resp_id_table: Vec::new(),
            active_unique_resp_id_table: Vec::new(),
            unique_resp_filter_ids: Vec::new(),
            cancelled_cops: HashSet::new(),
            held_lock_mask: 0,
            last_error: None,
            tx_held: VecDeque::new(),
            tx_suspended_by_ioctl: false,
            tx_suspended_by_lock: false,
            tx_suspended_by_error: false,
            error_clear_seq: 0,
            error_set_seq: 0,
            client_filters: HashMap::new(),
            repeat_message_ids: Vec::new(),
            pending_client_filters: HashMap::new(),
            registrants: Vec::new(),
            next_registrant_seq: 0,
            j1939_claimed_address: None,
            j1939_claim_cursor: 0,
            j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
            tp20_connection: None,
            tp20_broadcast_periodic: Some(Tp20BroadcastPeriodic {
                cop_handle: COP_HANDLE,
                message_id: Some(j2534_0404::PeriodicMessageId(777)),
                started_epoch,
                pending_clear_generation: 0,
            }),
        }
    }

    /// Same shape as `link_with_periodic`, but seeds the `None`-sentinel
    /// in-flight-start reservation instead of a committed entry -- for the
    /// sentinel-deferral tests below. `pending_clear_generation` starts at
    /// `0` (no clear has scanned it yet), matching a fresh
    /// `reserve_tp20_broadcast_periodic` write.
    fn link_with_pending_sentinel() -> LogicalLinkState {
        let mut link = link_with_periodic(0);
        link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
            cop_handle: COP_HANDLE,
            message_id: None,
            started_epoch: 0,
            pending_clear_generation: 0,
        });
        link
    }

    fn shared_channel(leaked: Vec<(j2534_0404::PeriodicMessageId, u64)>) -> SharedChannel {
        SharedChannel {
            channel_id: TEST_CHANNEL_ID,
            ref_count: 1,
            tx_queue: tokio::sync::mpsc::unbounded_channel().0,
            executing_cop: Arc::new(Mutex::new(None)),
            _poll_cancel: tokio::sync::oneshot::channel().0,
            connect_flags: 0,
            dead: false,
            occupancy_epoch: 0,
            leaked_repeat_message_ids: Vec::new(),
            leaked_periodic_message_ids: leaked,
            applied_analog_sample_rate: None,
            applied_analog_samples_per_reading: None,
            applied_analog_readings_per_msg: None,
            j1939_claims: HashMap::new(),
            j1939_claim_results: HashMap::new(),
            j1939_reclaim_pending: HashMap::new(),
            leaked_j1939_claims: Vec::new(),
            tp20_connections: HashMap::new(),
            tp20_connection_results: HashMap::new(),
            become_master_in_flight: Arc::new(portable_atomic::AtomicBool::new(false)),
            tp20_passive: None,
        }
    }

    /// Builds a minimal `J2534Service` backed by the mock J2534 cdylib, with
    /// `link` at `TEST_CLL`, `self.primitives` seeded with a single entry for
    /// `COP_HANDLE`, `self.periodic_clear_epoch` seeded to `initial_epoch`,
    /// and (if `Some`) `sc` installed as the `TEST_CHANNEL_KEY` shared
    /// channel. Never opens a device or connects a channel -- see this
    /// module's own doc comment for why that is safe here.
    async fn service_with_link(
        link: LogicalLinkState,
        initial_epoch: u64,
        sc: Option<SharedChannel>,
    ) -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");

        let mut primitives = HashMap::new();
        primitives.insert(
            COP_HANDLE,
            CopEntry {
                cll_handle: TEST_CLL,
                dispatched: true,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        let mut shared_channels = HashMap::new();
        if let Some(sc) = sc {
            shared_channels.insert(TEST_CHANNEL_KEY, sc);
        }

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::default(),
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(initial_epoch)),
            logical_links: Arc::new(Mutex::new(HashMap::from([(TEST_CLL, link)]))),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(shared_channels)),
            primitives: Arc::new(Mutex::new(primitives)),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn clear_periodic_msgs_request(
        cll_handle: u32,
    ) -> Request<vci_service_interface::IoCtlRequest> {
        Request::new(vci_service_interface::IoCtlRequest {
            handle: Some(vci_service_interface::io_ctl_request::Handle::CllHandle(
                vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle,
                },
            )),
            io_ctrl_command: Some(
                vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                    j2534_0404::CLEAR_PERIODIC_MSGS,
                ),
            ),
            input_data: None,
            has_output: false,
        })
    }

    /// Same shape as `clear_periodic_msgs_request`, but for `CLEAR_RX_BUFFER`
    /// -- used by `legacy_ioctl_rejects_a_hard_errored_cll_despite_its_
    /// retained_channel_key` below, which needs an arm that reaches the
    /// native call directly (unlike `CLEAR_PERIODIC_MSGS`, whose own arm
    /// does extra epoch bookkeeping irrelevant to that test).
    fn clear_rx_buffer_request(cll_handle: u32) -> Request<vci_service_interface::IoCtlRequest> {
        Request::new(vci_service_interface::IoCtlRequest {
            handle: Some(vci_service_interface::io_ctl_request::Handle::CllHandle(
                vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle,
                },
            )),
            io_ctrl_command: Some(
                vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                    j2534_0404::CLEAR_RX_BUFFER,
                ),
            ),
            input_data: None,
            has_output: false,
        })
    }

    /// A committed entry whose `started_epoch` provably precedes this
    /// clear's own `clear_generation` (0 < 1, the ordinary case every
    /// normal-flow start/clear sequence produces) is reconciled and
    /// finalized exactly as before this fix -- confirms the epoch gate does
    /// not regress the pre-existing, already-covered
    /// `tests/grpc_mock/tp20.rs::clear_periodic_msgs_reconciles_a_live_
    /// broadcast_periodic_cop` scenario at the unit level too.
    #[tokio::test]
    async fn stale_epoch_entry_is_reconciled_and_finalized() {
        let service =
            service_with_link(link_with_periodic(0), 0, Some(shared_channel(Vec::new()))).await;

        service
            .rpc_io_ctl(clear_periodic_msgs_request(TEST_CLL))
            .await
            .expect("CLEAR_PERIODIC_MSGS should succeed");

        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_CLL).unwrap();
        assert!(
            link.tp20_broadcast_periodic.is_none(),
            "an entry whose started_epoch predates this clear's own clear_generation must be \
             taken and finalized"
        );
        let queue = link.rx_buf.lock().await;
        assert_eq!(
            queue.items.len(),
            1,
            "the owning COP must be finalized with exactly one PduCopstFinished status"
        );
    }

    /// The actual regression test for the bug this fix closes: a committed
    /// entry whose `started_epoch` is AT OR AFTER this clear's own
    /// `clear_generation` -- meaning its native start provably raced AFTER
    /// this clear's own native `clear_periodic_messages` call returned --
    /// must be left untouched: not taken, not finalized, no
    /// `PduCopstFinished` emitted, and a later `CancelComPrimitive` on it
    /// still works normally (stops it via the ordinary path).
    #[tokio::test]
    async fn fresh_epoch_entry_is_left_alone_and_still_cancellable() {
        // periodic_clear_epoch seeded to 5; CLEAR_PERIODIC_MSGS bumps it to 6
        // (clear_generation = 6). started_epoch: 6 sits exactly at the
        // boundary -- >= clear_generation must NOT be finalized.
        let service =
            service_with_link(link_with_periodic(6), 5, Some(shared_channel(Vec::new()))).await;

        service
            .rpc_io_ctl(clear_periodic_msgs_request(TEST_CLL))
            .await
            .expect("CLEAR_PERIODIC_MSGS should succeed");

        assert_eq!(
            service
                .periodic_clear_epoch
                .load(portable_atomic::Ordering::Relaxed),
            6,
            "a successful native clear must bump periodic_clear_epoch by exactly one"
        );

        {
            let links = service.logical_links.lock().await;
            let link = links.get(&TEST_CLL).unwrap();
            assert_eq!(
                link.tp20_broadcast_periodic,
                Some(Tp20BroadcastPeriodic {
                    cop_handle: COP_HANDLE,
                    message_id: Some(j2534_0404::PeriodicMessageId(777)),
                    started_epoch: 6,
                    pending_clear_generation: 0,
                }),
                "a post-clear entry (started_epoch >= clear_generation) must be left untouched"
            );
            assert!(
                link.rx_buf.lock().await.items.is_empty(),
                "no PduCopstFinished may be emitted for an entry this clear never touched"
            );
        }

        let cancel_request = Request::new(vci_service_interface::CancelComPrimitiveRequest {
            cop_handle: Some(vci_service_interface::ComPrimitiveHandle {
                module_handle: DEFAULT_MODULE_HANDLE,
                cll_handle: TEST_CLL,
                cop_handle: COP_HANDLE,
            }),
        });
        service
            .rpc_cancel_com_primitive(cancel_request)
            .await
            .expect(
                "CancelComPrimitive must still work normally on an entry the epoch gate left \
                 untouched",
            );
        assert!(
            !service.primitives.lock().await.contains_key(&COP_HANDLE),
            "the COP must be fully cancelled and removed from primitives"
        );
        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "the cancel must clear the CLL's own tracking entry"
        );
    }

    /// `SharedChannel::leaked_periodic_message_ids` retain: an entry whose
    /// epoch predates `clear_generation` is moot and dropped; an entry at or
    /// after it is a genuine post-clear leak and stays tracked.
    #[tokio::test]
    async fn leaked_periodic_message_ids_retains_only_post_clear_entries() {
        let stale_id = j2534_0404::PeriodicMessageId(111);
        let fresh_id = j2534_0404::PeriodicMessageId(222);
        // No live broadcast-periodic entry on the CLL itself for this test --
        // only the channel's own leaked-list matters here.
        let mut link = link_with_periodic(0);
        link.tp20_broadcast_periodic = None;
        let sc = shared_channel(vec![(stale_id, 0), (fresh_id, 6)]);
        let service = service_with_link(link, 5, Some(sc)).await;

        service
            .rpc_io_ctl(clear_periodic_msgs_request(TEST_CLL))
            .await
            .expect("CLEAR_PERIODIC_MSGS should succeed");

        let chans = service.shared_channels.lock().await;
        let sc = chans.get(&TEST_CHANNEL_KEY).unwrap();
        assert_eq!(
            sc.leaked_periodic_message_ids,
            vec![(fresh_id, 6)],
            "only the post-clear (epoch >= clear_generation) entry must survive the retain"
        );
    }

    /// Sentinel deferral fix (edge-case-hunter finding, design-advisor-
    /// approved, ADR-192): a still-in-flight `None`-sentinel reservation
    /// (`reserve_tp20_broadcast_periodic`'s write, before its own native
    /// `start_periodic_message` call has returned) is NOT taken/finalized
    /// by the `CLEAR_PERIODIC_MSGS` scan -- its ordering against this
    /// clear's own native call is undecidable at scan time. The scan
    /// instead stamps `clear_generation` onto `pending_clear_generation`
    /// and leaves it tracked: the entry survives, no `PduCopstFinished` is
    /// emitted, and the COP still reports `Executing` per `GetStatus`
    /// (`finalize_or_orphan_broadcast_periodic_start_tests` in
    /// `rpc_primitive.rs` covers how that deferred decision is later
    /// resolved).
    #[tokio::test]
    async fn sentinel_scan_defers_instead_of_finalizing_and_stamps_pending_clear_generation() {
        let service = service_with_link(
            link_with_pending_sentinel(),
            0,
            Some(shared_channel(Vec::new())),
        )
        .await;

        service
            .rpc_io_ctl(clear_periodic_msgs_request(TEST_CLL))
            .await
            .expect("CLEAR_PERIODIC_MSGS should succeed");

        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_CLL).unwrap();
        assert_eq!(
            link.tp20_broadcast_periodic,
            Some(Tp20BroadcastPeriodic {
                cop_handle: COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 1,
            }),
            "a still-in-flight sentinel must survive the scan, with this clear's own \
             clear_generation stamped onto pending_clear_generation"
        );
        assert!(
            link.rx_buf.lock().await.items.is_empty(),
            "a deferred sentinel must not have PduCopstFinished emitted for it at scan time"
        );
        drop(links);

        let status = service
            .rpc_get_status(Request::new(vci_service_interface::GetStatusRequest {
                handle: Some(
                    vci_service_interface::get_status_request::Handle::CopHandle(
                        vci_service_interface::ComPrimitiveHandle {
                            module_handle: DEFAULT_MODULE_HANDLE,
                            cll_handle: TEST_CLL,
                            cop_handle: COP_HANDLE,
                        },
                    ),
                ),
            }))
            .await
            .expect("GetStatus(CopHandle) should succeed")
            .into_inner();
        assert_eq!(
            status.status,
            Some(vci_service_interface::status_response::Status::CopStatus(
                vci_service_interface::PduComPrimitiveStatus::PduCopstExecuting as i32
            )),
            "the COP must still be reported live/Executing while its sentinel's resolution is \
             deferred"
        );
    }

    /// The `.max()` half of the sentinel-deferral fix: a SECOND
    /// `CLEAR_PERIODIC_MSGS` racing the same still-pending sentinel must not
    /// regress an already-recorded higher generation from a first clear.
    /// `finalize_or_orphan_broadcast_periodic_start_tests` in
    /// `rpc_primitive.rs` covers the resolution outcome this stamped value
    /// later feeds into (that method is private to the `rpc_primitive`
    /// module, so this test only exercises the scan's own stamping, not the
    /// finalize-time resolution).
    #[tokio::test]
    async fn second_racing_clear_does_not_regress_pending_clear_generation() {
        let service = service_with_link(
            link_with_pending_sentinel(),
            0,
            Some(shared_channel(Vec::new())),
        )
        .await;

        service
            .rpc_io_ctl(clear_periodic_msgs_request(TEST_CLL))
            .await
            .expect("first CLEAR_PERIODIC_MSGS should succeed");
        let first_pending = {
            let links = service.logical_links.lock().await;
            links
                .get(&TEST_CLL)
                .unwrap()
                .tp20_broadcast_periodic
                .unwrap()
                .pending_clear_generation
        };
        assert_eq!(first_pending, 1, "the first clear's own generation is 1");

        service
            .rpc_io_ctl(clear_periodic_msgs_request(TEST_CLL))
            .await
            .expect("second CLEAR_PERIODIC_MSGS should succeed");
        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_CLL).unwrap();
        assert_eq!(
            link.tp20_broadcast_periodic,
            Some(Tp20BroadcastPeriodic {
                cop_handle: COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 2,
            }),
            "a second racing clear must advance pending_clear_generation to its own (larger) \
             generation, never regress it"
        );
    }

    /// `edge-case-hunter` finding (PR #101 round 5 verification): the sibling
    /// test above only covers two racing clears. `.max()` accumulation is a
    /// monotonic reduction, so induction says N clears works the same way,
    /// but this exercises three concretely rather than trusting the
    /// argument alone.
    #[tokio::test]
    async fn third_racing_clear_advances_pending_clear_generation_to_the_running_max() {
        let service = service_with_link(
            link_with_pending_sentinel(),
            0,
            Some(shared_channel(Vec::new())),
        )
        .await;

        for expected_generation in 1..=3u64 {
            service
                .rpc_io_ctl(clear_periodic_msgs_request(TEST_CLL))
                .await
                .expect("each CLEAR_PERIODIC_MSGS should succeed");
            let links = service.logical_links.lock().await;
            assert_eq!(
                links
                    .get(&TEST_CLL)
                    .unwrap()
                    .tp20_broadcast_periodic
                    .unwrap()
                    .pending_clear_generation,
                expected_generation,
                "pending_clear_generation must track the running max across N racing clears, \
                 not just two"
            );
        }
    }

    /// Regression test for `edge-case-hunter` finding #1 (this PR's own
    /// close-out pass, closed by `resolve_live_legacy_link`'s
    /// `link.connected` gate): before that fix, a CLL that had suffered a
    /// hard channel error would still resolve a "live" `channel_id` for
    /// any of the 4 legacy IOCTLs, because `events::handle_channel_hard_
    /// error` deliberately leaves `channel_key` set (so a later
    /// Disconnect/Destroy can still release the shared-channel ref) even
    /// though it clears `connected`/`channel_id` and marks the matching
    /// `SharedChannel` entry `dead` (without removing it). A bare
    /// `channel_key` -> `chans.get()` resolution with no liveness check
    /// would still find that dead entry and let a legacy IOCTL reach
    /// `PassThruIoctl` on an offline channel instead of correctly
    /// rejecting `PDU_ERR_CLL_NOT_CONNECTED`.
    ///
    /// Mirrors `handle_channel_hard_error`'s own post-error state directly
    /// (hand-built link + `dead: true` `SharedChannel`, per this module's
    /// own doc comment on why these tests bypass the gRPC connect/
    /// hard-error flow) rather than trying to construct the error through
    /// the mock driver. Uses `CLEAR_RX_BUFFER`, not `CLEAR_PERIODIC_MSGS`,
    /// since that arm reaches the native call with no epoch bookkeeping to
    /// set up first. Deliberately not duplicated for the other 3 legacy
    /// IDs (`CLEAR_TX_BUFFER`/`CLEAR_PERIODIC_MSGS`/`CLEAR_MSG_FILTERS`):
    /// they all resolve `channel_id` through this exact same
    /// `resolve_live_legacy_link` call, so a second `edge-case-hunter`
    /// pass (this PR's own close-out) judged one arm's coverage of the
    /// shared helper sufficient, not a per-arm gap.
    #[tokio::test]
    async fn legacy_ioctl_rejects_a_hard_errored_cll_despite_its_retained_channel_key() {
        let mut link = link_with_periodic(0);
        link.tp20_broadcast_periodic = None;
        // Mirrors `handle_channel_hard_error`'s own post-error state
        // (`events.rs`): `connected`/`channel_id` cleared, `channel_key`
        // deliberately retained.
        link.connected = false;
        link.channel_id = None;

        let mut sc = shared_channel(Vec::new());
        sc.dead = true;

        let service = service_with_link(link, 0, Some(sc)).await;

        let err = service
            .rpc_io_ctl(clear_rx_buffer_request(TEST_CLL))
            .await
            .expect_err(
                "a hard-errored CLL's retained channel_key must not resolve a live channel_id",
            );
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        // `Code::FailedPrecondition` alone is reused by several other rejection
        // sites in this file -- pin down the specific PduError too, so a future
        // refactor that accidentally routed this scenario through a different
        // FailedPrecondition branch would still be caught (`edge-case-hunter`
        // finding, this PR's second close-out pass).
        let detail = vci_service_interface::error_detail_from_status(&err)
            .expect("ErrorDetail should be attached");
        assert_eq!(
            detail.pdu_error,
            PduError::PduErrCllNotConnected as i32,
            "must be rejected specifically as not-connected, not some other FailedPrecondition"
        );
    }

    fn clear_tx_queue_request(cll_handle: u32) -> Request<vci_service_interface::IoCtlRequest> {
        Request::new(vci_service_interface::IoCtlRequest {
            handle: Some(vci_service_interface::io_ctl_request::Handle::CllHandle(
                vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle,
                },
            )),
            io_ctrl_command: Some(
                vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                    PDU_IOCTL_CLEAR_TX_QUEUE,
                ),
            ),
            input_data: None,
            has_output: false,
        })
    }

    fn sw_can_hs_request(cll_handle: u32) -> Request<vci_service_interface::IoCtlRequest> {
        Request::new(vci_service_interface::IoCtlRequest {
            handle: Some(vci_service_interface::io_ctl_request::Handle::CllHandle(
                vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle,
                },
            )),
            io_ctrl_command: Some(
                vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                    PDU_IOCTL_SW_CAN_HS,
                ),
            ),
            input_data: None,
            has_output: false,
        })
    }

    fn set_poll_response_request(cll_handle: u32) -> Request<vci_service_interface::IoCtlRequest> {
        Request::new(vci_service_interface::IoCtlRequest {
            handle: Some(vci_service_interface::io_ctl_request::Handle::CllHandle(
                vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle,
                },
            )),
            io_ctrl_command: Some(
                vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                    PDU_IOCTL_SET_POLL_RESPONSE,
                ),
            ),
            input_data: Some(vci_service_interface::DataItem {
                data: Some(vci_service_interface::data_item::Data::BytearrayData(
                    vci_service_interface::IoBytearray {
                        data: vec![0xAA, 0xBB],
                    },
                )),
            }),
            has_output: false,
        })
    }

    fn get_ndis_adapter_info_request(
        cll_handle: u32,
    ) -> Request<vci_service_interface::IoCtlRequest> {
        Request::new(vci_service_interface::IoCtlRequest {
            handle: Some(vci_service_interface::io_ctl_request::Handle::CllHandle(
                vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle,
                },
            )),
            io_ctrl_command: Some(
                vci_service_interface::io_ctl_request::IoCtrlCommand::IoCtrlCommandId(
                    PDU_IOCTL_GET_NDIS_ADAPTER_INFO,
                ),
            ),
            input_data: None,
            has_output: false,
        })
    }

    /// Reads the mock's cumulative, cross-channel `PassThruIoctl
    /// (CLEAR_TX_BUFFER)` call counter (`__mock_get_clear_tx_buffer_count`,
    /// same shared dynamically-loaded cdylib instance this module's `service`
    /// already holds open -- see `set_stop_periodic_message_error`'s own doc
    /// comment in `terminate_tp20_broadcast_periodic_for_suspension_tests`
    /// for why a fresh `Library::new` on the identical path still resolves
    /// to the SAME loaded shared object). Needed because `IOCTL_CLEAR_TX_
    /// BUFFER`'s mock handler (unlike `IOCTL_SW_CAN_HS`/`_NS`/`IOCTL_SET_
    /// POLL_RESPONSE`) does not check whether `channel_id` is a registered
    /// channel before returning `no_error()` -- so this counter is the only
    /// available signal for whether `ioctl_clear_tx_queue`'s native call was
    /// actually reached.
    fn clear_tx_buffer_count() -> usize {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = Library::new(&lib_path).expect("mock library should be loadable");
            let f: Symbol<unsafe extern "system" fn() -> usize> = lib
                .get(b"__mock_get_clear_tx_buffer_count\0")
                .expect("__mock_get_clear_tx_buffer_count should be exported");
            f()
        }
    }

    /// Regression test for `edge-case-hunter` finding #2 (PR #105 close-out
    /// backlog entry, fixed here): mirrors `legacy_ioctl_rejects_a_hard_
    /// errored_cll_despite_its_retained_channel_key`'s hand-built
    /// post-hard-error state (`connected: false`, `channel_id: None`,
    /// `channel_key` still `Some`, paired with a `dead: true` `SharedChannel`
    /// entry). Unlike that test's `CLEAR_RX_BUFFER` arm, `ioctl_clear_tx_
    /// queue`'s native hardware clear is optional (its RPC always returns
    /// `Ok(())` either way), so the only way to observe whether the gate
    /// actually skipped the native call is the mock's own call counter --
    /// see `clear_tx_buffer_count`'s own doc comment. `#[serial]`'d against
    /// that same shared, process-global mock counter, mirroring this file's
    /// own `tp20_stop_periodic_call_counter` precedent
    /// (`terminate_tp20_broadcast_periodic_for_suspension_tests`).
    #[tokio::test]
    #[serial(clear_tx_buffer_counter)]
    async fn ioctl_clear_tx_queue_skips_the_native_call_for_a_hard_errored_cll() {
        let mut link = link_with_periodic(0);
        link.tp20_broadcast_periodic = None;
        link.connected = false;
        link.channel_id = None;

        let mut sc = shared_channel(Vec::new());
        sc.dead = true;

        let service = service_with_link(link, 0, Some(sc)).await;

        let before = clear_tx_buffer_count();
        service
            .rpc_io_ctl(clear_tx_queue_request(TEST_CLL))
            .await
            .expect("CLEAR_TX_QUEUE must still succeed as a no-op on a hard-errored CLL");
        let after = clear_tx_buffer_count();
        assert_eq!(
            before, after,
            "a hard-errored CLL's retained channel_key must not reach the native \
             CLEAR_TX_BUFFER call"
        );
    }

    /// Regression test for `edge-case-hunter` finding #2 (PR #105 close-out
    /// backlog entry, fixed here), covering `ioctl_sw_can_mode` via its
    /// `ioctl_sw_can_hs` entry point. Unlike `ioctl_clear_tx_queue`'s mock
    /// handler, `IOCTL_SW_CAN_HS`'s own mock handler DOES check whether
    /// `channel_id` is a registered channel before succeeding, returning an
    /// error for the unregistered `channel_id` a dead `SharedChannel` entry
    /// would resolve to -- so the RPC's own Ok/Err result is already a
    /// direct signal, no shared mock counter (and no `#[serial]`) needed.
    #[tokio::test]
    async fn ioctl_sw_can_hs_no_ops_for_a_hard_errored_cll() {
        let mut link = link_with_periodic(0);
        link.tp20_broadcast_periodic = None;
        link.connected = false;
        link.channel_id = None;
        link.hw_protocol_id = j2534_0404::PROTOCOL_SW_CAN_PS;

        let mut sc = shared_channel(Vec::new());
        sc.dead = true;

        let service = service_with_link(link, 0, Some(sc)).await;

        service
            .rpc_io_ctl(sw_can_hs_request(TEST_CLL))
            .await
            .expect(
                "a hard-errored CLL's retained channel_key must fall back to the no-op \
                 \"not yet connected\" path, not reach the native call on a dead channel",
            );
    }

    /// Regression test for `edge-case-hunter` finding #2 (PR #105 close-out
    /// backlog entry, fixed here), covering `require_gm_uart_link`'s shared
    /// `connected` gate via `ioctl_set_poll_response` (simpler input
    /// requirements than `ioctl_become_master`'s `spawn_blocking`/in-flight
    /// machinery -- this file's own "one test for a shared mechanism is
    /// enough" convention, matching how `legacy_ioctl_rejects_a_hard_
    /// errored_cll_despite_its_retained_channel_key` above covers all 4
    /// `rpc_io_ctl_legacy` arms via one arm alone). `IOCTL_SET_POLL_
    /// RESPONSE`'s own mock handler checks channel registration before
    /// succeeding (like `IOCTL_SW_CAN_HS`), so the RPC's own Ok/Err result
    /// is a direct signal here too.
    #[tokio::test]
    async fn ioctl_set_poll_response_no_ops_for_a_hard_errored_cll() {
        let mut link = link_with_periodic(0);
        link.tp20_broadcast_periodic = None;
        link.connected = false;
        link.channel_id = None;
        link.hw_protocol_id = j2534_0404::PROTOCOL_GM_UART_PS;

        let mut sc = shared_channel(Vec::new());
        sc.dead = true;

        let service = service_with_link(link, 0, Some(sc)).await;

        service
            .rpc_io_ctl(set_poll_response_request(TEST_CLL))
            .await
            .expect(
                "a hard-errored CLL's retained channel_key must fall back to the no-op \
                 \"not yet connected\" path, not reach the native call on a dead channel",
            );
    }

    /// Regression test for `edge-case-hunter` finding #2 (PR #105 close-out
    /// backlog entry, fixed here), covering `ioctl_get_ndis_adapter_info`.
    /// Same shape as `legacy_ioctl_rejects_a_hard_errored_cll_despite_its_
    /// retained_channel_key` above: this handler already rejects an
    /// unconnected CLL with `PDU_ERR_CLL_NOT_CONNECTED`, so a hard-errored
    /// CLL must fall into that same rejection instead of resolving the dead
    /// `SharedChannel` entry's `channel_id` as if it were live.
    #[tokio::test]
    async fn ioctl_get_ndis_adapter_info_rejects_a_hard_errored_cll_despite_its_retained_channel_key()
     {
        let mut link = link_with_periodic(0);
        link.tp20_broadcast_periodic = None;
        link.connected = false;
        link.channel_id = None;
        link.hw_protocol_id = j2534_0404::PROTOCOL_ETHERNET_NDIS;

        let mut sc = shared_channel(Vec::new());
        sc.dead = true;

        let service = service_with_link(link, 0, Some(sc)).await;

        let err = service
            .rpc_io_ctl(get_ndis_adapter_info_request(TEST_CLL))
            .await
            .expect_err(
                "a hard-errored CLL's retained channel_key must not resolve a live channel_id",
            );
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&err)
            .expect("ErrorDetail should be attached");
        assert_eq!(
            detail.pdu_error,
            PduError::PduErrCllNotConnected as i32,
            "must be rejected specifically as not-connected, not some other FailedPrecondition"
        );
    }
}

/// Direct unit-level coverage of `terminate_tp20_broadcast_periodic_for_
/// suspension`'s session-gated restore-vs-leak-track branch (Codex review
/// fix, P2, PR #101, round 10). A genuine concurrent disconnect (or
/// disconnect+reconnect) landing between a caller capturing `periodic`/
/// `connect_generation` and this function's own failure branch running is
/// infeasible to construct through the gRPC layer with this crate's
/// established test techniques -- the same class of narrow-window
/// infeasibility `rollback_stop_comm_pending_tests` (`rpc_primitive.rs`)
/// already documents for its own generation gate. These tests instead call
/// the function directly with a deliberately mismatched `connect_generation`,
/// exercising the same guard logic that would fire if the race actually
/// happened, using `__mock_set_stop_periodic_message_error` (added by the
/// round-2 `CoptCancel` failure-handling fix, commit 3f3cd98) to force the
/// native `PassThruStopPeriodicMsg` call to fail deterministically.
#[cfg(test)]
mod terminate_tp20_broadcast_periodic_for_suspension_tests {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::os::raw::c_long;
    use std::sync::Arc;

    use j2534_0404_sys::libloading::{Library, Symbol};
    use serial_test::serial;
    use tokio::sync::Mutex;

    use super::*;

    const TEST_CLL: u32 = 1;
    const COP_HANDLE: u32 = 200;
    const TEST_CHANNEL_ID: ChannelId = ChannelId(9);
    const TEST_CHANNEL_KEY: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);
    /// The `connect_generation` this test's `periodic`/`channel_id` were
    /// captured against (before the simulated disconnect/reconnect).
    const CAPTURED_GENERATION: u64 = 1;
    /// The link's LIVE `connect_generation` at the time the failure branch
    /// runs -- deliberately different from `CAPTURED_GENERATION`, simulating
    /// a disconnect+reconnect landing in the gap.
    const LIVE_GENERATION: u64 = 2;

    /// Mirrors `finalize_or_orphan_broadcast_periodic_start_tests::
    /// stop_periodic_call_count`'s own documented rationale: a fresh
    /// `Library::new` on the identical path resolves to the SAME
    /// dynamically-loaded shared object `service.api` itself mutates, not a
    /// separate statically-linked copy of `MockState`.
    fn set_stop_periodic_message_error(code: Option<c_long>) {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = Library::new(&lib_path).expect("mock library should be loadable");
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = lib
                .get(b"__mock_set_stop_periodic_message_error\0")
                .expect("__mock_set_stop_periodic_message_error should be exported");
            f(code.unwrap_or(0));
        }
    }

    /// A connected `LogicalLinkState` at `LIVE_GENERATION`, with no live
    /// `tp20_broadcast_periodic` entry of its own (the caller already took
    /// it before calling the function under test, mirroring every real call
    /// site's own "captured, not re-derived" shape).
    fn live_link() -> LogicalLinkState {
        LogicalLinkState {
            channel_id: Some(TEST_CHANNEL_ID),
            protocol: ChannelProtocol::CAN,
            hw_protocol_id: j2534_0404::CAN,
            software_isotp: false,
            uudt_channel_id: None,
            uudt_channel_key: None,
            isotp_rx: Arc::new(Mutex::new(HashMap::new())),
            connect_in_flight: std::sync::Weak::new(),
            connected: true,
            comm_started: true,
            raw_mode: false,
            checksum_mode: false,
            connect_generation: LIVE_GENERATION,
            stop_comm_pending: false,
            channel_key: Some(TEST_CHANNEL_KEY),
            pin_select: None,
            channel_index: None,
            base_hw_protocol_override: None,
            rx_buf: Arc::new(Mutex::new(CllEventQueue {
                event_queue_cap: 16,
                ..CllEventQueue::default()
            })),
            working: ComParamSet::default(),
            active: ComParamSet::default(),
            tester_present_state: TesterPresentState::None,
            tester_present_base_tx_flags: 0,
            open_tp_discards: Vec::new(),
            working_unique_resp_id_table: Vec::new(),
            active_unique_resp_id_table: Vec::new(),
            unique_resp_filter_ids: Vec::new(),
            cancelled_cops: HashSet::new(),
            held_lock_mask: 0,
            last_error: None,
            tx_held: VecDeque::new(),
            tx_suspended_by_ioctl: false,
            tx_suspended_by_lock: false,
            tx_suspended_by_error: false,
            error_clear_seq: 0,
            error_set_seq: 0,
            client_filters: HashMap::new(),
            repeat_message_ids: Vec::new(),
            pending_client_filters: HashMap::new(),
            registrants: Vec::new(),
            next_registrant_seq: 0,
            j1939_claimed_address: None,
            j1939_claim_cursor: 0,
            j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
            tp20_connection: None,
            tp20_broadcast_periodic: None,
        }
    }

    fn shared_channel(ref_count: u32) -> SharedChannel {
        SharedChannel {
            channel_id: TEST_CHANNEL_ID,
            ref_count,
            tx_queue: tokio::sync::mpsc::unbounded_channel().0,
            executing_cop: Arc::new(Mutex::new(None)),
            _poll_cancel: tokio::sync::oneshot::channel().0,
            connect_flags: 0,
            dead: false,
            occupancy_epoch: 0,
            leaked_repeat_message_ids: Vec::new(),
            leaked_periodic_message_ids: Vec::new(),
            applied_analog_sample_rate: None,
            applied_analog_samples_per_reading: None,
            applied_analog_readings_per_msg: None,
            j1939_claims: HashMap::new(),
            j1939_claim_results: HashMap::new(),
            j1939_reclaim_pending: HashMap::new(),
            leaked_j1939_claims: Vec::new(),
            tp20_connections: HashMap::new(),
            tp20_connection_results: HashMap::new(),
            become_master_in_flight: Arc::new(portable_atomic::AtomicBool::new(false)),
            tp20_passive: None,
        }
    }

    /// Builds a minimal `J2534Service` with `live_link()` at `TEST_CLL` and
    /// (if `Some`) `sc` installed as the `TEST_CHANNEL_KEY` shared channel.
    /// Mirrors `clear_periodic_msgs_epoch_gating_tests::service_with_link`'s
    /// own construction shape.
    async fn service_with_live_link(sc: Option<SharedChannel>) -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");

        let mut primitives = HashMap::new();
        primitives.insert(
            COP_HANDLE,
            CopEntry {
                cll_handle: TEST_CLL,
                dispatched: true,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        let mut shared_channels = HashMap::new();
        if let Some(sc) = sc {
            shared_channels.insert(TEST_CHANNEL_KEY, sc);
        }

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::default(),
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(HashMap::from([(TEST_CLL, live_link())]))),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(shared_channels)),
            primitives: Arc::new(Mutex::new(primitives)),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn test_periodic() -> Tp20BroadcastPeriodic {
        Tp20BroadcastPeriodic {
            cop_handle: COP_HANDLE,
            message_id: Some(j2534_0404::PeriodicMessageId(777)),
            started_epoch: 3,
            pending_clear_generation: 0,
        }
    }

    /// A generation mismatch (simulating a disconnect+reconnect racing this
    /// failure branch) with a matching `shared_channels` entry that a
    /// sibling CLL still keeps open (`ref_count > 1`): the failed stop must
    /// NOT be restored onto the (now-different-session) link, and must
    /// instead be leak-tracked against the shared channel.
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn generation_mismatch_leak_tracks_instead_of_restoring_when_channel_survives() {
        // `service_with_live_link` must run FIRST: its `api` field keeps the
        // mock cdylib loaded for the rest of this test, so the fault
        // injected via a separate `Library::new` handle just below actually
        // lands on the SAME dynamically-loaded shared object -- injecting
        // the fault before anything else holds the library open risks the
        // loader unloading (and resetting) the mock's static state between
        // the two independent `Library::new` calls.
        let service = service_with_live_link(Some(shared_channel(2))).await;
        set_stop_periodic_message_error(Some(j2534_0404::ERR_FAILED as c_long));
        let periodic = test_periodic();

        service
            .terminate_tp20_broadcast_periodic_for_suspension(
                TEST_CLL,
                periodic,
                Some(TEST_CHANNEL_ID),
                CAPTURED_GENERATION,
                Some(TEST_CHANNEL_KEY),
            )
            .await;

        set_stop_periodic_message_error(None);

        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "a generation-mismatched failure must never restore onto the (now-different-\
             session) link"
        );
        drop(links);

        let chans = service.shared_channels.lock().await;
        let sc = chans.get(&TEST_CHANNEL_KEY).unwrap();
        assert_eq!(
            sc.leaked_periodic_message_ids,
            vec![(j2534_0404::PeriodicMessageId(777), 3)],
            "the failed stop must be leak-tracked against the surviving shared channel"
        );
    }

    /// Same generation mismatch, but the physical channel has ALSO fully
    /// closed by the time this runs (no `shared_channels` entry for
    /// `channel_key`): nothing to restore, nothing to leak-track -- just the
    /// accepted-residual double-fault warn path, no panic.
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn generation_mismatch_with_no_surviving_channel_does_not_panic_or_leak() {
        // See the sibling test above for why `service_with_live_link` must
        // run before the fault is injected.
        let service = service_with_live_link(None).await;
        set_stop_periodic_message_error(Some(j2534_0404::ERR_FAILED as c_long));
        let periodic = test_periodic();

        service
            .terminate_tp20_broadcast_periodic_for_suspension(
                TEST_CLL,
                periodic,
                Some(TEST_CHANNEL_ID),
                CAPTURED_GENERATION,
                Some(TEST_CHANNEL_KEY),
            )
            .await;

        set_stop_periodic_message_error(None);

        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "nothing must be restored when the original session is gone"
        );
        drop(links);
        assert!(
            service.shared_channels.lock().await.is_empty(),
            "there is nothing to leak-track when the physical channel has also fully closed"
        );
    }

    /// Generation mismatch, and `channel_key` DOES resolve to a live
    /// `SharedChannel` entry -- but its `channel_id` differs from the
    /// captured one, simulating the physical channel having been closed and
    /// a completely unrelated new connection installed at the same
    /// `ChannelKey` (same protocol/baud/flags/pin) in the meantime
    /// (edge-case-hunter finding, PR #101 round 10 verification). The
    /// failed stop must NOT be leak-tracked onto this unrelated channel --
    /// doing so would corrupt its tracking for a message that was never
    /// actually running on it.
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn generation_mismatch_with_channel_id_mismatch_does_not_leak_onto_unrelated_channel() {
        // See the first test above for why `service_with_live_link` must run
        // before the fault is injected.
        const UNRELATED_CHANNEL_ID: ChannelId = ChannelId(999);
        let mut sc = shared_channel(1);
        sc.channel_id = UNRELATED_CHANNEL_ID;
        let service = service_with_live_link(Some(sc)).await;
        set_stop_periodic_message_error(Some(j2534_0404::ERR_FAILED as c_long));
        let periodic = test_periodic();

        service
            .terminate_tp20_broadcast_periodic_for_suspension(
                TEST_CLL,
                periodic,
                Some(TEST_CHANNEL_ID),
                CAPTURED_GENERATION,
                Some(TEST_CHANNEL_KEY),
            )
            .await;

        set_stop_periodic_message_error(None);

        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "nothing must be restored when the original session is gone"
        );
        drop(links);
        let chans = service.shared_channels.lock().await;
        assert!(
            chans
                .get(&TEST_CHANNEL_KEY)
                .unwrap()
                .leaked_periodic_message_ids
                .is_empty(),
            "a channel_id mismatch must never leak-track onto the unrelated channel now \
             occupying the same ChannelKey"
        );
    }

    /// Codex review finding 1 (round 11, PR #101): a matching
    /// `connect_generation` alone is not enough to call this the same
    /// session -- a PLAIN disconnect (no reconnect) leaves
    /// `connect_generation` unchanged and only clears `connected`
    /// (`rpc_link.rs`'s disconnect path), so a generation-only gate would
    /// wrongly restore onto a link that has since been disconnected. This
    /// simulates exactly that: `connect_generation` still matches
    /// `LIVE_GENERATION`, but `connected` is `false`.
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn matching_generation_but_disconnected_leak_tracks_instead_of_restoring() {
        // See the first test above for why `service_with_live_link` must run
        // before the fault is injected.
        let service = service_with_live_link(Some(shared_channel(2))).await;
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_CLL)
            .unwrap()
            .connected = false;
        set_stop_periodic_message_error(Some(j2534_0404::ERR_FAILED as c_long));
        let periodic = test_periodic();

        service
            .terminate_tp20_broadcast_periodic_for_suspension(
                TEST_CLL,
                periodic,
                Some(TEST_CHANNEL_ID),
                LIVE_GENERATION,
                Some(TEST_CHANNEL_KEY),
            )
            .await;

        set_stop_periodic_message_error(None);

        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "a matching-generation-but-disconnected failure must never restore onto the \
             (now-disconnected) link"
        );
        drop(links);

        let chans = service.shared_channels.lock().await;
        let sc = chans.get(&TEST_CHANNEL_KEY).unwrap();
        assert_eq!(
            sc.leaked_periodic_message_ids,
            vec![(j2534_0404::PeriodicMessageId(777), 3)],
            "the failed stop must be leak-tracked against the surviving shared channel"
        );
    }

    /// Codex review fix (P2, PR #101, round 13): `same_session` holds (same
    /// `connect_generation`, still `connected`), but a concurrent
    /// `StartComPrimitive` already reclaimed `tp20_broadcast_periodic` with
    /// a fresh, unrelated reservation before this failure could be
    /// recorded. Before round 13 this dead-ended in a `warn!` with nothing
    /// tracked anywhere (the accepted residual documented in
    /// the backlog, closed by this fix); now it
    /// must fall through to the same leak-tracking path a session mismatch
    /// uses, and the fresh reservation occupying the slot must be left
    /// completely untouched.
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn same_session_with_slot_reclaimed_leak_tracks_instead_of_dropping() {
        let service = service_with_live_link(Some(shared_channel(2))).await;
        let fresh_reservation = Tp20BroadcastPeriodic {
            cop_handle: COP_HANDLE + 1,
            message_id: Some(j2534_0404::PeriodicMessageId(888)),
            started_epoch: 5,
            pending_clear_generation: 0,
        };
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_CLL)
            .unwrap()
            .tp20_broadcast_periodic = Some(fresh_reservation);

        let old_periodic = test_periodic();
        service
            .restore_or_leak_track_broadcast_periodic(
                TEST_CLL,
                old_periodic,
                j2534_0404::PeriodicMessageId(777),
                CapturedBroadcastPeriodicSession {
                    connect_generation: LIVE_GENERATION,
                    channel_key: Some(TEST_CHANNEL_KEY),
                    channel_id: Some(TEST_CHANNEL_ID),
                },
                "slot-reclaimed test",
            )
            .await;

        let links = service.logical_links.lock().await;
        assert_eq!(
            links.get(&TEST_CLL).unwrap().tp20_broadcast_periodic,
            Some(fresh_reservation),
            "the fresh reservation occupying the slot must be left untouched"
        );
        drop(links);

        let chans = service.shared_channels.lock().await;
        let sc = chans.get(&TEST_CHANNEL_KEY).unwrap();
        assert_eq!(
            sc.leaked_periodic_message_ids,
            vec![(j2534_0404::PeriodicMessageId(777), 3)],
            "the old failed-stop message must now be leak-tracked instead of silently dropped"
        );
    }

    /// Codex review fix (P2, PR #101, round 15): `ERR_INVALID_MSG_ID` from
    /// the native `PassThruStopPeriodicMsg` call this function issues is an
    /// AUTHORITATIVE "device already forgot this message" signal, not a
    /// genuine failure (e.g. `CLEAR_PERIODIC_MSGS` winning `self.api`'s lock
    /// after this function's caller already took the entry off
    /// `link.tp20_broadcast_periodic` but before this function's own native
    /// stop runs). It must fall through to the same clean `PduCopstFinished`
    /// finalization the ordinary `Ok(())` success arm performs, not the
    /// restore-or-leak-track path a genuine failure takes -- mirrors round
    /// 14's identical `CoptCancel` fix
    /// (`rpc_primitive.rs`'s
    /// `cancel_treats_err_invalid_msg_id_from_stop_as_already_cancelled`)
    /// and this crate's two earlier precedents for the same idiom,
    /// `retry_leaked_periodic_message_stops` and
    /// `retry_leaked_repeat_message_stops` (both above in this file). Uses a
    /// live/matching-session link (`LIVE_GENERATION`, `connected: true`) --
    /// the same-session case is exercised deliberately, since
    /// `is_invalid_msg_id` is checked BEFORE the session gate and must skip
    /// straight to finalization regardless of session state.
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn err_invalid_msg_id_finalizes_cop_instead_of_restoring_or_leak_tracking() {
        let service = service_with_live_link(Some(shared_channel(2))).await;
        set_stop_periodic_message_error(Some(j2534_0404::ERR_INVALID_MSG_ID as c_long));
        let periodic = test_periodic();

        service
            .terminate_tp20_broadcast_periodic_for_suspension(
                TEST_CLL,
                periodic,
                Some(TEST_CHANNEL_ID),
                LIVE_GENERATION,
                Some(TEST_CHANNEL_KEY),
            )
            .await;

        set_stop_periodic_message_error(None);

        assert!(
            !service.primitives.lock().await.contains_key(&COP_HANDLE),
            "the COP must be finalized -- ERR_INVALID_MSG_ID falls through to the ordinary \
             PduCopstFinished success path, same as a real Ok(()) stop"
        );

        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_CLL).unwrap();
        assert!(
            link.tp20_broadcast_periodic.is_none(),
            "nothing must be restored onto the link -- the device says the message is already \
             gone, there is nothing to retry"
        );
        let queue = link.rx_buf.lock().await;
        assert_eq!(
            queue.items.len(),
            1,
            "a single PduCopstFinished terminal status must be emitted for the finalized COP"
        );
        drop(queue);
        drop(links);

        let chans = service.shared_channels.lock().await;
        assert!(
            chans
                .get(&TEST_CHANNEL_KEY)
                .unwrap()
                .leaked_periodic_message_ids
                .is_empty(),
            "nothing must be leak-tracked against the shared channel either -- there is \
             nothing left to clean up device-side"
        );
    }
}
