#![allow(clippy::too_many_arguments)]
#![allow(unsafe_code)]
#![allow(non_snake_case)]

use std::collections::{HashMap, VecDeque};
use std::os::raw::{c_char, c_long, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use j2534_0404_sys::bindings::{
    BLOCK_FILTER,
    CONFIG_BIT_SAMPLE_POINT,
    CONFIG_CAN_MIXED_FORMAT,
    CONFIG_FD_CAN_DATA_PHASE_RATE,
    // SAE J2534-2 clause 10.3.3.2 Analog Inputs remaining parameters
    // (ADR-216): the three genuinely read-only capability parameters, plus
    // the two writable ones this mock needs to distinguish
    // `SET_CONFIG`-time (`ChannelState::new`'s seeded defaults, the
    // `IOCTL_SET_CONFIG` handler's read-only rejection and
    // rate-armed-rejects-batch-size-change simulation below).
    CONFIG_INPUT_RANGE_HIGH,
    CONFIG_INPUT_RANGE_LOW,
    CONFIG_J1962_PINS,
    CONFIG_NON_VOLATILE_STORE_1,
    CONFIG_NON_VOLATILE_STORE_2,
    CONFIG_NON_VOLATILE_STORE_3,
    CONFIG_NON_VOLATILE_STORE_4,
    CONFIG_NON_VOLATILE_STORE_5,
    CONFIG_NON_VOLATILE_STORE_6,
    CONFIG_NON_VOLATILE_STORE_7,
    CONFIG_NON_VOLATILE_STORE_8,
    CONFIG_NON_VOLATILE_STORE_9,
    CONFIG_NON_VOLATILE_STORE_10,
    CONFIG_READINGS_PER_MSG,
    CONFIG_SAMPLE_RATE,
    CONFIG_SAMPLE_RESOLUTION,
    CONFIG_SAMPLES_PER_READING,
    CONFIG_SYNC_JUMP_WIDTH,
    // SAE J2534-2 clause 19.3.1/Table 77 TP2.0 passive connections
    // (ADR-190/Phase 7 Stage 7b): the two passive-listener enablers --
    // `IOCTL_SET_CONFIG`'s own range-validation gate below, and
    // `__mock_inject_tp20_passive_connection`'s own reads of the channel's
    // currently-armed values.
    CONFIG_TP2_0_IDENTIFER,
    CONFIG_TP2_0_RXIDPASSIVE,
    DEVICE_INFO_ANALOG_IN_SIMULTANEOUS,
    DEVICE_INFO_ANALOG_IN_SUPPORTED,
    DEVICE_INFO_CAN_SIMULTANEOUS,
    DEVICE_INFO_CAN_SUPPORTED,
    // ADR-194/Phase 16: this mock advertises this flag supported, matching
    // the protocol it actually implements -- see the `IOCTL_GET_DEVICE_INFO`
    // handler's own comment.
    DEVICE_INFO_ETHERNET_NDIS_SUPPORTED,
    // ADR-213/Round 3: this mock advertises these two flags supported, each
    // with its own dedicated packed `_CHx` capacity override (mirroring
    // Fault-Tolerant CAN's/Single Wire CAN's own precedent) -- see the
    // `IOCTL_GET_DEVICE_INFO` handler's own comment.
    DEVICE_INFO_FD_CAN_SUPPORTED,
    DEVICE_INFO_FD_ISO15765_SUPPORTED,
    // ADR-185 Stage 1: this mock advertises these six flags supported,
    // matching the protocols it actually implements -- see the
    // `IOCTL_GET_DEVICE_INFO` handler's own comment.
    DEVICE_INFO_FT_CAN_SUPPORTED,
    DEVICE_INFO_FT_ISO15765_SUPPORTED,
    // ADR-189/Phase 8: this mock advertises this flag supported, matching
    // the protocol it actually implements -- see the `IOCTL_GET_DEVICE_INFO`
    // handler's own comment.
    DEVICE_INFO_GM_UART_SUPPORTED,
    DEVICE_INFO_HONDA_DIAGH_SUPPORTED,
    DEVICE_INFO_ISO9141_SIMULTANEOUS,
    DEVICE_INFO_ISO9141_SUPPORTED,
    DEVICE_INFO_ISO14230_SIMULTANEOUS,
    DEVICE_INFO_ISO14230_SUPPORTED,
    DEVICE_INFO_ISO15765_SIMULTANEOUS,
    DEVICE_INFO_ISO15765_SUPPORTED,
    DEVICE_INFO_J1708_SUPPORTED,
    DEVICE_INFO_J1850PWM_SIMULTANEOUS,
    DEVICE_INFO_J1850PWM_SUPPORTED,
    DEVICE_INFO_J1850VPW_SIMULTANEOUS,
    DEVICE_INFO_J1850VPW_SUPPORTED,
    // Mechanical extension of ADR-211's/ADR-212's own pattern: this mock
    // advertises this flag supported with a real packed `_CHx` count --
    // see the `IOCTL_GET_DEVICE_INFO` handler's own comment.
    DEVICE_INFO_J1939_SUPPORTED,
    DEVICE_INFO_J2610_SUPPORTED,
    // SAE J2534-2 clause 18 Device Configuration (ADR-176/Phase 14, ADR-185
    // Stage 2): this mock implements GET/SET_DEVICE_CONFIG against ten
    // NON_VOLATILE_STORE_1.._10 slots, so it must also advertise
    // `DEVICE_INFO_MAX_NON_VOLATILE_STORAGE` -- see the `IOCTL_GET_DEVICE_INFO`
    // handler's own comment.
    DEVICE_INFO_MAX_NON_VOLATILE_STORAGE,
    DEVICE_INFO_PGM_VOLTAGE_J1962,
    DEVICE_INFO_READ_J1962PIN_VOLTAGE_MAX,
    DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED,
    DEVICE_INFO_SCI_A_ENGINE_SIMULTANEOUS,
    DEVICE_INFO_SCI_A_ENGINE_SUPPORTED,
    DEVICE_INFO_SCI_A_TRANS_SIMULTANEOUS,
    DEVICE_INFO_SCI_A_TRANS_SUPPORTED,
    DEVICE_INFO_SCI_B_ENGINE_SIMULTANEOUS,
    DEVICE_INFO_SCI_B_ENGINE_SUPPORTED,
    DEVICE_INFO_SCI_B_TRANS_SIMULTANEOUS,
    DEVICE_INFO_SCI_B_TRANS_SUPPORTED,
    DEVICE_INFO_SHORT_TO_GND_J1962,
    DEVICE_INFO_SW_CAN_SUPPORTED,
    DEVICE_INFO_SW_ISO15765_SUPPORTED,
    // ADR-188/Phase 7 Stage 7a: this mock advertises this flag supported,
    // matching the protocol it actually implements -- see the
    // `IOCTL_GET_DEVICE_INFO` handler's own comment.
    DEVICE_INFO_TP2_0_SUPPORTED,
    DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED,
    ERR_ADDRESS_NOT_CLAIMED,
    ERR_BUFFER_EMPTY,
    ERR_CHANNEL_IN_USE,
    ERR_EXCEEDED_LIMIT,
    ERR_FAILED,
    ERR_INVALID_CHANNEL_ID,
    ERR_INVALID_IOCTL_ID,
    ERR_INVALID_IOCTL_PARAM_ID,
    ERR_INVALID_IOCTL_VALUE,
    ERR_INVALID_MSG,
    ERR_INVALID_MSG_ID,
    ERR_INVALID_PROTOCOL_ID,
    // ADR-188/Phase 7 Stage 7a (SAE J2534-2 clause 19 TP2.0, Table 83):
    // `PassThruWriteMsgs`'s clause 19.4.4 oversized-non-connection-write
    // rejection.
    // ADR-194/Phase 16 (SAE J2534-2 clause 24 Ethernet_NDIS):
    // `__mock_set_ndis_connect_error`'s activation-failure injection.
    ERR_NO_CONNECTION_ESTABLISHED,
    ERR_NOT_SUPPORTED,
    // ADR-188/Phase 7 Stage 7a, Codex review finding (PR #97, round 11):
    // `IOCTL_REQUEST_CONNECTION`'s clause 19.3.3.2 duplicate-RX-ID rejection.
    ERR_NOT_UNIQUE,
    ERR_NULL_PARAMETER,
    ERR_PIN_INVALID,
    ERR_RESOURCE_IN_USE,
    // ADR-189/Phase 8: SAE J2534-2 clause 11 GM UART Protocol's `BECOME_
    // MASTER` ChannelID-scoped IOCTL.
    IOCTL_BECOME_MASTER,
    IOCTL_CLEAR_FUNCT_MSG_LOOKUP_TABLE,
    IOCTL_CLEAR_MSG_FILTERS,
    IOCTL_CLEAR_PERIODIC_MSGS,
    IOCTL_CLEAR_RX_BUFFER,
    IOCTL_CLEAR_TX_BUFFER,
    IOCTL_FAST_INIT,
    IOCTL_FIVE_BAUD_INIT,
    IOCTL_GET_CONFIG,
    IOCTL_GET_DEVICE_CONFIG,
    IOCTL_GET_DEVICE_INFO,
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): the one new
    // IOCTL this phase adds, channel-scoped.
    IOCTL_GET_NDIS_ADAPTER_INFO,
    IOCTL_GET_PROTOCOL_INFO,
    IOCTL_PROTECT_J1939_ADDR,
    IOCTL_QUERY_REPEAT_MESSAGE,
    IOCTL_READ_J1962PIN_VOLTAGE,
    IOCTL_READ_PROG_VOLTAGE,
    IOCTL_READ_VBATT,
    // ADR-188/Phase 7 Stage 7a: SAE J2534-2 clause 19 TP2.0's two
    // ChannelID-scoped IOCTLs.
    IOCTL_REQUEST_CONNECTION,
    IOCTL_SET_CONFIG,
    IOCTL_SET_DEVICE_CONFIG,
    // ADR-189/Phase 8: SAE J2534-2 clause 11 GM UART Protocol's `SET_POLL_
    // RESPONSE` ChannelID-scoped IOCTL.
    IOCTL_SET_POLL_RESPONSE,
    IOCTL_START_REPEAT_MESSAGE,
    IOCTL_STOP_REPEAT_MESSAGE,
    IOCTL_SW_CAN_HS,
    IOCTL_SW_CAN_NS,
    IOCTL_TEARDOWN_CONNECTION,
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): the canned
    // `IOCTL_GET_NDIS_ADAPTER_INFO` output struct.
    NDIS_ADAPTER_INFORMATION,
    PASS_FILTER,
    PASSTHRU_MSG,
    PROTOCOL_ANALOG_IN_1,
    PROTOCOL_ANALOG_IN_32,
    PROTOCOL_CAN,
    PROTOCOL_CAN_CH1,
    PROTOCOL_CAN_PS,
    // ADR-207: SAE J2534-2 clause 12 UART Echo Byte Protocol's `_CHx` block
    // base id (the header names this block without the "UART_" prefix its
    // `_PS` id uses, `PROTOCOL_UART_ECHO_BYTE_PS` below).
    PROTOCOL_ECHO_BYTE_CH1,
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): the one native
    // protocol id this phase supports.
    PROTOCOL_ETHERNET_NDIS,
    // ADR-213/Round 3: SAE J2534-2 clause 21 CAN FD's `_CHx` block base id
    // (and its own upper bound), plus clause 22 ISO15765-on-CAN-FD's `_CHx`
    // block base id -- the header names both blocks consistently with their
    // own `_PS` ids, no naming inconsistency (like Fault-Tolerant CAN's own
    // pair).
    PROTOCOL_FD_CAN_CH1,
    PROTOCOL_FD_CAN_CH128,
    PROTOCOL_FD_CAN_PS,
    PROTOCOL_FD_ISO15765_CH1,
    PROTOCOL_FD_ISO15765_CH128,
    PROTOCOL_FD_ISO15765_PS,
    // ADR-211: SAE J2534-2 clause 20 Fault-Tolerant CAN's two `_CHx` block
    // base ids (the header names both blocks consistently with their own
    // `_PS` ids, no naming inconsistency).
    PROTOCOL_FT_CAN_CH1,
    PROTOCOL_FT_CAN_CH128,
    PROTOCOL_FT_CAN_PS,
    PROTOCOL_FT_ISO15765_CH1,
    PROTOCOL_FT_ISO15765_CH128,
    PROTOCOL_FT_ISO15765_PS,
    // ADR-189/Phase 8: SAE J2534-2 clause 11 GM UART Protocol's one
    // `_PS`-only hardware id, plus its `_CHx` block's own bounds (this mock
    // is the first to implement a standalone protocol with `_CHx` support).
    PROTOCOL_GM_UART_CH1,
    PROTOCOL_GM_UART_CH128,
    PROTOCOL_GM_UART_PS,
    // ADR-208: SAE J2534-2 clause 13 Honda DIAG-H Protocol's `_CHx` block
    // base id (unlike UART Echo Byte's own block, the header names this
    // block consistently with its `_PS` id).
    PROTOCOL_HONDA_DIAGH_CH1,
    PROTOCOL_HONDA_DIAGH_PS,
    PROTOCOL_INFO_CAN_11_29_IDS_SUPPORTED,
    PROTOCOL_INFO_MAX_BLOCK_FILTER,
    PROTOCOL_INFO_MAX_PASS_FILTER,
    PROTOCOL_INFO_MAX_REPEAT_MESSAGING,
    PROTOCOL_INFO_MAX_REPEAT_MESSAGING_LENGTH,
    PROTOCOL_INFO_MAX_RX_BUFFER_SIZE,
    PROTOCOL_ISO9141,
    PROTOCOL_ISO9141_CH1,
    PROTOCOL_ISO9141_PS,
    PROTOCOL_ISO14230,
    PROTOCOL_ISO14230_CH1,
    PROTOCOL_ISO14230_PS,
    PROTOCOL_ISO15765,
    PROTOCOL_ISO15765_CH1,
    PROTOCOL_ISO15765_PS,
    // ADR-209: SAE J2534-2 clause 17 SAE J1708 Protocol's `_CHx` block base
    // id (like Honda DIAG-H's own block, the header names this block
    // consistently with its `_PS` id).
    PROTOCOL_J1708_CH1,
    PROTOCOL_J1708_PS,
    PROTOCOL_J1850PWM,
    PROTOCOL_J1850PWM_CH1,
    PROTOCOL_J1850PWM_PS,
    PROTOCOL_J1850VPW,
    PROTOCOL_J1850VPW_CH1,
    PROTOCOL_J1850VPW_PS,
    PROTOCOL_J1939_CH1,
    PROTOCOL_J1939_CH128,
    PROTOCOL_J1939_PS,
    PROTOCOL_J2610_CH1,
    PROTOCOL_J2610_PS,
    PROTOCOL_SCI_A_ENGINE,
    PROTOCOL_SCI_A_TRANS,
    PROTOCOL_SCI_B_ENGINE,
    PROTOCOL_SCI_B_TRANS,
    // ADR-212/Round 2: SAE J2534-2 clause 9 Single Wire CAN's two `_CHx`
    // block base ids -- UNLIKE every other family's block, the header names
    // BOTH of these blocks with a `SW_CAN_` prefix regardless of which `_PS`
    // id they extend (a confirmed naming inconsistency).
    PROTOCOL_SW_CAN_CAN_CH1,
    PROTOCOL_SW_CAN_CAN_CH128,
    PROTOCOL_SW_CAN_ISO15765_CH1,
    PROTOCOL_SW_CAN_ISO15765_CH128,
    PROTOCOL_SW_CAN_PS,
    PROTOCOL_SW_ISO15765_PS,
    // ADR-188/Phase 7 Stage 7a: SAE J2534-2 clause 19 TP2.0's one `_PS`-only
    // hardware id. ADR-210: `_CH1`/`_CH128` (the block's own bounds), for
    // clause 7 Additional Channels -- like Honda DIAG-H's/SAE J1708's own
    // blocks, the header names this block consistently with its `_PS` id.
    PROTOCOL_TP2_0_CH1,
    PROTOCOL_TP2_0_CH128,
    PROTOCOL_TP2_0_PS,
    PROTOCOL_UART_ECHO_BYTE_PS,
    REPEAT_MSG_SETUP,
    // ADR-188/Phase 7 Stage 7a (clause 19.4.4 Table 81): the TP2.0
    // connection-request indication bits, sharing the identical numeric
    // values RX_FLAG_J1939_ADDRESS_CLAIMED/_LOST use (disambiguated by
    // protocol id, the same overloading the service side already handles).
    RX_FLAG_CONNECTION_ESTABLISHED,
    RX_FLAG_CONNECTION_LOST,
    RX_FLAG_J1939_ADDRESS_CLAIMED,
    RX_FLAG_J1939_ADDRESS_LOST,
    SBYTE_ARRAY,
    SCONFIG,
    SCONFIG_LIST,
    SPARAM_LIST,
    STATUS_NOERROR,
};

/// SAE J2534-2 clause 6.3.1 Table 1's seven Pin Selection (`_PS`) protocol
/// IDs implemented by this mock (ADR-156 Decision 2, Phase 2a): the six
/// native-to-`_PS` mappings `j2534-0404-service`'s `ps_protocol_id` computes
/// for the base J2534-1 protocols this mock supports, plus `J2610_PS` (the
/// single `_PS` id the four SCI protocols consolidate onto). A channel
/// opened with one of these leaves its DLC pins unassigned until
/// `IOCTL_SET_CONFIG(CONFIG_J1962_PINS)` binds them (clause 6.3.3.2).
const PS_PROTOCOL_IDS: [u32; 7] = [
    PROTOCOL_J1850VPW_PS,
    PROTOCOL_J1850PWM_PS,
    PROTOCOL_ISO9141_PS,
    PROTOCOL_ISO14230_PS,
    PROTOCOL_CAN_PS,
    PROTOCOL_ISO15765_PS,
    PROTOCOL_J2610_PS,
];

fn is_ps_protocol(protocol_id: u32) -> bool {
    PS_PROTOCOL_IDS.contains(&protocol_id)
}

/// `PS_PROTOCOL_IDS`'s base protocol id, positionally -- the mock's own
/// small, crate-local inverse of that table (ADR-157), mirroring
/// `j2534-0404-service`'s `resources::base_protocol_id` in spirit, not
/// shared code across the crate boundary. `J2610_PS` collapses to one
/// representative SCI id (`SCI_A_ENGINE`), same simplification the service
/// makes for the same reason (the mock has no per-SCI-variant Plane B
/// consumer either). Identity for every other value, including every
/// already-base id.
/// ADR-189/Phase 8: `PROTOCOL_GM_UART_PS` is appended at index 7 -- read only
/// via `chx_base_protocol_id`'s own `CHX_BLOCK_BASE_IDS`-positional lookup
/// below (`PS_PROTOCOL_IDS.iter().position` above never returns an index
/// past 6, since `PS_PROTOCOL_IDS` itself stays length-7 -- GM UART has no
/// clause 6 Pin Selection `_PS` entry there at all, clause 11 not being one
/// of Table 1's seven families). ADR-206: `PROTOCOL_J1939_PS` is appended at
/// index 8, the same "no clause 6 `_PS` entry" reasoning applying to clause
/// 16. ADR-207: `PROTOCOL_UART_ECHO_BYTE_PS` is appended at index 9, the same
/// "no clause 6 `_PS` entry" reasoning applying to clause 12. ADR-208:
/// `PROTOCOL_HONDA_DIAGH_PS` is appended at index 10, the same "no clause 6
/// `_PS` entry" reasoning applying to clause 13. ADR-209: `PROTOCOL_J1708_PS`
/// is appended at index 11, the same "no clause 6 `_PS` entry" reasoning
/// applying to clause 17. ADR-210: `PROTOCOL_TP2_0_PS` is appended at index
/// 12, the same "no clause 6 `_PS` entry" reasoning applying to clause 19.
/// ADR-211: `PROTOCOL_FT_CAN_PS`/`PROTOCOL_FT_ISO15765_PS` are appended at
/// indices 13/14. Unlike every entry from index 7 on, Fault-Tolerant CAN/
/// ISO15765 DO have a true unqualified base (`CAN`/`ISO15765`, via
/// `base_protocol_id`'s own FT arms) -- they are keyed here by their own
/// `_PS` id anyway, matching every other entry's convention, because this
/// table is read only through `chx_base_protocol_id`'s positional lookup,
/// which `base_protocol_id` itself now recurses through once more (ADR-211's
/// shared-infrastructure fix) to reach the true base -- so keying by `_PS`
/// here does not skip that recursion the way it would have before the fix.
/// ADR-212/Round 2: `PROTOCOL_SW_CAN_PS`/`PROTOCOL_SW_ISO15765_PS` are
/// appended at indices 15/16, the same "own `_PS` id, safe because
/// `base_protocol_id` recurses through this table's lookup" reasoning as
/// Fault-Tolerant CAN's own pair just above -- Single Wire CAN/ISO15765 also
/// have a true unqualified base (`CAN`/`ISO15765`, via `base_protocol_id`'s
/// pre-existing SW arms, unchanged by this round). ADR-213/Round 3:
/// `PROTOCOL_FD_CAN_PS`/`PROTOCOL_FD_ISO15765_PS` are appended at indices
/// 17/18, the third and final CAN-collapse family in this series -- same
/// keying reasoning as Fault-Tolerant CAN's/Single Wire CAN's own pairs
/// above.
const BASE_PROTOCOL_IDS: [u32; 19] = [
    PROTOCOL_J1850VPW,
    PROTOCOL_J1850PWM,
    PROTOCOL_ISO9141,
    PROTOCOL_ISO14230,
    PROTOCOL_CAN,
    PROTOCOL_ISO15765,
    PROTOCOL_SCI_A_ENGINE,
    PROTOCOL_GM_UART_PS,
    PROTOCOL_J1939_PS,
    PROTOCOL_UART_ECHO_BYTE_PS,
    PROTOCOL_HONDA_DIAGH_PS,
    PROTOCOL_J1708_PS,
    PROTOCOL_TP2_0_PS,
    PROTOCOL_FT_CAN_PS,
    PROTOCOL_FT_ISO15765_PS,
    PROTOCOL_SW_CAN_PS,
    PROTOCOL_SW_ISO15765_PS,
    PROTOCOL_FD_CAN_PS,
    PROTOCOL_FD_ISO15765_PS,
];

/// SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 3/Phase 2b)
/// `_CH1` block-base hardware id for each of the fifteen in-scope protocol
/// families -- the mock's own crate-local range table, mirroring
/// `j2534-0404-service`'s `resources::chx_protocol_id`/`chx_base_protocol_id`
/// in spirit (ADR-157's no-shared-code-across-the-FFI-boundary rule), not
/// shared code. Positionally aligned with `PS_PROTOCOL_IDS`/`BASE_PROTOCOL_IDS`
/// above. All four native SAE J2610 SCI ids collapse onto the single
/// `PROTOCOL_J2610_CH1` block, mirroring `PS_PROTOCOL_IDS`' own SCI collapse.
/// ADR-189/Phase 8: `PROTOCOL_GM_UART_CH1` is appended at index 7, aligned
/// with `BASE_PROTOCOL_IDS`' own `PROTOCOL_GM_UART_PS` at the same index --
/// unlike the first seven entries (keyed by a native, unqualified base
/// hardware id), GM UART is keyed by its own `_PS`-only id, mirroring
/// `j2534-0404-service`'s `resources::chx_block_base`'s identical choice
/// (clause 11 defines no unqualified base id at all). ADR-206:
/// `PROTOCOL_J1939_CH1` is appended at index 8, aligned with
/// `BASE_PROTOCOL_IDS`' own `PROTOCOL_J1939_PS` at the same index -- the
/// same "own `_PS`-only id, no unqualified base id" keying GM UART's own
/// entry uses (clause 16 likewise defines no unqualified base id). ADR-207:
/// `PROTOCOL_ECHO_BYTE_CH1` is appended at index 9, aligned with
/// `BASE_PROTOCOL_IDS`' own `PROTOCOL_UART_ECHO_BYTE_PS` at the same index --
/// the same "own `_PS`-only id, no unqualified base id" keying GM UART's/SAE
/// J1939's own entries use (clause 12 likewise defines no unqualified base
/// id). The header names this block without the "UART_" prefix its `_PS` id
/// uses (ADR-207 Context) -- a vendor-header naming inconsistency, not a
/// mismatch here. ADR-208: `PROTOCOL_HONDA_DIAGH_CH1` is appended at index
/// 10, aligned with `BASE_PROTOCOL_IDS`' own `PROTOCOL_HONDA_DIAGH_PS` at the
/// same index -- the same "own `_PS`-only id, no unqualified base id"
/// keying GM UART's/SAE J1939's/UART Echo Byte's own entries use (clause 13
/// likewise defines no unqualified base id). Unlike UART Echo Byte's own
/// block, the header names this block consistently with its `_PS` id -- no
/// naming inconsistency here. ADR-209: `PROTOCOL_J1708_CH1` is appended at
/// index 11, aligned with `BASE_PROTOCOL_IDS`' own `PROTOCOL_J1708_PS` at the
/// same index -- the same "own `_PS`-only id, no unqualified base id" keying
/// GM UART's/SAE J1939's/UART Echo Byte's/Honda DIAG-H's own entries use
/// (clause 17 likewise defines no unqualified base id). Like Honda DIAG-H's
/// own block, the header names this block consistently with its `_PS` id --
/// no naming inconsistency here either. ADR-210: `PROTOCOL_TP2_0_CH1` is
/// appended at index 12, aligned with `BASE_PROTOCOL_IDS`' own
/// `PROTOCOL_TP2_0_PS` at the same index -- the same "own `_PS`-only id, no
/// unqualified base id" keying every entry above uses (clause 19 likewise
/// defines no unqualified base id). Like Honda DIAG-H's/SAE J1708's own
/// blocks, the header names this block consistently with its `_PS` id -- no
/// naming inconsistency here either. ADR-211: `PROTOCOL_FT_CAN_CH1`/
/// `PROTOCOL_FT_ISO15765_CH1` are appended at indices 13/14, aligned with
/// `BASE_PROTOCOL_IDS`' own `PROTOCOL_FT_CAN_PS`/`PROTOCOL_FT_ISO15765_PS` at
/// the same indices -- see that array's own doc comment for why keying these
/// two by their `_PS` id (rather than the true `CAN`/`ISO15765` base they DO
/// have, unlike every entry from index 7 on) is safe here. Both blocks are
/// named consistently with their own `_PS` ids -- no naming inconsistency.
/// ADR-212/Round 2: `PROTOCOL_SW_CAN_CAN_CH1`/`PROTOCOL_SW_CAN_ISO15765_CH1`
/// are appended at indices 15/16, aligned with `BASE_PROTOCOL_IDS`' own
/// `PROTOCOL_SW_CAN_PS`/`PROTOCOL_SW_ISO15765_PS` at the same indices -- see
/// that array's own doc comment for why keying these two by their `_PS` id is
/// safe here. UNLIKE every other family this table covers (including
/// Fault-Tolerant CAN's own pair just above), Single Wire CAN's block
/// constant names are BOTH prefixed `SW_CAN_` regardless of which `_PS` id
/// they extend -- a confirmed vendor-header naming inconsistency (ADR-212
/// Context item 2), the first one this table has to encode. ADR-213/Round 3:
/// `PROTOCOL_FD_CAN_CH1`/`PROTOCOL_FD_ISO15765_CH1` are appended at indices
/// 17/18, aligned with `BASE_PROTOCOL_IDS`' own `PROTOCOL_FD_CAN_PS`/
/// `PROTOCOL_FD_ISO15765_PS` at the same indices -- the third and final
/// CAN-collapse family in this series. Unlike Single Wire CAN's own pair,
/// this family's two blocks are named consistently with their own `_PS`
/// ids -- no naming inconsistency here, like Fault-Tolerant CAN's own pair.
const CHX_BLOCK_BASE_IDS: [u32; 19] = [
    PROTOCOL_J1850VPW_CH1,
    PROTOCOL_J1850PWM_CH1,
    PROTOCOL_ISO9141_CH1,
    PROTOCOL_ISO14230_CH1,
    PROTOCOL_CAN_CH1,
    PROTOCOL_ISO15765_CH1,
    PROTOCOL_J2610_CH1,
    PROTOCOL_GM_UART_CH1,
    PROTOCOL_J1939_CH1,
    PROTOCOL_ECHO_BYTE_CH1,
    PROTOCOL_HONDA_DIAGH_CH1,
    PROTOCOL_J1708_CH1,
    PROTOCOL_TP2_0_CH1,
    PROTOCOL_FT_CAN_CH1,
    PROTOCOL_FT_ISO15765_CH1,
    PROTOCOL_SW_CAN_CAN_CH1,
    PROTOCOL_SW_CAN_ISO15765_CH1,
    PROTOCOL_FD_CAN_CH1,
    PROTOCOL_FD_ISO15765_CH1,
];

/// `true` when `protocol_id` falls anywhere in the full clause-24/26 `_CHx`
/// vocabulary region (`PROTOCOL_CAN_CH1..=PROTOCOL_FD_ISO15765_CH128`), not
/// just the fifteen in-scope families -- mirrors
/// `resources::is_chx_protocol_id`'s role: the mock rejects a `_CHx` id
/// outside the fifteen in-scope blocks with `ERR_NOT_SUPPORTED` rather than
/// silently accepting it as an unrecognized raw id.
fn is_chx_protocol_id(protocol_id: u32) -> bool {
    (PROTOCOL_CAN_CH1..=PROTOCOL_FD_ISO15765_CH128).contains(&protocol_id)
}

/// Decomposes a `_CHx` protocol id inside one of the fifteen in-scope blocks
/// to `(base_protocol_id, channel_index)` (`channel_index` in `1..=128`);
/// `None` when `protocol_id` isn't in any in-scope block (including when
/// it's outside the full `_CHx` region entirely, or inside one of the three
/// remaining out-of-scope blocks -- see `is_chx_protocol_id`).
fn chx_base_protocol_id(protocol_id: u32) -> Option<(u32, u32)> {
    for (idx, &block_base) in CHX_BLOCK_BASE_IDS.iter().enumerate() {
        if protocol_id >= block_base && protocol_id < block_base + 128 {
            return Some((BASE_PROTOCOL_IDS[idx], protocol_id - block_base + 1));
        }
    }
    None
}

/// SAE J2534-2 clause 21 CAN FD (ADR-158/Phase 3a) and clause 22
/// ISO15765-on-CAN-FD (ADR-159/Phase 3b): `true` for either of the two
/// `_PS`-only FD protocol ids this mock simulates. Kept separate from
/// `is_ps_protocol`/`PS_PROTOCOL_IDS` (clause 6 Pin Selection) even though
/// all three need identical pin-assignment gating -- neither FD family has a
/// base id at all (unlike every `_PS` family `PS_PROTOCOL_IDS` covers), and
/// clause 21.3.2.5.1/22.3.2.6.1 layer additional FD-only rules
/// (data-phase-rate-before-pins sequencing, read-only bit-timing params)
/// that don't apply to `_PS`. Clause 22 reuses clause 21's own
/// `CONFIG_FD_CAN_DATA_PHASE_RATE` rate-before-pins mechanism unchanged
/// (ADR-159), so both ids share every check below rather than needing a
/// clause-22-specific mock config. Stays narrow (`_PS`-only) even after
/// ADR-213/Round 3 brought `_CHx` into scope for this family -- see
/// [`is_fd_family_protocol`]'s own doc comment for why, and for the one
/// check that DOES need the family-wide, `_CHx`-inclusive form.
fn is_fd_protocol(protocol_id: u32) -> bool {
    matches!(protocol_id, PROTOCOL_FD_CAN_PS | PROTOCOL_FD_ISO15765_PS)
}

/// SAE J2534-2 clause 21 CAN FD / clause 22 ISO15765-on-CAN-FD (ADR-213/
/// Round 3): `true` for `is_fd_protocol`'s own two `_PS` ids, or any id in
/// either family's own `_CH1..128` Additional Channel range -- mirrors
/// `resources::is_fd_protocol_id`'s widened shape (the service side widens
/// `is_fd_protocol_id` itself in place rather than splitting it, ADR-213
/// Decision item 2; the mock keeps the narrow/family-wide split every other
/// collapse family's own mock predicate uses instead, since unlike the
/// service side, `is_fd_protocol` here has a real narrow-only call site
/// (`ChannelState::new`'s `pins_assigned` gate) that must NOT widen -- a
/// `_CHx` FD channel has no J1962 pin concept and must keep reporting
/// `pins_assigned = true`, the same reason every other family's own narrow
/// predicate stays narrow there). This family-wide predicate exists
/// specifically for the `IOCTL_SET_CONFIG` BIT_SAMPLE_POINT/SYNC_JUMP_WIDTH
/// read-only-parameter rule (clause 21.3.2.5.1/22.3.2.6.1), which -- unlike
/// pin assignment -- does need to apply uniformly across `_PS` and `_CHx`.
fn is_fd_family_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_FD_CAN_PS
        || protocol_id == PROTOCOL_FD_ISO15765_PS
        || (PROTOCOL_FD_CAN_CH1..=PROTOCOL_FD_CAN_CH128).contains(&protocol_id)
        || (PROTOCOL_FD_ISO15765_CH1..=PROTOCOL_FD_ISO15765_CH128).contains(&protocol_id)
}

/// SAE J2534-2 clause 21.3.2.5.1/22.3.2.6.1 (ADR-213/Round 3): `true` when
/// `protocol_id` is specifically an FD `_CHx` id -- `is_fd_family_protocol`
/// minus `is_fd_protocol`'s own `_PS` cases. Unlike a `_PS` FD channel (which
/// binds pins via `CONFIG_J1962_PINS`, itself gated on the data-phase rate
/// already being set -- the existing `IOCTL_SET_CONFIG` ordering check), a
/// `_CHx` FD channel has no pin-selection step at all: each `_CHx` channel is
/// already vendor-pin-preassigned and stays electrically inert until the
/// data-phase rate is set, at which point it attaches. `PassThruReadMsgs`/
/// `PassThruWriteMsgs`/`PassThruStartPeriodicMsg`/`PassThruStartMsgFilter`
/// each consult this (alongside `channel.params.contains_key(&CONFIG_FD_CAN_
/// DATA_PHASE_RATE)`) to reject with `ERR_PIN_INVALID` until that attachment
/// happens -- new mock behavior this round, no prior precedent to mirror
/// (unlike `is_fd_family_protocol` above, which mirrors `is_ft_family_
/// protocol`'s established narrow/family-wide split shape).
fn is_fd_chx_protocol(protocol_id: u32) -> bool {
    is_fd_family_protocol(protocol_id) && !is_fd_protocol(protocol_id)
}

/// SAE J2534-2 clause 9 Single Wire CAN (SWCAN/GMLAN, ADR-164/Phase 4):
/// `true` for either of the two `_PS`-only SWCAN protocol ids this mock
/// simulates. Kept separate from `is_fd_protocol`/`is_ps_protocol` for the
/// same reason `is_fd_protocol` itself is kept separate (see its own doc
/// comment) -- clause 9 defines no unqualified base SWCAN id either, so a
/// channel opened with one of these two ids needs the same
/// pin-unassigned-until-`CONFIG_J1962_PINS` gating `is_ps_protocol`/
/// `is_fd_protocol` already provide, without inheriting either family's own
/// FD-only or `_PS`-only rules (this mock adds no SWCAN-specific SET_CONFIG
/// sequencing rule the way clause 21/22 need one).
fn is_sw_protocol(protocol_id: u32) -> bool {
    matches!(protocol_id, PROTOCOL_SW_CAN_PS | PROTOCOL_SW_ISO15765_PS)
}

/// SAE J2534-2 clause 9 Single Wire CAN (SWCAN/GMLAN, ADR-212/Round 2):
/// `true` for `is_sw_protocol`'s own two `_PS` ids, or any id in either
/// family's own `_CH1..128` Additional Channel range -- mirrors
/// `resources::is_sw_family_protocol_id`'s shape exactly, and
/// [`is_ft_family_protocol`]'s own range-inclusive shape, doubled for this
/// family's two `_PS`/block pairs instead of one. Unlike
/// [`is_ft_family_protocol`] (which has no real call site and stays
/// `#[allow(dead_code)]`), this one has a real, confirmed call site:
/// `IOCTL_SW_CAN_HS`/`IOCTL_SW_CAN_NS`'s own handler, re-keyed from the
/// narrow `is_sw_protocol` so these two IOCTLs still work on a
/// `_CHx`-connected SW-CAN channel. `ChannelState::new`'s `pins_assigned`
/// gate stays on the narrow `is_sw_protocol`, unchanged, matching every
/// other family's identical mock-side convention -- a `_CHx` id has no J1962
/// pin concept.
fn is_sw_family_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_SW_CAN_PS
        || protocol_id == PROTOCOL_SW_ISO15765_PS
        || (PROTOCOL_SW_CAN_CAN_CH1..=PROTOCOL_SW_CAN_CAN_CH128).contains(&protocol_id)
        || (PROTOCOL_SW_CAN_ISO15765_CH1..=PROTOCOL_SW_CAN_ISO15765_CH128).contains(&protocol_id)
}

/// SAE J2534-2 clause 20 Fault-Tolerant CAN (ISO 11898-3, ADR-168/Phase 6):
/// `true` for either of the two `_PS`-only FTCAN protocol ids this mock
/// simulates. Kept separate from `is_fd_protocol`/`is_ps_protocol` for the
/// same reason `is_sw_protocol` itself is kept separate (see its own doc
/// comment) -- clause 20 defines no unqualified base FTCAN id either, so a
/// channel opened with one of these two ids needs the same
/// pin-unassigned-until-`CONFIG_J1962_PINS` gating `is_ps_protocol`/
/// `is_fd_protocol`/`is_sw_protocol` already provide, without inheriting any
/// other family's own FD-only or SWCAN-only rules (this mock adds no
/// FTCAN-specific SET_CONFIG sequencing rule the way clause 21/22 need one,
/// and no LINK_FAULT-specific state machine -- LINK_FAULT is injected via the
/// existing generic RxStatus-injection test helper).
fn is_ft_protocol(protocol_id: u32) -> bool {
    matches!(protocol_id, PROTOCOL_FT_CAN_PS | PROTOCOL_FT_ISO15765_PS)
}

/// SAE J2534-2 clause 20 Fault-Tolerant CAN (ISO 11898-3, ADR-211): `true`
/// for `is_ft_protocol`'s own two `_PS` ids, or any id in either family's own
/// `_CH1..128` Additional Channel range -- mirrors
/// `resources::is_ft_family_protocol_id`'s shape exactly, and
/// [`is_tp2_0_family_protocol`]'s/[`is_gm_uart_protocol`]'s own
/// range-inclusive shape, doubled for this family's two `_PS`/block pairs
/// instead of one. Added for API-shape consistency with every other in-scope
/// family; this round's own direct investigation found no mock call site
/// beyond `is_ft_protocol`'s own narrow `pins_assigned`-gate use (which stays
/// on the narrow predicate, unchanged) that needs family-wide FT treatment --
/// unlike TP2.0's own round, this mock has no FT-specific write-size-limit or
/// broadcast-address validation for this predicate to widen.
#[allow(dead_code)] // no real call site yet, ADR-211: kept for API-shape
// consistency with every other in-scope family's mock predicate, and so a
// future FT-specific mock validation rule (should one ever be added) has
// this ready to use rather than needing to be reinvented.
fn is_ft_family_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_FT_CAN_PS
        || protocol_id == PROTOCOL_FT_ISO15765_PS
        || (PROTOCOL_FT_CAN_CH1..=PROTOCOL_FT_CAN_CH128).contains(&protocol_id)
        || (PROTOCOL_FT_ISO15765_CH1..=PROTOCOL_FT_ISO15765_CH128).contains(&protocol_id)
}

/// SAE J2534-2 clause 12 UART Echo Byte Protocol (ADR-170/Phase 9): `true`
/// for the one `_PS`-only UART Echo Byte protocol id this mock simulates.
/// Kept separate from `is_fd_protocol`/`is_ps_protocol`/`is_sw_protocol`/
/// `is_ft_protocol` for the same reason those are each kept separate (see
/// `is_sw_protocol`'s own doc comment) -- clause 12 defines no unqualified
/// base id either, so a channel opened with this id needs the same
/// pin-unassigned-until-`CONFIG_J1962_PINS` gating, without inheriting any
/// other family's own rules. Unlike SW/FT/FD, this protocol has no CAN-family
/// relationship at all -- `base_protocol_id` below has no arm for it (it
/// self-identifies, matching `j2534-0404-service`'s own
/// `ChannelProtocol::UART_ECHO_BYTE_PS`).
fn is_uart_echo_byte_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_UART_ECHO_BYTE_PS
}

/// SAE J2534-2 clause 13 Honda DIAG-H Protocol (ADR-174/Phase 10): `true`
/// for the one `_PS`-only Honda DIAG-H protocol id this mock simulates.
/// Kept separate from `is_fd_protocol`/`is_ps_protocol`/`is_sw_protocol`/
/// `is_ft_protocol`/`is_uart_echo_byte_protocol` for the same reason those
/// are each kept separate (see `is_sw_protocol`'s own doc comment) -- clause
/// 13 defines no unqualified base id either, so a channel opened with this
/// id needs the same pin-unassigned-until-`CONFIG_J1962_PINS` gating,
/// without inheriting any other family's own rules. Like UART Echo Byte,
/// this protocol has no CAN-family relationship at all -- `base_protocol_id`
/// below has no arm for it (it self-identifies, matching
/// `j2534-0404-service`'s own `ChannelProtocol::HONDA_DIAGH_PS`).
fn is_honda_diagh_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_HONDA_DIAGH_PS
}

/// SAE J2534-2 clause 17 SAE J1708 Protocol (ADR-175/Phase 11): `true` for
/// the one `_PS`-only J1708 protocol id this mock simulates. Kept separate
/// from `is_fd_protocol`/`is_ps_protocol`/`is_sw_protocol`/`is_ft_protocol`/
/// `is_uart_echo_byte_protocol`/`is_honda_diagh_protocol` for the same
/// reason those are each kept separate (see `is_sw_protocol`'s own doc
/// comment) -- clause 17 defines no unqualified base id either, so a channel
/// opened with this id needs the same pin-unassigned-until-
/// `CONFIG_J1962_PINS` gating, without inheriting any other family's own
/// rules. Like UART Echo Byte/Honda DIAG-H, this protocol has no CAN-family
/// relationship at all -- `base_protocol_id` below has no arm for it (it
/// self-identifies, matching `j2534-0404-service`'s own
/// `ChannelProtocol::J1708_PS`).
fn is_j1708_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_J1708_PS
}

/// SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5): `true` for
/// ONLY the exact `_PS` J1939 protocol id, deliberately NOT the
/// `J1939_CH1..128` Additional Channel range ADR-206 later wires up --
/// mirrors `is_gm_uart_protocol`'s own exact-vs-range split (that function's
/// doc comment, and `ChannelState::new`'s `protocol_id != PROTOCOL_GM_UART_PS`
/// exact check just below, for the identical reason): a `_CHx` id (clause
/// 7's vendor-connector model) has no J1962 pin concept at all, so it must
/// NOT be treated as this protocol's `_PS`-only pin-unassigned-until-
/// `CONFIG_J1962_PINS` shape the way this predicate's callers need. Kept
/// separate from the other single-id `is_*_protocol` helpers for the same
/// reason those are each kept separate -- clause 16 defines no unqualified
/// base id either. Like UART Echo Byte/Honda DIAG-H/J1708, this protocol has
/// no CAN-family relationship at all -- `base_protocol_id` below has no arm
/// for it (it self-identifies, matching `j2534-0404-service`'s own
/// `ChannelProtocol::J1939_PS`). For J1939-FAMILY behavior that must apply
/// uniformly regardless of `_PS` vs. `_CHx` (e.g. `PassThruWriteMsgs`'s
/// `ERR_ADDRESS_NOT_CLAIMED` enforcement below), use
/// [`is_j1939_family_protocol`] instead -- Codex review finding, PR #117,
/// after this predicate was originally `_PS`-only-by-construction (no
/// `_CHx` id could reach a live channel before ADR-206 wired
/// `chx_block_base`'s own J1939 entry, so the two purposes never diverged
/// until now).
fn is_j1939_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_J1939_PS
}

/// SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5, `_CHx` range
/// added by ADR-206): `true` for `PROTOCOL_J1939_PS` OR any id in the
/// `PROTOCOL_J1939_CH1..128` Additional Channel range -- mirrors
/// `is_gm_uart_protocol`'s own range-inclusive shape exactly, for the
/// identical reason (Codex review finding, PR #117): a `_CHx` link is still
/// a J1939 link for every family-wide behavioral rule EXCEPT the `_PS`-only
/// pin-gating [`is_j1939_protocol`] (this file's own exact-match predicate,
/// just above) exists for. Currently the only caller is
/// `PassThruWriteMsgs`'s clause 16.5 `ERR_ADDRESS_NOT_CLAIMED` simulation --
/// without this, a J1939 `_CHx` channel silently skipped that enforcement
/// entirely, since [`is_j1939_protocol`] alone never recognized it.
fn is_j1939_family_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_J1939_PS
        || (PROTOCOL_J1939_CH1..=PROTOCOL_J1939_CH128).contains(&protocol_id)
}

/// SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a): `true`
/// only for the exact `_PS` TP2.0 protocol id, deliberately NOT the
/// `TP2_0_CH1..128` Additional Channel range ADR-210 later wires up --
/// mirrors `is_j1939_protocol`'s own exact-vs-range split (that function's
/// doc comment, and `ChannelState::new`'s own exact check just below, for
/// the identical reason): a `_CHx` id (clause 7's vendor-connector model) has
/// no J1962 pin concept at all, so it must NOT be treated as this protocol's
/// `_PS`-only pin-unassigned-until-`CONFIG_J1962_PINS` shape the way this
/// predicate's own pin-gating caller needs. Kept separate from the other
/// single-id `is_*_protocol` helpers for the same reason those are each kept
/// separate -- clause 19 defines no unqualified base id either. Like UART
/// Echo Byte/Honda DIAG-H/J1708/J1939, this protocol has no CAN-family
/// relationship at all -- `base_protocol_id` below has no arm for it (it
/// self-identifies, matching `j2534-0404-service`'s own
/// `ChannelProtocol::TP2_0_PS`). For TP2.0-FAMILY behavior that must apply
/// uniformly regardless of `_PS` vs. `_CHx` (`PassThruWriteMsgs`'s/
/// `PassThruStartPeriodicMsg`'s own connection-bound write-size-limit and
/// broadcast-address-range validation below), use
/// [`is_tp2_0_family_protocol`] instead (ADR-210) -- unlike every prior
/// family's own mock predicate (pin-gating only, correctly narrow), this one
/// is consulted well beyond pin-gating, so widening it in place would be
/// wrong; a separate family-wide predicate is needed instead, mirroring
/// [`is_j1939_family_protocol`]'s own precedent.
fn is_tp2_0_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_TP2_0_PS
}

/// SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a, `_CHx`
/// range added by ADR-210): `true` for `PROTOCOL_TP2_0_PS` OR any id in the
/// `PROTOCOL_TP2_0_CH1..128` Additional Channel range -- mirrors
/// [`is_j1939_family_protocol`]'s own range-inclusive shape exactly, for the
/// identical reason: a `_CHx` link is still a TP2.0 link for every
/// family-wide behavioral rule EXCEPT the `_PS`-only pin-gating
/// [`is_tp2_0_protocol`] (this file's own exact-match predicate, just above)
/// exists for. Callers: `PassThruWriteMsgs`'s/`PassThruStartPeriodicMsg`'s
/// own connection-bound write-size-limit and `CP_TP20BroadcastAddress`
/// range-check validation (four call sites, ADR-210) -- without this, a
/// TP2.0 `_CHx` channel silently skipped that validation entirely, since
/// [`is_tp2_0_protocol`] alone never recognized it.
fn is_tp2_0_family_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_TP2_0_PS
        || (PROTOCOL_TP2_0_CH1..=PROTOCOL_TP2_0_CH128).contains(&protocol_id)
}

/// SAE J2534-2 clause 11 GM UART Protocol (SAE J2740, ADR-189/Phase 8):
/// `true` for `PROTOCOL_GM_UART_PS` or any id in the `GM_UART_CH1..128`
/// range -- mirrors `resources::is_gm_uart_protocol_id`'s own range-inclusive
/// shape (unlike `is_j1708_protocol`/`is_tp2_0_protocol`'s single-value
/// shape), since GM UART is also the first standalone protocol this mock
/// implements `_CHx` Additional Channels for (ADR-189 Decision 2). Like
/// UART Echo Byte/Honda DIAG-H/J1708/J1939/TP2.0, this protocol has no
/// CAN-family relationship at all -- `base_protocol_id` above already
/// normalizes a `_CHx` id in this range to `PROTOCOL_GM_UART_PS` via
/// `chx_base_protocol_id`'s own `CHX_BLOCK_BASE_IDS` entry, so `_PS` itself
/// needs no separate arm there (it self-identifies, matching
/// `j2534-0404-service`'s own `ChannelProtocol::GM_UART_PS`).
fn is_gm_uart_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_GM_UART_PS
        || (PROTOCOL_GM_UART_CH1..=PROTOCOL_GM_UART_CH128).contains(&protocol_id)
}

/// SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): `true` for any of
/// the 32 native, independent, read-only Analog Input protocol ids
/// (`PROTOCOL_ANALOG_IN_1`..`PROTOCOL_ANALOG_IN_32`). Unlike
/// `is_uart_echo_byte_protocol`/`is_honda_diagh_protocol`/`is_j1708_protocol`
/// above, this protocol has no `pins_assigned` gating at all -- clause 10
/// has no pin concept, so a channel opened with one of these ids is treated
/// like an ordinary base-protocol channel for pin purposes (`ChannelState::new`
/// below does not list it among the `_PS`/FD/SW/FT/UART-Echo-Byte/Honda-DIAG-H/
/// J1708 pin-unassigned-until-`CONFIG_J1962_PINS` protocols).
fn is_analog_in_protocol(protocol_id: u32) -> bool {
    (PROTOCOL_ANALOG_IN_1..=PROTOCOL_ANALOG_IN_32).contains(&protocol_id)
}

/// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): `true` only for
/// `PROTOCOL_ETHERNET_NDIS`. Like [`is_analog_in_protocol`], this protocol
/// has no `pins_assigned` gating at all -- clause 24 has no `_PS`/`_CHx`
/// pin-selection mechanics (pin usage is chosen by connect flag, not
/// `CONFIG_J1962_PINS`), so `ChannelState::new` below does not list it among
/// the pin-unassigned-until-`CONFIG_J1962_PINS` protocols either.
fn is_ethernet_ndis_protocol(protocol_id: u32) -> bool {
    protocol_id == PROTOCOL_ETHERNET_NDIS
}

fn base_protocol_id(protocol_id: u32) -> u32 {
    if protocol_id == PROTOCOL_FD_CAN_PS {
        return PROTOCOL_CAN;
    }
    if protocol_id == PROTOCOL_FD_ISO15765_PS {
        return PROTOCOL_ISO15765;
    }
    // ADR-164/Phase 4: same reasoning as the two FD arms just above -- an SW
    // resource row's own `hw_protocol_override` IS the raw `_PS` id
    // directly, so this mock's own Plane-B-equivalent call sites (the
    // K-line-only IOCTL gates, IOCTL_GET_PROTOCOL_INFO's validity check,
    // CONFIG_CAN_MIXED_FORMAT's ISO15765-family gate) treat an SW channel as
    // its base CAN/ISO15765 family for free.
    if protocol_id == PROTOCOL_SW_CAN_PS {
        return PROTOCOL_CAN;
    }
    if protocol_id == PROTOCOL_SW_ISO15765_PS {
        return PROTOCOL_ISO15765;
    }
    // ADR-168/Phase 6: the FT_CAN_PS/FT_ISO15765_PS analog of the two SW arms
    // just above -- an FT resource row's own `hw_protocol_override` IS the
    // raw `_PS` id directly (clause 20 defines no unqualified base FTCAN id
    // either), so this mock's own Plane-B-equivalent call sites treat an FT
    // channel as its base CAN/ISO15765 family for free, the same way an SW
    // channel already does.
    if protocol_id == PROTOCOL_FT_CAN_PS {
        return PROTOCOL_CAN;
    }
    if protocol_id == PROTOCOL_FT_ISO15765_PS {
        return PROTOCOL_ISO15765;
    }
    if let Some(idx) = PS_PROTOCOL_IDS.iter().position(|&id| id == protocol_id) {
        return BASE_PROTOCOL_IDS[idx];
    }
    // ADR-211 Decision item 2: recurse the extracted base back through
    // `base_protocol_id` itself rather than returning it raw. Strict no-op
    // for every one of the 13 families ADR-206 through ADR-210 closed (each
    // `_PS`/base id `CHX_BLOCK_BASE_IDS`/`BASE_PROTOCOL_IDS` are keyed on is
    // already a self-referential fixed point of `base_protocol_id`) but
    // REQUIRED for Fault-Tolerant CAN (ADR-211): `CHX_BLOCK_BASE_IDS` keys
    // `FT_CAN_CH1..128` off `PROTOCOL_FT_CAN_PS`, not the true base `CAN`, so
    // without this recursion `base_protocol_id(FT_CAN_CH3)` would incorrectly
    // return `PROTOCOL_FT_CAN_PS` instead of `CAN`.
    if let Some((base, _index)) = chx_base_protocol_id(protocol_id) {
        return base_protocol_id(base);
    }
    protocol_id
}

/// The largest `RepeatMsgData[0]` `DataSize` (in bytes) this mock accepts
/// for `IOCTL_START_REPEAT_MESSAGE` on a channel whose base protocol id
/// (`base_protocol_id`) is `base_id` -- ADR-186's SAE J2534-1 v04.04
/// §7.2.7 flat periodic-message cap, narrowed to 11 for ISO15765 by clause
/// 22.2.2(h) and to 10 for J1850PWM by `protocol.rs`'s own
/// `tx_message_size_range` (`3..=10`, below the flat 12-byte ceiling).
/// This is the enforcement cap `IOCTL_START_REPEAT_MESSAGE` applies
/// regardless of protocol id -- including `_PS`/`_CHx` variants (e.g.
/// `PROTOCOL_J1939_PS`, `PROTOCOL_UART_ECHO_BYTE_PS`) that
/// `IOCTL_GET_PROTOCOL_INFO`'s own allow-list rejects outright and so
/// never get a discovery answer from this function at all. Where a
/// discovery answer DOES exist, it is sourced from this same function
/// (`IOCTL_GET_PROTOCOL_INFO`'s `PROTOCOL_INFO_MAX_REPEAT_MESSAGING_LENGTH`
/// arm), so the two numbers cannot drift apart the way a second
/// hand-copied table would risk (Codex review finding, PR #95).
fn max_repeat_messaging_length(base_id: u32) -> u32 {
    match base_id {
        PROTOCOL_ISO15765 => 11,
        // SAE J2534-1 v04.04 Table (clause 6.4's TX message size table, as
        // codified by `protocol.rs`'s `ChannelProtocol::tx_message_size_range`)
        // caps ordinary J1850PWM TX at 10 bytes (`3..=10`), below the flat
        // 12-byte §7.2.7 periodic cap every other protocol here falls back
        // to -- so the periodic cap is really `min(10, 12) = 10` for this
        // protocol specifically. Every OTHER protocol this function
        // enforces against (J1850VPW, ISO9141, ISO14230/KWP, the four SCI
        // variants, J1939, and the remaining SAE J2534-2 `_PS` protocols
        // reachable here) has its own natural `tx_message_size_range` upper
        // bound at or above 12, so the flat `_ => 12` arm already produces
        // the correct `min(natural_max, 12)` value for all of them without
        // needing a dedicated arm (verified against `protocol.rs` directly,
        // edge-case-hunter review, PR #95 follow-up).
        PROTOCOL_J1850PWM => 10,
        _ => 12,
    }
}

/// SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 4/Phase 2b)
/// capacity this mock reports via `DEVICE_INFO_<PROTOCOL>_SUPPORTED`'s
/// packed value for each of the seven in-scope families, and enforces at
/// `PassThruConnect` time -- a reasonable-for-testing default, overridable
/// per test via `__mock_set_chx_capacity`.
const DEFAULT_CHX_CAPACITY: u32 = 4;

/// ADR-219 Decision item 2: two dedicated vendor-range (`>= 0x10000`)
/// `IoctlID`s this mock recognizes, one per header mode, so
/// `j2534-0404-service`'s vendor IOCTL passthrough dispatch has something
/// concrete to round-trip through `tests/grpc_mock` (PDU_IOCTL_BASE et al.
/// use `0x2900_0000+`, well clear of both). `pInput`/`pOutput` are a direct
/// `u32 *` pair -- matching the ADR's own "e.g. `u32 *`" raw-mode example.
/// The mock echoes `input + 1` (or `1` with no input) so a test can tell the
/// value actually round-tripped through this native call, not just an
/// artifact of client-side plumbing.
pub const MOCK_VENDOR_IOCTL_RAW_U32: u32 = 0x0001_0001;
/// ADR-219 Decision item 2's wrapped-mode counterpart to
/// [`MOCK_VENDOR_IOCTL_RAW_U32`]: `pInput`/`pOutput` are each an
/// `SBYTE_ARRAY`. The mock echoes the input bytes reversed (truncated to
/// `pOutput`'s own capacity, self-reporting the actual echoed length via
/// `NumOfBytes` -- the ADR's "wrapped mode self-reports its own length"
/// convention) -- reversal, not a plain copy, so a test can tell the value
/// actually round-tripped through this native call.
pub const MOCK_VENDOR_IOCTL_WRAPPED_ECHO: u32 = 0x0001_0002;

const MOCK_DEVICE_ID: u32 = 1;
const MOCK_VBATT_MV: u32 = 12000;
const MOCK_PROG_VOLTAGE_MV: u32 = 0;
// SAE J2534-2 clause 23 J1962 Pin Voltage Read (Phase 13): the fixed
// millivolt reading `IOCTL_READ_J1962PIN_VOLTAGE` reports for any supported
// pin other than pin 16 (which must match `MOCK_VBATT_MV`, per spec) -- a
// plausible nominal value for a 5V sensor-type pin.
const MOCK_J1962_PIN_VOLTAGE_MV: u32 = 5000;
const MOCK_TIMESTAMP: u32 = 4242;
// SAE J2534-2 clause 10.3.3.2 Analog Inputs (ADR-177/Phase 15): the fixed
// synthetic millivolt reading `PassThruReadMsgs` queues once a channel's
// sample rate is armed -- any deterministic, test-friendly value is fine
// per the ADR's own Decision, so a plausible nominal 5V-sensor-range value
// is used, mirroring `MOCK_J1962_PIN_VOLTAGE_MV`'s own convention.
const MOCK_ANALOG_READING_MV: i32 = 2_500;

// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): a fixed,
// deterministic test fixture `IOCTL_GET_NDIS_ADAPTER_INFO` reports on every
// call -- these are arbitrary-but-plausible values chosen for test
// observability, not sourced from any spec table (clause 24 defines no
// canned adapter identity).
const MOCK_NDIS_ADAPTER_UNIQUE_ID: &[u8] = b"MOCK-NDIS-ADAPTER-0001";
const MOCK_NDIS_ADAPTER_NAME: &[u8] = b"eth0";
const MOCK_NDIS_STATUS: u32 = 1;
const MOCK_NDIS_MAC_ADDRESS: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const MOCK_NDIS_IPV6_ADDRESS: [u8; 16] = [
    0xFE, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];
const MOCK_NDIS_IPV4_ADDRESS: [u8; 4] = [192, 168, 7, 1];
const MOCK_NDIS_ETHERNET_PIN_CONFIG: u32 = 1;

/// Builds the fixed [`MOCK_NDIS_ADAPTER_UNIQUE_ID`]-etc. canned
/// `NDIS_ADAPTER_INFORMATION` struct [`IOCTL_GET_NDIS_ADAPTER_INFO`] writes
/// to `pOutput` -- `AdapterUniqueID`/`AdapterName` are null-padded fixed-size
/// `c_char` arrays, per the native struct's own char-array semantics.
fn mock_ndis_adapter_info() -> NDIS_ADAPTER_INFORMATION {
    let mut adapter_unique_id = [0 as c_char; 128];
    for (dst, &src) in adapter_unique_id
        .iter_mut()
        .zip(MOCK_NDIS_ADAPTER_UNIQUE_ID)
    {
        *dst = src as c_char;
    }
    let mut adapter_name = [0 as c_char; 64];
    for (dst, &src) in adapter_name.iter_mut().zip(MOCK_NDIS_ADAPTER_NAME) {
        *dst = src as c_char;
    }
    NDIS_ADAPTER_INFORMATION {
        AdapterUniqueID: adapter_unique_id,
        AdapterName: adapter_name,
        Status: MOCK_NDIS_STATUS,
        MAC_Address: MOCK_NDIS_MAC_ADDRESS,
        IPV6_Address: MOCK_NDIS_IPV6_ADDRESS,
        IPV4_Address: MOCK_NDIS_IPV4_ADDRESS,
        EthernetPinConfig: MOCK_NDIS_ETHERNET_PIN_CONFIG,
    }
}

// Per-channel, per-type (PASS_FILTER/BLOCK_FILTER) cap enforced by
// `PassThruStartMsgFilter`, matching the value `IOCTL_GET_PROTOCOL_INFO`
// advertises via `PROTOCOL_INFO_MAX_PASS_FILTER`/`MAX_BLOCK_FILTER` (ADR-153).
const MAX_FILTERS_PER_TYPE: usize = 10;

// Canned K-line 5-baud-init response: 3 keyword bytes (ISO9141 style).
const MOCK_INIT_RESPONSE: &[u8] = &[0x55, 0x8F, 0xEA];

// Baud rate simulated as "negotiated during the 5-baud init sequence" -- the
// standard K-line rate a real adapter settles on after 5-baud init.
// `IOCTL_FIVE_BAUD_INIT` seeds `CONFIG_DATA_RATE` in the channel's params map
// to this value, so a subsequent `IOCTL_GET_CONFIG(CONFIG_DATA_RATE)` returns
// it, matching a real adapter's post-init baud readback (ADR-076).
const MOCK_FIVE_BAUD_NEGOTIATED_BAUD: u32 = 10_400;

// Canned K-line fast-init response: an ISO14230 StartCommunication positive
// response (fmt 0x83 = addressed + embedded LEN 3, target 0xF1, source 0x10,
// payload C1 E9 8F -- no checksum byte, same vendor-managed-checksum
// assumption as every other header/footer construction in this service, see
// `tx_header::kwp_header_bytes`). Distinct from `MOCK_INIT_RESPONSE` so
// FAST_INIT and FIVE_BAUD_INIT round-trip tests can each assert their own
// canned bytes (ADR-075).
const MOCK_FAST_INIT_RESPONSE: &[u8] = &[0x83, 0xF1, 0x10, 0xC1, 0xE9, 0x8F];

// Canned SAE J1850 OBD-II positive-response frames (Mode 41 PID 00) queued by
// the `j1850_bus_flavor` knob: header (format/priority, functional-response
// target 0x6B, source ECU address 0x10) + response payload `41 00` (echoed
// mode/PID) + `BE 1F A8 13`, the real 4-byte supported-PID bitmap Mode 01
// PID 00 always carries -- not padding, so all 4 bytes are kept (Codex
// review, PR #58 round 4: an earlier revision here dropped the 4th bitmap
// byte, mistaking it for a fabricated J1850 CRC). The auto-detect probe
// itself only checks for *any* decoded RX within its window, not specific
// bytes, but a genuinely well-formed OBD frame keeps the mock's simulated
// traffic self-documenting regardless. ADR-171: no trailing CRC byte on top
// of this -- J1850's wire checksum is verified/stripped by the interface
// before delivery, so the delivered frame is header + payload only (default
// `extra_data_index: None`, i.e. `ExtraDataIndex == DataSize`, no IFR
// bytes).
const MOCK_J1850_VPW_RESPONSE: &[u8] = &[0x48, 0x6B, 0x10, 0x41, 0x00, 0xBE, 0x1F, 0xA8, 0x13];
const MOCK_J1850_PWM_RESPONSE: &[u8] = &[0x41, 0x6B, 0x10, 0x41, 0x00, 0xBE, 0x1F, 0xA8, 0x13];

const FIRMWARE_VERSION: &[u8] = b"1.0.0\0";
const DLL_VERSION: &[u8] = b"1.0.0\0";
const API_VERSION: &[u8] = b"04.04\0";
const VERSION_BUF_SIZE: usize = 80;

// ── Stored message (compact, heap-allocated) ─────────────────────────────────

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub protocol_id: u32,
    pub rx_status: u32,
    pub tx_flags: u32,
    pub timestamp: u32,
    pub data: Vec<u8>,
    /// ADR-171: native `ExtraDataIndex` (offset within `data` where trailing
    /// IFR bytes begin), `None` meaning "no extra bytes" -- the correct
    /// default for every `from_passthru`-converted/client-written message,
    /// since `ExtraDataIndex` is don't-care there. `write_to_passthru`
    /// reports `unwrap_or(DataSize)` for this case.
    pub extra_data_index: Option<u32>,
}

impl StoredMessage {
    fn from_passthru(msg: &PASSTHRU_MSG) -> Self {
        let len = (msg.DataSize as usize).min(msg.Data.len());
        StoredMessage {
            protocol_id: msg.ProtocolID,
            rx_status: msg.RxStatus,
            tx_flags: msg.TxFlags,
            timestamp: msg.Timestamp,
            data: msg.Data[..len].to_vec(),
            extra_data_index: None,
        }
    }

    fn write_to_passthru(&self, dst: &mut PASSTHRU_MSG) {
        dst.ProtocolID = self.protocol_id;
        dst.RxStatus = self.rx_status;
        dst.TxFlags = 0;
        dst.Timestamp = self.timestamp;
        let len = self.data.len().min(dst.Data.len());
        dst.Data[..len].copy_from_slice(&self.data[..len]);
        dst.DataSize = len as u32;
        // ADR-171: deliberately NOT clamped to `DataSize` here -- the mock
        // must be able to report an out-of-range `ExtraDataIndex` so a test
        // can exercise the service-side defensive clamp
        // (`header_footer_len`'s J1850 arm) as the actual trust boundary.
        dst.ExtraDataIndex = self.extra_data_index.unwrap_or(len as u32);
    }
}

/// A message filter installed via `PassThruStartMsgFilter`, as recorded by the mock.
/// `mask` / `pattern` / `flow_control` are stored in full (including `TxFlags`,
/// e.g. `CAN_29BIT_ID` / `ISO15765_ADDR_TYPE`) since callers building extended or
/// 29-bit-CAN-Id ISO15765 filters set flags independently on each message.
#[derive(Debug, Clone)]
pub struct MockFilter {
    pub filter_type: u32,
    pub mask: StoredMessage,
    pub pattern: StoredMessage,
    pub flow_control: Option<StoredMessage>,
}

/// SAE J2534-2 clause 14 Repeat Messaging slot (ADR-165/Phase 12), created by
/// `IOCTL_START_REPEAT_MESSAGE` and referenced by `MsgId` from
/// `IOCTL_QUERY_REPEAT_MESSAGE`/`IOCTL_STOP_REPEAT_MESSAGE`. A real device
/// runs its own retransmission timer and, for `Condition == 1`, evaluates
/// incoming frames against `mask`/`pattern` itself -- this mock drives the
/// same behavior with a background thread per slot (`spawn_repeat_worker`),
/// since this crate has no existing shared tick-loop/async runtime to
/// piggyback on (every other IOCTL here is synchronous FFI dispatch); a
/// thread per slot mirrors the `std::thread::sleep`-based timing this crate
/// already uses for `__mock_arm_write_rx_injection`'s `hold_ms` delay.
#[derive(Debug, Clone)]
struct RepeatSlot {
    time_interval_ms: u32,
    condition: u32,
    message: StoredMessage,
    mask: Vec<u8>,
    pattern: Vec<u8>,
    /// The RX-direction equivalent (`RxStatus` space) of the `TxFlags` SAE
    /// J2534-2 clause 14's REPEAT_MSG_SETUP carries on `RepeatMsgData[1]`/
    /// `[2]` -- the service always sets the identical value on both (round 2
    /// Finding 2), so only one is needed here. Only the CAN-ID-width/
    /// ISO15765-addressing-type bits (`TX_FLAG_CAN_29BIT_ID`/
    /// `TX_FLAG_ISO15765_ADDR_TYPE`, the wrapper crate's
    /// `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE`) are objective wire-format facts
    /// a real device would use to distinguish an incoming frame's actual
    /// format from what the mask/pattern's `Data` bytes alone cannot encode
    /// (both an 11-bit and a 29-bit CAN ID produce an identical 4-byte
    /// `Data` header for a coincidentally-equal numeric value, per
    /// `tx_header::can_header_bytes`) -- Codex review, ADR-165 PR #42 round
    /// 9. Populated at slot-creation time by
    /// [`tx_format_flags_to_rx_status_bits`], translating the setup's
    /// TX-direction `TxFlags` into the RX-direction `RxStatus` bits a
    /// genuinely-received frame with that wire format would actually report
    /// -- round 9's original check compared this against the incoming
    /// frame's `TxFlags` (which `StoredMessage::write_to_passthru` always
    /// zeroes for RX-direction messages, so the check never actually fired
    /// as intended), corrected to compare against `RxStatus` instead
    /// (Codex review, ADR-165 PR #42 round 14).
    response_format_rx_bits: u32,
    /// The mask/pattern's `ProtocolID` (`mask.protocol_id`/`pattern.
    /// protocol_id` -- the service always sets the identical value on both,
    /// same as `response_format_rx_bits`, so only one is needed here). A
    /// native-mixed CAN channel (`CanChannelMode::NativeMixed`, ADR-160/162)
    /// can carry both raw-CAN and ISO15765 frames on one physical channel,
    /// distinguished only by each incoming frame's own `ProtocolID` -- the
    /// byte-level `Data` match and `response_format_rx_bits` check alone
    /// cannot tell a raw-CAN frame from an ISO15765 one when their wire bytes
    /// coincide, so this is compared as a third, independent condition.
    /// Codex review, ADR-165 PR #42 round 10.
    response_protocol_id: u32,
    /// Test-introspection-only: the mask/pattern `PassThruMessage`s' raw,
    /// UNMASKED native `TxFlags` (`mask.tx_flags`/`pattern.tx_flags`, which
    /// the service always sets identically -- same rationale as
    /// `response_protocol_id` above), captured verbatim as staged in the
    /// `IOCTL_START_REPEAT_MESSAGE` call. Distinct from
    /// `response_format_rx_bits` above (which is a derived, RX-direction,
    /// spec-comparison-masked value never intended to carry e.g. FD format
    /// bits) -- this field exists solely so
    /// `__mock_get_repeat_slot_mask_pattern_tx_flags` can let a test directly
    /// confirm what the service actually staged on the mask/pattern
    /// templates themselves (Codex review, ADR-165 PR #42 round 15), which
    /// no other accessor exposes.
    mask_pattern_tx_flags: u32,
    /// The terminal state (ADR-173 Decision 1/2), set either by
    /// [`note_rx_frame_for_repeat_slots`] on a per-frame trigger (a matching
    /// frame for `condition == 0`, a non-matching frame for `condition ==
    /// 1`) or by [`advance_repeat_slot_windows`] on a `condition == 1`
    /// silence stop-trigger (a whole grid window elapsing with no eligible
    /// received frame) -- called from `note_rx_frame_for_repeat_slots`,
    /// `spawn_repeat_worker`, and the `IOCTL_QUERY_REPEAT_MESSAGE` handler
    /// (round 4 fix, ADR-173 Decision 7), so any of the three can observe
    /// and close a silent window (round 3 grid-anchored fix, ADR-173
    /// Decision 6, superseding the round-1/round-2 `interval_deadline`-gate/
    /// `carry_over_rx_seen` patches). A terminated slot is retained, not
    /// removed, until an explicit `IOCTL_STOP_REPEAT_MESSAGE` -- clause
    /// 14.2.2.3 requires `MsgId` to survive self-termination, and the QUERY
    /// handler reports `1` while this is `false`, `0` once it becomes
    /// `true`.
    terminated: bool,
    /// Whether an eligible frame (match or non-match, but never an
    /// ineligible frame per `REPEAT_INELIGIBLE_RX_STATUS_MASK`) has been
    /// received during the single grid window `interval_deadline` currently
    /// tracks. Ownership of resetting/consulting this field belongs entirely
    /// to [`advance_repeat_slot_windows`] (round 3 fix) -- neither
    /// `spawn_repeat_worker` nor `note_rx_frame_for_repeat_slots` resets it
    /// directly any more. Consulted only by `condition == 1`: a window
    /// closing with this still `false` (silence) terminates the slot
    /// (ADR-173 Decision 1). Irrelevant to `condition == 0`, which never
    /// self-terminates on silence.
    rx_seen_this_interval: bool,
    /// The end instant of the EARLIEST grid window that could still receive
    /// credit -- `None` means the grid hasn't started yet (the worker has
    /// not transmitted for the first time). Round 3 grid-anchored fix
    /// (ADR-173, Codex review PR #60 round 3, design-advisor consult):
    /// rounds 1/2 recomputed this as `Instant::now() + interval` at every
    /// transmit, which re-anchors the grid to whenever the worker happens to
    /// actually wake -- a worker delayed by more than one full
    /// `TimeInterval` (e.g. a severe scheduler stall) then silently skips
    /// every intervening interval's silence check instead of evaluating it.
    /// [`advance_repeat_slot_windows`] is the single routine that advances
    /// this field, by whole `TimeInterval` steps from wherever it already
    /// is, never by re-anchoring to `now` -- see its own doc comment for the
    /// full invariant. `None` also fixes a latent bug the `Instant::now()`
    /// sentinel this field previously used at slot-creation time had: a
    /// frame arriving before the worker's very first transmit must credit
    /// no window (ADR-173's "current interval" has not logically started
    /// yet), which `Option::None` makes the natural, self-documenting state
    /// for, instead of relying on an "already elapsed" sentinel value.
    interval_deadline: Option<std::time::Instant>,
}

/// SAE J2534-2 clause 14's own mask/pattern evaluation rule (ADR-165
/// Context/Decision 6): a `data` frame shorter than the compared
/// mask/pattern prefix never matches; bytes beyond it are don't-care.
/// Deliberately NOT `ExpectedResponse::matches`-style leniency (that
/// service-side comparison silently truncates to the shortest of mask/
/// pattern/data instead of failing outright on a too-short frame) -- this
/// phase's own short-frame rule is stricter on purpose.
///
/// Codex-review Finding 3 (PR #42, round 14, defense in depth) originally
/// claimed that using `.max()`, not `.min()`, of `mask`/`pattern`'s lengths
/// for `cmp_len` alone routed any mask/pattern length mismatch through the
/// `data.len() < cmp_len` short-frame check above. Codex review round 20
/// showed this was WRONG: when `mask.len() != pattern.len()`, the SHORTER
/// array's missing tail bytes default to `0` via `.unwrap_or(0)` in the loop
/// below rather than being rejected outright, so a `data` byte that happens
/// to be `0` after masking can spuriously "match" a position the shorter
/// array never actually specified (counter-example: `mask = [0xFF, 0xFF]`,
/// `pattern = [0xAA]`, `data = [0xAA, 0x00]` incorrectly matched under the
/// old code). The service caller (`ioctl_start_repeat_message`) already
/// rejects a mismatched-length mask_data/pattern_data before composing the
/// native call, so `mask`/`pattern` are always equal length in practice via
/// that path -- but this function is defense-in-depth for any other caller
/// of this native IOCTL, so the length-equality check below is now explicit
/// rather than relying on `.max()`/short-frame-rejection alone. Once lengths
/// are known equal, `.max()` and `.min()` are equivalent for `cmp_len`;
/// `.max()` is kept only because it was already there, not because it adds
/// any guarantee beyond the explicit check.
fn repeat_mask_pattern_matches(mask: &[u8], pattern: &[u8], data: &[u8]) -> bool {
    if mask.len() != pattern.len() {
        return false;
    }
    let cmp_len = mask.len().max(pattern.len());
    if data.len() < cmp_len {
        return false;
    }
    (0..cmp_len).all(|i| {
        let mask_byte = mask.get(i).copied().unwrap_or(0);
        let pattern_byte = pattern.get(i).copied().unwrap_or(0);
        (data[i] & mask_byte) == pattern_byte
    })
}

/// Translates SAE J2534-2 clause 14's REPEAT_MSG_SETUP `TxFlags` bits
/// (RepeatMsgData[1]/[2], TX-direction) into the equivalent RX-direction
/// RxStatus bits (SAE J2534-1 Figure 43/45) a genuinely-received frame
/// with that wire format would report -- the two fields' bit VALUES
/// happen to coincide (0x100/0x80) but are semantically distinct fields
/// for opposite directions; map explicitly rather than relying on the
/// numeric coincidence (Codex review, ADR-165 PR #42 round 14).
///
/// Also translates the FD format/BRS bits (SAE J2534-1 Table 92's RX-side
/// additions) as of round 15, so a genuinely-received/echoed FD frame
/// reports honest `RxStatus` FD bits (RX-side echo fidelity, e.g. the
/// `PassThruWriteMsgs` loopback-echo call site below). This is NOT because
/// these bits participate in repeat-slot response matching -- SAE J2534-2
/// 21.2.2(g)/22.2.2(d) require FD-capable-channel filtering to ignore CAN
/// message format entirely, so `IOCTL_START_REPEAT_MESSAGE`'s slot-creation
/// site masks these bits back out of the value it stores for comparison
/// (see `REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK` and the masking step where
/// `RepeatSlot::response_format_rx_bits` is set).
fn tx_format_flags_to_rx_status_bits(tx_flags: u32) -> u32 {
    let mut bits = 0u32;
    if tx_flags & j2534_0404_sys::bindings::TX_FLAG_CAN_29BIT_ID != 0 {
        bits |= j2534_0404_sys::bindings::RX_FLAG_CAN_29BIT_ID;
    }
    if tx_flags & j2534_0404_sys::bindings::TX_FLAG_ISO15765_ADDR_TYPE != 0 {
        bits |= j2534_0404_sys::bindings::RX_FLAG_ISO15765_ADDR_TYPE;
    }
    if tx_flags & j2534_0404_sys::bindings::TX_FLAG_FD_CAN_FORMAT != 0 {
        bits |= j2534_0404_sys::bindings::RX_FLAG_FD_CAN_FORMAT;
    }
    if tx_flags & j2534_0404_sys::bindings::TX_FLAG_FD_CAN_BRS != 0 {
        bits |= j2534_0404_sys::bindings::RX_FLAG_FD_CAN_BRS;
    }
    bits
}

/// Wire-format bits objectively distinguishable from `Data` bytes alone
/// (Codex review, ADR-165 PR #42 round 9): CAN ID width (11-bit vs 29-bit)
/// and ISO15765 addressing type (normal vs extended) are carried only in
/// `RxStatus` for a genuinely-received frame, never in `Data` --
/// `tx_header::can_header_bytes` always encodes the numeric CAN ID as the
/// same 4-byte big-endian representation regardless of which of these an
/// incoming frame actually used, so a coincidentally-equal numeric ID can
/// produce byte-for-byte identical `Data` for two frames of genuinely
/// different wire format. Round 9 originally compared this against the
/// incoming frame's `TxFlags`, which `StoredMessage::write_to_passthru`
/// always zeroes for RX-direction messages -- corrected to `RxStatus`,
/// the field SAE J2534-1 actually reports these bits on for a received
/// frame (Codex review, ADR-165 PR #42 round 14). See
/// `RepeatSlot::response_format_rx_bits`'s doc comment for the full
/// rationale.
///
/// FD format/BRS (and ESI) are deliberately EXCLUDED from this mask (Codex
/// review, ADR-165 PR #42 round 15): SAE J2534-2 21.2.2(g)/22.2.2(d) require
/// FD-capable-channel filtering/matching to ignore CAN message format --
/// two frames with identical address+data are the same message whether sent
/// as classic or FD, so comparing these bits would violate the spec.
/// `tx_format_flags_to_rx_status_bits` now translates them (for RX-echo
/// fidelity only), but the value stored on `RepeatSlot::response_format_rx_bits`
/// is masked to this constant at slot-creation time specifically so that
/// translation can never leak an FD bit into the comparison.
const REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK: u32 = j2534_0404_sys::bindings::RX_FLAG_CAN_29BIT_ID
    | j2534_0404_sys::bindings::RX_FLAG_ISO15765_ADDR_TYPE;

/// SAE J2534-1's `RxStatus` bit 0 (set on a frame that is the interface's
/// own transmission looped back) -- not exported by `j2534_0404_sys::bindings`:
/// `j2534-0404-sys/build.rs`'s `allowlist_var` regex only captures
/// `RX_FLAG_.*`/`TX_FLAG_.*`-prefixed names, and the header's
/// `RX_TX_MSG_TYPE` `#define` doesn't match that pattern, so bindgen never
/// generated it. Defined locally with the same numeric value the header
/// documents, rather than widening the allowlist regex and regenerating
/// bindings (out of scope for this fix -- regenerating the
/// bindings for every target is a separate step) -- mirrors this crate's own pre-round-14
/// code, which used the identical bare `0x01` literal at this bit's one
/// call site for the same reason.
const RX_TX_MSG_TYPE: u32 = 0x0000_0001;

/// SAE J2534-2 clause 14's stop condition evaluates *received* messages
/// only (spec clause 14) -- a loopback echo of the device's own
/// transmission (RxStatus bit 0, SAE J2534-1 Figure 43) or an
/// indication frame (TX-done, start-of-message, break) is not a message
/// received from another node and must never satisfy a repeat slot's
/// mask/pattern match, even if its bytes coincidentally match (Codex
/// review, ADR-165 PR #42 round 14).
const REPEAT_INELIGIBLE_RX_STATUS_MASK: u32 = RX_TX_MSG_TYPE
    | j2534_0404_sys::bindings::RX_FLAG_START_OF_MESSAGE
    | j2534_0404_sys::bindings::RX_FLAG_BREAK
    | j2534_0404_sys::bindings::RX_FLAG_TX_INDICATION;

/// Invariant: `interval_deadline`, when `Some`, is always the end of the
/// EARLIEST window that could still receive credit -- every earlier window
/// has already been closed and (for `condition == 1`) had its silence
/// stop-trigger evaluated. `rx_seen_this_interval` refers only to that one
/// stored window. Advancing the grid by whole `TimeInterval` steps (rather
/// than re-anchoring to `Instant::now()` at each call) means stop-condition
/// evaluation stays exact regardless of how late any thread notices a
/// boundary has passed -- a `Condition == 1` slot cannot survive a
/// scheduler stall spanning multiple intervals, since every silent window
/// within the stall is still closed and evaluated here, not skipped (round
/// 3 grid-anchored fix, ADR-173, superseding the round-1/round-2
/// `interval_deadline`-gate/`carry_over_rx_seen` patches).
///
/// Depends on this file's existing invariant that every frame reaching
/// `channel.rx_queue` is routed through `note_rx_frame_for_repeat_slots`
/// (see that function's own doc comment for the enumerated call sites) --
/// if some future RX path ever bypassed it, a window this function treats
/// as "closed and silent" could actually have had traffic it never saw.
///
/// Must only ever be called while `state()`'s lock is held -- all three
/// existing call sites (`note_rx_frame_for_repeat_slots`,
/// `spawn_repeat_worker`, and the `IOCTL_QUERY_REPEAT_MESSAGE` handler,
/// round 4 fix, ADR-173 Decision 7) already hold it. A QUERY landing before
/// either of the other two next notices an elapsed deadline must advance
/// the windows itself, or it reports a stale `terminated` snapshot -- see
/// the QUERY handler's own comment.
fn advance_repeat_slot_windows(slot: &mut RepeatSlot, now: std::time::Instant) {
    let Some(mut d) = slot.interval_deadline else {
        return; // grid hasn't started yet (no transmit has happened)
    };
    let interval = std::time::Duration::from_millis(slot.time_interval_ms.max(1) as u64);
    if now < d {
        return; // still within the current window, nothing to close
    }
    // Close the current window at its own grid boundary `d`.
    if slot.condition == 1 && !slot.rx_seen_this_interval {
        slot.terminated = true;
        return;
    }
    slot.rx_seen_this_interval = false;
    d += interval;
    if now >= d {
        // One or more FURTHER full windows already elapsed. None of them can
        // have been credited: any eligible frame inside one would have called
        // this same function first (via note_rx_frame_for_repeat_slots) and
        // advanced `d` past it already -- so every one of them was genuinely
        // silent. `Condition == 1` terminates on the first of them; `Condition
        // == 0` never silence-terminates, so just fast-forward the grid past
        // `now` in O(1) rather than looping per boundary.
        if slot.condition == 1 {
            slot.terminated = true;
            return;
        }
        let elapsed_ms = (now - d).as_millis();
        let interval_ms = interval.as_millis().max(1);
        let missed = elapsed_ms / interval_ms + 1;
        let missed = u32::try_from(missed).unwrap_or(u32::MAX);
        d += interval * missed;
    }
    slot.interval_deadline = Some(d);
}

/// Evaluates `data` against every still-live (non-terminated) repeat slot on
/// `channel`, regardless of `condition` (ADR-173 Decision 1/2 -- corrects
/// the prior `condition == 1`-only gate, which left `condition == 0` never
/// evaluating received traffic at all). Returns early, before any slot is
/// evaluated, when `rx_status` carries any of
/// [`REPEAT_INELIGIBLE_RX_STATUS_MASK`]'s bits -- a loopback echo or other
/// indication frame is not a message received from another node, so SAE
/// J2534-2 clause 14's stop condition can never be satisfied by one, no
/// matter how well its `Data` bytes happen to match (round 14 fix, Codex
/// review), and it also never counts as "received" for `condition == 1`'s
/// silence check.
///
/// For each eligible frame reaching a non-terminated slot:
/// [`advance_repeat_slot_windows`] closes/evaluates any window(s) already
/// elapsed as of this frame's arrival (round 3 grid-anchored fix, ADR-173),
/// then -- provided the grid has actually started (`interval_deadline` is
/// `Some`; a frame arriving before the slot's first transmit credits no
/// window, per that field's own doc comment) -- `slot.rx_seen_this_interval`
/// is set unconditionally for the (possibly just-advanced) current window
/// (this frame was received, whether or not it goes on to match). The
/// existing three-part match -- byte-level mask/pattern, `response_format_rx_bits` (masked to
/// [`REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK`]) against `rx_status`, and
/// `response_protocol_id` against `protocol_id` -- decides match vs.
/// non-match. The byte-level match alone cannot distinguish an 11-bit from
/// a 29-bit CAN ID (or normal from extended ISO15765 addressing) when the
/// numeric ID value coincides (round 9 fix, Codex review), and neither the
/// byte-level nor `RxStatus` check can distinguish a raw-CAN frame from an
/// ISO15765 one on a native-mixed CAN channel (`CanChannelMode::
/// NativeMixed`, ADR-160/162) when their wire bytes and format flags
/// coincide (round 10 fix, Codex review). `ProtocolID` is compared for
/// exact equality, not masked -- unlike `RxStatus`, it is a single native
/// protocol identifier, not a bitfield.
///
/// Corrected `Condition` semantics (ADR-173 Decision 1, superseding ADR-165
/// Decision 6's inverted pairing): `condition == 0`
/// (`REPEAT_MESSAGE_UNTIL_MATCH`) terminates on a MATCH; `condition == 1`
/// (`REPEAT_MESSAGE_WHILE_MATCH`) terminates on a NON-MATCH (the silence
/// half of `condition == 1`'s stop rule is [`advance_repeat_slot_windows`]'s
/// own grid-window check above, not this function's per-frame match/
/// non-match evaluation below).
///
/// Called at every site that pushes a frame into `channel.rx_queue` --
/// mirroring a real device evaluating each incoming frame against every
/// active slot's stop condition as it arrives, not just at
/// `spawn_repeat_worker`'s next poll tick (which only reads `terminated`/
/// `rx_seen_this_interval`, never scans `rx_queue` itself, so a frame
/// already drained by a concurrent `PassThruReadMsgs` before that tick is
/// never missed).
fn note_rx_frame_for_repeat_slots(
    channel: &mut ChannelState,
    data: &[u8],
    rx_status: u32,
    protocol_id: u32,
) {
    if rx_status & REPEAT_INELIGIBLE_RX_STATUS_MASK != 0 {
        return;
    }
    let now = std::time::Instant::now();
    for slot in channel.repeat_slots.values_mut() {
        if slot.terminated {
            continue;
        }
        // Round 3 grid-anchored fix (ADR-173, Codex review PR #60 round 3):
        // close/evaluate any window(s) already elapsed as of this frame's
        // arrival BEFORE crediting it, so a frame arriving after a window's
        // grid boundary cannot rescue that already-silent window -- only the
        // window it actually falls into (see `advance_repeat_slot_windows`'s
        // own doc comment for the full invariant).
        advance_repeat_slot_windows(slot, now);
        if slot.terminated {
            continue;
        }
        // `interval_deadline.is_some()` is the "grid has started" check -- a
        // frame arriving before the slot's first transmit correctly credits
        // no window (see `RepeatSlot::interval_deadline`'s doc comment); its
        // own match/non-match stop-trigger below still applies regardless.
        if slot.interval_deadline.is_some() {
            slot.rx_seen_this_interval = true;
        }
        let matches = repeat_mask_pattern_matches(&slot.mask, &slot.pattern, data)
            && (rx_status & REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK) == slot.response_format_rx_bits
            && slot.response_protocol_id == protocol_id;
        // `condition == 0` (UNTIL_MATCH) terminates on a match; `condition
        // == 1` (WHILE_MATCH) terminates on a non-match -- two conditions
        // that happen to share an identical terminal action, so they are
        // combined into one boolean rather than two near-identical `if`
        // arms (which `clippy::if_same_then_else` flags as suspicious).
        let stop_trigger = (slot.condition == 0 && matches) || (slot.condition == 1 && !matches);
        if stop_trigger {
            slot.terminated = true;
        }
    }
}

/// SAE J2534-2 clause 19.3.2.2 (ADR-192/Phase 7 Stage 7c): the fixed number
/// of frames a single TP2.0 broadcast burst sends, alternating the last two
/// data bytes each transmission.
const TP20_BROADCAST_BURST_COUNT: usize = 5;

/// SAE J2534-2 clause 19.3.2.2/19.3.2.3 (ADR-192/Phase 7 Stage 7c): simulates
/// the device sending `base` [`TP20_BROADCAST_BURST_COUNT`] times, alternating
/// the last two data bytes `0xAA`/`0x55` each transmission -- shared by
/// `PassThruWriteMsgs`'s single-burst broadcast and `PassThruStartPeriodicMsg`'s
/// own immediate initial burst (clause 19.3.2.3) before it settles into its
/// ongoing periodic rate. `base.data`'s own last two bytes (whatever the
/// caller supplied) are overwritten for every one of the 5 sends; a payload
/// shorter than 2 bytes is left byte-for-byte unmodified (nothing to
/// alternate) but still sent 5 times.
fn push_tp20_broadcast_burst(channel: &mut ChannelState, base: &StoredMessage, loopback: bool) {
    for i in 0..TP20_BROADCAST_BURST_COUNT {
        let mut stored = base.clone();
        if stored.data.len() >= 2 {
            let len = stored.data.len();
            let byte = if i % 2 == 0 { 0xAA } else { 0x55 };
            stored.data[len - 2] = byte;
            stored.data[len - 1] = byte;
        }
        if loopback {
            let mut echo = stored.clone();
            echo.rx_status = RX_TX_MSG_TYPE | tx_format_flags_to_rx_status_bits(stored.tx_flags);
            echo.timestamp = MOCK_TIMESTAMP;
            note_rx_frame_for_repeat_slots(channel, &echo.data, echo.rx_status, echo.protocol_id);
            channel.rx_queue.push_back(echo);
        }
        channel.record_written(stored);
    }
}

/// SAE J2534-2 clause 14's own minimum (ADR-165/Phase 12): the device
/// supports at least this many concurrent repeat slots per physical channel.
const MAX_REPEAT_SLOTS_PER_CHANNEL: usize = 10;

/// SAE J2534-2 clause 19.3.1's own requirement (ADR-188/Phase 7 Stage 7a):
/// four simultaneous TP2.0 connections total per physical channel (Context
/// correction: the passive connection, Stage 7b, counts as one of the four,
/// not an additional fifth).
const TP20_MAX_CONNECTIONS_PER_CHANNEL: usize = 4;

/// The largest length that fits in one raw CAN frame at this mock's own
/// ordinary `PassThruWriteMsgs` size convention (4-byte ID + up to 8 data
/// bytes) -- ADR-188/Phase 7 Stage 7a's own clause 19.4.4 "fits in a single
/// CAN message" threshold for a non-connection TP2.0 write.
const TP20_SINGLE_CAN_MSG_MAX_LEN: usize = 12;

/// Keeps the mock DLL mapped once a repeat worker thread exists.
///
/// Tests load the mock, start repeat messages and drop the library while the detached
/// worker may still be polling. `FreeLibrary` then unmaps code the worker is executing
/// (`STATUS_ACCESS_VIOLATION` on Windows CI). glibc never unloads a Rust cdylib that has
/// registered TLS destructors, so Linux does not show it. Pinning the module matches that.
fn pin_module_while_workers_may_run() {
    #[cfg(windows)]
    {
        static PIN: std::sync::Once = std::sync::Once::new();
        PIN.call_once(|| {
            const GET_MODULE_HANDLE_EX_FLAG_PIN: u32 = 0x1;
            const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x4;
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetModuleHandleExW(
                    flags: u32,
                    name: *const u16,
                    module: *mut *mut c_void,
                ) -> i32;
            }
            let mut module = std::ptr::null_mut();
            // Safety: FROM_ADDRESS takes any address inside this module; the handle is not used.
            unsafe {
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                    pin_module_while_workers_may_run as *const () as *const u16,
                    &mut module,
                );
            }
        });
    }
}

/// Drives one repeat slot's autonomous retransmission/stop-condition timing
/// (ADR-173 Decision 1/2, superseding ADR-165 Decision 6's inverted
/// pairing), started right after `IOCTL_START_REPEAT_MESSAGE` inserts
/// `msg_id`'s slot. One unified loop for both `condition` values -- unlike
/// the prior two-branch model (a free-running `condition == 0` loop with no
/// RX evaluation, and a transmit-once-then-poll `condition == 1` branch
/// that never actually repeated), both conditions now genuinely retransmit
/// on the same interval-timer cadence; only the device-side continue/stop
/// rule differs (`note_rx_frame_for_repeat_slots`'s per-frame evaluation
/// for both, plus this function's own per-interval silence check for
/// `condition == 1` only).
///
/// Exits as soon as any of: the channel is gone, the slot is gone (removed
/// by an explicit `IOCTL_STOP_REPEAT_MESSAGE`/`__mock_reset`), or the slot
/// is `terminated` -- a terminated slot is NOT removed by this worker (see
/// `RepeatSlot::terminated`'s doc comment: it stays in `channel.
/// repeat_slots` for QUERY/STOP to see until an explicit STOP), the worker
/// simply stops transmitting into it and exits. There is nothing further
/// to join or cancel from the caller's side.
///
/// Each iteration: under the transmit lock acquisition, call
/// [`advance_repeat_slot_windows`] first (round 3 grid-anchored fix,
/// ADR-173) to close/evaluate any window(s) already elapsed as of now -- if
/// this terminates the slot, return immediately without transmitting again
/// (this replaces the round-1/round-2 patches' separate inline silence
/// check that used to live in the poll loop below, and preserves the
/// "read `rx_seen_this_interval` and write `terminated` under one lock
/// acquisition" property the edge-case-hunter finding on the round-1 fix
/// required, since `advance_repeat_slot_windows` runs entirely under this
/// same held guard). Otherwise transmit `message` into `written_msgs`
/// (loopback echo unchanged, still routed through
/// `note_rx_frame_for_repeat_slots` so the echo is evaluated and correctly
/// no-ops per `REPEAT_INELIGIBLE_RX_STATUS_MASK`). If this is the slot's
/// very first transmit (`interval_deadline` was `None`), start the grid now
/// by setting `interval_deadline = Some(Instant::now() + interval)`;
/// otherwise `interval_deadline` is left exactly as
/// `advance_repeat_slot_windows` just set it -- re-anchoring it to
/// `Instant::now() + interval` again on every transmit is exactly the
/// round-3 bug this fix corrects. The worker's own local `deadline`
/// variable (used only for this iteration's poll-loop sleep cadence) is
/// then read from that same (now guaranteed-`Some`) `slot.interval_deadline`
/// value. The inner loop polls (in small steps, not a single blocking
/// sleep, Codex review PR #42 round 21/round 8) until `deadline`,
/// re-checking epoch and slot existence/termination at each step so a
/// concurrent STOP/disconnect/reset/self-termination is noticed promptly;
/// once `Instant::now() >= deadline` it simply breaks back to the top of
/// the outer loop, which re-acquires the lock and calls
/// `advance_repeat_slot_windows` again -- now doing the real work of
/// closing/evaluating the window(s) that just elapsed. If
/// `note_rx_frame_for_repeat_slots` sets `terminated = true` asynchronously
/// during the poll wait (observed at one of the periodic under-lock
/// re-checks), the worker exits immediately without transmitting again.
///
/// `epoch` is [`REPEAT_WORKER_EPOCH`]'s value at the moment this slot was
/// inserted (captured by the caller under the same `state()` lock that
/// performed the insert, before spawning this thread -- edge-case-hunter
/// fix, Bug 4, tightened by a round-8 fix, Codex review, ADR-165 PR #42
/// round 8, design-advisor consult). Re-checked lock-free at the start of
/// every loop iteration as a fast path, AND again under `state()`'s lock
/// immediately after each lock acquisition, before any mutating step (the
/// transmit, or a termination write) -- preserved exactly at every single
/// point the pre-ADR-173 code checked it, per that code's own established
/// discipline. The lock-free checks alone are not sufficient: since
/// `epoch`'s bump now lives inside `__mock_reset`'s/`mock_reset`'s own
/// `state()` critical section (see their doc comments), only an under-lock
/// re-check is guaranteed to observe a reset that completed while this
/// thread was blocked waiting for the same lock -- pairing the lock-free
/// check with an under-lock one closes that window without giving up the
/// lock-free fast path. A mismatch at either check means a reset ran since
/// this slot was started -- treated exactly like "channel gone"/"slot gone"
/// (return immediately, no write, no slot mutation, since a fresh
/// post-reset slot may already occupy this same `(channel_id, msg_id)` key
/// and is not this thread's to touch).
fn spawn_repeat_worker(channel_id: u32, msg_id: u32, epoch: u32) {
    const POLL_STEP_MS: u64 = 5;
    pin_module_while_workers_may_run();
    std::thread::spawn(move || {
        loop {
            if REPEAT_WORKER_EPOCH.load(std::sync::atomic::Ordering::SeqCst) != epoch {
                return;
            }
            let (interval_ms, message) = {
                let guard = state().lock().expect("mock state poisoned");
                let Some(channel) = guard.channels.get(&channel_id) else {
                    return;
                };
                let Some(slot) = channel.repeat_slots.get(&msg_id) else {
                    return;
                };
                if slot.terminated {
                    return;
                }
                (slot.time_interval_ms.max(1), slot.message.clone())
            };

            let deadline = {
                let mut guard = state().lock().expect("mock state poisoned");
                if REPEAT_WORKER_EPOCH.load(std::sync::atomic::Ordering::SeqCst) != epoch {
                    return;
                }
                let Some(channel) = guard.channels.get_mut(&channel_id) else {
                    return;
                };
                let Some(slot) = channel.repeat_slots.get_mut(&msg_id) else {
                    return; // stopped concurrently, before this transmit
                };
                if slot.terminated {
                    return;
                }
                // Round 3 grid-anchored fix (ADR-173, Codex review PR #60
                // round 3, design-advisor consult): close/evaluate any
                // window(s) already elapsed as of now BEFORE transmitting --
                // a worker delayed by more than one full `TimeInterval`
                // (e.g. a severe scheduler stall) must still have every
                // intervening silent window evaluated, not skipped by
                // re-anchoring the grid to whenever this thread happens to
                // actually wake.
                advance_repeat_slot_windows(slot, std::time::Instant::now());
                if slot.terminated {
                    return;
                }
                // Codex-review round 20 (PR #42): mirror `PassThruWriteMsgs`'s
                // own loopback-echo path (above) for this worker's autonomous
                // transmissions -- otherwise a client sees loopback echoes for
                // ordinary writes but never for repeat traffic, purely
                // because of which code path happened to transmit an
                // otherwise-identical frame. Clone the echo BEFORE moving
                // `message` into `written_msgs`, same ordering as
                // `PassThruWriteMsgs`.
                if channel.loopback_enabled() {
                    let mut echo = message.clone();
                    echo.rx_status =
                        RX_TX_MSG_TYPE | tx_format_flags_to_rx_status_bits(message.tx_flags);
                    echo.timestamp = MOCK_TIMESTAMP;
                    note_rx_frame_for_repeat_slots(
                        channel,
                        &echo.data,
                        echo.rx_status,
                        echo.protocol_id,
                    );
                    channel.rx_queue.push_back(echo);
                }
                channel.record_written(message);
                // Re-fetch: `slot`'s borrow above ended at its last use
                // (before the `channel.loopback_enabled()`/
                // `note_rx_frame_for_repeat_slots(channel, ..)` calls, which
                // need `channel` itself mutably) -- still under the same
                // lock acquisition, so nothing else could have removed it.
                let Some(slot) = channel.repeat_slots.get_mut(&msg_id) else {
                    return; // stopped concurrently, before this transmit completed
                };
                if slot.interval_deadline.is_none() {
                    // This slot's very first transmit -- start the grid now.
                    slot.interval_deadline = Some(
                        std::time::Instant::now()
                            + std::time::Duration::from_millis(interval_ms as u64),
                    );
                }
                // Otherwise the grid already started: leave `interval_deadline`
                // exactly as `advance_repeat_slot_windows` left it above --
                // do NOT recompute it as `Instant::now() + interval` again,
                // which is exactly the round-3 re-anchoring bug.
                slot.interval_deadline
                    .expect("set immediately above when it was None")
            };

            loop {
                std::thread::sleep(std::time::Duration::from_millis(
                    POLL_STEP_MS.min(interval_ms as u64),
                ));
                if REPEAT_WORKER_EPOCH.load(std::sync::atomic::Ordering::SeqCst) != epoch {
                    return;
                }
                let mut guard = state().lock().expect("mock state poisoned");
                // Round-8/round-21 fix (Codex review, ADR-165 PR #42):
                // re-check under the same lock -- without this, a worker
                // that passed the lock-free check above, then blocked on
                // this lock while a reset (bump-then-clear) and a fresh
                // same-keyed START both completed, would keep observing the
                // FRESH slot's state under this thread's own stale
                // `deadline`/`interval_ms`.
                if REPEAT_WORKER_EPOCH.load(std::sync::atomic::Ordering::SeqCst) != epoch {
                    return;
                }
                let Some(channel) = guard.channels.get_mut(&channel_id) else {
                    return;
                };
                let Some(slot) = channel.repeat_slots.get_mut(&msg_id) else {
                    // A missing slot here means an explicit STOP already
                    // removed it concurrently -- nothing left to do.
                    return;
                };
                // `note_rx_frame_for_repeat_slots` may have terminated this
                // slot asynchronously (a matching frame under condition 0,
                // or a non-matching frame under condition 1) since the last
                // check -- exit immediately without transmitting again,
                // per this function's own doc comment.
                if slot.terminated {
                    return;
                }
                let at_deadline = std::time::Instant::now() >= deadline;
                if !at_deadline {
                    drop(guard);
                    continue;
                }
                // Round 3 grid-anchored fix (ADR-173, Codex review PR #60
                // round 3): the deadline has passed -- rather than
                // evaluating the silence stop-trigger inline here (as
                // rounds 1/2 did), simply break back to the outer loop,
                // which re-acquires this same lock and calls
                // `advance_repeat_slot_windows` at its top, doing the real
                // work of closing/evaluating every window that has elapsed
                // (possibly more than one, under a scheduler stall).
                drop(guard);
                break;
            }
        }
    });
}

// ── Per-channel state ─────────────────────────────────────────────────────────

/// A one-shot RX injection armed by `__mock_arm_write_rx_injection`, fired on
/// the NEXT `PassThruWriteMsgs` call for the channel it was armed on.
struct WriteRxInjection {
    frame: StoredMessage,
    hold_ms: u32,
}

#[derive(Default)]
struct ChannelState {
    /// The `PassThruConnect` protocol id, consulted by `PassThruIoctl`'s
    /// `IOCTL_FIVE_BAUD_INIT`/`IOCTL_FAST_INIT` handlers to reject the call
    /// with `ERR_NOT_SUPPORTED` on non-K-line channels, matching real J2534
    /// adapter behaviour.
    protocol_id: u32,
    baud_rate: u32,
    flags: u32,
    params: HashMap<u32, u32>,
    rx_queue: VecDeque<StoredMessage>,
    periodic_msgs: HashMap<u32, StoredMessage>,
    written_msgs: Vec<StoredMessage>,
    /// When each entry of `written_msgs` was stored (same index). Lets a test
    /// measure the real gap between two transmits instead of timing its own
    /// polling of `__mock_get_written_msg_count`, which adds the poll interval
    /// and the OS timer resolution to the measurement.
    written_at: Vec<std::time::Instant>,
    /// Input frame (`PASSTHRU_MSG.Data[..DataSize]`) of the most recent
    /// `IOCTL_FAST_INIT` call with a non-null `p_input`; `None` before any
    /// such call, or when the caller passed a null input. Lets tests assert
    /// the KWP wakeup header `j2534-0404-service` constructs at
    /// `StartComPrimitive` call time (ADR-075).
    fast_init_input: Option<Vec<u8>>,
    /// Target address byte (`SBYTE_ARRAY.BytePtr[0]`) of the most recent
    /// `IOCTL_FIVE_BAUD_INIT` call with a non-null `p_input`; `None` before
    /// any such call, or when the caller passed a null input. Lets tests
    /// assert the address `j2534-0404-service` resolves at `StartComPrimitive`
    /// call time -- either from `CP_5BaudAddressFunc`/`CP_5BaudAddressPhys`
    /// (the spec-mandated 5-baud contract) or the legacy `cop_data[0]` byte
    /// (ADR-076).
    five_baud_init_input: Option<u8>,
    next_periodic_id: u32,
    /// `TimeInterval` argument of the most recent `PassThruStartPeriodicMsg`
    /// call; `None` before any such call. Lets tests confirm
    /// `j2534-0404-service`'s ADR-072-style µs-to-ms conversion of
    /// `CP_TesterPresentTime` (ADR-083) actually reaches this argument.
    last_start_periodic_time_interval: Option<u32>,
    next_filter_id: u32,
    /// `(filter_id, filter)` pairs, in installation order. A `Vec` (rather than
    /// a map) so index-based `__mock_get_filter_*` accessors mirror the
    /// established `written_msgs` pattern; removal is O(n) but filter counts
    /// per channel are small (single digits).
    filters: Vec<(u32, MockFilter)>,
    /// One-shot RX injection armed via `__mock_arm_write_rx_injection`; `None`
    /// unless a test has armed one that has not yet fired. Cleared (`take`n)
    /// by the next `PassThruWriteMsgs` on this channel, whether or not one was
    /// armed. Lets tests deterministically land an RX frame (e.g. a
    /// FlowControl) exactly during a slow write, simulating real hardware
    /// write latency via `std::thread::sleep` (ADR-095 amendment).
    write_rx_injection: Option<WriteRxInjection>,
    /// `Parameter` id of every `IOCTL_SET_CONFIG` entry successfully applied
    /// on this channel, in call order (ADR-158/Phase 3a) -- a single
    /// `IOCTL_SET_CONFIG` call carrying N params pushes N entries in the
    /// order the caller supplied them. Lets tests assert cross-call
    /// ordering (e.g. `CONFIG_FD_CAN_DATA_PHASE_RATE` before
    /// `CONFIG_J1962_PINS`, clause 21.3.2.5.1) that `__mock_get_config_value`
    /// alone (final value only, no ordering) cannot.
    set_config_param_log: Vec<u32>,
    /// SAE J2534-2 clause 6.3.3.2 pin-assignment gate (ADR-156 Decision 2,
    /// Phase 2a; extended to clause 21 CAN FD by ADR-158/Phase 3a, clause 22
    /// ISO15765-on-CAN-FD by ADR-159/Phase 3b, clause 9 Single Wire CAN by
    /// ADR-164/Phase 4, clause 20 Fault-Tolerant CAN by ADR-168/Phase 6,
    /// clause 12 UART Echo Byte Protocol by ADR-170/Phase 9, and clause 17
    /// SAE J1708 by ADR-175/Phase 11):
    /// `true` for a channel opened with a non-`_PS`/non-FD/non-SW/non-FT/
    /// non-UART-Echo-Byte/non-J1708 protocol id (pins are
    /// meaningless/already-implicit, no gating -- today's behavior
    /// unchanged), `false` for a `_PS`, `FD_CAN_PS`, `FD_ISO15765_PS`,
    /// `SW_CAN_PS`, `SW_ISO15765_PS`, `FT_CAN_PS`, `FT_ISO15765_PS`,
    /// `UART_ECHO_BYTE_PS`, or `J1708_PS` channel until
    /// `IOCTL_SET_CONFIG(CONFIG_J1962_PINS)` binds its pins. While `false`,
    /// I/O operations that need wired pins
    /// (write, read, filter install, periodic-message start) are rejected
    /// with `ERR_PIN_INVALID`.
    pins_assigned: bool,
    /// SAE J2534-2 clause 8 Mixed Format Frames on a CAN Network
    /// (ADR-160/Phase 3c): the most recently `IOCTL_SET_CONFIG`'d
    /// `CONFIG_CAN_MIXED_FORMAT` value on this channel (`0` = OFF, the
    /// default). Only ever accepted on an ISO15765-family channel (see the
    /// `IOCTL_SET_CONFIG` handler's family gate) -- always `0` on every
    /// other channel.
    can_mixed_format: u32,
    /// SAE J2534-2 clause 14 Repeat Messaging (ADR-165/Phase 12): live
    /// `(MsgId -> slot)` entries started via `IOCTL_START_REPEAT_MESSAGE` and
    /// not yet stopped (explicitly via `IOCTL_STOP_REPEAT_MESSAGE`, or
    /// implicitly by `spawn_repeat_worker` removing a `Condition == 1` slot
    /// once it matches or times out). Capacity-checked against
    /// `MAX_REPEAT_SLOTS_PER_CHANNEL` at `START` time.
    repeat_slots: HashMap<u32, RepeatSlot>,
    next_repeat_msg_id: u32,
    /// SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5): source
    /// addresses this channel currently simulates as claimed and defended,
    /// populated by a successful `IOCTL_PROTECT_J1939_ADDR` claim (removed
    /// by that IOCTL's cancel form -- all-zero NAME bytes -- or by
    /// `__mock_set_j1939_claim_lost` forcing a subsequent claim attempt to
    /// resolve LOST instead of CLAIMED). Consulted by `PassThruWriteMsgs`'s
    /// `ERR_ADDRESS_NOT_CLAIMED` simulation (clause 16.5).
    j1939_claimed_addresses: std::collections::HashSet<u8>,
    /// SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a): the
    /// four-slot active-connection table, keyed on each connection's own
    /// RX-ID (clause 19.3.1's "four simultaneous connections total"
    /// maximum). Populated by a successful `IOCTL_REQUEST_CONNECTION`,
    /// removed by `IOCTL_TEARDOWN_CONNECTION`. Consulted by
    /// `PassThruWriteMsgs`'s clause 19.4.4 frame-routing rule.
    tp20_connections: HashMap<u32, Tp20MockConnection>,
    /// SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8): the bytes
    /// most recently staged via a successful `IOCTL_SET_POLL_RESPONSE` on
    /// this channel; empty before any such call. `IOCTL_BECOME_MASTER`
    /// itself does not consult this (clause 11's bus-mastership negotiation
    /// logic is a client-application responsibility, ADR-189 Context) --
    /// exposed purely for test observation via `__mock_get_poll_response`.
    poll_response: Vec<u8>,
    /// SAE J2534-2 clause 19.3.3.1 TP2.0 passive connections (ADR-190/Phase
    /// 7 Stage 7b): counts the number of times
    /// `__mock_inject_tp20_passive_connection` silently no-op'd -- either
    /// `CONFIG_TP2_0_IDENTIFER`/`_RXIDPASSIVE` isn't armed (still `0`), or
    /// this channel's four-slot `tp20_connections` table is already full --
    /// the same network-side rejection reason `0xD8` a real adapter never
    /// surfaces to the application as an indication (this mock's own
    /// injection hook mirrors that: no `rx_queue` push either). Exposed
    /// purely for test observation via `__mock_get_passive_connection_
    /// rejected_count`, since nothing else about a silent no-op is otherwise
    /// visible from outside the mock.
    passive_connection_rejected_count: u32,
}

/// One [`ChannelState::tp20_connections`] entry (ADR-188/Phase 7 Stage 7a):
/// the connection's own established TX-ID, the mock's own counterpart to
/// `j2534-0404-service`'s `LogicalLinkState::Tp20Connection::established_tx_id`.
#[derive(Debug, Clone, Copy)]
struct Tp20MockConnection {
    tx_id: u32,
}

impl ChannelState {
    /// Store a transmitted message in `written_msgs`, stamped in `written_at`.
    fn record_written(&mut self, message: StoredMessage) {
        self.written_msgs.push(message);
        self.written_at.push(std::time::Instant::now());
    }

    fn new(protocol_id: u32, baud_rate: u32, flags: u32) -> Self {
        // SAE J2534-2 clause 10.3.3.2.6-.2.8 Analog Inputs (ADR-216): seed
        // the three genuinely read-only capability parameters with
        // reasonable-for-testing values (Table 16's own example values) so a
        // `GET_CONFIG` on them (`j2534-0404-service`'s own connect-time
        // readback) returns something realistic instead of the generic `0`
        // default every other unseeded native config id reports. Two's-
        // complement 32-bit encoding for the signed millivolt range pair
        // (Table 16's own encoding) -- `(-20_000i32) as u32` for the low end.
        let mut params = HashMap::new();
        if is_analog_in_protocol(protocol_id) {
            params.insert(CONFIG_SAMPLE_RESOLUTION, 12);
            params.insert(CONFIG_INPUT_RANGE_LOW, (-20_000i32) as u32);
            params.insert(CONFIG_INPUT_RANGE_HIGH, 20_000);
        }
        ChannelState {
            protocol_id,
            baud_rate,
            flags,
            params,
            pins_assigned: !is_ps_protocol(protocol_id)
                && !is_fd_protocol(protocol_id)
                && !is_sw_protocol(protocol_id)
                && !is_ft_protocol(protocol_id)
                && !is_uart_echo_byte_protocol(protocol_id)
                && !is_honda_diagh_protocol(protocol_id)
                && !is_j1708_protocol(protocol_id)
                && !is_j1939_protocol(protocol_id)
                && !is_tp2_0_protocol(protocol_id)
                // ADR-189/Phase 8: exact `_PS` identity only, NOT
                // `is_gm_uart_protocol`'s own range-inclusive shape -- a
                // `_CHx` id (clause 7's vendor-connector model) has no J1962
                // pin concept at all, the same reason `is_ps_protocol` above
                // (Table 1's closed `_PS` set) never gates a base family's
                // own `_CHx` variant either.
                && protocol_id != PROTOCOL_GM_UART_PS,
            ..Default::default()
        }
    }

    fn loopback_enabled(&self) -> bool {
        self.params
            .get(&j2534_0404_sys::bindings::CONFIG_LOOPBACK)
            .copied()
            .unwrap_or(0)
            != 0
    }
}

// ── Call counters ─────────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy)]
struct CallCounters {
    open: usize,
    close: usize,
    connect: usize,
    disconnect: usize,
    read_msgs: usize,
    write_msgs: usize,
    start_periodic: usize,
    stop_periodic: usize,
    start_filter: usize,
    stop_filter: usize,
    set_config: usize,
    get_config: usize,
    read_vbatt: usize,
    five_baud_init: usize,
    fast_init: usize,
    get_device_info: usize,
    get_protocol_info: usize,
    clear_rx_buffer: usize,
    clear_tx_buffer: usize,
    /// SAE J2534-2 clause 9 Single Wire CAN (ADR-164/Phase 4): counts of
    /// successful (right protocol) `PassThruIoctl(SW_CAN_HS)`/`(SW_CAN_NS)`
    /// calls across every channel. Used by `j2534-0404-service` tests to
    /// confirm the `SharedChannel::ref_count == 1` gate (`ioctl_sw_can_mode`)
    /// actually skips the native call on a shared channel, not just returns
    /// success for an unrelated reason.
    sw_can_hs: usize,
    sw_can_ns: usize,
}

/// Signal/rendezvous primitive backing `MockState::become_master_hold` (PR
/// #98 GM UART `BECOME_MASTER`-in-flight race regression test): a plain
/// `std::sync::{Mutex, Condvar}` pair, deliberately independent of
/// `state()`'s own mutex -- the `IOCTL_BECOME_MASTER` dispatch arm releases
/// `state()` before waiting on this, so a blocked call never blocks any
/// other mock entry point a concurrent test action might need (e.g. a
/// second CLL's `PassThruConnect` on the same physical channel, which is
/// exactly what the regression test needs to run while this call is held
/// open).
///
/// `engaged` flips to `true` the moment the dispatch arm starts waiting,
/// letting a test poll `__mock_become_master_hold_engaged` for "the call is
/// genuinely in flight" instead of guessing a timing margin (ADR-149,
/// `j2534-0404-service/tests/grpc_mock/harness.rs`'s own module doc);
/// `released` is what the `Condvar` wait actually blocks on, set by
/// `__mock_release_become_master_hold`.
struct BecomeMasterHold {
    state: Mutex<BecomeMasterHoldState>,
    condvar: Condvar,
}

#[derive(Default)]
struct BecomeMasterHoldState {
    engaged: bool,
    released: bool,
}

// ── Global mock state ─────────────────────────────────────────────────────────

#[derive(Default)]
struct MockState {
    next_channel_id: u32,
    channels: HashMap<u32, ChannelState>,
    counters: CallCounters,
    last_error: String,
    /// Maximum number of simultaneously open channels; `0` (the `Default`
    /// value) means unlimited. Set via `__mock_set_max_channels` to
    /// exercise callers' handling of `ERR_EXCEEDED_LIMIT` from
    /// `PassThruConnect` (e.g. `can_channel_mode = "auto"` capability
    /// probing in `j2534-0404-service`, ADR-046).
    max_channels: usize,
    /// `Flags` argument of every successful `PassThruConnect`, in call order.
    /// Kept independent of `ChannelState` (which is removed on disconnect) so
    /// the log stays complete even for channels that were later closed --
    /// e.g. the dual-channel-capability probe in `j2534-0404-service`
    /// (ADR-046 addendum) connects and immediately disconnects.
    connect_flags_log: Vec<u32>,
    /// Simulated SAE J1850 bus flavor, set via `__mock_set_j1850_bus_flavor`:
    /// `None` (the default after `__mock_reset`) means a silent bus -- no
    /// unsolicited RX ever appears. `Some(PROTOCOL_J1850VPW)` /
    /// `Some(PROTOCOL_J1850PWM)` means the bus "answers" only a
    /// `PassThruConnect` opened with that exact protocol id: a canned
    /// response frame is queued on the new channel immediately, before any
    /// `PassThruWriteMsgs`, so both an active probe (which reads after
    /// writing) and a purely passive listener (which never writes) observe
    /// it. Lets `j2534-0404-service` tests drive the VPW/PWM auto-detect
    /// probe (ADR-070) deterministically.
    j1850_bus_flavor: Option<u32>,
    /// Pin-gated variant of `j1850_bus_flavor`, set via
    /// `__mock_set_j1850_bus_flavor_requiring_pins`: `(flavor, pin_select)`.
    /// `None` (the default after `__mock_reset`) disables this simulation
    /// entirely. Unlike `j1850_bus_flavor` (which answers unconditionally at
    /// `PassThruConnect` time, regardless of pin state), this variant only
    /// queues a canned response once a channel opened with `flavor`'s `_PS`
    /// variant has `IOCTL_SET_CONFIG(CONFIG_J1962_PINS)` applied with
    /// EXACTLY the given `pin_select` value -- letting
    /// `j2534-0404-service` tests prove SAE J1850 VPW/PWM auto-detect
    /// (ADR-070/156/157 Bug B) actually binds the caller's selected pins on
    /// its probe channel before the bus can be observed as responding,
    /// rather than probing on default wiring. Checked/queued from
    /// `IOCTL_SET_CONFIG`, not `PassThruConnect` -- see that handler.
    j1850_bus_flavor_requiring_pins: Option<(u32, u32)>,
    /// Raw J2534 error code `IOCTL_FAST_INIT` returns instead of its normal
    /// success behavior, set via `__mock_set_fast_init_error`. `None` (the
    /// default after `__mock_reset`) means no override -- `IOCTL_FAST_INIT`
    /// behaves normally. Checked after the channel/protocol validity checks
    /// (so a bad `channel_id` or non-K-line protocol still reports its own,
    /// more specific error) but before the success counter increments or any
    /// input/output is touched -- a forced failure never simulates a partial
    /// success. Lets `j2534-0404-service` tests exercise the wakeup-only
    /// fast-init failure path (ADR-077: `PduErrEvtInitError` + temp-param
    /// revert + `PduCopstFinished`) without a real adapter.
    fast_init_error: Option<c_long>,
    /// Raw J2534 error code `PassThruConnect` returns instead of its normal
    /// success behavior when `protocol_id == PROTOCOL_ETHERNET_NDIS`, set via
    /// `__mock_set_ndis_connect_error` (SAE J2534-2 clause 24 Ethernet_NDIS,
    /// ADR-194/Phase 16). `None` (the default after `__mock_reset`) means no
    /// override -- an Ethernet_NDIS `PassThruConnect` behaves normally.
    /// Checked after the `max_channels`/`_CHx` validity checks (so those
    /// still report their own, more specific error) but before the channel
    /// is actually inserted into `channels` -- a forced failure never
    /// simulates a partial connect. Lets `j2534-0404-service` tests exercise
    /// the clause-24 activation-line-not-achieved connect failure path
    /// (`ERR_NO_CONNECTION_ESTABLISHED` -> `PDU_ERR_NO_CABLE_DETECTED`)
    /// without a real adapter. Unaffected by `protocol_id`s other than
    /// `PROTOCOL_ETHERNET_NDIS` -- mirrors `j1850_bus_flavor`'s own
    /// protocol-scoped (not blanket) connect-time behavior, not
    /// `fast_init_error`'s blanket "every channel" shape.
    ndis_connect_error: Option<c_long>,
    /// Raw J2534 error code `PassThruStopMsgFilter` returns instead of its
    /// normal success behavior, set via `__mock_set_stop_filter_error`.
    /// `None` (the default after `__mock_reset`) means no override --
    /// `PassThruStopMsgFilter` behaves normally. Checked after the bad-
    /// `channel_id` check (so an unknown `channel_id` still reports its own,
    /// more specific error) but before the success counter increments or the
    /// filter is removed from `ChannelState::filters` -- a forced failure
    /// never simulates a partial success. Lets `j2534-0404-service` tests
    /// exercise `PDU_IOCTL_STOP_MSG_FILTER`/`PDU_IOCTL_CLEAR_MSG_FILTER`'s
    /// `PDU_ERR_FCT_FAILED` reporting path (ADR-114) without a real adapter.
    stop_filter_error: Option<c_long>,
    /// Raw J2534 error code `PassThruWriteMsgs` returns instead of its
    /// normal success behavior, set via `__mock_set_write_msgs_error`.
    /// `None` (the default after `__mock_reset`) means no override --
    /// `PassThruWriteMsgs` behaves normally. Checked after the bad-
    /// `channel_id`/unassigned-pins/analog-in/Ethernet_NDIS validity checks
    /// (so those still report their own, more specific error) but before the
    /// message batch is processed/stored/echoed -- a forced failure never
    /// simulates a partial success. Lets `j2534-0404-service` tests exercise
    /// the RC21/RC23 (NRC 0x21/0x23) auto-re-request path's own retransmit
    /// failure (`TxFailure::Event(PduErrEvtTxError)` ->
    /// `ReceivePhaseOutcome::ReRequestTxFailed`, ADR-087) without a real
    /// adapter, mirroring `stop_filter_error`'s convention -- this closes
    /// the backlog entry
    /// citing ADR-087 for that path's lack of test coverage.
    write_msgs_error: Option<c_long>,
    /// The `pName` argument of the most recent `PassThruOpen` call: `None` if
    /// it was called with a NULL `pName` (or has never been called since
    /// `__mock_reset`), `Some(bytes)` (NUL terminator excluded) if it was
    /// called with a non-NULL `pName`. Lets `j2534-0404-service` tests verify
    /// the ADR-106 device-selection `pname` connection-target string reaches
    /// `PassThruOpen` as expected (and that `NULL` reaches it for the
    /// default/synthetic module entry).
    last_open_pname: Option<Vec<u8>>,
    /// Raw J2534 error code `PassThruSetProgrammingVoltage` returns instead
    /// of its normal success behavior, set via
    /// `__mock_set_prog_voltage_error`. `None` (the default after
    /// `__mock_reset`) means no override -- `PassThruSetProgrammingVoltage`
    /// behaves normally. Lets `j2534-0404-service` tests exercise
    /// `PDU_IOCTL_SET_PROG_VOLTAGE`'s native-failure mapping (A2-21: a
    /// distinct `PDU_ERR_MUX_RSC_NOT_SUPPORTED` for `ERR_PIN_INVALID`,
    /// `PDU_ERR_VOLTAGE_NOT_SUPPORTED` for anything else) without a real
    /// adapter, mirroring `stop_filter_error`'s convention.
    prog_voltage_error: Option<c_long>,
    /// Raw J2534 error code `IOCTL_READ_J1962PIN_VOLTAGE` returns instead of
    /// its normal success behavior, set via
    /// `__mock_set_j1962_pin_voltage_error`. `None` (the default after
    /// `__mock_reset`) means no override -- the pin-based validation in the
    /// `IOCTL_READ_J1962PIN_VOLTAGE` dispatch arm runs normally. Lets
    /// `j2534-0404-service` tests exercise `ioctl_read_j1962_pin_voltage`'s
    /// generic `map_native_error_for_link` fallback branch (any native
    /// failure other than `ERR_PIN_INVALID`), which the pin-based checks
    /// alone can never reach, mirroring `prog_voltage_error`'s convention.
    j1962_pin_voltage_error: Option<c_long>,
    /// Raw J2534 error code `PassThruIoctl(IOCTL_BECOME_MASTER)` returns
    /// instead of its normal success behavior, set via
    /// `__mock_set_become_master_error`. `None` (the default after
    /// `__mock_reset`) means no override -- `IOCTL_BECOME_MASTER` succeeds
    /// unconditionally (clause 11.3.3.2's "poll message matched or Poll_ID
    /// zero" outcome). Lets `j2534-0404-service` tests exercise
    /// `ioctl_become_master`'s generic `map_native_error_for_link` mapping
    /// (e.g. the documented `ERR_FAILED` "no poll message within 2s"
    /// outcome) without a real adapter or an actual 2-second wait, mirroring
    /// `prog_voltage_error`'s convention.
    become_master_error: Option<c_long>,
    /// PR #98 GM UART `BECOME_MASTER`-in-flight race regression coverage:
    /// armed via `__mock_arm_become_master_hold`, makes the next
    /// `IOCTL_BECOME_MASTER` dispatch arm block (after releasing `state()`'s
    /// own lock, so it never blocks any other mock entry point -- including
    /// a concurrent `PassThruConnect`/`PassThruDisconnect` the test drives
    /// while this call is held open) until `__mock_release_become_master_hold`
    /// is called. `None` (the default after `__mock_reset`) means no hold --
    /// `IOCTL_BECOME_MASTER` returns immediately, exactly as before this
    /// backdoor was added. See [`BecomeMasterHold`]'s own doc comment for
    /// the rendezvous mechanism.
    become_master_hold: Option<Arc<BecomeMasterHold>>,
    /// Raw J2534 error code `PassThruReadMsgs` returns instead of its normal
    /// success/`ERR_BUFFER_EMPTY` behavior, set via
    /// `__mock_set_read_msgs_error`. `None` (the default after
    /// `__mock_reset`) means no override -- `PassThruReadMsgs` behaves
    /// normally. Checked after the `channel_id` validity check (so an
    /// unknown `channel_id` still reports its own, more specific error) but
    /// before any queued frame is popped or `*p_num_msgs`/`*p_msg` is
    /// touched -- a forced failure never simulates a partial read. Any
    /// non-`ERR_BUFFER_EMPTY` code installed here drives
    /// `j2534-0404-service`'s background poll task
    /// (`poll_rx_inner`/`handle_channel_hard_error`) down its hard-channel-
    /// error path, letting tests exercise `ErrorDetail.error_event_data`
    /// (ADR-105) for a real `PDU_ERR_EVT_LOST_COMM_TO_VCI` tracked error
    /// without a real adapter, mirroring `stop_filter_error`'s convention.
    read_msgs_error: Option<c_long>,
    /// Raw J2534 error code `PassThruIoctl(IOCTL_STOP_REPEAT_MESSAGE)`
    /// returns instead of its normal success/`ERR_INVALID_MSG_ID` behavior,
    /// set via `__mock_set_stop_repeat_message_error`. `None` (the default
    /// after `__mock_reset`) means no override -- `IOCTL_STOP_REPEAT_MESSAGE`
    /// behaves normally. Checked before the real
    /// `channel.repeat_slots.remove(...)` call, so a forced failure never
    /// simulates a partial stop -- the slot stays live. Lets
    /// `j2534-0404-service` tests exercise ADR-180 Decision 20's
    /// leaked-STOP retry mechanism (`push_leaked_repeat_slots`/
    /// `record_leaked_repeat_slots`, retried via
    /// `retry_leaked_repeat_message_stops`) for a `MsgId` that is genuinely
    /// still alive and retransmitting, mirroring `stop_filter_error`'s
    /// convention.
    stop_repeat_message_error: Option<c_long>,
    /// SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 4/Phase
    /// 2b) capacity override, set via `__mock_set_chx_capacity`:
    /// `Some(count)` reports `count` as the `_CHx` capacity for EVERY
    /// in-scope protocol family via `DEVICE_INFO_<PROTOCOL>_SUPPORTED`'s
    /// packed value, and enforces it at `PassThruConnect`
    /// (`channel_index > count` fails `ERR_NOT_SUPPORTED`). `None` (the
    /// default after `__mock_reset`) uses `DEFAULT_CHX_CAPACITY` for both.
    chx_capacity_override: Option<u32>,
    /// ADR-211 (Codex review round-2 correction, PR #124): an INDEPENDENT
    /// capacity override for `DEVICE_INFO_FT_CAN_SUPPORTED`/
    /// `DEVICE_INFO_FT_ISO15765_SUPPORTED`'s own packed `_CHx` count,
    /// set via `__mock_set_ft_can_chx_capacity` -- distinct from
    /// `chx_capacity_override` above, which still governs both the native
    /// `PassThruConnect`-time enforcement (shared across every `_CHx`
    /// family, unaffected by this field) and every OTHER family's own
    /// Discovery-reported capacity. Exists so a test can prove
    /// `J2534Service::check_chx_capacity` actually consults the FT-specific
    /// `DEVICE_INFO_*_SUPPORTED` flag rather than the generic
    /// `DEVICE_INFO_CAN_SUPPORTED`/`DEVICE_INFO_ISO15765_SUPPORTED` one --
    /// with a single shared `chx_capacity_override`, the two flags always
    /// report the same count, so the Discovery-precheck's accept/reject
    /// outcome is structurally indistinguishable between "correctly
    /// consulted the FT flag" and "incorrectly consulted the generic one"
    /// (an `edge-case-hunter` finding: reverting `check_chx_capacity`'s call
    /// site back to the generic base id left every existing gRPC-mock test
    /// passing). `None` (the default after `__mock_reset`) falls back to
    /// `chx_capacity_override.unwrap_or(DEFAULT_CHX_CAPACITY)`, i.e. FT-CAN
    /// behaves identically to every other packed-capacity family unless a
    /// test explicitly diverges it.
    ft_can_chx_capacity_override: Option<u32>,
    /// ADR-212/Round 2: the Single Wire CAN analog of
    /// `ft_can_chx_capacity_override` just above -- an INDEPENDENT capacity
    /// override for `DEVICE_INFO_SW_CAN_SUPPORTED`/
    /// `DEVICE_INFO_SW_ISO15765_SUPPORTED`'s own packed `_CHx` count, set via
    /// `__mock_set_sw_can_chx_capacity` -- distinct from
    /// `chx_capacity_override` above for the same reason `ft_can_chx_capacity_
    /// override` is: it lets a test genuinely discriminate "correctly
    /// consulted the SW-CAN-specific flag" from "incorrectly consulted the
    /// generic `DEVICE_INFO_CAN_SUPPORTED`/`DEVICE_INFO_ISO15765_SUPPORTED`
    /// one" (the same class of gap FT-CAN's own field needed a Codex
    /// review round-2 correction to add -- built discriminating from the
    /// start here instead, ADR-212 Context item 3). `None` (the default
    /// after `__mock_reset`) falls back to
    /// `chx_capacity_override.unwrap_or(DEFAULT_CHX_CAPACITY)`, i.e. SW-CAN
    /// behaves identically to every other packed-capacity family unless a
    /// test explicitly diverges it.
    sw_can_chx_capacity_override: Option<u32>,
    /// ADR-213/Round 3: the CAN FD analog of `ft_can_chx_capacity_override`/
    /// `sw_can_chx_capacity_override` above -- an INDEPENDENT capacity
    /// override for `DEVICE_INFO_FD_CAN_SUPPORTED`'s own packed `_CHx`
    /// count, set via `__mock_set_fd_can_chx_capacity` -- distinct from
    /// `chx_capacity_override` above for the same discriminating-test reason
    /// FT-CAN's/SW-CAN's own fields are. `None` (the default after
    /// `__mock_reset`) falls back to
    /// `chx_capacity_override.unwrap_or(DEFAULT_CHX_CAPACITY)`, i.e. CAN FD
    /// behaves identically to every other packed-capacity family unless a
    /// test explicitly diverges it.
    fd_can_chx_capacity_override: Option<u32>,
    /// ADR-213/Round 3: the ISO15765-on-CAN-FD analog of
    /// `fd_can_chx_capacity_override` just above -- an INDEPENDENT capacity
    /// override for `DEVICE_INFO_FD_ISO15765_SUPPORTED`'s own packed `_CHx`
    /// count, set via `__mock_set_fd_iso15765_chx_capacity`. `None` (the
    /// default after `__mock_reset`) falls back to
    /// `chx_capacity_override.unwrap_or(DEFAULT_CHX_CAPACITY)`.
    fd_iso15765_chx_capacity_override: Option<u32>,
    /// SAE J2534-2 clause 8 Mixed Format Frames on a CAN Network
    /// (ADR-160/Phase 3c), set via `__mock_set_can_mixed_format_unsupported`:
    /// `true` forces every `IOCTL_SET_CONFIG(CONFIG_CAN_MIXED_FORMAT)` call
    /// to fail `ERR_NOT_SUPPORTED`, regardless of the channel's protocol
    /// family -- simulating a device that doesn't implement clause 8 at
    /// all. `false` (the default after `__mock_reset`) leaves the
    /// ISO15765-family-only gate (the `IOCTL_SET_CONFIG` handler) as the
    /// only check.
    can_mixed_format_unsupported: bool,
    /// SAE J2534-2 clause 18 Device Configuration (ADR-176/Phase 14): the
    /// device's ten `NON_VOLATILE_STORE_1`..`_10` slots, indexed `0..10`
    /// (slot N is index `N - 1`). Defaults to all-zero (`Default`, matching
    /// Table 73's own default-0 column). Lives on `MockState` (not
    /// `ChannelState`) since these two IOCTLs are `DeviceID`-scoped, not
    /// `ChannelID`-scoped, and per ADR-176 the store must survive
    /// `PassThruClose`/`PassThruOpen` within the same mock process (mirroring
    /// real non-volatile persistence) -- only `__mock_reset` clears it, the
    /// same rule every other `MockState` field follows.
    non_volatile_store: [u32; 10],
    /// SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5), set via
    /// `__mock_set_j1939_claim_lost`: `true` forces every subsequent
    /// `IOCTL_PROTECT_J1939_ADDR` claim attempt (non-cancel form) to
    /// resolve `J1939_ADDRESS_LOST` instead of `J1939_ADDRESS_CLAIMED`,
    /// simulating a higher-priority claimant already on the bus -- lets
    /// tests exercise `j2534-0404-service`'s retry-over-the-candidate-list
    /// behavior (ADR-179 Decision 3) without a real multi-node bus. `false`
    /// (the default after `__mock_reset`) is normal success behavior,
    /// mirroring `read_msgs_error`'s convention (a global override, not
    /// per-channel/per-address -- this mock's simplest simulation of an
    /// otherwise-real, non-blocking, address-agnostic device response).
    j1939_claim_lost: bool,
    /// Set via `__mock_set_j1939_claim_lost_count`: the number of upcoming
    /// `IOCTL_PROTECT_J1939_ADDR` claim attempts (non-cancel form, on a known
    /// channel, that produce an indication) that resolve
    /// `J1939_ADDRESS_LOST`. Each such attempt uses up one; at `0` (the
    /// default after `__mock_reset`) claims behave normally unless
    /// `j1939_claim_lost` is set. Unlike that global toggle, this makes
    /// "only the first candidate loses" deterministic: the test sets it
    /// before the claim starts and never has to race the retry loop.
    j1939_claim_lost_remaining: u32,
    /// SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8), set via
    /// `__mock_set_gm_uart_supported`: `true` forces `IOCTL_GET_DEVICE_INFO`'s
    /// `DEVICE_INFO_GM_UART_SUPPORTED` arm to report `Supported = 0` instead
    /// of this mock's normal `Supported = 1` -- lets tests exercise
    /// `enforce_discovery_capability`'s ADR-185 Stage 1 fail-fast
    /// connect-time rejection for this family, the same kind of coverage
    /// `chx_capacity_override` provides for the `_CHx` capacity precheck.
    /// `false` (the default after `__mock_reset`) reproduces this mock's
    /// normal advertised-supported behavior.
    gm_uart_unsupported: bool,
    /// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16), set via
    /// `__mock_set_ndis_supported`: `true` forces `IOCTL_GET_DEVICE_INFO`'s
    /// `DEVICE_INFO_ETHERNET_NDIS_SUPPORTED` arm to report `Supported = 0`
    /// instead of this mock's normal `Supported = 1` -- lets tests exercise
    /// `enforce_discovery_capability`'s ADR-185 Stage 1 fail-fast
    /// connect-time rejection for this family, the same
    /// `gm_uart_unsupported` shape just above.
    /// `false` (the default after `__mock_reset`) reproduces this mock's
    /// normal advertised-supported behavior.
    ndis_unsupported: bool,
    /// ADR-180 Decision 21 regression coverage, set via
    /// `__mock_set_j1939_claim_no_indication`: `true` makes every
    /// subsequent `IOCTL_PROTECT_J1939_ADDR` claim attempt (non-cancel
    /// form) still return success synchronously, but SKIP both its normal
    /// `ChannelState::j1939_claimed_addresses` insert and its normal
    /// `rx_queue` push -- i.e. the native call "succeeds" but the adapter
    /// never actually reports `J1939_ADDRESS_CLAIMED`/`_LOST`. This is what
    /// makes `j2534-0404-service`'s `run_j1939_claim_loop` bounded wait
    /// actually WAIT (instead of resolving instantly, the way
    /// `j1939_claim_lost` above's immediate `_LOST` push does), so a test
    /// can deterministically send `CancelComPrimitive` while the wait is in
    /// flight. `false` (the default after `__mock_reset`) is normal
    /// behavior, mirroring `j1939_claim_lost`'s own global-override
    /// convention.
    j1939_claim_no_indication: bool,
    /// ADR-180 Decision 22 regression coverage, set via
    /// `__mock_set_j1939_cancel_error`: `true` makes the `IOCTL_
    /// PROTECT_J1939_ADDR` CANCEL form (`is_cancel`, all-zero NAME) return
    /// a failure status instead of its normal unconditional success --
    /// WITHOUT removing the address from `ChannelState::
    /// j1939_claimed_addresses`, so the mock's own bookkeeping genuinely
    /// still reflects "still claimed" after the simulated failed cancel
    /// (test realism: a real adapter that rejects/fails a cancel presumably
    /// still defends the address). `false` (the default after
    /// `__mock_reset`) is normal behavior.
    j1939_cancel_error: bool,
    /// SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188), set via
    /// `__mock_set_tp20_no_indication`: `true` makes every subsequent
    /// `IOCTL_REQUEST_CONNECTION` (the non-slots-full success path) still
    /// allocate a real slot in `ChannelState::tp20_connections` -- so the
    /// four-slot accounting other tests already exercise (`connection_
    /// rejected_when_all_four_slots_are_full`) stays realistic -- but SKIP
    /// the normal `rx_queue` push, i.e. the native call "succeeds" without
    /// ever reporting `CONNECTION_ESTABLISHED`. Unlike `j1939_claim_no_
    /// indication` (which skips BOTH its own equivalent bookkeeping insert
    /// and its `rx_queue` push), this keeps the slot insert deliberately:
    /// it is what lets a test observe a later `IOCTL_TEARDOWN_CONNECTION`
    /// actually freeing that exact slot indirectly, via the same "all four
    /// slots full" 5th-attempt-rejected/accepted mechanic
    /// `connection_rejected_when_all_four_slots_are_full` already uses --
    /// this crate has no direct `IOCTL_TEARDOWN_CONNECTION` call-count
    /// backdoor (mirroring the identical "no direct call-count" reasoning
    /// `j2534-0404-service/tests/grpc_mock/j1939.rs`'s own claim-loop
    /// regression tests already document for `IOCTL_PROTECT_J1939_ADDR`).
    /// This is what makes `j2534-0404-service`'s `run_tp20_connection_
    /// request` bounded wait actually WAIT (instead of resolving within a
    /// single `POLL_INTERVAL_MS` tick), so a test can deterministically
    /// disconnect the requesting CLL while the wait is in flight. `false`
    /// (the default after `__mock_reset`) is normal behavior, mirroring
    /// `j1939_claim_no_indication`'s own global-override convention.
    ///
    /// Also gates `IOCTL_TEARDOWN_CONNECTION`'s own `rx_queue` push (Codex
    /// review finding, PR #97, 7th round -- ADR-188's own abandoned-RX-ID
    /// quarantine mechanism): armed, a teardown still genuinely removes the
    /// slot from `ChannelState::tp20_connections` (so the four-slot
    /// accounting above stays realistic), but does NOT queue its own
    /// `CONNECTION_LOST` reason-`0` indication. Without this, `j2534-0404-
    /// service`'s `best_effort_teardown_on_abandon` (issued for BOTH of
    /// `run_tp20_connection_request`'s local-abandonment paths) would have
    /// its OWN teardown call synchronously queue that indication regardless
    /// of this flag, which the physical channel's own poll task then drains
    /// on the very next loop tick -- before ANY subsequent client-driven RPC
    /// could possibly land -- releasing the new quarantine mechanism's own
    /// `abandoned` mark so promptly that a test cannot observe it blocking a
    /// same-`rx_id` retry at all. Gating this push here is what makes that
    /// quarantine window actually observable end-to-end.
    tp20_no_indication: bool,
    /// ADR-192/Phase 7 Stage 7c, Codex review round 2 (P2) fix: raw J2534
    /// error code `PassThruStopPeriodicMsg` returns instead of its normal
    /// unconditional-success behavior, set via
    /// `__mock_set_stop_periodic_message_error`. `None` (the default after
    /// `__mock_reset`) means no override -- `PassThruStopPeriodicMsg` behaves
    /// normally. Checked before the real `channel.periodic_msgs.remove(...)`
    /// call, so a forced failure never simulates a partial stop -- the
    /// periodic message stays live, mirroring `stop_repeat_message_error`'s
    /// identical "checked before the real removal" convention. Lets
    /// `j2534-0404-service` tests exercise `rpc_cancel_com_primitive`'s
    /// restore-and-error path for a TP2.0 broadcast periodic COP's
    /// `CoptCancel` (Fix 2, PR #101) for a `MsgId` that is genuinely still
    /// alive and retransmitting.
    stop_periodic_message_error: Option<c_long>,
}

impl MockState {
    fn next_channel(&mut self) -> u32 {
        if self.next_channel_id == 0 {
            self.next_channel_id = 1;
        }
        let id = self.next_channel_id;
        self.next_channel_id = self.next_channel_id.saturating_add(1);
        id
    }
}

fn state() -> &'static Mutex<MockState> {
    static STATE: OnceLock<Mutex<MockState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(MockState::default()))
}

/// Reset-epoch counter for [`spawn_repeat_worker`] (edge-case-hunter fix, Bug
/// 4, ADR-165). Deliberately module-level and OUTSIDE `MockState` -- `
/// __mock_reset`'s `*guard = MockState::default()` must NOT reset this, or it
/// would defeat its own purpose. `__mock_reset` increments it; each
/// `spawn_repeat_worker` thread captures the value in effect right after its
/// slot was inserted (before the thread itself is spawned) and re-checks it
/// on every loop iteration, exiting immediately (no write, no slot removal --
/// it is not this epoch's slot to touch) the moment the live value no longer
/// matches. Without this, `__mock_reset` resets `next_channel_id`/every
/// channel's `next_repeat_msg_id` back to 1, so a fresh test's first
/// `START_REPEAT_MESSAGE` on channel 1 is near-guaranteed to reuse the exact
/// `(channel_id, msg_id)` key pair a lingering worker thread from the
/// PREVIOUS test (one that returned without stopping its own slot, or whose
/// STOP raced its own thread's next poll tick) is still watching -- that
/// worker has no way to distinguish "my original slot" from "a different
/// test's slot with the same key", and becomes an indistinguishable second
/// worker silently duplicating writes into `written_msgs` for the new slot's
/// entire remaining lifetime.
static REPEAT_WORKER_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn no_error() -> c_long {
    STATUS_NOERROR as c_long
}

fn err_null() -> c_long {
    ERR_NULL_PARAMETER as c_long
}

fn err_channel() -> c_long {
    ERR_INVALID_CHANNEL_ID as c_long
}

fn err_not_supported() -> c_long {
    ERR_NOT_SUPPORTED as c_long
}

fn err_buffer_empty() -> c_long {
    ERR_BUFFER_EMPTY as c_long
}

fn err_pin_invalid() -> c_long {
    ERR_PIN_INVALID as c_long
}

fn err_invalid_msg() -> c_long {
    ERR_INVALID_MSG as c_long
}

fn err_channel_in_use() -> c_long {
    ERR_CHANNEL_IN_USE as c_long
}

fn err_failed() -> c_long {
    ERR_FAILED as c_long
}

fn err_invalid_ioctl_param_id() -> c_long {
    ERR_INVALID_IOCTL_PARAM_ID as c_long
}

/// Maps a `CONFIG_NON_VOLATILE_STORE_1`..`_10` native parameter id to its
/// `MockState::non_volatile_store` index (`0..10`), or `None` for anything
/// else -- SAE J2534-2 clause 18 Device Configuration (ADR-176/Phase 14).
/// This is where `GET_DEVICE_CONFIG`/`SET_DEVICE_CONFIG`'s range enforcement
/// actually lives (ADR-176: not in `j2534-0404-service`'s service layer).
fn non_volatile_store_slot(parameter: u32) -> Option<usize> {
    match parameter {
        CONFIG_NON_VOLATILE_STORE_1 => Some(0),
        CONFIG_NON_VOLATILE_STORE_2 => Some(1),
        CONFIG_NON_VOLATILE_STORE_3 => Some(2),
        CONFIG_NON_VOLATILE_STORE_4 => Some(3),
        CONFIG_NON_VOLATILE_STORE_5 => Some(4),
        CONFIG_NON_VOLATILE_STORE_6 => Some(5),
        CONFIG_NON_VOLATILE_STORE_7 => Some(6),
        CONFIG_NON_VOLATILE_STORE_8 => Some(7),
        CONFIG_NON_VOLATILE_STORE_9 => Some(8),
        CONFIG_NON_VOLATILE_STORE_10 => Some(9),
        _ => None,
    }
}

// ── Calling-convention macro (mirrors iso22900-mock pattern) ──────────────────

#[cfg(all(windows, target_arch = "x86"))]
macro_rules! exported_fn {
    ($(#[$meta:meta])* $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty $body:block) => {
        $(#[$meta])*
        /// # Safety
        ///
        /// This function is exported as part of the mock J2534 API's C ABI.
        /// The caller must uphold the same contract as the real J2534 API
        /// entry point of this name: any pointer argument must be either
        /// null or point to a valid, correctly-sized, and appropriately
        /// aligned instance of the expected type for the duration of the
        /// call, and the caller must not assume thread-safety beyond what
        /// the J2534 API specifies.
        #[unsafe(no_mangle)]
        pub unsafe extern "stdcall" fn $name($($arg: $ty),*) -> $ret $body
    };
}

#[cfg(not(all(windows, target_arch = "x86")))]
macro_rules! exported_fn {
    ($(#[$meta:meta])* $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty $body:block) => {
        $(#[$meta])*
        /// # Safety
        ///
        /// This function is exported as part of the mock J2534 API's C ABI.
        /// The caller must uphold the same contract as the real J2534 API
        /// entry point of this name: any pointer argument must be either
        /// null or point to a valid, correctly-sized, and appropriately
        /// aligned instance of the expected type for the duration of the
        /// call, and the caller must not assume thread-safety beyond what
        /// the J2534 API specifies.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> $ret $body
    };
}

// ── PassThru API exports ──────────────────────────────────────────────────────

exported_fn!(PassThruOpen(p_name: *mut c_void, p_device_id: *mut u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.open += 1;
    guard.last_open_pname = if p_name.is_null() {
        None
    } else {
        // Read a NUL-terminated narrow string, mirroring how a real J2534
        // DLL reads `pName` (ADR-106's out-of-spec device-selection
        // extension). SAFETY: the caller contract (see `exported_fn!`)
        // requires a non-null pName to point at a valid NUL-terminated
        // buffer.
        let mut bytes = Vec::new();
        let mut ptr = p_name as *const u8;
        unsafe {
            while *ptr != 0 {
                bytes.push(*ptr);
                ptr = ptr.add(1);
            }
        }
        Some(bytes)
    };
    if p_device_id.is_null() {
        return err_null();
    }
    unsafe { *p_device_id = MOCK_DEVICE_ID; }
    no_error()
});

exported_fn!(PassThruClose(_device_id: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.close += 1;
    no_error()
});

exported_fn!(PassThruConnect(
    _device_id: u32,
    protocol_id: u32,
    flags: u32,
    baud_rate: u32,
    p_channel_id: *mut u32
) -> c_long {
    if p_channel_id.is_null() {
        return err_null();
    }
    let mut guard = state().lock().expect("mock state poisoned");
    if guard.max_channels != 0 && guard.channels.len() >= guard.max_channels {
        return ERR_EXCEEDED_LIMIT as c_long;
    }
    // SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 3/4/Phase
    // 2b) connect-time simulation: distinguishes a genuinely-unsupported
    // `_CHx` resource (out-of-scope family, or an index beyond this mock's
    // simulated capacity -- `ERR_NOT_SUPPORTED`) from a resource conflict
    // (the exact same `_CHx` id already has a physical channel open --
    // `ERR_RESOURCE_IN_USE`), per the spec's clause-7 text. A non-`_CHx`
    // `protocol_id` is entirely unaffected -- this mock performs no
    // protocol-id validation at all on a plain `PassThruConnect` (unlike
    // `IOCTL_GET_PROTOCOL_INFO`), and Phase 2b does not change that.
    if is_chx_protocol_id(protocol_id) {
        let Some((_base, index)) = chx_base_protocol_id(protocol_id) else {
            return ERR_NOT_SUPPORTED as c_long;
        };
        let capacity = guard.chx_capacity_override.unwrap_or(DEFAULT_CHX_CAPACITY);
        if index > capacity {
            return ERR_NOT_SUPPORTED as c_long;
        }
        if guard.channels.values().any(|c| c.protocol_id == protocol_id) {
            return ERR_RESOURCE_IN_USE as c_long;
        }
    }
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
    // `__mock_set_ndis_connect_error`'s activation-failure injection --
    // checked only for `PROTOCOL_ETHERNET_NDIS`, before the channel is
    // actually inserted (a forced failure never simulates a partial
    // connect).
    if protocol_id == PROTOCOL_ETHERNET_NDIS
        && let Some(code) = guard.ndis_connect_error
    {
        return code;
    }
    guard.counters.connect += 1;
    let channel_id = guard.next_channel();
    guard.channels.insert(channel_id, ChannelState::new(protocol_id, baud_rate, flags));
    let logged_flags = guard.channels[&channel_id].flags;
    guard.connect_flags_log.push(logged_flags);

    // SAE J1850 bus-flavor simulation (ADR-070): if the configured flavor
    // matches this connect's protocol id, queue a canned response frame
    // immediately so both an active probe (writes then reads) and a passive
    // listener (never writes) observe it.
    // ADR-157: compare against the base protocol id -- a `J1850VPW_PS`/
    // `J1850PWM_PS` connect must still be recognized by this flavor
    // simulation (`flavor` itself is always a base id, see
    // `__mock_set_j1850_bus_flavor`).
    if let Some(flavor) = guard.j1850_bus_flavor
        && flavor == base_protocol_id(protocol_id)
    {
        let response = if base_protocol_id(protocol_id) == PROTOCOL_J1850PWM {
            MOCK_J1850_PWM_RESPONSE
        } else {
            MOCK_J1850_VPW_RESPONSE
        };
        if let Some(channel) = guard.channels.get_mut(&channel_id) {
            channel.rx_queue.push_back(StoredMessage {
                protocol_id,
                rx_status: 0,
                tx_flags: 0,
                timestamp: MOCK_TIMESTAMP,
                data: response.to_vec(),
                extra_data_index: None,
            });
        }
    }

    unsafe { *p_channel_id = channel_id; }
    no_error()
});

exported_fn!(PassThruDisconnect(channel_id: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.disconnect += 1;
    guard.channels.remove(&channel_id);
    no_error()
});

exported_fn!(PassThruReadMsgs(
    channel_id: u32,
    p_msg: *mut PASSTHRU_MSG,
    p_num_msgs: *mut u32,
    _timeout: u32
) -> c_long {
    if p_msg.is_null() || p_num_msgs.is_null() {
        return err_null();
    }
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.read_msgs += 1;

    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    // SAE J2534-2 clause 6.3.3.2 (ADR-156 Decision 2): a `_PS` channel's
    // pins must be assigned via IOCTL_SET_CONFIG(CONFIG_J1962_PINS) before
    // any I/O runs. Checked after the channel_id validity check above (so a
    // bad channel_id still reports its own, more specific error).
    if !channel.pins_assigned {
        return err_pin_invalid();
    }
    // SAE J2534-2 clause 21.3.2.5.1/22.3.2.6.1 (ADR-213/Round 3): a `_CHx`
    // FD channel stays electrically inert (I/O rejected `ERR_PIN_INVALID`)
    // until CONFIG_FD_CAN_DATA_PHASE_RATE has been SET_CONFIG'd on it -- see
    // `is_fd_chx_protocol`'s own doc comment.
    if is_fd_chx_protocol(channel.protocol_id)
        && !channel.params.contains_key(&CONFIG_FD_CAN_DATA_PHASE_RATE)
    {
        return err_pin_invalid();
    }
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): clause 24.2.5.2
    // -- `PassThruReadMsgs` always returns `ERR_NOT_SUPPORTED` on this
    // protocol, unconditionally (Table 104), so this check runs BEFORE the
    // `read_msgs_error` injection below -- this permanent protocol
    // characteristic is never overridable by that generic per-test knob,
    // unlike every ordinary channel's read behavior.
    if is_ethernet_ndis_protocol(channel.protocol_id) {
        return err_not_supported();
    }
    // Error injection (`__mock_set_read_msgs_error`): checked after the
    // channel_id validity/pin-assignment checks above so a bad channel_id
    // or an unassigned-pins channel still reports its own, more specific
    // error, but before any queued frame is popped or *p_num_msgs/*p_msg is
    // touched -- a forced failure never simulates a partial read.
    if let Some(code) = guard.read_msgs_error {
        return code;
    }
    let channel = guard
        .channels
        .get_mut(&channel_id)
        .expect("channel presence just checked above");

    // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): once a
    // channel's sample rate is armed (nonzero via IOCTL_SET_CONFIG
    // (CONFIG_SAMPLE_RATE)), the device is simulated as producing readings
    // -- a fixed, deterministic 4-byte signed-LE millivolt value tagged with
    // the channel's own native protocol id, queued here when the RX queue is
    // otherwise empty so a connect -> set rate -> read test can observe data
    // without simulating real sample-rate timing. A rate of 0 (the default)
    // produces no readings at all, matching clause 10.3.3.2.2's own
    // "disabled" semantics.
    if is_analog_in_protocol(channel.protocol_id)
        && channel.params.get(&CONFIG_SAMPLE_RATE).copied().unwrap_or(0) != 0
        && channel.rx_queue.is_empty()
    {
        channel.rx_queue.push_back(StoredMessage {
            protocol_id: channel.protocol_id,
            rx_status: 0,
            tx_flags: 0,
            timestamp: MOCK_TIMESTAMP,
            data: MOCK_ANALOG_READING_MV.to_le_bytes().to_vec(),
            extra_data_index: None,
        });
    }

    let requested = unsafe { *p_num_msgs } as usize;
    let mut filled = 0usize;
    while filled < requested {
        let Some(msg) = channel.rx_queue.pop_front() else { break; };
        unsafe { msg.write_to_passthru(&mut *p_msg.add(filled)); }
        filled += 1;
    }

    unsafe { *p_num_msgs = filled as u32; }

    if filled == 0 {
        err_buffer_empty()
    } else {
        no_error()
    }
});

exported_fn!(PassThruWriteMsgs(
    channel_id: u32,
    p_msg: *mut PASSTHRU_MSG,
    p_num_msgs: *mut u32,
    _timeout: u32
) -> c_long {
    if p_msg.is_null() || p_num_msgs.is_null() {
        return err_null();
    }
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.write_msgs += 1;
    // Captured before `channel` below takes a mutable borrow of
    // `guard.channels` that stays live through this handler's later
    // channel-field checks (`loopback_enabled`, `protocol_id`, ...) --
    // mirrors `become_master_error`'s/`j1939_cancel_error`'s own
    // capture-before-borrow convention elsewhere in this file.
    let write_msgs_error = guard.write_msgs_error;

    let Some(channel) = guard.channels.get_mut(&channel_id) else {
        return err_channel();
    };
    // SAE J2534-2 clause 6.3.3.2 (ADR-156 Decision 2): a `_PS` channel's
    // pins must be assigned via IOCTL_SET_CONFIG(CONFIG_J1962_PINS) before
    // any I/O runs.
    if !channel.pins_assigned {
        return err_pin_invalid();
    }
    // SAE J2534-2 clause 21.3.2.5.1/22.3.2.6.1 (ADR-213/Round 3): see
    // `PassThruReadMsgs`'s identical gate -- `is_fd_chx_protocol`'s own doc
    // comment has the full rationale.
    if is_fd_chx_protocol(channel.protocol_id)
        && !channel.params.contains_key(&CONFIG_FD_CAN_DATA_PHASE_RATE)
    {
        return err_pin_invalid();
    }
    // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): clause 10
    // defines this protocol as read-only -- the service layer already
    // rejects a write before it reaches this mock, but per clause 10's own
    // text the device itself must also reject it (defense in depth, and so
    // this mock behaves like a real spec-conforming adapter if ever driven
    // directly).
    if is_analog_in_protocol(channel.protocol_id) {
        return err_not_supported();
    }
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): clause
    // 24.2.5.3 -- `PassThruWriteMsgs` always returns `ERR_NOT_SUPPORTED` on
    // this protocol, unconditionally.
    if is_ethernet_ndis_protocol(channel.protocol_id) {
        return err_not_supported();
    }

    // Error injection (`__mock_set_write_msgs_error`): checked after the
    // channel_id/pins/analog-in/Ethernet_NDIS validity checks above (so
    // those still report their own, more specific error) but before the
    // message batch is processed/stored/echoed -- a forced failure never
    // simulates a partial write.
    if let Some(code) = write_msgs_error {
        return code;
    }

    let count = unsafe { *p_num_msgs } as usize;
    let loopback = channel.loopback_enabled();

    // SAE J2534-2 clause 16.5 (ADR-179/Phase 5): `ERR_ADDRESS_NOT_CLAIMED`
    // simulation for a J1939 channel -- a message whose data payload (the
    // bytes after the clause 16.4.3 5-byte ID+destination prefix) exceeds
    // 8 bytes, sent with a source address (`Data[3]`, the same byte
    // `tx_header::j1939_header_bytes` composes) that is not currently in
    // `j1939_claimed_addresses`, is rejected outright -- checked as a
    // read-only pre-pass over the whole batch (mirroring this codebase's
    // "a forced failure never simulates a partial write" convention at
    // every other error-injection site in this file) so no message in the
    // batch is written/echoed if any one of them violates this rule.
    // `is_j1939_family_protocol` (Codex review finding, PR #117), NOT
    // `is_j1939_protocol`'s own `_PS`-only exact match -- this enforcement
    // is a J1939-family behavior, not a `_PS`-only pin-gating one, so it
    // must also apply to a `_CHx` channel (ADR-206) the same as the `_PS`
    // channel does.
    if is_j1939_family_protocol(channel.protocol_id) {
        for i in 0..count {
            let raw = unsafe { &*p_msg.add(i) };
            let len = (raw.DataSize as usize).min(raw.Data.len());
            let data = &raw.Data[..len];
            if data.len() > 13
                && let Some(&source_address) = data.get(3)
                && !channel.j1939_claimed_addresses.contains(&source_address)
            {
                return ERR_ADDRESS_NOT_CLAIMED as c_long;
            }
        }
    }

    // SAE J2534-2 clause 19.4.4 (ADR-188/Phase 7 Stage 7a): a TP2.0 write
    // whose leading 4-byte address prefix does not match any of this
    // channel's established connections' own TX-IDs is rejected with
    // `ERR_NO_CONNECTION_ESTABLISHED` when it exceeds a single raw CAN
    // frame's own size -- checked as the same read-only pre-pass-over-the-
    // whole-batch shape the J1939 `ERR_ADDRESS_NOT_CLAIMED` check above
    // uses, so no message in the batch is written/echoed if any one
    // violates this rule. A non-connection write that DOES fit a single CAN
    // frame (clause 19.4.4's third routing case) is accepted here for mock/
    // spec completeness even though this service's own payload-only TX
    // contract for a connection-bound CLL never constructs one (ADR-188 §7).
    // ADR-210 Decision item 14: re-keyed from the narrow `is_tp2_0_protocol`
    // to `is_tp2_0_family_protocol` -- a `_CHx`-connected TP2.0 channel would
    // otherwise skip this connection-bound write-size-limit validation.
    if is_tp2_0_family_protocol(channel.protocol_id) {
        for i in 0..count {
            let raw = unsafe { &*p_msg.add(i) };
            let len = (raw.DataSize as usize).min(raw.Data.len());
            let data = &raw.Data[..len];
            if data.len() < 4 {
                continue;
            }
            let prefix = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let matches_established = channel
                .tp20_connections
                .values()
                .any(|c| c.tx_id == prefix);
            if !matches_established && data.len() > TP20_SINGLE_CAN_MSG_MAX_LEN {
                return ERR_NO_CONNECTION_ESTABLISHED as c_long;
            }
        }
    }

    // SAE J2534-2 clause 19.3.2.2 TP2.0 broadcast send (ADR-192/Phase 7
    // Stage 7c): `Data[0]`-range validation for any message in this batch
    // carrying `TX_FLAG_TP2_0_BROADCAST_MSG`, checked as the same read-only
    // pre-pass-over-the-whole-batch shape the two checks above use -- defense
    // in depth alongside `j2534-0404-service`'s own pre-native-call
    // rejection (`rpc_primitive.rs`), so this mock behaves like a real
    // spec-conforming adapter if ever driven directly.
    //
    // Gated on `is_tp2_0_family_protocol(channel.protocol_id)`, NOT the
    // TxFlags bit alone (Codex-review-class regression, found via
    // `j1708.rs::message_priority_in_range_reaches_native_tx_flags` failing):
    // `TX_FLAG_TP2_0_BROADCAST_MSG` is bit 16 (`0x1_0000`), which lies
    // squarely inside `TX_FLAG_MSG_PRIORITY_VALUE`'s own bits 16-19 --
    // SAE J2534-2 clause 17.4.5's `CP_MessagePriority` values whose low bit
    // is set (any odd 1-7) set this identical bit position on an entirely
    // unrelated J1708 send. The two features are mutually exclusive by
    // protocol in the real service (`apply_resolved_tx_flags` only ORs each
    // bit in under its own protocol gate), so this mock must reproduce that
    // same protocol gate rather than keying off the bit pattern alone.
    // ADR-210 Decision item 14: re-keyed from the narrow `is_tp2_0_protocol`
    // to `is_tp2_0_family_protocol`, mirroring the write-size-limit fix
    // above.
    if is_tp2_0_family_protocol(channel.protocol_id) {
        for i in 0..count {
            let raw = unsafe { &*p_msg.add(i) };
            if raw.TxFlags & j2534_0404_sys::bindings::TX_FLAG_TP2_0_BROADCAST_MSG == 0 {
                continue;
            }
            let len = (raw.DataSize as usize).min(raw.Data.len());
            match raw.Data[..len].first() {
                Some(&addr) if (0xF0..=0xFF).contains(&addr) => {}
                _ => return ERR_INVALID_MSG as c_long,
            }
        }
    }

    for i in 0..count {
        let raw = unsafe { &*p_msg.add(i) };
        let stored = StoredMessage::from_passthru(raw);
        // ADR-210 Decision item 14: re-keyed from the narrow
        // `is_tp2_0_protocol` to `is_tp2_0_family_protocol`, mirroring this
        // function's own fixes above.
        if is_tp2_0_family_protocol(channel.protocol_id)
            && stored.tx_flags & j2534_0404_sys::bindings::TX_FLAG_TP2_0_BROADCAST_MSG != 0
        {
            // SAE J2534-2 clause 19.3.2.2 (ADR-192/Phase 7 Stage 7c): a
            // single `PassThruWriteMsgs` call carrying this flag (with a
            // Data[0]-range-validated address, per the pre-pass above)
            // simulates the device sending the message 5x, alternating the
            // last two data bytes 0xAA/0x55 each transmission -- not the
            // single write this loop otherwise performs.
            push_tp20_broadcast_burst(channel, &stored, loopback);
            continue;
        }
        if loopback {
            let mut echo = stored.clone();
            echo.rx_status = RX_TX_MSG_TYPE | tx_format_flags_to_rx_status_bits(stored.tx_flags);
            echo.timestamp = MOCK_TIMESTAMP;
            note_rx_frame_for_repeat_slots(channel, &echo.data, echo.rx_status, echo.protocol_id);
            channel.rx_queue.push_back(echo);
        }
        channel.record_written(stored);
    }

    // One-shot write-triggered RX injection (`__mock_arm_write_rx_injection`):
    // simulates an RX frame (e.g. a FlowControl) landing on the wire while a
    // real adapter's write call is still blocked doing slow hardware I/O.
    // The frame is queued and the armed state cleared BEFORE the state lock
    // is dropped (so a concurrent `PassThruReadMsgs` sees it as soon as the
    // sleep below starts), then this call blocks for `hold_ms` before
    // returning, exactly as a slow real write would.
    let hold_ms = channel.write_rx_injection.take().map(|injection| {
        note_rx_frame_for_repeat_slots(
            channel,
            &injection.frame.data,
            injection.frame.rx_status,
            injection.frame.protocol_id,
        );
        channel.rx_queue.push_back(injection.frame);
        injection.hold_ms
    });

    drop(guard);
    if let Some(hold_ms) = hold_ms {
        std::thread::sleep(std::time::Duration::from_millis(hold_ms as u64));
    }

    no_error()
});

exported_fn!(PassThruStartPeriodicMsg(
    channel_id: u32,
    p_msg: *mut PASSTHRU_MSG,
    p_msg_id: *mut u32,
    time_interval: u32
) -> c_long {
    if p_msg.is_null() || p_msg_id.is_null() {
        return err_null();
    }
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.start_periodic += 1;

    let Some(channel) = guard.channels.get_mut(&channel_id) else {
        return err_channel();
    };
    // SAE J2534-2 clause 6.3.3.2 (ADR-156 Decision 2): a `_PS` channel's
    // pins must be assigned via IOCTL_SET_CONFIG(CONFIG_J1962_PINS) before
    // a periodic message can be registered.
    if !channel.pins_assigned {
        return err_pin_invalid();
    }
    // SAE J2534-2 clause 21.3.2.5.1/22.3.2.6.1 (ADR-213/Round 3): see
    // `PassThruReadMsgs`'s identical gate -- `is_fd_chx_protocol`'s own doc
    // comment has the full rationale.
    if is_fd_chx_protocol(channel.protocol_id)
        && !channel.params.contains_key(&CONFIG_FD_CAN_DATA_PHASE_RATE)
    {
        return err_pin_invalid();
    }
    // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): clause 10
    // defines this protocol as read-only -- same defense-in-depth reasoning
    // as `PassThruWriteMsgs`'s identical gate above.
    if is_analog_in_protocol(channel.protocol_id) {
        return err_not_supported();
    }
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): clause
    // 24.2.5.4 -- `PassThruStartPeriodicMsg` always returns
    // `ERR_NOT_SUPPORTED` on this protocol, unconditionally.
    if is_ethernet_ndis_protocol(channel.protocol_id) {
        return err_not_supported();
    }

    let stored = unsafe { StoredMessage::from_passthru(&*p_msg) };
    // Gated on protocol too, not just the TxFlags bit -- see
    // `PassThruWriteMsgs`'s identical gate and its own doc comment for why
    // (`TX_FLAG_TP2_0_BROADCAST_MSG` aliases `TX_FLAG_MSG_PRIORITY_VALUE`'s
    // low bit).
    // ADR-210 Decision item 14: re-keyed from the narrow `is_tp2_0_protocol`
    // to `is_tp2_0_family_protocol`, mirroring `PassThruWriteMsgs`'s own
    // identical fixes above -- this gate is `PassThruStartPeriodicMsg`'s own
    // counterpart.
    let is_tp20_broadcast = is_tp2_0_family_protocol(channel.protocol_id)
        && stored.tx_flags & j2534_0404_sys::bindings::TX_FLAG_TP2_0_BROADCAST_MSG != 0;
    // SAE J2534-2 clause 19.3.2.2 (ADR-192/Phase 7 Stage 7c): the same
    // `Data[0]`-range validation `PassThruWriteMsgs` performs -- defense in
    // depth alongside `j2534-0404-service`'s own pre-native-call rejection.
    if is_tp20_broadcast && !matches!(stored.data.first(), Some(&addr) if (0xF0..=0xFF).contains(&addr))
    {
        return ERR_INVALID_MSG as c_long;
    }

    channel.next_periodic_id = channel.next_periodic_id.saturating_add(1).max(1);
    let periodic_id = channel.next_periodic_id;
    // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c): a broadcast
    // periodic start immediately sends the same 5x `T_BR_INT`-spaced burst
    // `PassThruWriteMsgs`'s own broadcast case sends, before settling into
    // this call's own `time_interval`-paced periodic rate (tracked below,
    // unchanged, exactly like a non-broadcast periodic message).
    if is_tp20_broadcast {
        let loopback = channel.loopback_enabled();
        push_tp20_broadcast_burst(channel, &stored, loopback);
    }
    channel.periodic_msgs.insert(periodic_id, stored);
    channel.last_start_periodic_time_interval = Some(time_interval);
    unsafe { *p_msg_id = periodic_id; }
    no_error()
});

exported_fn!(PassThruStopPeriodicMsg(channel_id: u32, msg_id: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.stop_periodic += 1;
    THREAD_COUNTERS.with(|c| c.stop_periodic.set(c.stop_periodic.get() + 1));
    // Error injection (`__mock_set_stop_periodic_message_error`): checked
    // before the real `channel.periodic_msgs.remove(...)` below, mirroring
    // `stop_repeat_message_error`'s identical convention -- a forced failure
    // never simulates a partial stop, the periodic message stays live.
    if let Some(code) = guard.stop_periodic_message_error {
        return code;
    }
    if let Some(channel) = guard.channels.get_mut(&channel_id) {
        channel.periodic_msgs.remove(&msg_id);
    }
    no_error()
});

exported_fn!(PassThruStartMsgFilter(
    channel_id: u32,
    filter_type: u32,
    p_mask: *mut PASSTHRU_MSG,
    p_pattern: *mut PASSTHRU_MSG,
    p_flow_control: *mut PASSTHRU_MSG,
    p_filter_id: *mut u32
) -> c_long {
    if p_filter_id.is_null() || p_mask.is_null() || p_pattern.is_null() {
        return err_null();
    }
    let mask = unsafe { StoredMessage::from_passthru(&*p_mask) };
    let pattern = unsafe { StoredMessage::from_passthru(&*p_pattern) };
    let flow_control = if p_flow_control.is_null() {
        None
    } else {
        Some(unsafe { StoredMessage::from_passthru(&*p_flow_control) })
    };

    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.start_filter += 1;

    let Some(channel) = guard.channels.get_mut(&channel_id) else {
        return err_channel();
    };
    // SAE J2534-2 clause 6.3.3.2 (ADR-156 Decision 2): a `_PS` channel's
    // pins must be assigned via IOCTL_SET_CONFIG(CONFIG_J1962_PINS) before
    // a filter can be installed.
    if !channel.pins_assigned {
        return err_pin_invalid();
    }
    // SAE J2534-2 clause 21.3.2.5.1/22.3.2.6.1 (ADR-213/Round 3): see
    // `PassThruReadMsgs`'s identical gate -- `is_fd_chx_protocol`'s own doc
    // comment has the full rationale.
    if is_fd_chx_protocol(channel.protocol_id)
        && !channel.params.contains_key(&CONFIG_FD_CAN_DATA_PHASE_RATE)
    {
        return err_pin_invalid();
    }
    // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): clause 10
    // defines no filter concept at all -- same defense-in-depth reasoning as
    // `PassThruWriteMsgs`'s identical gate above.
    if is_analog_in_protocol(channel.protocol_id) {
        return err_not_supported();
    }
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): clause
    // 24.2.5.5 -- `PassThruStartMsgFilter` always returns
    // `ERR_NOT_SUPPORTED` on this protocol, unconditionally.
    if is_ethernet_ndis_protocol(channel.protocol_id) {
        return err_not_supported();
    }
    // Enforces the limit `IOCTL_GET_PROTOCOL_INFO` advertises
    // (`PROTOCOL_INFO_MAX_PASS_FILTER`/`MAX_BLOCK_FILTER` = 10, ADR-153) so
    // Discovery's claim matches actual behavior (Codex review, PR #25).
    // `FLOW_CONTROL_FILTER` is left uncapped since the mock reports
    // NOT_SUPPORTED for `PROTOCOL_INFO_MAX_FLOW_CONTROL_FILTER` -- it makes
    // no numeric claim to keep consistent.
    if matches!(filter_type, PASS_FILTER | BLOCK_FILTER) {
        let existing_of_type = channel
            .filters
            .iter()
            .filter(|(_, f)| f.filter_type == filter_type)
            .count();
        if existing_of_type >= MAX_FILTERS_PER_TYPE {
            return ERR_EXCEEDED_LIMIT as c_long;
        }
    }
    channel.next_filter_id = channel.next_filter_id.saturating_add(1).max(1);
    let filter_id = channel.next_filter_id;
    channel.filters.push((filter_id, MockFilter { filter_type, mask, pattern, flow_control }));
    unsafe { *p_filter_id = filter_id; }
    no_error()
});

exported_fn!(PassThruStopMsgFilter(channel_id: u32, filter_id: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    if !guard.channels.contains_key(&channel_id) {
        return err_channel();
    }
    // Error injection (`__mock_set_stop_filter_error`): checked after the
    // channel_id validity check above so a bad channel_id still reports its
    // own error, but before the success counter increments or the filter is
    // removed -- a forced failure never simulates a partial success.
    if let Some(code) = guard.stop_filter_error {
        return code;
    }
    guard.counters.stop_filter += 1;
    let channel = guard
        .channels
        .get_mut(&channel_id)
        .expect("channel presence just checked above");
    channel.filters.retain(|(id, _)| *id != filter_id);
    no_error()
});

exported_fn!(PassThruSetProgrammingVoltage(
    _device_id: u32,
    _pin_number: u32,
    _voltage: u32
) -> c_long {
    // Error injection (`__mock_set_prog_voltage_error`): checked before the
    // (currently unconditional) success behavior, mirroring
    // `PassThruStopMsgFilter`'s override check above.
    let guard = state().lock().expect("mock state poisoned");
    if let Some(code) = guard.prog_voltage_error {
        return code;
    }
    drop(guard);
    no_error()
});

exported_fn!(PassThruReadVersion(
    _device_id: u32,
    p_firmware: *mut c_char,
    p_dll: *mut c_char,
    p_api: *mut c_char
) -> c_long {
    if p_firmware.is_null() || p_dll.is_null() || p_api.is_null() {
        return err_null();
    }
    unsafe {
        write_version_str(p_firmware, FIRMWARE_VERSION);
        write_version_str(p_dll, DLL_VERSION);
        write_version_str(p_api, API_VERSION);
    }
    no_error()
});

unsafe fn write_version_str(dst: *mut c_char, src: &[u8]) {
    let len = src.len().min(VERSION_BUF_SIZE);
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr() as *const c_char, dst, len);
        *dst.add(len - 1) = 0;
    }
}

exported_fn!(PassThruGetLastError(p_error_desc: *mut c_char) -> c_long {
    if p_error_desc.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let msg = if guard.last_error.is_empty() {
        b"No error\0" as &[u8]
    } else {
        guard.last_error.as_bytes()
    };
    unsafe {
        let len = msg.len().min(VERSION_BUF_SIZE - 1);
        std::ptr::copy_nonoverlapping(msg.as_ptr() as *const c_char, p_error_desc, len);
        *p_error_desc.add(len) = 0;
    }
    no_error()
});

exported_fn!(PassThruIoctl(
    channel_id: u32,
    ioctl_id: u32,
    p_input: *mut c_void,
    p_output: *mut c_void
) -> c_long {
    match ioctl_id {
        x if x == IOCTL_GET_CONFIG => {
            if p_input.is_null() {
                return err_null();
            }
            let list = unsafe { &mut *(p_input as *mut SCONFIG_LIST) };
            if list.ConfigPtr.is_null() {
                return err_null();
            }
            let mut guard = state().lock().expect("mock state poisoned");
            guard.counters.get_config += 1;
            let Some(channel) = guard.channels.get(&channel_id) else {
                return err_channel();
            };
            let configs = unsafe {
                std::slice::from_raw_parts_mut(list.ConfigPtr, list.NumOfParams as usize)
            };
            for cfg in configs {
                cfg.Value = channel.params.get(&cfg.Parameter).copied().unwrap_or(0);
            }
            no_error()
        }

        x if x == IOCTL_SET_CONFIG => {
            if p_input.is_null() {
                return err_null();
            }
            let list = unsafe { &*(p_input as *const SCONFIG_LIST) };
            if list.ConfigPtr.is_null() {
                return err_null();
            }
            let configs = unsafe {
                std::slice::from_raw_parts(list.ConfigPtr as *const SCONFIG, list.NumOfParams as usize)
            };
            let mut guard = state().lock().expect("mock state poisoned");
            guard.counters.set_config += 1;
            // Read before `channel` (below) takes a mutable borrow of
            // `guard.channels` that outlives this arm's remaining checks --
            // a `bool` copy avoids the disjoint-field-borrow conflict a
            // later `guard.can_mixed_format_unsupported` read alongside a
            // live `channel` borrow would hit.
            let can_mixed_format_unsupported = guard.can_mixed_format_unsupported;
            let Some(channel) = guard.channels.get_mut(&channel_id) else {
                return err_channel();
            };
            // SAE J2534-2 clause 21.3.2.5.1/22.3.2.6.1 (ADR-158/Phase 3a,
            // ADR-159/Phase 3b): on an FD_CAN_PS/FD_ISO15765_PS channel (or,
            // as of ADR-213/Round 3, an FD `_CHx` channel -- this rule
            // applies uniformly across `_PS` and `_CHx`, so this check uses
            // the family-wide `is_fd_family_protocol` rather than the narrow
            // `is_fd_protocol`), BIT_SAMPLE_POINT/SYNC_JUMP_WIDTH become
            // read-only -- rejects
            // the whole batch (matching real hardware, where an unsupported
            // param in a batched SET_CONFIG call fails the call) rather than
            // silently applying the rest.
            if is_fd_family_protocol(channel.protocol_id)
                && configs.iter().any(|cfg| {
                    cfg.Parameter == CONFIG_BIT_SAMPLE_POINT
                        || cfg.Parameter == CONFIG_SYNC_JUMP_WIDTH
                })
            {
                return err_not_supported();
            }
            // SAE J2534-2 clause 10.3.3.2.6-.2.8 Analog Inputs (ADR-216):
            // `CONFIG_SAMPLE_RESOLUTION`/`CONFIG_INPUT_RANGE_LOW`/
            // `CONFIG_INPUT_RANGE_HIGH` are genuinely read-only -- a
            // `SET_CONFIG` on any of the three is rejected with
            // `ERR_INVALID_IOCTL_PARAM_ID`, rejecting the whole batch, same
            // convention as the FD-only gate above.
            // `j2534-0404-service`'s own `rpc_set_com_param` already rejects
            // a client attempt before ever reaching this call (ADR-216
            // Decision item 5); this is a backstop for a direct mock client,
            // matching the FD/mixed-format gates' own "service-side check is
            // primary, this is the backstop" convention.
            if configs.iter().any(|cfg| {
                cfg.Parameter == CONFIG_SAMPLE_RESOLUTION
                    || cfg.Parameter == CONFIG_INPUT_RANGE_LOW
                    || cfg.Parameter == CONFIG_INPUT_RANGE_HIGH
            }) {
                return err_invalid_ioctl_param_id();
            }
            // SAE J2534-2 clause 10.3.3.2.3/.2.4 Analog Inputs (ADR-216):
            // `CONFIG_SAMPLES_PER_READING`/`CONFIG_READINGS_PER_MSG` are
            // rejected with `ERR_NOT_SUPPORTED` whenever this channel's
            // currently-staged `CONFIG_SAMPLE_RATE` is already nonzero --
            // clause 10.3.3.2.3/.2.4's own rate-must-be-zero requirement.
            // This is what proves `j2534-0404-service`'s own connect-time
            // reorder (ADR-216 Decision item 9: the generic ComParam batch
            // applies BEFORE `CONFIG_SAMPLE_RATE` is armed) actually matters
            // end-to-end -- without that reorder, staging either of these
            // two ComParams together with a nonzero `CP_AnalogSampleRate` on
            // the same connect would always fail against this mock.
            if configs.iter().any(|cfg| {
                cfg.Parameter == CONFIG_SAMPLES_PER_READING || cfg.Parameter == CONFIG_READINGS_PER_MSG
            }) && channel.params.get(&CONFIG_SAMPLE_RATE).copied().unwrap_or(0) != 0
            {
                return err_not_supported();
            }
            // SAE J2534-2 clause 8 Mixed Format Frames on a CAN Network
            // (ADR-160/Phase 3c): `CONFIG_CAN_MIXED_FORMAT` is only valid on
            // an ISO15765-family channel (rejects the whole batch, same
            // convention as the FD-only gate above) -- or unconditionally,
            // regardless of protocol, when `__mock_set_can_mixed_format_
            // unsupported` has forced this device to simulate not
            // implementing clause 8 at all.
            if configs
                .iter()
                .any(|cfg| cfg.Parameter == CONFIG_CAN_MIXED_FORMAT)
                && (can_mixed_format_unsupported
                    || base_protocol_id(channel.protocol_id) != PROTOCOL_ISO15765)
            {
                return err_not_supported();
            }
            // SAE J2534-2 clause 6.3.2.7 (ADR-160 Correction): every
            // PassThruIoctl other than the pin-assignment SET_CONFIG itself
            // is rejected with PDU_ERR_PIN_INVALID until a `_PS`/`_CHx`-
            // qualified channel's pins are bound. This is the opposite
            // direction from the FD_CAN_DATA_PHASE_RATE-before-pins check
            // below (clause 21.3.2.5.1's carve-out requires that rate BE
            // SET before pins) -- clause 8 (SS8.2.1) grants
            // CONFIG_CAN_MIXED_FORMAT no such exception to clause 6.3.2.7's
            // general rule, so on a qualified channel pins must already be
            // bound before this param can be set. Checked against
            // `pins_assigned` state already present on the channel BEFORE
            // this call's own entries are applied (mirroring the
            // FD-rate-before-pins check's own "a single call carrying both
            // params does not itself satisfy the ordering" convention) and
            // rejects the whole batch, same convention as the gates above.
            if configs
                .iter()
                .any(|cfg| cfg.Parameter == CONFIG_CAN_MIXED_FORMAT)
                && !channel.pins_assigned
            {
                return err_pin_invalid();
            }
            // SAE J2534-2 clause 6.3.3.2 (ADR-156 Decision 2): pins are
            // bound exactly once per channel, immutably, until the channel
            // is torn down. A channel whose pins are already assigned --
            // whether because a prior CONFIG_J1962_PINS call already bound
            // them, or because it was opened with a non-`_PS` protocol id
            // (pins are meaningless/already-implicit there, so re-setting
            // them isn't a legitimate operation either) -- rejects a
            // CONFIG_J1962_PINS call outright: nothing in this call is
            // applied.
            let mut newly_bound_pins: Option<u32> = None;
            if let Some(cfg) = configs.iter().find(|cfg| cfg.Parameter == CONFIG_J1962_PINS) {
                if channel.pins_assigned {
                    return err_channel_in_use();
                }
                // SAE J2534-2 clause 21.3.2.5.1/22's identical rule
                // (ADR-158/Phase 3a, ADR-159/Phase 3b -- clause 22 reuses
                // clause 21's own CONFIG_FD_CAN_DATA_PHASE_RATE mechanism
                // unchanged, no clause-22-specific mock config):
                // FD_CAN_DATA_PHASE_RATE must be SET_CONFIG'd on this
                // channel before CONFIG_J1962_PINS -- checked against
                // state already present on the channel BEFORE this call's
                // own entries are applied below (a single call carrying
                // both params does not itself satisfy the "before"
                // ordering).
                if is_fd_protocol(channel.protocol_id)
                    && !channel.params.contains_key(&CONFIG_FD_CAN_DATA_PHASE_RATE)
                {
                    return err_failed();
                }
                // SAE J2534-2 clause 6.3.3.2: `0x00000000` is the packed
                // bitmask's own "no selection performed" sentinel, not a
                // real pin assignment (`compute_pin_select`, the service's
                // only producer of a real value, never packs it -- every
                // entry requires a concrete nonzero pin number). Leave
                // `pins_assigned` false for it (Codex review, PR #28): a
                // direct mock client sending the sentinel value must not
                // silently defeat the `ERR_PIN_INVALID` I/O gate, and a
                // follow-up call with a real value is still legitimate
                // since nothing was actually bound here.
                if cfg.Value != 0 {
                    channel.pins_assigned = true;
                    newly_bound_pins = Some(cfg.Value);
                }
            }
            // SAE J2534-2 clause 19.3.1/Table 77 TP2.0 passive connections
            // (ADR-190/Phase 7 Stage 7b): `CONFIG_TP2_0_IDENTIFER` is `0`
            // (disarmed) or `0x200-0x2EF`; `CONFIG_TP2_0_RXIDPASSIVE` is `0`
            // or `0x300-0x7FF`. Rejects the WHOLE BATCH with
            // `ERR_INVALID_IOCTL_VALUE`, same convention as the FD-rate-
            // before-pins/mixed-format gates above, rather than silently
            // applying the rest -- `j2534-0404-service`'s own passive arm
            // (`events_tp20_connection::resolve_tp20_passive_params`)
            // already validates these ranges service-side before ever
            // issuing this call, so this is a backstop, not the primary
            // gate (ADR-190 section 1).
            for cfg in configs {
                if cfg.Parameter == CONFIG_TP2_0_IDENTIFER
                    && cfg.Value != 0
                    && !(0x200..=0x2EF).contains(&cfg.Value)
                {
                    return ERR_INVALID_IOCTL_VALUE as c_long;
                }
                if cfg.Parameter == CONFIG_TP2_0_RXIDPASSIVE
                    && cfg.Value != 0
                    && !(0x300..=0x7FF).contains(&cfg.Value)
                {
                    return ERR_INVALID_IOCTL_VALUE as c_long;
                }
            }
            for cfg in configs {
                // SAE J2534-2 clause 8 Table 5 (ADR-160/Phase 3c): a
                // `CONFIG_CAN_MIXED_FORMAT` value change clears this
                // channel's TX/RX queue state and deletes its recorded
                // `PASS_FILTER`/`BLOCK_FILTER`s. `j2534-0404-service` never
                // actually reaches this path with pre-existing traffic/
                // filters (Decision 1's "exactly once, before any traffic"
                // invariant, ADR-160) -- simulated here anyway so a test
                // exercising the mock directly can assert it.
                if cfg.Parameter == CONFIG_CAN_MIXED_FORMAT && cfg.Value != channel.can_mixed_format
                {
                    channel.can_mixed_format = cfg.Value;
                    channel.rx_queue.clear();
                    channel.written_msgs.clear();
                    channel.written_at.clear();
                    channel
                        .filters
                        .retain(|(_, f)| f.filter_type != PASS_FILTER && f.filter_type != BLOCK_FILTER);
                }
                channel.params.insert(cfg.Parameter, cfg.Value);
                channel.set_config_param_log.push(cfg.Parameter);
            }

            // ADR-156/157 Bug B regression coverage: pin-gated J1850
            // bus-flavor simulation (`__mock_set_j1850_bus_flavor_requiring
            // _pins`) -- unlike `j1850_bus_flavor` (answers unconditionally
            // at `PassThruConnect` time), this variant only queues a canned
            // response once the pins actually bound by THIS call match the
            // configured requirement exactly, proving a probe that skips (or
            // mis-binds) pins never observes a response reachable only
            // through the caller's actually-requested wiring.
            if let Some(applied_pins) = newly_bound_pins {
                let channel_protocol_id = channel.protocol_id;
                if let Some((flavor, required_pin_select)) = guard.j1850_bus_flavor_requiring_pins
                    && base_protocol_id(channel_protocol_id) == flavor
                    && applied_pins == required_pin_select
                {
                    let response = if flavor == PROTOCOL_J1850PWM {
                        MOCK_J1850_PWM_RESPONSE
                    } else {
                        MOCK_J1850_VPW_RESPONSE
                    };
                    if let Some(channel) = guard.channels.get_mut(&channel_id) {
                        channel.rx_queue.push_back(StoredMessage {
                            protocol_id: channel_protocol_id,
                            rx_status: 0,
                            tx_flags: 0,
                            timestamp: MOCK_TIMESTAMP,
                            data: response.to_vec(),
                            extra_data_index: None,
                        });
                    }
                }
            }
            no_error()
        }

        x if x == IOCTL_READ_VBATT => {
            if p_output.is_null() {
                return err_null();
            }
            let mut guard = state().lock().expect("mock state poisoned");
            guard.counters.read_vbatt += 1;
            unsafe { *(p_output as *mut u32) = MOCK_VBATT_MV; }
            no_error()
        }

        x if x == IOCTL_READ_PROG_VOLTAGE => {
            if p_output.is_null() {
                return err_null();
            }
            unsafe { *(p_output as *mut u32) = MOCK_PROG_VOLTAGE_MV; }
            no_error()
        }

        // SAE J2534-2 clause 23 J1962 Pin Voltage Read (Phase 13): pins 4/5
        // and any pin outside 1-16 are always unsupported; pin 16 must
        // report the same voltage IOCTL_READ_VBATT would.
        x if x == IOCTL_READ_J1962PIN_VOLTAGE => {
            if p_input.is_null() || p_output.is_null() {
                return err_null();
            }
            // Error injection (`__mock_set_j1962_pin_voltage_error`): checked
            // before the pin-based validation below, mirroring
            // `prog_voltage_error`'s convention -- lets a test force a
            // generic native failure the pin checks alone can never produce.
            {
                let guard = state().lock().expect("mock state poisoned");
                if let Some(code) = guard.j1962_pin_voltage_error {
                    return code;
                }
            }
            let pin_number = unsafe { *(p_input as *const u32) };
            match pin_number {
                0 | 4 | 5 => return err_pin_invalid(),
                17.. => return err_pin_invalid(),
                16 => unsafe { *(p_output as *mut u32) = MOCK_VBATT_MV; },
                _ => unsafe { *(p_output as *mut u32) = MOCK_J1962_PIN_VOLTAGE_MV; },
            }
            no_error()
        }

        // SAE J2534-2 clause 18 Device Configuration (ADR-176, Phase 14): DeviceID-scoped,
        // like IOCTL_GET_DEVICE_INFO/GET_PROTOCOL_INFO -- no channel lookup. `channel_id`
        // here is actually the raw DeviceID handle the native call passed (PassThruIoctl's
        // exported signature names this param generically).
        x if x == IOCTL_GET_DEVICE_CONFIG => {
            if p_input.is_null() {
                return err_null();
            }
            let list = unsafe { &mut *(p_input as *mut SCONFIG_LIST) };
            if list.ConfigPtr.is_null() {
                return err_null();
            }
            let configs = unsafe {
                std::slice::from_raw_parts_mut(list.ConfigPtr, list.NumOfParams as usize)
            };
            let guard = state().lock().expect("mock state poisoned");
            for cfg in configs.iter_mut() {
                let Some(slot) = non_volatile_store_slot(cfg.Parameter) else {
                    return err_invalid_ioctl_param_id();
                };
                cfg.Value = guard.non_volatile_store[slot];
            }
            no_error()
        }

        x if x == IOCTL_SET_DEVICE_CONFIG => {
            if p_input.is_null() {
                return err_null();
            }
            let list = unsafe { &*(p_input as *const SCONFIG_LIST) };
            if list.ConfigPtr.is_null() {
                return err_null();
            }
            let configs = unsafe {
                std::slice::from_raw_parts(list.ConfigPtr as *const SCONFIG, list.NumOfParams as usize)
            };
            // Validate the WHOLE batch before writing anything (matching
            // IOCTL_SET_CONFIG's own "rejects the whole batch on an
            // unsupported param" precedent above): an invalid parameter_id in
            // a batched SET_DEVICE_CONFIG must not partially apply.
            let mut guard = state().lock().expect("mock state poisoned");
            let mut slots = Vec::with_capacity(configs.len());
            for cfg in configs {
                let Some(slot) = non_volatile_store_slot(cfg.Parameter) else {
                    return err_invalid_ioctl_param_id();
                };
                slots.push((slot, cfg.Value));
            }
            for (slot, value) in slots {
                guard.non_volatile_store[slot] = value;
            }
            no_error()
        }

        x if x == IOCTL_FIVE_BAUD_INIT => {
            let mut guard = state().lock().expect("mock state poisoned");
            // Real J2534 adapters only support the 5-baud/fast-init sequence
            // on the K-line protocols (ISO9141 / ISO14230); every other
            // protocol id connected via PassThruConnect returns
            // ERR_NOT_SUPPORTED.
            // ADR-157: also accept `ISO9141_PS`/`ISO14230_PS` -- a caller
            // with a `_PS` link can legitimately run 5-baud init on it.
            // ADR-170/Phase 9 (edge-case-hunter finding): also accept
            // `UART_ECHO_BYTE_PS` -- clause 12 is a K-line physical layer
            // too and needs FIVE_BAUD_INIT support the same way ISO9141/
            // ISO14230 do; `base_protocol_id` has no arm for it (it
            // self-identifies, see that function's own doc comment), so the
            // raw id itself is what this match sees.
            match guard.channels.get(&channel_id) {
                Some(channel)
                    if !matches!(
                        base_protocol_id(channel.protocol_id),
                        PROTOCOL_ISO9141 | PROTOCOL_ISO14230 | PROTOCOL_UART_ECHO_BYTE_PS
                    ) =>
                {
                    return err_not_supported();
                }
                None => return err_channel(),
                Some(_) => {}
            }
            // SAE J2534-2 clause 6.3.3.2 (ADR-156 Decision 2, Codex review PR
            // #28): 5-baud init communicates over the channel's DLC pins,
            // same as any other I/O -- a `_PS` channel's pins must be
            // assigned via IOCTL_SET_CONFIG(CONFIG_J1962_PINS) first. Checked
            // after the channel_id/protocol validity checks above (so those
            // still report their own, more specific error), mirroring every
            // other I/O IOCTL's pin gate in this file.
            if !guard.channels[&channel_id].pins_assigned {
                return err_pin_invalid();
            }
            guard.counters.five_baud_init += 1;
            // Record the target address byte the caller sent (ADR-076: the
            // KWP address is now resolved by `j2534-0404-service` at
            // `StartComPrimitive` call time -- either from
            // `CP_5BaudAddressFunc`/`CP_5BaudAddressPhys` or the legacy
            // `cop_data[0]` byte), so tests can assert it via
            // `mock_get_five_baud_init_input`.
            if !p_input.is_null() {
                let input = unsafe { &*(p_input as *const SBYTE_ARRAY) };
                if !input.BytePtr.is_null() && input.NumOfBytes >= 1 {
                    let address = unsafe { *input.BytePtr };
                    if let Some(channel) = guard.channels.get_mut(&channel_id) {
                        channel.five_baud_init_input = Some(address);
                    }
                }
            }
            // Simulate the adapter negotiating a baud rate during the 5-baud
            // init sequence -- a subsequent GET_CONFIG(CONFIG_DATA_RATE)
            // returns it (ADR-076).
            if let Some(channel) = guard.channels.get_mut(&channel_id) {
                channel.params.insert(
                    j2534_0404_sys::bindings::CONFIG_DATA_RATE,
                    MOCK_FIVE_BAUD_NEGOTIATED_BAUD,
                );
            }
            if !p_output.is_null() {
                let out = unsafe { &mut *(p_output as *mut SBYTE_ARRAY) };
                let write_len = MOCK_INIT_RESPONSE.len().min(out.NumOfBytes as usize);
                if !out.BytePtr.is_null() && write_len > 0 {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            MOCK_INIT_RESPONSE.as_ptr(),
                            out.BytePtr,
                            write_len,
                        );
                    }
                    out.NumOfBytes = write_len as u32;
                }
            }
            no_error()
        }

        x if x == IOCTL_FAST_INIT => {
            let mut guard = state().lock().expect("mock state poisoned");
            // Per J2534-1 v04.04, FIVE_BAUD_INIT and FAST_INIT are both
            // valid on ISO9141 and ISO14230 channels (the two K-line
            // protocols); every other protocol id connected via
            // PassThruConnect returns ERR_NOT_SUPPORTED, same as
            // IOCTL_FIVE_BAUD_INIT above. Also accepts `ISO9141_PS`/
            // `ISO14230_PS` (ADR-157), and `UART_ECHO_BYTE_PS`
            // (ADR-170/Phase 9, edge-case-hunter finding -- see
            // IOCTL_FIVE_BAUD_INIT's identical extension above).
            match guard.channels.get(&channel_id) {
                Some(channel)
                    if !matches!(
                        base_protocol_id(channel.protocol_id),
                        PROTOCOL_ISO9141 | PROTOCOL_ISO14230 | PROTOCOL_UART_ECHO_BYTE_PS
                    ) =>
                {
                    return err_not_supported();
                }
                None => return err_channel(),
                Some(_) => {}
            }
            // SAE J2534-2 clause 6.3.3.2 (ADR-156 Decision 2, Codex review PR
            // #28): fast init communicates over the channel's DLC pins, same
            // as any other I/O -- see IOCTL_FIVE_BAUD_INIT's identical gate
            // above.
            if !guard.channels[&channel_id].pins_assigned {
                return err_pin_invalid();
            }
            // Error injection (`__mock_set_fast_init_error`): checked after
            // the channel/protocol validity/pin-assignment checks above so a
            // bad `channel_id`/non-K-line protocol/unassigned-pins channel
            // still reports its own, more specific error, but before the
            // success counter increments or any input/output is touched --
            // lets tests exercise the init-failure path (ADR-077) without a
            // real adapter.
            if let Some(code) = guard.fast_init_error {
                return code;
            }
            guard.counters.fast_init += 1;
            // Record the wakeup frame the caller built (ADR-075: the KWP
            // header is now constructed by `j2534-0404-service` at
            // `StartComPrimitive` call time), so tests can assert its
            // addressing/content via `mock_get_fast_init_input`. Per
            // `fast_init_input`'s doc comment, a NULL `p_input` (ADR-077's
            // wakeup-only fast-init, whose optional service request the
            // D-PDU spec omits entirely) resets the recorded input to `None`
            // rather than leaving a previous call's value in place.
            if let Some(channel) = guard.channels.get_mut(&channel_id) {
                channel.fast_init_input = if p_input.is_null() {
                    None
                } else {
                    let input = unsafe { &*(p_input as *const PASSTHRU_MSG) };
                    let len = (input.DataSize as usize).min(input.Data.len());
                    Some(input.Data[..len].to_vec())
                };
            }
            if !p_output.is_null() {
                let out = unsafe { &mut *(p_output as *mut PASSTHRU_MSG) };
                if p_input.is_null() {
                    // ADR-077: no service request was sent, so per spec
                    // there is nothing for the ECU to respond to -- the
                    // adapter reports an empty response.
                    out.DataSize = 0;
                    out.ExtraDataIndex = 0;
                    out.Timestamp = MOCK_TIMESTAMP;
                } else {
                    let len = MOCK_FAST_INIT_RESPONSE.len().min(out.Data.len());
                    out.Data[..len].copy_from_slice(&MOCK_FAST_INIT_RESPONSE[..len]);
                    out.DataSize = len as u32;
                    out.ExtraDataIndex = out.DataSize;
                    out.Timestamp = MOCK_TIMESTAMP;
                }
            }
            no_error()
        }

        x if x == IOCTL_CLEAR_RX_BUFFER => {
            let mut guard = state().lock().expect("mock state poisoned");
            guard.counters.clear_rx_buffer += 1;
            if let Some(channel) = guard.channels.get_mut(&channel_id) {
                channel.rx_queue.clear();
            }
            no_error()
        }

        x if x == IOCTL_CLEAR_TX_BUFFER
            || x == IOCTL_CLEAR_PERIODIC_MSGS
            || x == IOCTL_CLEAR_MSG_FILTERS
            || x == IOCTL_CLEAR_FUNCT_MSG_LOOKUP_TABLE => {
            if x == IOCTL_CLEAR_TX_BUFFER {
                state().lock().expect("mock state poisoned").counters.clear_tx_buffer += 1;
                THREAD_COUNTERS.with(|c| c.clear_tx_buffer.set(c.clear_tx_buffer.get() + 1));
            }
            if x == IOCTL_CLEAR_PERIODIC_MSGS {
                let mut guard = state().lock().expect("mock state poisoned");
                if let Some(channel) = guard.channels.get_mut(&channel_id) {
                    channel.periodic_msgs.clear();
                }
            }
            if x == IOCTL_CLEAR_MSG_FILTERS {
                let mut guard = state().lock().expect("mock state poisoned");
                if let Some(channel) = guard.channels.get_mut(&channel_id) {
                    channel.filters.clear();
                }
            }
            no_error()
        }

        // SAE J2534-2 §25.3.2.2. Reports SUPPORTED for exactly the 10 base
        // J2534-1 protocols this mock implements, plus the consolidated
        // `DEVICE_INFO_J2610_SUPPORTED` (ADR-156 Decision 4/Phase 2b);
        // NOT_SUPPORTED for every other known parameter, including the
        // entire pin-bitmask-input family (ADR-153). The seven in-scope
        // `_CHx`-capacity-carrying `_SUPPORTED` params (the six base
        // families' own, plus the consolidated SCI one) pack `chx_capacity()`
        // into the clause-shaped `0xPPQQRRSS` value's `QQ` byte (bits 16-23
        // -- ADR-156 Decision 4 correction; NOT bits 8-15, which is `RR`,
        // the unrelated _PS channel count -- Codex review PR #29 caught
        // this mock packing at the same wrong bit position the service
        // originally read from, so the two errors concealed each other);
        // every other `_SUPPORTED`/`_SIMULTANEOUS` param here keeps the flat
        // `0x0000_0001` this mock reported before Phase 2b (RR/PP unused,
        // SS=1).
        x if x == IOCTL_GET_DEVICE_INFO => {
            if p_output.is_null() {
                return err_null();
            }
            let list = unsafe { &mut *(p_output as *mut SPARAM_LIST) };
            if list.ParamPtr.is_null() {
                return err_null();
            }
            let chx_capacity = {
                let guard = state().lock().expect("mock state poisoned");
                guard.chx_capacity_override.unwrap_or(DEFAULT_CHX_CAPACITY)
            };
            // ADR-211 (Codex review round-2 correction, PR #124): see
            // `ft_can_chx_capacity_override`'s own field doc.
            let ft_can_chx_capacity = {
                let guard = state().lock().expect("mock state poisoned");
                guard
                    .ft_can_chx_capacity_override
                    .unwrap_or(chx_capacity)
            };
            // ADR-212/Round 2: see `sw_can_chx_capacity_override`'s own field
            // doc.
            let sw_can_chx_capacity = {
                let guard = state().lock().expect("mock state poisoned");
                guard
                    .sw_can_chx_capacity_override
                    .unwrap_or(chx_capacity)
            };
            // ADR-213/Round 3: see `fd_can_chx_capacity_override`'s own field
            // doc.
            let fd_can_chx_capacity = {
                let guard = state().lock().expect("mock state poisoned");
                guard
                    .fd_can_chx_capacity_override
                    .unwrap_or(chx_capacity)
            };
            // ADR-213/Round 3: see `fd_iso15765_chx_capacity_override`'s own
            // field doc.
            let fd_iso15765_chx_capacity = {
                let guard = state().lock().expect("mock state poisoned");
                guard
                    .fd_iso15765_chx_capacity_override
                    .unwrap_or(chx_capacity)
            };
            // SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8) test
            // backdoor, `__mock_set_gm_uart_supported`: see `gm_uart_unsupported`'s
            // own field doc.
            let gm_uart_unsupported = {
                let guard = state().lock().expect("mock state poisoned");
                guard.gm_uart_unsupported
            };
            // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16) test
            // backdoor, `__mock_set_ndis_supported`: see `ndis_unsupported`'s
            // own field doc.
            let ndis_unsupported = {
                let guard = state().lock().expect("mock state poisoned");
                guard.ndis_unsupported
            };
            state()
                .lock()
                .expect("mock state poisoned")
                .counters
                .get_device_info += 1;
            THREAD_COUNTERS.with(|c| c.get_device_info.set(c.get_device_info.get() + 1));
            let params = unsafe {
                std::slice::from_raw_parts_mut(list.ParamPtr, list.NumOfParams as usize)
            };
            for p in params.iter_mut() {
                match p.Parameter {
                    // ADR-189/Phase 8: unlike every other `_SUPPORTED` flag in
                    // this handler (always reported supported), this one has
                    // a dedicated test backdoor (`gm_uart_unsupported`) so
                    // `enforce_discovery_capability`'s ADR-185 Stage 1
                    // fail-fast connect-time rejection can be exercised for
                    // this family -- consulted here rather than folded into
                    // the flat `0x0000_0001` group arm below.
                    x if x == DEVICE_INFO_GM_UART_SUPPORTED => {
                        if gm_uart_unsupported {
                            p.Value = 0;
                            p.Supported = 0;
                        } else {
                            p.Value = 0x0000_0001;
                            p.Supported = 1;
                        }
                    }
                    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase
                    // 16): the same dedicated-test-backdoor shape
                    // `DEVICE_INFO_GM_UART_SUPPORTED` just above uses (not
                    // the flat, always-supported group arm below), so
                    // `enforce_discovery_capability`'s ADR-185 Stage 1
                    // fail-fast connect-time rejection can be exercised for
                    // this family too.
                    x if x == DEVICE_INFO_ETHERNET_NDIS_SUPPORTED => {
                        if ndis_unsupported {
                            p.Value = 0;
                            p.Supported = 0;
                        } else {
                            p.Value = 0x0000_0001;
                            p.Supported = 1;
                        }
                    }
                    // Mechanical extension of ADR-211's/ADR-212's own moves
                    // just below: SAE J1939, UART Echo Byte, Honda DIAG-H,
                    // SAE J1708, and TP2.0 join this shared packed-capacity
                    // arm (closing the `_CHx` channel-count-cap enforcement
                    // gap the Prioritized Backlog had
                    // recorded for them; see `resources::chx_device_info_
                    // supported_parameter`'s own doc comment). Unlike FT-CAN/
                    // SW-CAN, none of these five collapse onto another
                    // family's base via `base_protocol_id` -- each is
                    // independently self-identifying -- so sharing the
                    // generic `chx_capacity` override here is sufficient for
                    // a genuinely discriminating test; no dedicated
                    // per-family override field was needed.
                    x if x == DEVICE_INFO_J1850PWM_SUPPORTED
                        || x == DEVICE_INFO_J1850VPW_SUPPORTED
                        || x == DEVICE_INFO_ISO9141_SUPPORTED
                        || x == DEVICE_INFO_ISO14230_SUPPORTED
                        || x == DEVICE_INFO_CAN_SUPPORTED
                        || x == DEVICE_INFO_ISO15765_SUPPORTED
                        || x == DEVICE_INFO_J2610_SUPPORTED
                        || x == DEVICE_INFO_J1939_SUPPORTED
                        || x == DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED
                        || x == DEVICE_INFO_HONDA_DIAGH_SUPPORTED
                        || x == DEVICE_INFO_J1708_SUPPORTED
                        || x == DEVICE_INFO_TP2_0_SUPPORTED =>
                    {
                        p.Value = (chx_capacity & 0xFF) << 16 | 0x0000_0001;
                        p.Supported = 1;
                    }
                    // ADR-211 (Codex review correction, PR #124): Fault-
                    // Tolerant CAN moved here from the flat `0x0000_0001`
                    // group arm below -- `J2534Service::check_chx_capacity`
                    // now correctly keys FT-CAN's own `_CHx` capacity check
                    // on these two flags (not the generic CAN/ISO15765
                    // ones), so this mock must advertise a real packed
                    // `_CHx` count for them, exactly like the seven original
                    // families, or every FT-CAN `_CHx` connect in this mock
                    // would be synchronously rejected as "0 channels
                    // available" regardless of `chx_capacity_override`. In
                    // its own arm (not folded into the one above), keyed on
                    // `ft_can_chx_capacity` rather than the shared
                    // `chx_capacity`, so a test can diverge FT-CAN's own
                    // reported count from the generic families' -- the only
                    // way to prove `check_chx_capacity` actually consults
                    // this flag rather than `DEVICE_INFO_CAN_SUPPORTED`/
                    // `DEVICE_INFO_ISO15765_SUPPORTED` through the real
                    // gRPC/mock stack (see `ft_can_chx_capacity_override`'s
                    // own field doc; an `edge-case-hunter` finding). Single
                    // Wire CAN's own two flags moved to their own packed-
                    // capacity arm just below (ADR-212/Round 2), built
                    // discriminating from the start rather than needing a
                    // follow-up fix the way this arm did.
                    x if x == DEVICE_INFO_FT_CAN_SUPPORTED
                        || x == DEVICE_INFO_FT_ISO15765_SUPPORTED =>
                    {
                        p.Value = (ft_can_chx_capacity & 0xFF) << 16 | 0x0000_0001;
                        p.Supported = 1;
                    }
                    // ADR-212/Round 2: Single Wire CAN moved here from the
                    // flat `0x0000_0001` group arm below, mirroring
                    // Fault-Tolerant CAN's own move just above --
                    // `J2534Service::check_chx_capacity` keys SW-CAN's own
                    // `_CHx` capacity check on these two flags (not the
                    // generic CAN/ISO15765 ones), so this mock must advertise
                    // a real packed `_CHx` count for them. In its own arm,
                    // keyed on `sw_can_chx_capacity` rather than the shared
                    // `chx_capacity`, so a test can diverge SW-CAN's own
                    // reported count from the generic families' (see
                    // `sw_can_chx_capacity_override`'s own field doc).
                    x if x == DEVICE_INFO_SW_CAN_SUPPORTED
                        || x == DEVICE_INFO_SW_ISO15765_SUPPORTED =>
                    {
                        p.Value = (sw_can_chx_capacity & 0xFF) << 16 | 0x0000_0001;
                        p.Supported = 1;
                    }
                    // ADR-213/Round 3: CAN FD, the third and final CAN-
                    // collapse family in this series -- unlike the "five
                    // previously-uncovered families" arm above, CAN FD
                    // collapses onto the generic CAN/ISO15765 base the same
                    // way FT-CAN/SW-CAN do (`resources::base_protocol_id`'s
                    // own FD arms), so a shared `chx_capacity` override would
                    // NOT make a regression test genuinely discriminating --
                    // this mock had zero existing FD device-info handling
                    // before this round at all (ADR-158/159 Decision item 7's
                    // own deferral), so it is built discriminating from the
                    // start, in its own arm keyed on `fd_can_chx_capacity`
                    // rather than the shared `chx_capacity`, mirroring FT-CAN's/
                    // SW-CAN's own precedent (see `fd_can_chx_capacity_override`'s
                    // own field doc).
                    x if x == DEVICE_INFO_FD_CAN_SUPPORTED => {
                        p.Value = (fd_can_chx_capacity & 0xFF) << 16 | 0x0000_0001;
                        p.Supported = 1;
                    }
                    // ADR-213/Round 3: ISO15765-on-CAN-FD, the
                    // `fd_iso15765_chx_capacity`-keyed analog of the CAN FD
                    // arm just above (see `fd_iso15765_chx_capacity_override`'s
                    // own field doc).
                    x if x == DEVICE_INFO_FD_ISO15765_SUPPORTED => {
                        p.Value = (fd_iso15765_chx_capacity & 0xFF) << 16 | 0x0000_0001;
                        p.Supported = 1;
                    }
                    x if x == DEVICE_INFO_J1850PWM_SIMULTANEOUS
                        || x == DEVICE_INFO_J1850VPW_SIMULTANEOUS
                        || x == DEVICE_INFO_ISO9141_SIMULTANEOUS
                        || x == DEVICE_INFO_ISO14230_SIMULTANEOUS
                        || x == DEVICE_INFO_CAN_SIMULTANEOUS
                        || x == DEVICE_INFO_ISO15765_SIMULTANEOUS
                        || x == DEVICE_INFO_SCI_A_ENGINE_SUPPORTED
                        || x == DEVICE_INFO_SCI_A_ENGINE_SIMULTANEOUS
                        || x == DEVICE_INFO_SCI_A_TRANS_SUPPORTED
                        || x == DEVICE_INFO_SCI_A_TRANS_SIMULTANEOUS
                        || x == DEVICE_INFO_SCI_B_ENGINE_SUPPORTED
                        || x == DEVICE_INFO_SCI_B_ENGINE_SIMULTANEOUS
                        || x == DEVICE_INFO_SCI_B_TRANS_SUPPORTED
                        || x == DEVICE_INFO_SCI_B_TRANS_SIMULTANEOUS
                        // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase
                        // 15): this mock implements the 32 native
                        // PROTOCOL_ANALOG_IN_x ids, so it must advertise
                        // both here too -- the flat 0x0000_0001 shape,
                        // mirroring the SCI variants just above, since this
                        // protocol has no `_CHx`-capacity concept for the
                        // `chx_capacity`-packed arm above to apply.
                        // `DEVICE_INFO_ANALOG_IN_SUPPORTED` is consulted at
                        // connect time since ADR-185 Stage 1;
                        // `_SIMULTANEOUS` remains an unwired accepted
                        // residual (ADR-177 Consequences).
                        || x == DEVICE_INFO_ANALOG_IN_SUPPORTED
                        || x == DEVICE_INFO_ANALOG_IN_SIMULTANEOUS
                        // ADR-185 Stage 1: this mock implements SAE J2534-2
                        // clause 12 UART Echo Byte, clause 13 Honda DIAG-H,
                        // and clause 17 SAE J1708 (see each protocol's own
                        // `tests/grpc_mock/*.rs` file); `J2534Service::
                        // enforce_discovery_capability` consults their
                        // `DEVICE_INFO_*_SUPPORTED` flags at connect time.
                        // Their own `_SUPPORTED` bits moved OUT of this flat
                        // arm and into the `chx_capacity`-packed arm above
                        // (mechanical extension of ADR-211's/ADR-212's own
                        // moves, alongside SAE J1939 and TP2.0's own bit),
                        // since `check_chx_capacity` now enforces a real
                        // `_CHx` cap for them too -- only their `_SIMULTANEOUS`
                        // companion bits (unwired; no Stage 1/Stage 2 call
                        // site consults them) stay flat here. TP2.0's own
                        // `_SIMULTANEOUS`/`_PS_J1962` stay unwired too
                        // (ADR-188 §7's own accepted residual, mirroring
                        // every ADR-185 Stage-1 family's identical
                        // `_SIMULTANEOUS` residual) -- its `_SUPPORTED` bit
                        // likewise moved to the packed-capacity arm above.
                        =>
                    {
                        p.Value = 0x0000_0001;
                        p.Supported = 1;
                    }
                    // SAE J2534-2 clause 23 (Phase 13, Codex review PR #48
                    // finding): this mock implements
                    // `IOCTL_READ_J1962PIN_VOLTAGE`, so it must also
                    // advertise it here -- an opted-in client following the
                    // clause 25 discovery-first workflow would otherwise
                    // conclude the operation is unavailable and never call
                    // the already-implemented path. Per Table 111/clause
                    // 25.3.2.2, `Value` is a per-pin INPUT bitmap using the
                    // same LOW-half convention as `SHORT_TO_GND_J1962` (bit 0
                    // = pin 1, mirrored below) -- NOT `PGM_VOLTAGE_J1962`'s
                    // high-half convention -- and must remain un-altered.
                    // Mock-fidelity fix (ADR-185 backlog): this arm used to
                    // ignore `Value` and unconditionally report `Supported =
                    // 1`, contradicting this mock's own real
                    // `IOCTL_READ_J1962PIN_VOLTAGE` handler, which rejects
                    // pins 0/4/5/17+ with `ERR_PIN_INVALID` -- now decodes
                    // the same single-pin selector and reports `Supported =
                    // 0` for exactly that rejection set, matching the real
                    // IOCTL exactly.
                    x if x == DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED => {
                        let pin_bits = p.Value & 0xFFFF;
                        let pin = if pin_bits.count_ones() == 1 {
                            pin_bits.trailing_zeros() + 1
                        } else {
                            0
                        };
                        p.Supported = match pin {
                            0 | 4 | 5 => 0,
                            17.. => 0,
                            _ => 1,
                        };
                    }
                    x if x == DEVICE_INFO_READ_J1962PIN_VOLTAGE_MAX => {
                        // Millivolts -- clause 23 requires a measurable range
                        // of at least 0-24 VDC; this mock reports exactly
                        // that minimum-required maximum.
                        p.Value = 24_000;
                        p.Supported = 1;
                    }
                    // SAE J2534-2 clause 18 Device Configuration (ADR-176/
                    // Phase 14, ADR-185 Stage 2): this mock implements
                    // GET/SET_DEVICE_CONFIG against ten
                    // NON_VOLATILE_STORE_1.._10 slots, so it must also
                    // advertise this parameter -- without this arm the
                    // default `_ => Supported = 0` below would make the new
                    // Discovery-cache enforcement reject every Device
                    // Configuration IOCTL on an opted-in module.
                    x if x == DEVICE_INFO_MAX_NON_VOLATILE_STORAGE => {
                        p.Value = 10;
                        p.Supported = 1;
                    }
                    // SAE J2534-2 clause 15.3.2.1: pin 9 gains both
                    // short-to-ground and programming-voltage capability on
                    // the SAE J1962 connector, in addition to pin 15's
                    // pre-existing J2534-1-era support for the same two
                    // capabilities. `Value` is an INPUT bit-mapped pin
                    // selector (clause 25.3.2.2) that must remain
                    // un-altered -- only `Supported` is set here, unlike the
                    // two `READ_J1962PIN_VOLTAGE_*` arms above. A `Value`
                    // with anything other than exactly one bit set in the
                    // relevant nibble-pair is a spec-invalid query and is
                    // answered NOT_SUPPORTED rather than guessed at.
                    x if x == DEVICE_INFO_SHORT_TO_GND_J1962 => {
                        let pin_bits = p.Value & 0xFFFF;
                        let pin = if pin_bits.count_ones() == 1 {
                            pin_bits.trailing_zeros() + 1
                        } else {
                            0
                        };
                        p.Supported = if pin == 9 || pin == 15 { 1 } else { 0 };
                    }
                    x if x == DEVICE_INFO_PGM_VOLTAGE_J1962 => {
                        let pin_bits = (p.Value >> 16) & 0xFFFF;
                        let pin = if pin_bits.count_ones() == 1 {
                            pin_bits.trailing_zeros() + 1
                        } else {
                            0
                        };
                        p.Supported = if pin == 9 || pin == 15 { 1 } else { 0 };
                    }
                    _ => {
                        p.Supported = 0;
                    }
                }
            }
            no_error()
        }

        // SAE J2534-2 §25.3.2.3. `ERR_INVALID_PROTOCOL_ID` for any protocol
        // outside the 10 base J2534-1 IDs this mock implements; for a known
        // protocol, a small real subset of parameters (matching this mock's
        // actual limits) with NOT_SUPPORTED for the rest (ADR-153).
        x if x == IOCTL_GET_PROTOCOL_INFO => {
            if p_input.is_null() || p_output.is_null() {
                return err_null();
            }
            state()
                .lock()
                .expect("mock state poisoned")
                .counters
                .get_protocol_info += 1;
            let protocol_id = unsafe { *(p_input as *const u32) };
            // ADR-157: a `_PS` id is a legitimate query too -- a caller with
            // a `_PS` link can query GetProtocolInfo for it -- so validation
            // and every capability answer below key off the base protocol
            // id, not the raw (possibly `_PS`) one.
            let base_id = base_protocol_id(protocol_id);
            // Codex review, PR #64: `base_id` self-identifies for every
            // standalone `_PS`-only protocol (UART Echo Byte, Honda DIAG-H,
            // J1708 -- `base_protocol_id` has no arm collapsing any of them
            // onto a different family), so this allow-list must name each
            // one individually rather than relying on `base_protocol_id`
            // ever changing them. `PROTOCOL_HONDA_DIAGH_PS`/
            // `PROTOCOL_J1708_PS` are included here since both genuinely
            // support Repeat Messaging (clause 13/17 define no exclusion
            // equivalent to clause 12.3.3.1's) and the
            // `PROTOCOL_INFO_MAX_REPEAT_MESSAGING`/`_LENGTH` arms below are
            // deliberately protocol-independent once past this gate.
            // `PROTOCOL_UART_ECHO_BYTE_PS` is deliberately NOT included:
            // clause 12.3.3.1 explicitly rejects Repeat Messaging for it, so
            // its continued exclusion from this handler is consistent with
            // that rejection, not the same oversight this fix corrects.
            if !matches!(
                base_id,
                PROTOCOL_J1850PWM
                    | PROTOCOL_J1850VPW
                    | PROTOCOL_ISO9141
                    | PROTOCOL_ISO14230
                    | PROTOCOL_CAN
                    | PROTOCOL_ISO15765
                    | PROTOCOL_SCI_A_ENGINE
                    | PROTOCOL_SCI_A_TRANS
                    | PROTOCOL_SCI_B_ENGINE
                    | PROTOCOL_SCI_B_TRANS
                    | PROTOCOL_HONDA_DIAGH_PS
                    | PROTOCOL_J1708_PS
            ) {
                return ERR_INVALID_PROTOCOL_ID as c_long;
            }
            let is_can_family = matches!(base_id, PROTOCOL_CAN | PROTOCOL_ISO15765);
            let list = unsafe { &mut *(p_output as *mut SPARAM_LIST) };
            if list.ParamPtr.is_null() {
                return err_null();
            }
            let params = unsafe {
                std::slice::from_raw_parts_mut(list.ParamPtr, list.NumOfParams as usize)
            };
            for p in params.iter_mut() {
                match p.Parameter {
                    // Matches PASSTHRU_MSG::Data's actual size (j2534-0404's
                    // MAX_MESSAGE_DATA), not an arbitrary stand-in.
                    x if x == PROTOCOL_INFO_MAX_RX_BUFFER_SIZE => {
                        p.Value = 4128;
                        p.Supported = 1;
                    }
                    x if x == PROTOCOL_INFO_MAX_PASS_FILTER
                        || x == PROTOCOL_INFO_MAX_BLOCK_FILTER =>
                    {
                        p.Value = 10;
                        p.Supported = 1;
                    }
                    x if x == PROTOCOL_INFO_CAN_11_29_IDS_SUPPORTED && is_can_family => {
                        p.Value = 1;
                        p.Supported = 1;
                    }
                    // SAE J2534-2 clause 25.3.2.3 (Repeat Messaging
                    // discovery): matches this mock's own real, already-
                    // implemented `IOCTL_START_REPEAT_MESSAGE` enforcement
                    // (`MAX_REPEAT_SLOTS_PER_CHANNEL`), reused directly here
                    // so the two never drift apart. Flat/protocol-
                    // independent, matching this handler's own existing
                    // precedent for structural-limit-style parameters
                    // (`MAX_PASS_FILTER`/`MAX_BLOCK_FILTER` above); no
                    // protocol-family gating is needed since a
                    // Repeat-Messaging-incapable protocol id (e.g.
                    // UART Echo Byte) is already rejected above by the
                    // `base_id` allow-list before reaching here.
                    x if x == PROTOCOL_INFO_MAX_REPEAT_MESSAGING => {
                        p.Value = MAX_REPEAT_SLOTS_PER_CHANNEL as u32;
                        p.Supported = 1;
                    }
                    // Codex review finding, PR #64 (values updated for
                    // ADR-186): unlike `PROTOCOL_INFO_MAX_RX_BUFFER_SIZE`
                    // above (a genuine structural buffer capacity, accurate
                    // for every protocol this mock implements), this
                    // parameter reports the largest message a
                    // repeat-messaging TX can actually carry --
                    // `ioctl_start_repeat_message`
                    // (`j2534-0404-service/src/service/rpc_misc.rs`) no
                    // longer enforces each protocol's own ordinary TX size
                    // range (`protocol.rs`'s `tx_message_size_range`) on
                    // `RepeatMsgData[0]`; ADR-186 replaced that with SAE
                    // J2534-1 v04.04 §7.2.7's flat periodic-message cap,
                    // narrower than the old ranges for every protocol --
                    // advertising the old, wider values here would let a
                    // client believe a message size the service now rejects.
                    // ISO15765 gets its own 11-byte cap (clause 22.2.2(h));
                    // every other protocol this handler accepts -- including
                    // SAE J1939 (`PROTOCOL_J1939_PS`), which has no
                    // clause-16 periodic-specific restatement and so
                    // inherits the flat cap like the rest (ADR-186 Decision
                    // item 1, third round) -- gets the generic 12-byte cap.
                    // Sourced from `max_repeat_messaging_length`, the same
                    // function `IOCTL_START_REPEAT_MESSAGE` enforces against,
                    // so the two values cannot drift apart.
                    x if x == PROTOCOL_INFO_MAX_REPEAT_MESSAGING_LENGTH => {
                        p.Value = max_repeat_messaging_length(base_id);
                        p.Supported = 1;
                    }
                    _ => {
                        p.Supported = 0;
                    }
                }
            }
            no_error()
        }

        // SAE J2534-2 clause 9 Single Wire CAN (SWCAN/GMLAN, ADR-164 Decision
        // 3/Phase 4): SW_CAN_HS/SW_CAN_NS switch an SW-connected channel's
        // bus speed mode -- no input/output parameters, per clause 9's own
        // command definition (mirrors `IOCTL_CLEAR_RX_BUFFER`'s ignore-both
        // shape). Accepted (no state change simulated -- the actual
        // speed-transition/`SW_CAN_HS_RX`/`SW_CAN_NS_RX` indication is an
        // explicit accepted residual, ADR-164 Consequences) only on a
        // channel connected with `PROTOCOL_SW_CAN_PS`/`PROTOCOL_SW_ISO15765_PS`
        // or a `_CHx`-connected SW-CAN channel (ADR-212/Round 2: re-keyed from
        // the narrow `is_sw_protocol` to the family-wide `is_sw_family_
        // protocol` so these two IOCTLs still work on a `_CHx`-connected
        // channel); rejected `ERR_NOT_SUPPORTED` otherwise, the same "wrong
        // protocol for this IOCTL" answer `IOCTL_FIVE_BAUD_INIT`/
        // `IOCTL_FAST_INIT` already give for a non-K-line channel.
        x if x == IOCTL_SW_CAN_HS || x == IOCTL_SW_CAN_NS => {
            let mut guard = state().lock().expect("mock state poisoned");
            match guard.channels.get(&channel_id) {
                None => err_channel(),
                Some(channel) if !is_sw_family_protocol(channel.protocol_id) => {
                    err_not_supported()
                }
                Some(_) => {
                    if x == IOCTL_SW_CAN_HS {
                        guard.counters.sw_can_hs += 1;
                    } else {
                        guard.counters.sw_can_ns += 1;
                    }
                    no_error()
                }
            }
        }

        // SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8): `pInput`
        // is Table 28's `PollResponseMsg[100]` (an `SBYTE_ARRAY`, at most 100
        // bytes) -- no output. Accepted only on a channel connected with
        // `PROTOCOL_GM_UART_PS`/`GM_UART_CHx`; rejected `ERR_NOT_SUPPORTED`
        // otherwise, the same "wrong protocol for this IOCTL" answer
        // `IOCTL_SW_CAN_HS`/`_NS` above give. This mock does not pre-validate
        // the 100-byte cap either (a thin passthrough, mirroring
        // `j2534-0404-service`'s own handler) -- it simply stores the bytes
        // for test observation (`__mock_get_poll_response`); the bus-
        // mastership negotiation logic itself is a client-application
        // responsibility (ADR-189 Context), so this mock has no state
        // machine to drive.
        x if x == IOCTL_SET_POLL_RESPONSE => {
            if p_input.is_null() {
                return err_null();
            }
            let mut guard = state().lock().expect("mock state poisoned");
            let Some(existing) = guard.channels.get(&channel_id) else {
                return err_channel();
            };
            if !is_gm_uart_protocol(existing.protocol_id) {
                return err_not_supported();
            }
            if !existing.pins_assigned {
                return err_pin_invalid();
            }
            let input = unsafe { &*(p_input as *const SBYTE_ARRAY) };
            if input.NumOfBytes > 0 && input.BytePtr.is_null() {
                return err_null();
            }
            // Codex review, PR #98 round 5: Table 28's `PollResponseMsg[100]`
            // is a fixed-size 100-byte array (clause 11.3.3.1) -- the
            // service intentionally delegates this bound to the native
            // layer (ADR-189 Decision item 4: a thin passthrough, no
            // client-side length pre-validation), so this mock is the only
            // thing standing in for a conforming adapter's own rejection.
            // Without this check, every `grpc_mock` test using this mock
            // observes success for an oversized payload a real adapter
            // would reject, and no test in this codebase could ever catch
            // a service-side regression that started forwarding an
            // oversized `bytearray_data` unchecked.
            if input.NumOfBytes > 100 {
                return ERR_INVALID_IOCTL_VALUE as c_long;
            }
            let bytes = if input.NumOfBytes == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(input.BytePtr, input.NumOfBytes as usize) }
                    .to_vec()
            };
            let Some(channel) = guard.channels.get_mut(&channel_id) else {
                return err_channel();
            };
            channel.poll_response = bytes;
            no_error()
        }

        // SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8): `pInput`
        // is Table 32's single `Poll_ID` byte (an `SBYTE_ARRAY`) -- no
        // output. Non-blocking by default in this mock -- the real ~2 second
        // native-side wait (clause 11.3.3.2) is not simulated, an accepted
        // residual matching this mock's own established precedent for a
        // native call whose only observable effect this test harness needs
        // is success/failure (e.g. `IOCTL_SW_CAN_HS`/`_NS` above). Accepted
        // unconditionally on a GM_UART_PS/GM_UART_CHx channel unless
        // `__mock_set_become_master_error` has armed a forced failure (lets
        // tests exercise the documented `ERR_FAILED` "no poll message within
        // 2s" outcome without an actual 2-second wait).
        //
        // PR #98 regression coverage: when `__mock_arm_become_master_hold`
        // has armed a `become_master_hold`, this call instead blocks (after
        // releasing `state()`, so no other mock entry point is affected)
        // until `__mock_release_become_master_hold` is called -- letting a
        // test observe `j2534-0404-service`'s own `become_master_in_flight`
        // reservation actually reject a sibling CLL's concurrent connect
        // attempt while this call is still outstanding.
        x if x == IOCTL_BECOME_MASTER => {
            if p_input.is_null() {
                return err_null();
            }
            let guard = state().lock().expect("mock state poisoned");
            let Some(existing) = guard.channels.get(&channel_id) else {
                return err_channel();
            };
            if !is_gm_uart_protocol(existing.protocol_id) {
                return err_not_supported();
            }
            if !existing.pins_assigned {
                return err_pin_invalid();
            }
            let input = unsafe { &*(p_input as *const SBYTE_ARRAY) };
            if input.BytePtr.is_null() || input.NumOfBytes != 1 {
                return err_null();
            }
            let hold = guard.become_master_hold.clone();
            let forced_error = guard.become_master_error;
            // Released BEFORE any blocking wait below -- a held call must
            // never block a concurrent test action (e.g. a sibling CLL's
            // PassThruConnect) that itself needs `state()`.
            drop(guard);
            if let Some(hold) = hold {
                let mut hstate = hold.state.lock().expect("become_master hold state poisoned");
                hstate.engaged = true;
                hold.condvar.notify_all();
                while !hstate.released {
                    hstate = hold
                        .condvar
                        .wait(hstate)
                        .expect("become_master hold condvar poisoned");
                }
            }
            if let Some(code) = forced_error {
                return code;
            }
            no_error()
        }

        // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): no input;
        // `pOutput` receives the canned `mock_ndis_adapter_info()` struct
        // directly (a `#[repr(C)]` struct-out IOCTL, unlike every
        // byte-array-in/out IOCTL above). Accepted only on a channel
        // connected with `PROTOCOL_ETHERNET_NDIS`; rejected `ERR_NOT_
        // SUPPORTED` otherwise, the same "wrong protocol for this IOCTL"
        // answer `IOCTL_SW_CAN_HS`/`_NS`/`IOCTL_SET_POLL_RESPONSE` above
        // give. `EthernetPinConfig` is derived from the channel's own
        // connect-time flags rather than always echoing the canned fixture's
        // Option 1 default (Codex review, PR #102 round 9): `2` when
        // `CONNECT_FLAG_NDIS_PINS_OPTION2` alone was set at `PassThruConnect`
        // time, `1` otherwise. This covers Option 1, auto/unset (neither bit),
        // AND both bits set together -- `rpc_link.rs`'s own
        // `ndis_pin_option_connect_flags`/ADR-194 document that the native
        // table treats both bits set as equivalent to neither, so this mock
        // mirrors that "both = neither" convention exactly rather than
        // reporting `2` for a both-bits-set channel (unreachable through the
        // real gRPC service, whose `ndis_pin_option_connect_flags` always
        // emits at most one bit -- but a direct FFI caller of this mock can
        // set both, and this codebase already treats that as a legitimate
        // usage mode elsewhere, edge-case-hunter finding, PR #102 close-out).
        // Clause 24 defines no separate "unknown"/"auto" pin-config value
        // for this field, so all three non-Option-2-alone cases share `1`.
        x if x == IOCTL_GET_NDIS_ADAPTER_INFO => {
            if p_output.is_null() {
                return err_null();
            }
            let guard = state().lock().expect("mock state poisoned");
            let Some(existing) = guard.channels.get(&channel_id) else {
                return err_channel();
            };
            if !is_ethernet_ndis_protocol(existing.protocol_id) {
                return err_not_supported();
            }
            const OPTION1: u32 = j2534_0404_sys::bindings::CONNECT_FLAG_NDIS_PINS_OPTION1;
            const OPTION2: u32 = j2534_0404_sys::bindings::CONNECT_FLAG_NDIS_PINS_OPTION2;
            let ethernet_pin_config =
                if existing.flags & OPTION2 != 0 && existing.flags & OPTION1 == 0 {
                    2
                } else {
                    1
                };
            drop(guard);
            unsafe {
                let mut info = mock_ndis_adapter_info();
                info.EthernetPinConfig = ethernet_pin_config;
                *(p_output as *mut NDIS_ADAPTER_INFORMATION) = info;
            }
            no_error()
        }

        // SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5):
        // `pInput` is a 9-byte `SBYTE_ARRAY` (`BytePtr[0]` the source
        // address to claim/cancel, `[1..9]` the NAME), `pOutput` is NULL
        // (clause 16.3.3.2). Non-blocking on real hardware -- this mock
        // resolves the outcome synchronously, pushing a
        // `J1939_ADDRESS_CLAIMED`/`_LOST`-flagged indication directly onto
        // `channel.rx_queue` (mirroring the simpler synchronous-push
        // precedent `PassThruConnect`'s own SAE J1850 bus-flavor simulation
        // uses, rather than `spawn_repeat_worker`'s background-thread
        // shape -- `j2534-0404-service`'s `run_j1939_claim_loop` actively
        // drives `poll_rx` in a loop regardless of which shape the mock
        // uses, so an immediately-available indication exercises the same
        // real code path a delayed one would, with far less mock
        // complexity) rather than an explicit cancel form (`BytePtr[1..9]`
        // all zero, clause 16.3.3.2), which has no indication at all --
        // it just drops the address from `j1939_claimed_addresses`.
        x if x == IOCTL_PROTECT_J1939_ADDR => {
            if p_input.is_null() {
                return err_null();
            }
            let mut guard = state().lock().expect("mock state poisoned");
            let Some(existing) = guard.channels.get(&channel_id) else {
                return err_channel();
            };
            // SAE J2534-2 clause 6.3.3.2 (ADR-156 Decision 2): a `_PS`
            // channel's pins must be assigned via
            // IOCTL_SET_CONFIG(CONFIG_J1962_PINS) before any I/O runs,
            // mirroring every other I/O IOCTL's pin gate in this file
            // (e.g. IOCTL_FIVE_BAUD_INIT above).
            if !existing.pins_assigned {
                return err_pin_invalid();
            }
            let input = unsafe { &*(p_input as *const SBYTE_ARRAY) };
            if input.BytePtr.is_null() || input.NumOfBytes != 9 {
                return err_null();
            }
            let bytes = unsafe { std::slice::from_raw_parts(input.BytePtr, 9) };
            let address = bytes[0];
            let name = &bytes[1..9];
            // Clause 16.3.3.2: 254 is never allowed, 255 is the conceptual
            // power-on default only -- neither is a valid explicit claim
            // (or cancel) target.
            if address == 254 || address == 255 {
                return ERR_INVALID_IOCTL_VALUE as c_long;
            }
            let is_cancel = name.iter().all(|&b| b == 0);
            // ADR-180 Decision 21 regression coverage: see
            // `j1939_claim_no_indication`'s own field doc.
            let no_indication = guard.j1939_claim_no_indication;
            let loses_from_count = !is_cancel
                && !no_indication
                && guard.j1939_claim_lost_remaining > 0
                && guard.channels.contains_key(&channel_id);
            if loses_from_count {
                guard.j1939_claim_lost_remaining -= 1;
            }
            let claimed = !(guard.j1939_claim_lost || loses_from_count);
            // ADR-180 Decision 22 regression coverage: see
            // `j1939_cancel_error`'s own field doc.
            let cancel_error = guard.j1939_cancel_error;
            let Some(channel) = guard.channels.get_mut(&channel_id) else {
                return err_channel();
            };
            if is_cancel {
                if cancel_error {
                    // Deliberately does NOT remove `address` from
                    // `j1939_claimed_addresses` -- the mock's own
                    // bookkeeping keeps reflecting "still claimed", the same
                    // way a real adapter that fails/rejects a cancel would
                    // presumably still be defending the address.
                    return err_failed();
                }
                channel.j1939_claimed_addresses.remove(&address);
                return no_error();
            }
            if no_indication {
                // Success synchronously, but no `j1939_claimed_addresses`
                // insert and no `rx_queue` push -- the adapter "succeeds"
                // without ever reporting CLAIMED/LOST, so
                // `run_j1939_claim_loop`'s bounded wait genuinely waits.
                return no_error();
            }
            let rx_status = if claimed {
                RX_FLAG_J1939_ADDRESS_CLAIMED
            } else {
                RX_FLAG_J1939_ADDRESS_LOST
            };
            if claimed {
                channel.j1939_claimed_addresses.insert(address);
            } else {
                channel.j1939_claimed_addresses.remove(&address);
            }
            channel.rx_queue.push_back(StoredMessage {
                protocol_id: channel.protocol_id,
                rx_status,
                tx_flags: 0,
                timestamp: MOCK_TIMESTAMP,
                data: vec![address],
                extra_data_index: None,
            });
            no_error()
        }

        // SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a):
        // `pInput` is an 11-byte `SBYTE_ARRAY` (Table 78) -- `BytePtr[0..3]`
        // the setup CAN ID, `[4]` destination, `[5]` the fixed opcode
        // (`0xC0`), `[6..7]` the proposed TX-ID, `[8..9]` the proposed
        // RX-ID, `[10]` application type. Non-blocking, same synchronous-push
        // shape `IOCTL_PROTECT_J1939_ADDR` above uses: allocates a slot in
        // this channel's four-slot `tp20_connections` table and queues a
        // `CONNECTION_ESTABLISHED` indication (`Data[0..3]` echoing the
        // requested RX-ID, `Data[4..7]` a mock-assigned TX-ID -- the
        // requested RX-ID zero-extended and OR'd with a fixed marker bit
        // (`0x1000_0000`, bit 28 -- a valid bit for a 29-bit CAN identifier,
        // clause 19's own addressing range; Codex review fix, PR #97, round
        // 17: an earlier version OR'd with `0x8000_0000`, bit 31, which
        // exceeds the 29-bit maximum `0x1FFF_FFFF` and so could never be a
        // real CAN identifier a conforming adapter returns), so it is
        // trivially distinguishable from the RX-ID in a test/log without
        // needing a separate side channel), or, when all four slots are
        // already taken, a `CONNECTION_LOST` indication with reason `0xD8`
        // (a temporary shortage of free resources).
        x if x == IOCTL_REQUEST_CONNECTION => {
            if p_input.is_null() {
                return err_null();
            }
            let mut guard = state().lock().expect("mock state poisoned");
            let Some(existing) = guard.channels.get(&channel_id) else {
                return err_channel();
            };
            if !existing.pins_assigned {
                return err_pin_invalid();
            }
            let input = unsafe { &*(p_input as *const SBYTE_ARRAY) };
            if input.BytePtr.is_null() || input.NumOfBytes != 11 {
                return ERR_INVALID_IOCTL_VALUE as c_long;
            }
            let bytes = unsafe { std::slice::from_raw_parts(input.BytePtr, 11) };
            let rx_id_proposal = u32::from(bytes[8]) << 8 | u32::from(bytes[9]);
            // ADR-188 regression coverage: see `tp20_no_indication`'s own
            // field doc.
            let no_indication = guard.tp20_no_indication;
            let Some(channel) = guard.channels.get_mut(&channel_id) else {
                return err_channel();
            };
            // Clause 19.3.3.2 (Codex review finding, PR #97, round 11): a
            // proposed RX-ID already established on this physical channel is
            // rejected synchronously with `ERR_NOT_UNIQUE`, never silently
            // overwriting the existing slot -- unlike the "all four slots
            // full" case below (a resource-exhaustion condition, reported
            // asynchronously via `CONNECTION_LOST`/`0xD8`), a duplicate-ID
            // collision is a synchronous native-call failure regardless of
            // remaining capacity.
            if channel.tp20_connections.contains_key(&rx_id_proposal) {
                return ERR_NOT_UNIQUE as c_long;
            }
            if channel.tp20_connections.len() >= TP20_MAX_CONNECTIONS_PER_CHANNEL {
                let mut data = rx_id_proposal.to_be_bytes().to_vec();
                data.push(0xD8);
                channel.rx_queue.push_back(StoredMessage {
                    protocol_id: channel.protocol_id,
                    rx_status: RX_FLAG_CONNECTION_LOST,
                    tx_flags: 0,
                    timestamp: MOCK_TIMESTAMP,
                    data,
                    extra_data_index: None,
                });
                return no_error();
            }
            let tx_id = rx_id_proposal | 0x1000_0000;
            channel
                .tp20_connections
                .insert(rx_id_proposal, Tp20MockConnection { tx_id });
            if no_indication {
                // Success synchronously, slot genuinely allocated above, but
                // no `rx_queue` push -- the adapter "succeeds" without ever
                // reporting `CONNECTION_ESTABLISHED`, so
                // `run_tp20_connection_request`'s bounded wait genuinely
                // waits. See `tp20_no_indication`'s own field doc for why
                // the slot insert above is deliberately NOT skipped, unlike
                // `j1939_claim_no_indication`'s otherwise-identical shape.
                return no_error();
            }
            let mut data = rx_id_proposal.to_be_bytes().to_vec();
            data.extend_from_slice(&tx_id.to_be_bytes());
            channel.rx_queue.push_back(StoredMessage {
                protocol_id: channel.protocol_id,
                rx_status: RX_FLAG_CONNECTION_ESTABLISHED,
                tx_flags: 0,
                timestamp: MOCK_TIMESTAMP,
                data,
                extra_data_index: None,
            });
            no_error()
        }

        // SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a):
        // `pInput` is a 4-byte `SBYTE_ARRAY` (Table 79) -- `BytePtr[0..3]`
        // the connection's own receiving CAN address (RX-ID), MSB first,
        // the same address `IOCTL_REQUEST_CONNECTION` was originally issued
        // with. Removes the matching `tp20_connections` slot and, unless
        // `tp20_no_indication` is armed (see its own field doc -- Codex
        // review finding, PR #97, 7th round), queues a `CONNECTION_LOST`
        // indication with reason `0` (teardown); `ERR_INVALID_IOCTL_VALUE`
        // when `NumOfBytes != 4` or no slot matches.
        x if x == IOCTL_TEARDOWN_CONNECTION => {
            if p_input.is_null() {
                return err_null();
            }
            let input = unsafe { &*(p_input as *const SBYTE_ARRAY) };
            if input.BytePtr.is_null() || input.NumOfBytes != 4 {
                return ERR_INVALID_IOCTL_VALUE as c_long;
            }
            let bytes = unsafe { std::slice::from_raw_parts(input.BytePtr, 4) };
            let rx_id = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let mut guard = state().lock().expect("mock state poisoned");
            let no_indication = guard.tp20_no_indication;
            let Some(channel) = guard.channels.get_mut(&channel_id) else {
                return err_channel();
            };
            if channel.tp20_connections.remove(&rx_id).is_none() {
                return ERR_INVALID_IOCTL_VALUE as c_long;
            }
            if no_indication {
                return no_error();
            }
            let mut data = rx_id.to_be_bytes().to_vec();
            data.push(0);
            channel.rx_queue.push_back(StoredMessage {
                protocol_id: channel.protocol_id,
                rx_status: RX_FLAG_CONNECTION_LOST,
                tx_flags: 0,
                timestamp: MOCK_TIMESTAMP,
                data,
                extra_data_index: None,
            });
            no_error()
        }

        // SAE J2534-2 clause 14 Repeat Messaging (ADR-165/Phase 12):
        // `pInput` is a `REPEAT_MSG_SETUP*` (`RepeatMsgData[0]` the message
        // to retransmit, `[1]`/`[2]` the mask/pattern pair), `pOutput` a
        // `uint32_t*` for the assigned `MsgId`. Rejects `ERR_INVALID_IOCTL_VALUE`
        // for a `Condition` outside `{0, 1}` (round 10, Codex review, ADR-165
        // PR #42), `ERR_EXCEEDED_LIMIT` beyond `MAX_REPEAT_SLOTS_PER_CHANNEL`,
        // `ERR_PIN_INVALID` on a `_PS`/FD/SW channel whose pins are not yet
        // bound (mirrors `PassThruStartPeriodicMsg`'s identical gate) -- this
        // mock, unlike the real device, does not itself apply Pass/Block
        // filtering ahead of a repeat slot's own mask/pattern evaluation (no
        // native filtering exists in this mock to begin with), so that
        // ordering guarantee is out of scope for a mock, not a modeling gap.
        x if x == IOCTL_START_REPEAT_MESSAGE => {
            if p_input.is_null() || p_output.is_null() {
                return err_null();
            }
            let setup = unsafe { &*(p_input as *const REPEAT_MSG_SETUP) };
            // SAE J2534-2 clause 14 only defines `Condition == 0`
            // (`REPEAT_MESSAGE_UNTIL_MATCH`: retransmit until a matching
            // frame arrives) and `Condition == 1`
            // (`REPEAT_MESSAGE_WHILE_MATCH`: retransmit while matching,
            // stop on the first non-match or on a silent interval, ADR-173
            // Decision 1) -- any other value is rejected up front, before
            // any state mutation, rather than silently accepted into a slot
            // that `spawn_repeat_worker`/`note_rx_frame_for_repeat_slots`
            // has no defined behavior for. Codex review, ADR-165 PR #42
            // round 10.
            if setup.Condition != 0 && setup.Condition != 1 {
                return ERR_INVALID_IOCTL_VALUE as c_long;
            }
            let message = StoredMessage::from_passthru(&setup.RepeatMsgData[0]);
            let mask = StoredMessage::from_passthru(&setup.RepeatMsgData[1]);
            let pattern = StoredMessage::from_passthru(&setup.RepeatMsgData[2]);

            let mut guard = state().lock().expect("mock state poisoned");
            let Some(channel) = guard.channels.get_mut(&channel_id) else {
                return err_channel();
            };
            // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15, Codex
            // review finding on PR #66): Repeat Messaging is an autonomous
            // TRANSMIT mechanism -- clause 10 defines this protocol as
            // strictly read-only, and (unlike a `_PS` protocol) it is never
            // pin-gated, so the `!channel.pins_assigned` check just below
            // does not incidentally block it (`pins_assigned` defaults
            // `true` for a non-`_PS` protocol at channel creation). A real
            // spec-conforming adapter must also reject this the same way
            // `PassThruWriteMsgs`/`PassThruStartPeriodicMsg`/
            // `PassThruStartMsgFilter` already do for this protocol.
            if is_analog_in_protocol(channel.protocol_id) {
                return ERR_NOT_SUPPORTED as c_long;
            }
            // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16, Codex
            // review finding on PR #102): Repeat Messaging is an autonomous
            // TRANSMIT mechanism -- clause 24 routes all Ethernet payload
            // traffic outside the J2534 API entirely, so this protocol has
            // no autonomous device-side retransmission capability, the same
            // reasoning as the Analog Inputs rejection just above. The
            // service already rejects this at `ioctl_start_repeat_message`
            // before reaching here, but this mock mirrors every other
            // service-side protocol rejection to stay a faithful, standalone
            // stand-in for a real spec-conforming adapter.
            if is_ethernet_ndis_protocol(channel.protocol_id) {
                return ERR_NOT_SUPPORTED as c_long;
            }
            if !channel.pins_assigned {
                return err_pin_invalid();
            }
            // Codex review finding, PR #95: this handler used to accept
            // `RepeatMsgData[0]` at any size, never enforcing the cap this
            // mock itself advertises via `PROTOCOL_INFO_MAX_REPEAT_MESSAGING_LENGTH`
            // (`IOCTL_GET_PROTOCOL_INFO` above) -- so no test could exercise
            // the device-side `ERR_INVALID_MSG` rejection (clause 14.2.2.1)
            // that advertised value promises. `max_repeat_messaging_length`
            // is the same function the discovery arm calls, so the enforced
            // cap here can never drift from the advertised one.
            if message.data.len() > max_repeat_messaging_length(base_protocol_id(channel.protocol_id)) as usize
            {
                return err_invalid_msg();
            }
            if channel.repeat_slots.len() >= MAX_REPEAT_SLOTS_PER_CHANNEL {
                return ERR_EXCEEDED_LIMIT as c_long;
            }
            channel.next_repeat_msg_id = channel.next_repeat_msg_id.saturating_add(1).max(1);
            let msg_id = channel.next_repeat_msg_id;
            // `mask.tx_flags`/`pattern.tx_flags` are identical (round 2
            // Finding 2, `rpc_misc.rs`'s `ioctl_start_repeat_message` sets
            // the same `TxFlags` on both `RepeatMsgData[1]` and `[2]`) --
            // captured here, before `mask`/`pattern` are reduced to their
            // `.data` below, and translated into RX-direction `RxStatus`
            // space (`response_format_rx_bits`) once here rather than at
            // every incoming-frame comparison (round 14 fix, Codex review).
            // Masked to `REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK` (Codex review,
            // ADR-165 PR #42 round 15): as of round 15, `rpc_misc.rs` now sets
            // `TX_FD_CAN_FORMAT` on `pattern.tx_flags` for every FD-connected
            // link's slot (SAE J2534-2 21.4.4 template-DataSize validity, not
            // for matching), and `tx_format_flags_to_rx_status_bits` now
            // translates that into `RX_FLAG_FD_CAN_FORMAT` (for RX-echo
            // fidelity elsewhere). Without this mask, that translated FD bit
            // would end up stored here, but `note_rx_frame_for_repeat_slots`
            // masks the INCOMING frame's `rx_status` to
            // `REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK` (which excludes the FD
            // bits, per 21.2.2(g)/22.2.2(d)'s format-agnostic matching rule)
            // before comparing -- an unmasked value here would never equal
            // that masked incoming value, so every FD-link `Condition == 1`
            // slot would silently never stop.
            let response_format_rx_bits =
                tx_format_flags_to_rx_status_bits(pattern.tx_flags) & REPEAT_RESPONSE_FORMAT_RX_STATUS_MASK;
            // `mask.protocol_id`/`pattern.protocol_id` are likewise
            // identical (`rpc_misc.rs`'s `ioctl_start_repeat_message` builds
            // both `PassThruMessage`s from the same `hw_protocol_id`) --
            // captured from `pattern` specifically, mirroring
            // `response_format_rx_bits` above and the byte-level match,
            // which also compares against `pattern`'s reduced `.data`.
            // Codex review, ADR-165 PR #42 round 10.
            let response_protocol_id = pattern.protocol_id;
            // Captured before `pattern` is reduced to `.data` below --
            // test-introspection-only, see `RepeatSlot::mask_pattern_tx_flags`'s
            // doc comment (Codex review, ADR-165 PR #42 round 15).
            let mask_pattern_tx_flags = pattern.tx_flags;
            channel.repeat_slots.insert(
                msg_id,
                RepeatSlot {
                    time_interval_ms: setup.TimeInterval,
                    condition: setup.Condition,
                    message,
                    mask: mask.data,
                    pattern: pattern.data,
                    response_format_rx_bits,
                    response_protocol_id,
                    mask_pattern_tx_flags,
                    terminated: false,
                    rx_seen_this_interval: false,
                    // The grid hasn't started yet -- the worker's first
                    // transmit sets this to `Some(..)` (round 3
                    // grid-anchored fix, ADR-173). A frame arriving in the
                    // brief gap between insertion and that first transmit
                    // correctly credits no window, since `None` is the
                    // "grid hasn't started" state `note_rx_frame_for_repeat_
                    // slots` checks for (Codex review PR #60 round 3, fixing
                    // a latent bug in the round-1/round-2 patches' own
                    // `Instant::now()`-sentinel approach to this same gap).
                    interval_deadline: None,
                },
            );
            // Round-8 fix (Codex review, ADR-165 PR #42 round 8, design-
            // advisor consult): captured while `guard` is still held, so
            // this capture and __mock_reset's/mock_reset's own epoch bumps
            // (now also moved inside their own state-lock critical
            // sections) are mutually ordered by the same mutex -- either
            // this capture happens-before a subsequent reset's bump
            // (workers' later under-lock re-checks will then see the
            // bumped epoch and exit), or a prior reset's bump-then-clear
            // fully happens-before this insert (this slot legitimately
            // belongs to the post-reset world, and the captured epoch
            // matches the current one). Loading this AFTER `drop(guard)`
            // (the pre-round-8 ordering) left a window where a reset's
            // lock-free epoch bump could land between the insert and the
            // load, letting a spawned worker capture an already-bumped
            // epoch for a slot about to be wiped -- see
            // spawn_repeat_worker's doc comment for how that worker would
            // then never detect its own staleness.
            let epoch = REPEAT_WORKER_EPOCH.load(std::sync::atomic::Ordering::SeqCst);
            drop(guard);
            spawn_repeat_worker(channel_id, msg_id, epoch);
            unsafe {
                *(p_output as *mut u32) = msg_id;
            }
            no_error()
        }

        // SAE J2534-2 clause 14 Repeat Messaging (ADR-173, superseding
        // ADR-165/Phase 12's status polarity): `pInput`/`pOutput` are both
        // `uint32_t*` (`MsgId` in, status out). Table 53's QUERY status is
        // `1` for a still-live (not yet `terminated`) slot, `0` for a
        // terminated-but-not-yet-stopped one -- clause 14.2.2.3 requires
        // `MsgId` to survive self-termination, so a terminated slot is still
        // present here and reports `0`, not `ERR_INVALID_MSG_ID`. The latter
        // is reserved for an unknown/already-stopped `MsgId` (genuinely
        // removed by `IOCTL_STOP_REPEAT_MESSAGE`), scoped to this
        // `channel_id` (a `MsgId` from another channel never resolves,
        // since `repeat_slots` is per-`ChannelState`).
        //
        // Codex review (PR #60, round 4): QUERY is a third reader of
        // `interval_deadline`/`rx_seen_this_interval`/`terminated`, alongside
        // `note_rx_frame_for_repeat_slots` and `spawn_repeat_worker` -- both
        // of which already call `advance_repeat_slot_windows` before
        // consulting `terminated`. Without the same call here, a QUERY
        // landing in the gap between a `Condition == 1` window's deadline
        // and either of those two call sites next noticing it would read
        // the stale `terminated == false` and report status 1 (live) for a
        // slot that has, in fact, already gone silent -- and the service's
        // `prune_stale_repeat_message_ids` (which QUERY backs) would then
        // wrongly keep blocking a `LOCK_PHYSICAL_TX_QUEUE` grant on it.
        // Advancing here, under the same lock, closes that gap the same
        // way the other two call sites already do.
        x if x == IOCTL_QUERY_REPEAT_MESSAGE => {
            if p_input.is_null() || p_output.is_null() {
                return err_null();
            }
            let msg_id = unsafe { *(p_input as *const u32) };
            let mut guard = state().lock().expect("mock state poisoned");
            let Some(channel) = guard.channels.get_mut(&channel_id) else {
                return err_channel();
            };
            if let Some(slot) = channel.repeat_slots.get_mut(&msg_id) {
                // Skip the advance on an already-terminated slot, mirroring
                // `note_rx_frame_for_repeat_slots`/`spawn_repeat_worker`'s
                // own `if slot.terminated { ... }` short-circuit before
                // calling `advance_repeat_slot_windows` -- harmless either
                // way (the function only ever sets `terminated = true`,
                // never clears it, so a redundant call can't change the
                // reported status), but avoids pointlessly advancing
                // `interval_deadline`/`rx_seen_this_interval` on a slot
                // nothing else will ever consult those fields for again.
                if !slot.terminated {
                    advance_repeat_slot_windows(slot, std::time::Instant::now());
                }
                unsafe {
                    *(p_output as *mut u32) = if slot.terminated { 0 } else { 1 };
                }
                no_error()
            } else {
                ERR_INVALID_MSG_ID as c_long
            }
        }

        // SAE J2534-2 clause 14 Repeat Messaging (ADR-165/Phase 12).
        // `pInput` is a `uint32_t*` (`MsgId`), no output. Removes the slot
        // (ending `spawn_repeat_worker`'s loop on its next iteration, if it
        // has not already exited on its own after the slot was
        // `terminated`) and returns `ERR_INVALID_MSG_ID` for an
        // unknown/already-stopped `MsgId`, scoped to this `channel_id` the
        // same way `QUERY` is. Unaffected by ADR-173's status-polarity
        // correction above -- STOP always removes the slot outright
        // regardless of whether it was live or already `terminated`, so its
        // own behavior needs no change.
        x if x == IOCTL_STOP_REPEAT_MESSAGE => {
            if p_input.is_null() {
                return err_null();
            }
            let msg_id = unsafe { *(p_input as *const u32) };
            let mut guard = state().lock().expect("mock state poisoned");
            if !guard.channels.contains_key(&channel_id) {
                return err_channel();
            }
            // Error injection (`__mock_set_stop_repeat_message_error`):
            // checked after the bad-`channel_id` check above (so an unknown
            // `channel_id` still reports its own, more specific error) but
            // before the real `repeat_slots.remove(...)` below, mirroring
            // `stop_filter_error`'s convention (the channel-scoped
            // precedent this actually mirrors -- `j1962_pin_voltage_error`
            // is device-scoped, with no `channel_id` check to order
            // against) -- lets a test force a native STOP failure for a
            // `MsgId` that is genuinely still alive, which
            // `ERR_INVALID_MSG_ID` (the only failure this arm otherwise
            // produces) can never simulate.
            if let Some(code) = guard.stop_repeat_message_error {
                return code;
            }
            let channel = guard
                .channels
                .get_mut(&channel_id)
                .expect("channel presence just confirmed above");
            if channel.repeat_slots.remove(&msg_id).is_some() {
                no_error()
            } else {
                ERR_INVALID_MSG_ID as c_long
            }
        }

        // ADR-219 Decision item 2: the two dedicated vendor-range IoctlIDs
        // this mock recognizes, for `tests/grpc_mock` coverage of
        // `j2534-0404-service`'s vendor IOCTL passthrough dispatch -- see
        // `MOCK_VENDOR_IOCTL_RAW_U32`/`MOCK_VENDOR_IOCTL_WRAPPED_ECHO`'s own
        // doc comments for each mode's exact wire shape and echo transform.
        // Neither looks up `channel_id` (the generic native handle
        // `PassThruIoctl` was called with, DeviceID or ChannelID
        // depending on which the service resolved) -- a vendor IOCTL's
        // native target is opaque to this mock, nothing to validate against.
        x if x == MOCK_VENDOR_IOCTL_RAW_U32 => {
            let input_value = if p_input.is_null() {
                None
            } else {
                Some(unsafe { *(p_input as *const u32) })
            };
            if !p_output.is_null() {
                unsafe {
                    *(p_output as *mut u32) = input_value.unwrap_or(0).wrapping_add(1);
                }
            }
            no_error()
        }

        x if x == MOCK_VENDOR_IOCTL_WRAPPED_ECHO => {
            let input_bytes: Vec<u8> = if p_input.is_null() {
                Vec::new()
            } else {
                let input = unsafe { &*(p_input as *const SBYTE_ARRAY) };
                if input.NumOfBytes == 0 || input.BytePtr.is_null() {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(input.BytePtr, input.NumOfBytes as usize) }
                        .to_vec()
                }
            };
            if !p_output.is_null() {
                let output = unsafe { &mut *(p_output as *mut SBYTE_ARRAY) };
                let capacity = output.NumOfBytes as usize;
                let mut echoed: Vec<u8> = input_bytes.iter().rev().copied().collect();
                echoed.truncate(capacity);
                if !echoed.is_empty() {
                    if output.BytePtr.is_null() {
                        return err_null();
                    }
                    let out_slice =
                        unsafe { std::slice::from_raw_parts_mut(output.BytePtr, capacity) };
                    out_slice[..echoed.len()].copy_from_slice(&echoed);
                }
                // Self-reports the ACTUAL echoed length, not the requested
                // capacity -- ADR-219 Decision item 2's wrapped-mode output
                // convention `j2534-0404-service` reads back via
                // `min(NumOfBytes, output_capacity)`.
                output.NumOfBytes = echoed.len() as u32;
            }
            no_error()
        }

        _ => ERR_INVALID_IOCTL_ID as c_long,
    }
});

// ── Back-door exports (FFI) ───────────────────────────────────────────────────

exported_fn!(
    /// Reset all mock state. Call between tests for isolation.
    ///
    /// Bumps [`REPEAT_WORKER_EPOCH`] (edge-case-hunter fix, Bug 4, ADR-165)
    /// so any `spawn_repeat_worker` thread still running from a prior test
    /// -- one whose slot was never explicitly stopped, or whose STOP raced
    /// its own thread's next poll tick -- exits on its own next loop
    /// iteration instead of surviving into a fresh test's identical
    /// post-reset `(channel_id, msg_id)` key space and silently duplicating
    /// writes into a same-keyed new slot.
    ///
    /// Round-8 fix (Codex review, ADR-165 PR #42 round 8, design-advisor
    /// consult): the bump happens INSIDE this function's own `state()` lock
    /// critical section, not before it. Bumping lock-free (the pre-round-8
    /// shape) let this bump interleave between `START_REPEAT_MESSAGE`'s
    /// slot insert and its epoch capture, so a worker spawned for a
    /// brand-new slot could capture an already-bumped epoch and never
    /// detect its own staleness. With the bump under the same mutex
    /// `START_REPEAT_MESSAGE` captures its epoch under (see that handler's
    /// own comment) and every worker's under-lock re-checks acquire, the
    /// bump and every other epoch-sensitive critical section are totally
    /// ordered against each other -- not merely "promptly visible".
    __mock_reset() -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    REPEAT_WORKER_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *guard = MockState::default();
    no_error()
});

exported_fn!(
    /// Caps the number of simultaneously open channels; `PassThruConnect`
    /// returns `ERR_EXCEEDED_LIMIT` once `limit` channels are open. `limit = 0`
    /// removes the cap (the default after `__mock_reset`). Lets tests simulate a
    /// J2534 device that cannot open an `ISO15765` and a `CAN` channel at once.
    __mock_set_max_channels(limit: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.max_channels = limit as usize;
    no_error()
});

exported_fn!(
    /// SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 4/Phase
    /// 2b): overrides the `_CHx` capacity this mock reports (via
    /// `DEVICE_INFO_<PROTOCOL>_SUPPORTED`'s packed value) and enforces (at
    /// `PassThruConnect`) for EVERY in-scope protocol family, replacing
    /// `DEFAULT_CHX_CAPACITY`. Lets tests exercise both sides of
    /// `j2534-0404-service`'s capacity precheck boundary
    /// (`channel_index <= count` succeeds, `channel_index > count` fails
    /// `ERR_NOT_SUPPORTED`) deterministically. There is no "clear override"
    /// sentinel distinct from `__mock_reset`: unlike `__mock_set_max_channels`'s
    /// `limit = 0` (a legitimate "unlimited" value), `count = 0` is itself a
    /// legitimate "no _CHx capacity for this family" answer a real device
    /// could report, so it cannot double as a no-op sentinel.
    __mock_set_chx_capacity(count: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.chx_capacity_override = Some(count);
    no_error()
});

exported_fn!(
    /// ADR-211 (Codex review round-2 correction, PR #124): overrides ONLY
    /// `DEVICE_INFO_FT_CAN_SUPPORTED`/`DEVICE_INFO_FT_ISO15765_SUPPORTED`'s
    /// own packed `_CHx` count, independent of `__mock_set_chx_capacity`
    /// above (which continues to govern every other family's Discovery
    /// answer, and native `PassThruConnect`-time enforcement for every
    /// family including FT-CAN, unaffected by this override). See
    /// `ft_can_chx_capacity_override`'s own field doc for why this exists:
    /// without it, no gRPC-mock test can distinguish `check_chx_capacity`
    /// correctly consulting the FT-specific flag from it incorrectly
    /// consulting the generic `DEVICE_INFO_CAN_SUPPORTED`/
    /// `DEVICE_INFO_ISO15765_SUPPORTED` one, since both would otherwise
    /// always report the same count.
    __mock_set_ft_can_chx_capacity(count: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.ft_can_chx_capacity_override = Some(count);
    no_error()
});

exported_fn!(
    /// ADR-212/Round 2: the Single Wire CAN analog of
    /// `__mock_set_ft_can_chx_capacity` just above -- overrides ONLY
    /// `DEVICE_INFO_SW_CAN_SUPPORTED`/`DEVICE_INFO_SW_ISO15765_SUPPORTED`'s
    /// own packed `_CHx` count, independent of `__mock_set_chx_capacity`
    /// above (which continues to govern every other family's Discovery
    /// answer, and native `PassThruConnect`-time enforcement for every
    /// family including SW-CAN, unaffected by this override). See
    /// `sw_can_chx_capacity_override`'s own field doc for why this exists:
    /// without it, no gRPC-mock test can distinguish `check_chx_capacity`
    /// correctly consulting the SW-CAN-specific flag from it incorrectly
    /// consulting the generic `DEVICE_INFO_CAN_SUPPORTED`/
    /// `DEVICE_INFO_ISO15765_SUPPORTED` one, since both would otherwise
    /// always report the same count.
    __mock_set_sw_can_chx_capacity(count: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.sw_can_chx_capacity_override = Some(count);
    no_error()
});

exported_fn!(
    /// ADR-213/Round 3: the CAN FD analog of `__mock_set_ft_can_chx_capacity`/
    /// `__mock_set_sw_can_chx_capacity` above -- overrides ONLY
    /// `DEVICE_INFO_FD_CAN_SUPPORTED`'s own packed `_CHx` count, independent
    /// of `__mock_set_chx_capacity` above (which continues to govern every
    /// other family's Discovery answer, and native `PassThruConnect`-time
    /// enforcement for every family including CAN FD, unaffected by this
    /// override). See `fd_can_chx_capacity_override`'s own field doc for why
    /// this exists: without it, no gRPC-mock test can distinguish
    /// `check_chx_capacity` correctly consulting the CAN-FD-specific flag
    /// from it incorrectly consulting the generic `DEVICE_INFO_CAN_SUPPORTED`
    /// one, since both would otherwise always report the same count.
    __mock_set_fd_can_chx_capacity(count: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.fd_can_chx_capacity_override = Some(count);
    no_error()
});

exported_fn!(
    /// ADR-213/Round 3: the ISO15765-on-CAN-FD analog of
    /// `__mock_set_fd_can_chx_capacity` just above -- overrides ONLY
    /// `DEVICE_INFO_FD_ISO15765_SUPPORTED`'s own packed `_CHx` count,
    /// independent of `__mock_set_chx_capacity` above (which continues to
    /// govern every other family's Discovery answer, and native
    /// `PassThruConnect`-time enforcement for every family including
    /// ISO15765-on-CAN-FD, unaffected by this override). See
    /// `fd_iso15765_chx_capacity_override`'s own field doc for why this
    /// exists.
    __mock_set_fd_iso15765_chx_capacity(count: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.fd_iso15765_chx_capacity_override = Some(count);
    no_error()
});

exported_fn!(
    /// SAE J2534-2 clause 8 Mixed Format Frames on a CAN Network
    /// (ADR-160/Phase 3c): `unsupported != 0` forces every subsequent
    /// `IOCTL_SET_CONFIG(CONFIG_CAN_MIXED_FORMAT)` call, on any channel, to
    /// fail `ERR_NOT_SUPPORTED` -- simulating a device that doesn't
    /// implement clause 8 at all, so a test can exercise
    /// `j2534-0404-service`'s connect-time rollback path
    /// (`connect_new_physical_channel`'s `native_mixed_format` step)
    /// without a real non-conformant adapter. `unsupported == 0` clears the
    /// override (the default after `__mock_reset`), leaving only the
    /// ISO15765-family-only gate `IOCTL_SET_CONFIG` already enforces.
    __mock_set_can_mixed_format_unsupported(unsupported: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.can_mixed_format_unsupported = unsupported != 0;
    no_error()
});

exported_fn!(
    /// Sets the simulated SAE J1850 bus flavor for the VPW/PWM auto-detect
    /// probe (ADR-070): `0` = silent (no unsolicited RX; the default after
    /// `__mock_reset`), `PROTOCOL_J1850VPW` (1) = the bus "answers" only a
    /// `J1850VPW` connect, `PROTOCOL_J1850PWM` (2) = only a `J1850PWM`
    /// connect. Any other value also means silent. Takes effect on every
    /// `PassThruConnect` from this call onward (does not retroactively queue
    /// a response on channels already open).
    __mock_set_j1850_bus_flavor(flavor: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.j1850_bus_flavor = match flavor {
        PROTOCOL_J1850VPW | PROTOCOL_J1850PWM => Some(flavor),
        _ => None,
    };
    no_error()
});

exported_fn!(
    /// Pin-gated variant of `__mock_set_j1850_bus_flavor` (ADR-156/157 Bug B
    /// regression coverage): `flavor` must be `PROTOCOL_J1850VPW` (1) or
    /// `PROTOCOL_J1850PWM` (2) (any other value disables the simulation,
    /// same as `__mock_set_j1850_bus_flavor`'s convention). Unlike that
    /// knob, the bus only "answers" once a channel opened with `flavor`'s
    /// `_PS` variant has had `IOCTL_SET_CONFIG(CONFIG_J1962_PINS)` applied
    /// with exactly `pin_select` -- a probe that never binds pins, or binds
    /// the wrong ones, never observes a response. Takes effect on every
    /// `IOCTL_SET_CONFIG` from this call onward.
    __mock_set_j1850_bus_flavor_requiring_pins(flavor: u32, pin_select: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.j1850_bus_flavor_requiring_pins = match flavor {
        PROTOCOL_J1850VPW | PROTOCOL_J1850PWM => Some((flavor, pin_select)),
        _ => None,
    };
    no_error()
});

exported_fn!(
    /// Forces `IOCTL_FAST_INIT` to fail with the given raw J2534 error code
    /// on every subsequent call, on every channel, instead of its normal
    /// success behavior -- lets tests exercise the `CoptStartcomm` init-
    /// failure path (`PduErrEvtInitError`, temp-param revert, `PduCopstFinished`,
    /// ADR-077) without a real adapter. `code == STATUS_NOERROR` (`0`) clears
    /// the override (the default after `__mock_reset`), mirroring
    /// `__mock_set_max_channels`'s `limit = 0` convention. The channel/
    /// protocol validity checks `IOCTL_FAST_INIT` already performs (bad
    /// `channel_id`, non-K-line protocol) still take priority over this
    /// override, and a forced failure never increments `__mock_get_fast_init_count`.
    __mock_set_fast_init_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.fast_init_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// Forces `PassThruConnect` to fail with the given raw J2534 error code
    /// on every subsequent `PROTOCOL_ETHERNET_NDIS` connect attempt, instead
    /// of its normal success behavior -- lets `j2534-0404-service` tests
    /// exercise SAE J2534-2 clause 24's activation-line-not-achieved connect
    /// failure path (`ERR_NO_CONNECTION_ESTABLISHED` mapping to
    /// `PDU_ERR_NO_CABLE_DETECTED`, ADR-194/Phase 16) without a real
    /// adapter. `code == STATUS_NOERROR` (`0`) clears the override (the
    /// default after `__mock_reset`), mirroring `__mock_set_fast_init_error`'s
    /// convention. Unlike that blanket "every channel" override, this one
    /// only ever affects a connect attempt whose `protocol_id` is
    /// `PROTOCOL_ETHERNET_NDIS` -- every other protocol's `PassThruConnect`
    /// is unaffected.
    __mock_set_ndis_connect_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.ndis_connect_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// Forces `PassThruStopMsgFilter` to fail with the given raw J2534 error
    /// code on every subsequent call, on every channel, instead of its
    /// normal success behavior -- lets tests exercise `j2534-0404-service`'s
    /// `PDU_ERR_FCT_FAILED` reporting for `PDU_IOCTL_STOP_MSG_FILTER`/
    /// `PDU_IOCTL_CLEAR_MSG_FILTER` (ADR-114) without a real adapter.
    /// `code == STATUS_NOERROR` (`0`) clears the override (the default after
    /// `__mock_reset`), mirroring `__mock_set_fast_init_error`'s convention.
    /// The bad-`channel_id` check `PassThruStopMsgFilter` already performs
    /// still takes priority over this override, and a forced failure never
    /// increments `__mock_get_stop_filter_count`.
    __mock_set_stop_filter_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.stop_filter_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// Forces `PassThruWriteMsgs` to fail with the given raw J2534 error code
    /// on every subsequent call, on every channel, instead of its normal
    /// success behavior -- lets tests exercise `j2534-0404-service`'s RC21/
    /// RC23 (NRC 0x21/0x23) auto-re-request path's own retransmit failure
    /// (`TxFailure::Event(PduErrEvtTxError)` ->
    /// `ReceivePhaseOutcome::ReRequestTxFailed`, ADR-087) without a real
    /// adapter. `code == STATUS_NOERROR` (`0`) clears the override (the
    /// default after `__mock_reset`), mirroring
    /// `__mock_set_stop_filter_error`'s convention. The bad-`channel_id`/
    /// unassigned-pins/analog-in/Ethernet_NDIS checks `PassThruWriteMsgs`
    /// already performs still take priority over this override.
    __mock_set_write_msgs_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.write_msgs_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// Forces `PassThruIoctl(IOCTL_STOP_REPEAT_MESSAGE)` to fail with the
    /// given raw J2534 error code on every subsequent call, on every
    /// channel, instead of its normal success/`ERR_INVALID_MSG_ID` behavior
    /// -- lets tests exercise `j2534-0404-service`'s
    /// `PDU_IOCTL_STOP_REPEAT_MESSAGE` handler (`ioctl_stop_repeat_message`,
    /// which maps a native failure through the generic
    /// `map_native_error_for_link` fallback since no dedicated mapping
    /// table entry exists for it) for a `MsgId` that is genuinely still
    /// alive and retransmitting, something `ERR_INVALID_MSG_ID` alone can
    /// never simulate. `code == STATUS_NOERROR` (`0`) clears the override
    /// (the default after `__mock_reset`), mirroring
    /// `set_stop_filter_error`'s convention.
    __mock_set_stop_repeat_message_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.stop_repeat_message_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// ADR-192/Phase 7 Stage 7c, Codex review round 2 (P2) fix: forces
    /// `PassThruStopPeriodicMsg` to fail with the given raw J2534 error code
    /// on every subsequent call, on every channel, instead of its normal
    /// unconditional-success behavior -- lets tests exercise
    /// `j2534-0404-service`'s `rpc_cancel_com_primitive` restore-and-error
    /// path for a TP2.0 broadcast periodic COP's `CoptCancel`, for a
    /// `MsgId` that is genuinely still alive and retransmitting.
    /// `code == STATUS_NOERROR` (`0`) clears the override (the default after
    /// `__mock_reset`), mirroring `__mock_set_stop_repeat_message_error`'s
    /// convention.
    __mock_set_stop_periodic_message_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.stop_periodic_message_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// Forces `PassThruSetProgrammingVoltage` to fail with the given raw
    /// J2534 error code on every subsequent call instead of its normal
    /// success behavior -- lets tests exercise `j2534-0404-service`'s
    /// `PDU_IOCTL_SET_PROG_VOLTAGE` native-failure mapping (A2-21) without a
    /// real adapter. `code == STATUS_NOERROR` (`0`) clears the override (the
    /// default after `__mock_reset`), mirroring `set_stop_filter_error`'s
    /// convention.
    __mock_set_prog_voltage_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.prog_voltage_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// Forces `PassThruIoctl(IOCTL_BECOME_MASTER)` to fail with the given raw
    /// J2534 error code on every subsequent call instead of its normal
    /// success behavior -- lets tests exercise `j2534-0404-service`'s
    /// `PDU_IOCTL_BECOME_MASTER` native-failure mapping (e.g. the documented
    /// `ERR_FAILED` "no poll message within 2s" outcome) without a real
    /// adapter or an actual 2-second wait. `code == STATUS_NOERROR` (`0`)
    /// clears the override (the default after `__mock_reset`), mirroring
    /// `set_prog_voltage_error`'s convention.
    __mock_set_become_master_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.become_master_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// PR #98 GM UART `BECOME_MASTER`-in-flight race regression test: arms
    /// the next `IOCTL_BECOME_MASTER` call to block until
    /// `__mock_release_become_master_hold` releases it (see
    /// [`BecomeMasterHold`]'s own doc comment for the rendezvous mechanism).
    /// Cleared by `__mock_reset` (`MockState::default()`); calling this
    /// again installs a brand-new, independent hold for subsequent calls --
    /// this mock's own tests only ever arm one hold at a time.
    __mock_arm_become_master_hold() -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.become_master_hold = Some(Arc::new(BecomeMasterHold {
        state: Mutex::new(BecomeMasterHoldState::default()),
        condvar: Condvar::new(),
    }));
    no_error()
});

exported_fn!(
    /// PR #98 GM UART `BECOME_MASTER`-in-flight race regression test: `1`
    /// once the currently-armed hold's `IOCTL_BECOME_MASTER` call has
    /// actually entered its wait (not merely that
    /// `__mock_arm_become_master_hold` was called) -- `0` if no hold is
    /// armed, or the armed hold's call has not reached its wait yet. Lets a
    /// test wait deterministically for "the native call is genuinely in
    /// flight" instead of a fixed sleep (ADR-149).
    __mock_become_master_hold_engaged() -> u32 {
    let guard = state().lock().expect("mock state poisoned");
    let engaged = guard.become_master_hold.as_ref().is_some_and(|hold| {
        hold.state
            .lock()
            .expect("become_master hold state poisoned")
            .engaged
    });
    engaged as u32
});

exported_fn!(
    /// PR #98 GM UART `BECOME_MASTER`-in-flight race regression test:
    /// releases the currently-armed hold (`__mock_arm_become_master_hold`),
    /// letting its blocked `IOCTL_BECOME_MASTER` call return. A no-op if no
    /// hold is armed -- the release is observed by a call that has not yet
    /// reached its wait, so there is no ordering requirement against
    /// `__mock_become_master_hold_engaged`.
    __mock_release_become_master_hold() -> c_long {
    let guard = state().lock().expect("mock state poisoned");
    if let Some(hold) = guard.become_master_hold.as_ref() {
        let mut hstate = hold.state.lock().expect("become_master hold state poisoned");
        hstate.released = true;
        hold.condvar.notify_all();
    }
    no_error()
});

exported_fn!(
    /// Forces `IOCTL_READ_J1962PIN_VOLTAGE` to fail with the given raw J2534
    /// error code on every subsequent call, before the pin-based validation
    /// runs -- lets tests exercise `j2534-0404-service`'s
    /// `PDU_IOCTL_READ_J1962PIN_VOLTAGE` generic native-failure fallback
    /// (any code other than `ERR_PIN_INVALID`) without a real adapter.
    /// `code == STATUS_NOERROR` (`0`) clears the override (the default after
    /// `__mock_reset`), mirroring `set_prog_voltage_error`'s convention.
    __mock_set_j1962_pin_voltage_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.j1962_pin_voltage_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// Forces `PassThruReadMsgs` to fail with the given raw J2534 error code
    /// on every subsequent call, on every channel, instead of its normal
    /// success/`ERR_BUFFER_EMPTY` behavior -- lets tests exercise
    /// `j2534-0404-service`'s background poll task's hard-channel-error path
    /// (`poll_rx_inner`/`handle_channel_hard_error`, ADR-105) without a real
    /// adapter. `code == STATUS_NOERROR` (`0`) clears the override (the
    /// default after `__mock_reset`), mirroring `set_stop_filter_error`'s
    /// convention. The bad-`channel_id` check `PassThruReadMsgs` already
    /// performs still takes priority over this override; `__mock_get_read_msgs_count`
    /// still increments on a forced failure, matching `PassThruReadMsgs`'s
    /// pre-existing unconditional-count behavior (the counter increments
    /// before this override is even checked).
    __mock_set_read_msgs_error(code: c_long) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.read_msgs_error = if code == STATUS_NOERROR as c_long {
        None
    } else {
        Some(code)
    };
    no_error()
});

exported_fn!(
    /// SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5): forces
    /// every subsequent `IOCTL_PROTECT_J1939_ADDR` claim attempt (non-cancel
    /// form) on every channel to resolve `J1939_ADDRESS_LOST` instead of
    /// `J1939_ADDRESS_CLAIMED` -- lets tests exercise
    /// `j2534-0404-service`'s retry-over-the-`CP_J1939PreferredAddress`-list
    /// behavior (ADR-179 Decision 3) without a real multi-node bus.
    /// `lost == 0` clears the override (the default after `__mock_reset`),
    /// mirroring `__mock_set_read_msgs_error`'s convention.
    __mock_set_j1939_claim_lost(lost: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.j1939_claim_lost = lost != 0;
    no_error()
});

exported_fn!(
    /// SAE J2534-2 clause 16 SAE J1939 Protocol: makes only the next
    /// `count` `IOCTL_PROTECT_J1939_ADDR` claim attempts (non-cancel form,
    /// on a known channel, that produce an indication) resolve
    /// `J1939_ADDRESS_LOST`; later attempts claim normally. Replaces any
    /// remaining count; `count == 0` clears it (the default after
    /// `__mock_reset`). Use this instead of toggling
    /// `__mock_set_j1939_claim_lost` mid-claim when a test needs a specific
    /// candidate to win: the service issues the next candidate within one
    /// poll tick, faster than a test can react.
    __mock_set_j1939_claim_lost_count(count: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.j1939_claim_lost_remaining = count;
    no_error()
});

exported_fn!(
    /// SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8): `supported
    /// == 0` forces `IOCTL_GET_DEVICE_INFO`'s `DEVICE_INFO_GM_UART_SUPPORTED`
    /// arm to report `Supported = 0` on every subsequent call, instead of
    /// this mock's normal `Supported = 1` -- lets tests exercise
    /// `enforce_discovery_capability`'s ADR-185 Stage 1 fail-fast
    /// connect-time rejection for this family. `supported != 0` (the default
    /// after `__mock_reset`) restores normal advertised-supported behavior.
    __mock_set_gm_uart_supported(supported: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.gm_uart_unsupported = supported == 0;
    no_error()
});

exported_fn!(
    /// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): `supported ==
    /// 0` forces `IOCTL_GET_DEVICE_INFO`'s `DEVICE_INFO_ETHERNET_NDIS_
    /// SUPPORTED` arm to report `Supported = 0` on every subsequent call,
    /// instead of this mock's normal `Supported = 1` -- lets tests exercise
    /// `enforce_discovery_capability`'s ADR-185 Stage 1 fail-fast
    /// connect-time rejection for this family. `supported != 0` (the default
    /// after `__mock_reset`) restores normal advertised-supported behavior.
    __mock_set_ndis_supported(supported: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.ndis_unsupported = supported == 0;
    no_error()
});

exported_fn!(
    /// ADR-180 Decision 21 regression coverage (design-advisor consult,
    /// Codex review PR #72): forces every subsequent `IOCTL_PROTECT_J1939_
    /// ADDR` claim attempt (non-cancel form) on every channel to return
    /// success synchronously without ever reporting `J1939_ADDRESS_CLAIMED`/
    /// `_LOST` -- see `j1939_claim_no_indication`'s own field doc for why
    /// this is what makes `j2534-0404-service`'s bounded claim wait actually
    /// wait, letting a test deterministically send `CancelComPrimitive`
    /// while it is in flight. `enabled == 0` clears the override (the
    /// default after `__mock_reset`), mirroring `__mock_set_j1939_claim_lost`'s
    /// own convention.
    __mock_set_j1939_claim_no_indication(enabled: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.j1939_claim_no_indication = enabled != 0;
    no_error()
});

exported_fn!(
    /// ADR-180 Decision 22 regression coverage (design-advisor consult,
    /// Codex review PR #72): forces every subsequent `IOCTL_PROTECT_J1939_
    /// ADDR` CANCEL form (all-zero NAME) on every channel to return a
    /// failure status instead of its normal unconditional success, WITHOUT
    /// removing the address from the mock's own `j1939_claimed_addresses`
    /// bookkeeping -- see `j1939_cancel_error`'s own field doc.
    /// `enabled == 0` clears the override (the default after
    /// `__mock_reset`), mirroring `__mock_set_j1939_claim_lost`'s own
    /// convention.
    __mock_set_j1939_cancel_error(enabled: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.j1939_cancel_error = enabled != 0;
    no_error()
});

exported_fn!(
    /// ADR-188 regression coverage (Codex review, PR #97, 4th round): forces
    /// every subsequent `IOCTL_REQUEST_CONNECTION` (non-slots-full success
    /// path) on every channel to still allocate a real
    /// `ChannelState::tp20_connections` slot but return success synchronously
    /// without ever reporting `CONNECTION_ESTABLISHED` -- see
    /// `tp20_no_indication`'s own field doc for why this is what makes
    /// `j2534-0404-service`'s bounded connection-request wait actually wait,
    /// letting a test deterministically disconnect the requesting CLL while
    /// it is in flight. `enabled == 0` clears the override (the default
    /// after `__mock_reset`), mirroring `__mock_set_j1939_claim_no_
    /// indication`'s own convention.
    __mock_set_tp20_no_indication(enabled: u32) -> c_long {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.tp20_no_indication = enabled != 0;
    no_error()
});

exported_fn!(
    /// SAE J2534-2 clause 19.3.3.1 TP2.0 passive connections (ADR-190/Phase
    /// 7 Stage 7b): simulates an unsolicited inbound connection request
    /// arriving for `channel_id`'s currently-armed passive listener (the
    /// device-side accept a real adapter performs autonomously, with no
    /// `IOCTL_REQUEST_CONNECTION` call at all -- clause 19.3.3.1). Reads
    /// this channel's own `CONFIG_TP2_0_IDENTIFER`/`_RXIDPASSIVE` values
    /// (defaulting to `0`, i.e. unset, if never `SET_CONFIG`'d); if either
    /// is `0`, or this channel's `tp20_connections` table is already at
    /// [`TP20_MAX_CONNECTIONS_PER_CHANNEL`] capacity, silently no-ops --
    /// mirroring clause 19.3.3.1's own network-side rejection (reason
    /// `0xD8`), which never reaches the application as an indication:
    /// nothing is pushed to `rx_queue`, only
    /// `ChannelState::passive_connection_rejected_count` increments, for
    /// test observability (`__mock_get_passive_connection_rejected_count`).
    ///
    /// On success: inserts a `Tp20MockConnection { tx_id: peer_tx_id }` into
    /// `tp20_connections`, keyed on the channel's own currently-armed
    /// `CONFIG_TP2_0_RXIDPASSIVE` value (not `peer_tx_id` -- this connection
    /// becomes established AT that RX-ID, the same `tp20_connections` keying
    /// convention `IOCTL_REQUEST_CONNECTION` already uses for an active
    /// connection), and queues a `CONNECTION_ESTABLISHED` indication with
    /// `Data[0..3] = rx_id_passive` (the receiving address, `RX-ID-P` per
    /// Table 81) and `Data[4..7] = peer_tx_id` (mirroring
    /// `IOCTL_REQUEST_CONNECTION`'s own success-path indication shape,
    /// clause 19.4.4 Table 81 -- otherwise identical for a passive
    /// connection, ADR-190 Context).
    ///
    /// Returns `1` if the injection actually landed (a connection was
    /// established), `0` if it silently no-op'd -- lets a test assert on
    /// the outcome directly instead of only via the rejection counter.
    ///
    /// Respects `tp20_no_indication` (`__mock_set_tp20_no_indication`, ADR-
    /// 188's own toggle -- see its own doc comment): when armed, the native
    /// slot is still genuinely allocated below, but no `CONNECTION_
    /// ESTABLISHED` indication is queued -- the same "success synchronously,
    /// slot genuinely allocated, but the service never finds out" shape
    /// `IOCTL_REQUEST_CONNECTION`'s own `no_indication` arm already uses,
    /// letting a test deterministically construct "the service disarms
    /// while genuinely unaware a native accept already landed" (ADR-190
    /// section 4's own disarm-races-an-accept scenario) without depending
    /// on the channel poll task's own timing.
    __mock_inject_tp20_passive_connection(channel_id: u32, peer_tx_id: u32) -> u32 {
    let mut guard = state().lock().expect("mock state poisoned");
    let no_indication = guard.tp20_no_indication;
    let Some(channel) = guard.channels.get_mut(&channel_id) else {
        return 0;
    };
    let identifier = channel.params.get(&CONFIG_TP2_0_IDENTIFER).copied().unwrap_or(0);
    let rx_id_passive = channel
        .params
        .get(&CONFIG_TP2_0_RXIDPASSIVE)
        .copied()
        .unwrap_or(0);
    // No occupancy check against an existing `tp20_connections` entry for
    // the SAME `rx_id_passive` here, deliberately: this crate's own
    // "spontaneous loss" test technique (`inject_rx_with_status`, used by
    // e.g. `passive_connection_re_listens_after_a_spontaneous_loss_and_
    // re_establishes`) raw-injects a `CONNECTION_LOST` indication directly
    // into `rx_queue` without touching this mock's own `tp20_connections`
    // bookkeeping at all (mirroring the identical technique this file
    // already uses for a spontaneously-lost ACTIVE connection) -- so a
    // re-accept on the SAME `rx_id_passive` immediately afterward, while
    // this mock's own stale slot entry is technically still present, is a
    // legitimate test scenario this backdoor must allow, not a double-
    // injection bug to guard against.
    if identifier == 0
        || rx_id_passive == 0
        || channel.tp20_connections.len() >= TP20_MAX_CONNECTIONS_PER_CHANNEL
    {
        channel.passive_connection_rejected_count += 1;
        return 0;
    }
    channel
        .tp20_connections
        .insert(rx_id_passive, Tp20MockConnection { tx_id: peer_tx_id });
    if no_indication {
        return 1;
    }
    let mut data = rx_id_passive.to_be_bytes().to_vec();
    data.extend_from_slice(&peer_tx_id.to_be_bytes());
    channel.rx_queue.push_back(StoredMessage {
        protocol_id: channel.protocol_id,
        rx_status: RX_FLAG_CONNECTION_ESTABLISHED,
        tx_flags: 0,
        timestamp: MOCK_TIMESTAMP,
        data,
        extra_data_index: None,
    });
    1
});

exported_fn!(
    /// SAE J2534-2 clause 19.3.3.1 TP2.0 passive connections (ADR-190/Phase
    /// 7 Stage 7b): the number of times
    /// `__mock_inject_tp20_passive_connection` silently no-op'd for
    /// `channel_id` since `__mock_reset` -- see that function's own doc
    /// comment. `u32::MAX` if `channel_id` is unknown.
    __mock_get_passive_connection_rejected_count(channel_id: u32) -> u32 {
    let guard = state().lock().expect("mock state poisoned");
    guard
        .channels
        .get(&channel_id)
        .map_or(u32::MAX, |channel| channel.passive_connection_rejected_count)
});

exported_fn!(__mock_get_open_count() -> usize {
    state().lock().expect("mock state poisoned").counters.open
});

exported_fn!(
    /// `1` if the most recent `PassThruOpen` call (since `__mock_reset`)
    /// passed a non-NULL `pName`, `0` if it was NULL or `PassThruOpen` has
    /// never been called (ADR-106).
    __mock_open_pname_is_set() -> u32 {
    state().lock().expect("mock state poisoned").last_open_pname.is_some() as u32
});

exported_fn!(
    /// Copy the most recent `PassThruOpen` call's non-NULL `pName` bytes
    /// (NUL terminator excluded) into `buf`. Sets `*out_len` to the actual
    /// byte count. Returns STATUS_NOERROR when a non-NULL `pName` was
    /// recorded, or ERR_NULL_PARAMETER when it was NULL or `PassThruOpen`
    /// has never been called since `__mock_reset` -- check
    /// `__mock_open_pname_is_set` first to tell those two cases apart if
    /// needed (ADR-106).
    __mock_get_open_pname(buf: *mut u8, buf_len: u32, out_len: *mut u32) -> c_long {
    let guard = state().lock().expect("mock state poisoned");
    let Some(data) = guard.last_open_pname.as_ref() else {
        return err_null();
    };
    let copy_len = data.len().min(buf_len as usize);
    if !buf.is_null() && copy_len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf, copy_len); }
    }
    if !out_len.is_null() {
        unsafe { *out_len = data.len() as u32; }
    }
    no_error()
});

exported_fn!(__mock_get_close_count() -> usize {
    state().lock().expect("mock state poisoned").counters.close
});

exported_fn!(__mock_get_connect_count() -> usize {
    state().lock().expect("mock state poisoned").counters.connect
});

exported_fn!(__mock_get_disconnect_count() -> usize {
    state().lock().expect("mock state poisoned").counters.disconnect
});

exported_fn!(
    /// Number of successful `PassThruConnect` calls recorded in the
    /// connect-flags log (same count as `__mock_get_connect_count`).
    __mock_get_connect_flags_log_len() -> usize {
    state().lock().expect("mock state poisoned").connect_flags_log.len()
});

exported_fn!(
    /// Read the `Flags` argument of the `index`-th successful `PassThruConnect`
    /// call (0-based, call order). Writes to `*out_value`. Returns
    /// STATUS_NOERROR or `ERR_INVALID_CHANNEL_ID` if `index` is out of range.
    __mock_get_connect_flags_log_entry(
    index: usize,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(&flags) = guard.connect_flags_log.get(index) else {
        return err_channel();
    };
    unsafe { *out_value = flags; }
    no_error()
});

exported_fn!(__mock_get_read_msgs_count() -> usize {
    state().lock().expect("mock state poisoned").counters.read_msgs
});

exported_fn!(__mock_get_write_msgs_count() -> usize {
    state().lock().expect("mock state poisoned").counters.write_msgs
});

exported_fn!(__mock_get_start_periodic_count() -> usize {
    state().lock().expect("mock state poisoned").counters.start_periodic
});

exported_fn!(__mock_get_stop_periodic_count() -> usize {
    state().lock().expect("mock state poisoned").counters.stop_periodic
});

exported_fn!(
    /// Number of periodic messages currently live (started, not yet
    /// stopped/cleared) on `channel_id` -- ADR-192/Phase 7 Stage 7c: lets a
    /// `j2534-0404-service` test assert that `CoptCancel`/CLL teardown/
    /// `CLEAR_PERIODIC_MSGS` actually issued a native
    /// `PassThruStopPeriodicMsg` for a TP2.0 broadcast periodic COP (device-
    /// side state, not just this service's own internal tracking), mirroring
    /// `mock_get_periodic_msg_count`'s identical in-crate helper (used only
    /// by this crate's own `mod tests`, which cannot reach it -- this is the
    /// FFI-exported counterpart `j2534-0404-service`'s dylib-loaded
    /// `Backdoor` needs).
    __mock_get_periodic_msg_count(channel_id: u32) -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .channels
        .get(&channel_id)
        .map(|c| c.periodic_msgs.len())
        .unwrap_or(0)
});

exported_fn!(
    /// Read the `TimeInterval` argument of the most recent
    /// `PassThruStartPeriodicMsg` call on `channel_id`. Writes to
    /// `*out_value`. Returns `ERR_INVALID_CHANNEL_ID` if the channel does not
    /// exist or no `PassThruStartPeriodicMsg` call has been made on it since
    /// `__mock_reset`.
    __mock_get_last_periodic_time_interval(
    channel_id: u32,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some(interval) = channel.last_start_periodic_time_interval else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    unsafe { *out_value = interval; }
    no_error()
});

exported_fn!(__mock_get_set_config_count() -> usize {
    state().lock().expect("mock state poisoned").counters.set_config
});

exported_fn!(
    /// Cumulative count of successful `PassThruStartMsgFilter` calls across
    /// every channel, since `__mock_reset`. Used by `j2534-0404-service`
    /// tests to distinguish "no filter I/O happened" from "an identical
    /// filter was stopped and reinstalled" (ADR-068's promotion diff gate).
    __mock_get_start_filter_count() -> usize {
    state().lock().expect("mock state poisoned").counters.start_filter
});

exported_fn!(
    /// Cumulative count of successful `PassThruStopMsgFilter` calls across
    /// every channel, since `__mock_reset`. See `__mock_get_start_filter_count`.
    __mock_get_stop_filter_count() -> usize {
    state().lock().expect("mock state poisoned").counters.stop_filter
});

exported_fn!(__mock_get_get_config_count() -> usize {
    state().lock().expect("mock state poisoned").counters.get_config
});

exported_fn!(__mock_get_five_baud_init_count() -> usize {
    state().lock().expect("mock state poisoned").counters.five_baud_init
});

exported_fn!(__mock_get_fast_init_count() -> usize {
    state().lock().expect("mock state poisoned").counters.fast_init
});

exported_fn!(__mock_get_get_device_info_count() -> usize {
    state().lock().expect("mock state poisoned").counters.get_device_info
});

/// Per-thread twins of a few `MockState::counters` fields. Each counts only
/// the calls made on the calling thread, and `__mock_reset` does not clear
/// them. The process-wide counters are shared by every test in a process,
/// so a test that asserts an exact before/after delta on one sees other
/// tests' calls whenever they run in parallel (`cargo test`). A test that
/// issues its native calls on its own thread (a current-thread
/// `#[tokio::test]` driving the service directly) reads these instead, via
/// the `__mock_get_*_count_on_current_thread` exports.
#[derive(Default)]
struct ThreadCounters {
    get_device_info: std::cell::Cell<usize>,
    stop_periodic: std::cell::Cell<usize>,
    clear_tx_buffer: std::cell::Cell<usize>,
}

thread_local! {
    static THREAD_COUNTERS: ThreadCounters = ThreadCounters::default();
}

exported_fn!(
    /// `IOCTL_GET_DEVICE_INFO` calls made on the calling thread. See
    /// `ThreadCounters`.
    __mock_get_get_device_info_count_on_current_thread() -> usize {
    THREAD_COUNTERS.with(|c| c.get_device_info.get())
});

exported_fn!(
    /// `PassThruStopPeriodicMsg` calls made on the calling thread. See
    /// `ThreadCounters`.
    __mock_get_stop_periodic_count_on_current_thread() -> usize {
    THREAD_COUNTERS.with(|c| c.stop_periodic.get())
});

exported_fn!(
    /// `PassThruIoctl(CLEAR_TX_BUFFER)` calls made on the calling thread.
    /// See `ThreadCounters`.
    __mock_get_clear_tx_buffer_count_on_current_thread() -> usize {
    THREAD_COUNTERS.with(|c| c.clear_tx_buffer.get())
});

exported_fn!(__mock_get_get_protocol_info_count() -> usize {
    state().lock().expect("mock state poisoned").counters.get_protocol_info
});

exported_fn!(
    /// Cumulative count of `PassThruIoctl(CLEAR_RX_BUFFER)` calls across
    /// every channel, since `__mock_reset`. Used by `j2534-0404-service`
    /// tests to confirm `PDU_IOCTL_RESET`'s hardware teardown clears a
    /// shared physical channel's buffers exactly once per channel, not once
    /// per ComLogicalLink sharing it (ADR-161 Phase 1).
    __mock_get_clear_rx_buffer_count() -> usize {
    state().lock().expect("mock state poisoned").counters.clear_rx_buffer
});

exported_fn!(
    /// Cumulative count of `PassThruIoctl(CLEAR_TX_BUFFER)` calls across
    /// every channel, since `__mock_reset`. See
    /// `__mock_get_clear_rx_buffer_count`.
    __mock_get_clear_tx_buffer_count() -> usize {
    state().lock().expect("mock state poisoned").counters.clear_tx_buffer
});

exported_fn!(
    /// Cumulative count of successful (right-protocol) `PassThruIoctl(SW_CAN_HS)`
    /// calls across every channel, since `__mock_reset` (ADR-164/Phase 4).
    /// Used to confirm `PDU_IOCTL_SW_CAN_HS`'s `SharedChannel::ref_count == 1`
    /// gate actually skips the native call on a shared physical channel.
    __mock_get_sw_can_hs_count() -> usize {
    state().lock().expect("mock state poisoned").counters.sw_can_hs
});

exported_fn!(
    /// Cumulative count of successful (right-protocol) `PassThruIoctl(SW_CAN_NS)`
    /// calls across every channel, since `__mock_reset` (ADR-164/Phase 4). See
    /// `__mock_get_sw_can_hs_count`.
    __mock_get_sw_can_ns_count() -> usize {
    state().lock().expect("mock state poisoned").counters.sw_can_ns
});

exported_fn!(
    /// Copy the input frame (`PASSTHRU_MSG.Data[..DataSize]`) of the most
    /// recent `IOCTL_FAST_INIT` call on `channel_id` into `buf`. Sets
    /// `*out_len` to the actual byte count. Returns STATUS_NOERROR when an
    /// input frame was recorded (a `FAST_INIT` call with a non-null
    /// `p_input`), or ERR_INVALID_CHANNEL_ID when the channel doesn't exist
    /// or no input frame has been recorded.
    __mock_get_fast_init_input(
    channel_id: u32,
    buf: *mut u8,
    buf_len: u32,
    out_len: *mut u32
) -> c_long {
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some(data) = channel.fast_init_input.as_ref() else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    let copy_len = data.len().min(buf_len as usize);
    if !buf.is_null() && copy_len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf, copy_len); }
    }
    if !out_len.is_null() {
        unsafe { *out_len = data.len() as u32; }
    }
    no_error()
});

exported_fn!(
    /// Copy the target address byte of the most recent `IOCTL_FIVE_BAUD_INIT`
    /// call on `channel_id` into `*out_address`. Returns STATUS_NOERROR when
    /// an address byte was recorded (a `FIVE_BAUD_INIT` call with a non-null
    /// `p_input`), or ERR_INVALID_CHANNEL_ID when the channel doesn't exist
    /// or no address has been recorded (ADR-076, mirrors
    /// `__mock_get_fast_init_input`).
    __mock_get_five_baud_init_input(
    channel_id: u32,
    out_address: *mut u8
) -> c_long {
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some(address) = channel.five_baud_init_input else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    if !out_address.is_null() {
        unsafe { *out_address = address; }
    }
    no_error()
});

/// Shared body for the `__mock_inject_rx_msg` export -- see its doc comment
/// for the argument contract. `TxFlags` is hardcoded to `0` on the resulting
/// `StoredMessage`: no code path reads an RX-direction frame's `TxFlags`
/// anymore (round 14 fix, Codex review, ADR-165 PR #42 -- the round-9/13
/// `tx_flags`-threading mechanism this helper previously carried, including
/// the now-removed `__mock_inject_rx_msg_with_flags` export, compared the
/// wrong PASSTHRU_MSG field; see `RepeatSlot::response_format_rx_bits`'s
/// doc comment).
fn inject_rx_msg_ffi(
    channel_id: u32,
    data: *const u8,
    data_len: u32,
    protocol_id: u32,
    rx_status: u32,
    extra_data_index: Option<u32>,
) -> c_long {
    if data.is_null() && data_len > 0 {
        return err_null();
    }
    let slice = if data_len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data, data_len as usize) }
    };
    let msg = StoredMessage {
        protocol_id,
        rx_status,
        tx_flags: 0,
        timestamp: MOCK_TIMESTAMP,
        data: slice.to_vec(),
        extra_data_index,
    };
    let mut guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get_mut(&channel_id) else {
        return err_channel();
    };
    note_rx_frame_for_repeat_slots(channel, &msg.data, msg.rx_status, msg.protocol_id);
    channel.rx_queue.push_back(msg);
    no_error()
}

exported_fn!(
    /// Inject a message into the RX queue of `channel_id`, with `TxFlags`
    /// implicitly `0` -- an RX-direction frame's `TxFlags` is never read by
    /// any code path (round 14 fix, Codex review, ADR-165 PR #42; a
    /// `Condition == 1` repeat slot's format check now consults `RxStatus`
    /// instead, see `RepeatSlot::response_format_rx_bits`'s doc comment).
    /// Returns STATUS_NOERROR (0) or ERR_INVALID_CHANNEL_ID.
    __mock_inject_rx_msg(
    channel_id: u32,
    data: *const u8,
    data_len: u32,
    protocol_id: u32,
    rx_status: u32
) -> c_long {
    inject_rx_msg_ffi(channel_id, data, data_len, protocol_id, rx_status, None)
});

exported_fn!(
    /// ADR-171: same as `__mock_inject_rx_msg`, plus an explicit
    /// `extra_data_index` (native `ExtraDataIndex`) reported back to the
    /// caller via `write_to_passthru` instead of the `DataSize` default --
    /// lets a test inject a genuine IFR-bearing J1850 PWM response, or an
    /// out-of-range value to exercise the service-side defensive clamp.
    /// Kept as a separate export rather than widening
    /// `__mock_inject_rx_msg`'s five-argument signature (round 14 history,
    /// see `docs/implementation-notes.md`'s Repeat Messaging section).
    /// Returns STATUS_NOERROR (0) or ERR_INVALID_CHANNEL_ID.
    __mock_inject_rx_msg_with_edi(
    channel_id: u32,
    data: *const u8,
    data_len: u32,
    protocol_id: u32,
    rx_status: u32,
    extra_data_index: u32
) -> c_long {
    inject_rx_msg_ffi(channel_id, data, data_len, protocol_id, rx_status, Some(extra_data_index))
});

exported_fn!(
    /// Arms a ONE-SHOT write-triggered RX injection on `channel_id`: on the
    /// NEXT `PassThruWriteMsgs` call for this channel, `data` is pushed into
    /// the RX queue and the call then blocks for `hold_ms` (simulating slow
    /// hardware write I/O) before returning -- letting tests land an RX frame
    /// (e.g. a FlowControl) deterministically DURING a specific write, rather
    /// than merely before or after it. Overwrites any previously-armed,
    /// not-yet-fired injection for this channel. Returns STATUS_NOERROR (0)
    /// or ERR_INVALID_CHANNEL_ID.
    __mock_arm_write_rx_injection(
    channel_id: u32,
    data: *const u8,
    data_len: u32,
    protocol_id: u32,
    hold_ms: u32
) -> c_long {
    if data.is_null() && data_len > 0 {
        return err_null();
    }
    let slice = if data_len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data, data_len as usize) }
    };
    let frame = StoredMessage {
        protocol_id,
        rx_status: 0,
        tx_flags: 0,
        timestamp: MOCK_TIMESTAMP,
        data: slice.to_vec(),
        extra_data_index: None,
    };
    let mut guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get_mut(&channel_id) else {
        return err_channel();
    };
    channel.write_rx_injection = Some(WriteRxInjection { frame, hold_ms });
    no_error()
});

exported_fn!(
    /// Copy the bytes most recently staged via a successful
    /// `IOCTL_SET_POLL_RESPONSE` on `channel_id` into `buf` (SAE J2534-2
    /// clause 11, ADR-189/Phase 8). Sets `*out_len` to the actual byte count
    /// (`0` before any successful call). Returns STATUS_NOERROR or error,
    /// mirroring `__mock_get_written_msg`'s own out-buffer shape.
    __mock_get_poll_response(
    channel_id: u32,
    buf: *mut u8,
    buf_len: u32,
    out_len: *mut u32
) -> c_long {
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let copy_len = channel.poll_response.len().min(buf_len as usize);
    if !buf.is_null() && copy_len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(channel.poll_response.as_ptr(), buf, copy_len); }
    }
    if !out_len.is_null() {
        unsafe { *out_len = channel.poll_response.len() as u32; }
    }
    no_error()
});

exported_fn!(
    /// Number of messages written via PassThruWriteMsgs on `channel_id`.
    __mock_get_written_msg_count(channel_id: u32) -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.channels.get(&channel_id).map(|c| c.written_msgs.len()).unwrap_or(0)
});

exported_fn!(
    /// Microseconds from written message `from_index` to written message
    /// `to_index` on `channel_id` (negative if `to_index` was written first),
    /// measured when the mock stored each one. Returns STATUS_NOERROR or error.
    __mock_get_written_msg_gap_us(
    channel_id: u32,
    from_index: usize,
    to_index: usize,
    out_us: *mut i64
) -> c_long {
    if out_us.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let (Some(from), Some(to)) = (channel.written_at.get(from_index), channel.written_at.get(to_index)) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    let gap_us = if to >= from {
        to.duration_since(*from).as_micros() as i64
    } else {
        -(from.duration_since(*to).as_micros() as i64)
    };
    unsafe { *out_us = gap_us; }
    no_error()
});

exported_fn!(
    /// Copy data bytes of written message `index` for `channel_id` into `buf`.
    /// Sets `*out_len` to actual byte count. Returns STATUS_NOERROR or error.
    __mock_get_written_msg(
    channel_id: u32,
    index: usize,
    buf: *mut u8,
    buf_len: u32,
    out_len: *mut u32
) -> c_long {
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some(msg) = channel.written_msgs.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    let copy_len = msg.data.len().min(buf_len as usize);
    if !buf.is_null() && copy_len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(msg.data.as_ptr(), buf, copy_len); }
    }
    if !out_len.is_null() {
        unsafe { *out_len = msg.data.len() as u32; }
    }
    no_error()
});

exported_fn!(
    /// Read the stored config value for `param_id` on `channel_id`.
    /// Writes to `*out_value`. Returns STATUS_NOERROR or error.
    __mock_get_config_value(
    channel_id: u32,
    param_id: u32,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    unsafe { *out_value = channel.params.get(&param_id).copied().unwrap_or(0); }
    no_error()
});

exported_fn!(
    /// Number of `IOCTL_SET_CONFIG` param entries successfully applied on
    /// `channel_id`, in call order (ADR-158/Phase 3a; see
    /// `ChannelState::set_config_param_log`'s own doc comment).
    __mock_get_set_config_param_log_count(channel_id: u32) -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.channels.get(&channel_id).map(|c| c.set_config_param_log.len()).unwrap_or(0)
});

exported_fn!(
    /// Read the `Parameter` id of the `index`-th `IOCTL_SET_CONFIG` entry
    /// applied on `channel_id` (0-based, call order). Writes to
    /// `*out_value`. Returns STATUS_NOERROR or `ERR_INVALID_CHANNEL_ID` if
    /// `channel_id`/`index` is out of range.
    __mock_get_set_config_param_log_entry(
    channel_id: u32,
    index: usize,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some(&param_id) = channel.set_config_param_log.get(index) else {
        return err_channel();
    };
    unsafe { *out_value = param_id; }
    no_error()
});

exported_fn!(
    /// Read the baud rate `channel_id` was opened with via `PassThruConnect`.
    ///
    /// `DATA_RATE` is supplied as the `PassThruConnect` argument rather than via
    /// `IOCTL_SET_CONFIG`, so it never appears in `channel.params` and is not
    /// observable through `__mock_get_config_value` / `IOCTL_GET_CONFIG`. This
    /// accessor is the only way to confirm the baud rate an out-of-process caller
    /// (e.g. a dynamically-loaded copy of this library) actually connected with.
    /// Writes to `*out_value`. Returns STATUS_NOERROR or error.
    __mock_get_channel_baud_rate(
    channel_id: u32,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    unsafe { *out_value = channel.baud_rate; }
    no_error()
});

exported_fn!(
    /// Read the J2534 protocol ID `channel_id` was opened with via
    /// `PassThruConnect` (ADR-158/Phase 3a) -- lets a test confirm the
    /// actual native `ProtocolID` argument (e.g. `PROTOCOL_FD_CAN_PS` vs.
    /// plain `PROTOCOL_CAN`) a connect used. Writes to `*out_value`.
    /// Returns STATUS_NOERROR or error.
    __mock_get_channel_protocol_id(
    channel_id: u32,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    unsafe { *out_value = channel.protocol_id; }
    no_error()
});

exported_fn!(
    /// Read the J2534 protocol ID a written message (`PassThruWriteMsgs`) at
    /// `index` for `channel_id` carried. Writes to `*out_value`. Returns
    /// STATUS_NOERROR or error.
    __mock_get_written_msg_protocol_id(
    channel_id: u32,
    index: usize,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some(msg) = channel.written_msgs.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    unsafe { *out_value = msg.protocol_id; }
    no_error()
});

exported_fn!(
    /// Read the `TxFlags` a written message (`PassThruWriteMsgs`) at `index` for
    /// `channel_id` carried. Writes to `*out_value`. Returns STATUS_NOERROR or
    /// error.
    __mock_get_written_msg_tx_flags(
    channel_id: u32,
    index: usize,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some(msg) = channel.written_msgs.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    unsafe { *out_value = msg.tx_flags; }
    no_error()
});

exported_fn!(
    /// Read a still-live `Condition == 1`/`0` repeat slot's mask/pattern
    /// `PassThruMessage`s' raw, UNMASKED native `TxFlags`
    /// (`RepeatSlot::mask_pattern_tx_flags`) -- test-introspection only, lets
    /// a test directly confirm what `IOCTL_START_REPEAT_MESSAGE` actually
    /// staged on the mask/pattern templates themselves, distinct from the
    /// derived/masked `response_format_rx_bits` this mock uses internally
    /// for RX-frame comparison (Codex review, ADR-165 PR #42 round 15).
    /// Writes to `*out_value`. Returns `STATUS_NOERROR`, `ERR_INVALID_CHANNEL_ID`
    /// for an unknown channel, or `ERR_INVALID_MSG_ID` for an unknown/already-
    /// stopped slot.
    __mock_get_repeat_slot_mask_pattern_tx_flags(
    channel_id: u32,
    msg_id: u32,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some(slot) = channel.repeat_slots.get(&msg_id) else {
        return ERR_INVALID_MSG_ID as c_long;
    };
    unsafe { *out_value = slot.mask_pattern_tx_flags; }
    no_error()
});

exported_fn!(
    /// Number of message filters currently installed (not yet stopped, and not
    /// wiped by CLEAR_MSG_FILTERS) on `channel_id`.
    __mock_get_filter_count(channel_id: u32) -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.channels.get(&channel_id).map(|c| c.filters.len()).unwrap_or(0)
});

exported_fn!(
    /// Read the `FilterType` (`PASS_FILTER` / `BLOCK_FILTER` / `FLOW_CONTROL_FILTER`)
    /// of filter `index` for `channel_id`. Writes to `*out_value`. Returns
    /// STATUS_NOERROR or error.
    __mock_get_filter_type(
    channel_id: u32,
    index: usize,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some((_, filter)) = channel.filters.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    unsafe { *out_value = filter.filter_type; }
    no_error()
});

exported_fn!(
    /// Copy `pMaskMsg` data bytes of filter `index` for `channel_id` into `buf`.
    /// Sets `*out_len` to actual byte count. Returns STATUS_NOERROR or error.
    __mock_get_filter_mask(
    channel_id: u32,
    index: usize,
    buf: *mut u8,
    buf_len: u32,
    out_len: *mut u32
) -> c_long {
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some((_, filter)) = channel.filters.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    let copy_len = filter.mask.data.len().min(buf_len as usize);
    if !buf.is_null() && copy_len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(filter.mask.data.as_ptr(), buf, copy_len); }
    }
    if !out_len.is_null() {
        unsafe { *out_len = filter.mask.data.len() as u32; }
    }
    no_error()
});

exported_fn!(
    /// Copy `pPatternMsg` data bytes of filter `index` for `channel_id` into `buf`.
    /// Sets `*out_len` to actual byte count. Returns STATUS_NOERROR or error.
    __mock_get_filter_pattern(
    channel_id: u32,
    index: usize,
    buf: *mut u8,
    buf_len: u32,
    out_len: *mut u32
) -> c_long {
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some((_, filter)) = channel.filters.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    let copy_len = filter.pattern.data.len().min(buf_len as usize);
    if !buf.is_null() && copy_len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(filter.pattern.data.as_ptr(), buf, copy_len); }
    }
    if !out_len.is_null() {
        unsafe { *out_len = filter.pattern.data.len() as u32; }
    }
    no_error()
});

exported_fn!(
    /// Read the `TxFlags` (e.g. `CAN_29BIT_ID`, `ISO15765_ADDR_TYPE`) of `pPatternMsg`
    /// for filter `index` for `channel_id`. Writes to `*out_value`. Returns
    /// STATUS_NOERROR or error.
    __mock_get_filter_pattern_tx_flags(
    channel_id: u32,
    index: usize,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some((_, filter)) = channel.filters.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    unsafe { *out_value = filter.pattern.tx_flags; }
    no_error()
});

exported_fn!(
    /// Read the `ProtocolID` of `pPatternMsg` for filter `index` for
    /// `channel_id`. Writes to `*out_value`. Returns STATUS_NOERROR or error.
    /// Lets tests confirm a point-to-point `FLOW_CONTROL_FILTER` (ADR-048)
    /// carries the channel's actual connect-time `hw_protocol_id` (e.g.
    /// `ISO15765_PS`), not a hard-coded base protocol constant (ADR-157
    /// Plane A; found by Codex review, PR #28). `pMaskMsg`/`pFlowControlMsg`
    /// are built from the same `protocol_id` in the same call
    /// (`can_filter_message`), so checking the pattern message alone is
    /// representative of all three.
    __mock_get_filter_pattern_protocol_id(
    channel_id: u32,
    index: usize,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some((_, filter)) = channel.filters.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    unsafe { *out_value = filter.pattern.protocol_id; }
    no_error()
});

exported_fn!(
    /// Writes 1 to `*out_value` if filter `index` for `channel_id` was installed
    /// with a non-null `pFlowControlMsg`, 0 otherwise. Returns STATUS_NOERROR or error.
    __mock_get_filter_has_flow_control(
    channel_id: u32,
    index: usize,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some((_, filter)) = channel.filters.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    unsafe { *out_value = if filter.flow_control.is_some() { 1 } else { 0 }; }
    no_error()
});

exported_fn!(
    /// Copy `pFlowControlMsg` data bytes of filter `index` for `channel_id` into
    /// `buf`. Sets `*out_len` to actual byte count. Returns STATUS_NOERROR, or
    /// `ERR_INVALID_CHANNEL_ID` if the filter has no flow-control message (check
    /// `__mock_get_filter_has_flow_control` first).
    __mock_get_filter_flow_control(
    channel_id: u32,
    index: usize,
    buf: *mut u8,
    buf_len: u32,
    out_len: *mut u32
) -> c_long {
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some((_, filter)) = channel.filters.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    let Some(fc) = &filter.flow_control else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    let copy_len = fc.data.len().min(buf_len as usize);
    if !buf.is_null() && copy_len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(fc.data.as_ptr(), buf, copy_len); }
    }
    if !out_len.is_null() {
        unsafe { *out_len = fc.data.len() as u32; }
    }
    no_error()
});

exported_fn!(
    /// Read the `TxFlags` (e.g. `CAN_29BIT_ID`, `ISO15765_ADDR_TYPE`) of
    /// `pFlowControlMsg` for filter `index` for `channel_id`. Writes to
    /// `*out_value`. Returns STATUS_NOERROR, or `ERR_INVALID_CHANNEL_ID` if the
    /// filter has no flow-control message — check `__mock_get_filter_has_flow_control` first.
    __mock_get_filter_flow_control_tx_flags(
    channel_id: u32,
    index: usize,
    out_value: *mut u32
) -> c_long {
    if out_value.is_null() {
        return err_null();
    }
    let guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get(&channel_id) else {
        return err_channel();
    };
    let Some((_, filter)) = channel.filters.get(index) else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    let Some(fc) = &filter.flow_control else {
        return ERR_INVALID_CHANNEL_ID as c_long;
    };
    unsafe { *out_value = fc.tx_flags; }
    no_error()
});

// ── Back-door API (Rust in-process) ──────────────────────────────────────────

/// Reset all mock state and counters for test isolation.
///
/// Bumps [`REPEAT_WORKER_EPOCH`], same as the `__mock_reset` FFI export --
/// this in-process entry point resets the identical state
/// (`next_channel_id`/every channel's `next_repeat_msg_id`) and is subject to
/// the identical Bug 4 lingering-worker race (used by this crate's own
/// `serial_test!`-driven unit tests, not just the FFI callers).
///
/// Round-8 fix (Codex review, ADR-165 PR #42 round 8, design-advisor
/// consult): same reordering as `__mock_reset`'s own fix -- the bump now
/// happens under `state()`'s lock, not before acquiring it, so this reset's
/// bump is totally ordered (via that mutex) against `START_REPEAT_MESSAGE`'s
/// insert+epoch-capture and every worker's under-lock re-checks. See
/// `__mock_reset`'s doc comment for the full rationale.
pub fn mock_reset() {
    let mut guard = state().lock().expect("mock state poisoned");
    REPEAT_WORKER_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *guard = MockState::default();
}

/// Inject a message into the RX queue for `channel_id`. `rx_status` is the
/// frame's actual `RxStatus` (ADR-165 PR #42 round 9, corrected from
/// `TxFlags` in round 14 -- see `RepeatSlot::response_format_rx_bits`'s doc
/// comment), `protocol_id` its actual `ProtocolID` (ADR-165 PR #42 round
/// 10). `TxFlags` is hardcoded to `0`: no code path reads an RX-direction
/// frame's `TxFlags` anymore.
pub fn mock_inject_rx_message(
    channel_id: u32,
    data: Vec<u8>,
    protocol_id: u32,
    rx_status: u32,
) -> bool {
    let mut guard = state().lock().expect("mock state poisoned");
    let Some(channel) = guard.channels.get_mut(&channel_id) else {
        return false;
    };
    note_rx_frame_for_repeat_slots(channel, &data, rx_status, protocol_id);
    channel.rx_queue.push_back(StoredMessage {
        protocol_id,
        rx_status,
        tx_flags: 0,
        timestamp: MOCK_TIMESTAMP,
        data,
        extra_data_index: None,
    });
    true
}

/// Return a clone of all messages written via `PassThruWriteMsgs` for `channel_id`.
pub fn mock_get_written_messages(channel_id: u32) -> Vec<StoredMessage> {
    let guard = state().lock().expect("mock state poisoned");
    guard
        .channels
        .get(&channel_id)
        .map(|c| c.written_msgs.clone())
        .unwrap_or_default()
}

/// Return the stored config value for `(channel_id, param_id)`.
pub fn mock_get_config_value(channel_id: u32, param_id: u32) -> Option<u32> {
    let guard = state().lock().expect("mock state poisoned");
    guard
        .channels
        .get(&channel_id)
        .and_then(|c| c.params.get(&param_id).copied())
}

/// Return the baud rate `channel_id` was opened with via `PassThruConnect`.
pub fn mock_get_channel_baud_rate(channel_id: u32) -> Option<u32> {
    let guard = state().lock().expect("mock state poisoned");
    guard.channels.get(&channel_id).map(|c| c.baud_rate)
}

/// Return the number of active periodic messages on `channel_id`.
pub fn mock_get_periodic_msg_count(channel_id: u32) -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard
        .channels
        .get(&channel_id)
        .map(|c| c.periodic_msgs.len())
        .unwrap_or(0)
}

/// Return the message filters currently installed (not yet stopped, and not
/// wiped by a `CLEAR_MSG_FILTERS`) on `channel_id`.
pub fn mock_get_channel_filters(channel_id: u32) -> Vec<MockFilter> {
    let guard = state().lock().expect("mock state poisoned");
    guard
        .channels
        .get(&channel_id)
        .map(|c| c.filters.iter().map(|(_, f)| f.clone()).collect())
        .unwrap_or_default()
}

pub fn mock_get_open_count() -> usize {
    state().lock().expect("mock state poisoned").counters.open
}

/// The `pName` argument of the most recent `PassThruOpen` call: `None` if it
/// was NULL (or `PassThruOpen` has never been called since `__mock_reset`),
/// `Some(bytes)` (NUL terminator excluded) otherwise (ADR-106).
pub fn mock_get_open_pname() -> Option<Vec<u8>> {
    state()
        .lock()
        .expect("mock state poisoned")
        .last_open_pname
        .clone()
}

pub fn mock_get_close_count() -> usize {
    state().lock().expect("mock state poisoned").counters.close
}

pub fn mock_get_connect_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .connect
}

pub fn mock_get_disconnect_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .disconnect
}

/// Sets the simulated SAE J1850 bus flavor (ADR-070). See
/// `__mock_set_j1850_bus_flavor` for the accepted values.
pub fn mock_set_j1850_bus_flavor(flavor: Option<u32>) {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.j1850_bus_flavor = flavor;
}

/// Return the `Flags` argument of every successful `PassThruConnect`, in call order.
pub fn mock_get_connect_flags_log() -> Vec<u32> {
    state()
        .lock()
        .expect("mock state poisoned")
        .connect_flags_log
        .clone()
}

pub fn mock_get_write_msgs_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .write_msgs
}

pub fn mock_get_read_msgs_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .read_msgs
}

pub fn mock_get_start_periodic_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .start_periodic
}

pub fn mock_get_stop_periodic_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .stop_periodic
}

pub fn mock_get_set_config_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .set_config
}

pub fn mock_get_get_config_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .get_config
}

/// Count of `IOCTL_GET_DEVICE_INFO` calls (SAE J2534-2 §25, ADR-153).
pub fn mock_get_get_device_info_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .get_device_info
}

/// Count of `IOCTL_GET_PROTOCOL_INFO` calls, including calls rejected with
/// `ERR_INVALID_PROTOCOL_ID` (SAE J2534-2 §25, ADR-153).
pub fn mock_get_get_protocol_info_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .get_protocol_info
}

/// Count of successful `IOCTL_FIVE_BAUD_INIT` calls (rejected calls -- wrong
/// protocol -- do not increment this).
pub fn mock_get_five_baud_init_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .five_baud_init
}

/// Count of successful `IOCTL_FAST_INIT` calls (rejected calls -- wrong
/// protocol -- do not increment this).
pub fn mock_get_fast_init_count() -> usize {
    state()
        .lock()
        .expect("mock state poisoned")
        .counters
        .fast_init
}

/// Return the input frame (`PASSTHRU_MSG.Data[..DataSize]`) of the most
/// recent `IOCTL_FAST_INIT` call on `channel_id`, or `None` if no such call
/// with a non-null input has been made.
pub fn mock_get_fast_init_input(channel_id: u32) -> Option<Vec<u8>> {
    let guard = state().lock().expect("mock state poisoned");
    guard.channels.get(&channel_id)?.fast_init_input.clone()
}

/// Return the target address byte of the most recent `IOCTL_FIVE_BAUD_INIT`
/// call on `channel_id`, or `None` if no such call with a non-null input has
/// been made (ADR-076).
pub fn mock_get_five_baud_init_input(channel_id: u32) -> Option<u8> {
    let guard = state().lock().expect("mock state poisoned");
    guard.channels.get(&channel_id)?.five_baud_init_input
}

// ── Library-path helpers (mirrors iso22900-mock) ──────────────────────────────

pub fn mock_library_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "j2534_0404_mock.dll"
    } else if cfg!(target_os = "macos") {
        "libj2534_0404_mock.dylib"
    } else {
        "libj2534_0404_mock.so"
    }
}

fn is_mock_library_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case(mock_library_file_name()))
        .unwrap_or(false)
}

/// Recursively collects every mock library file under `dir` into `found`.
fn collect_mock_libraries(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && is_mock_library_file(&path) {
            found.push(path);
        } else if path.is_dir() {
            collect_mock_libraries(&path, found);
        }
    }
}

/// Returns the most recently modified mock library under `dir`. The target
/// directory can hold several copies (e.g. Cargo's uplifted
/// `target/debug/lib*.so` next to the freshly compiled artifact in
/// `target/debug/deps/`), and the uplifted copy is not refreshed by every
/// build — picking the first directory-walk hit could load a stale build
/// whose FFI constants disagree with the code under test.
fn search_directory(dir: &Path) -> Option<PathBuf> {
    let mut found = Vec::new();
    collect_mock_libraries(dir, &mut found);
    found
        .into_iter()
        .max_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
}

/// Locate the built mock shared library under the Cargo target directory.
pub fn mock_library_path() -> Result<PathBuf, std::io::Error> {
    if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        if let Some(found) = search_directory(&target_dir) {
            return Ok(found);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "could not find {} under {}",
                mock_library_file_name(),
                target_dir.display()
            ),
        ));
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // The workspace target directory is the nearest ancestor that has one.
    let target_dir = manifest_dir
        .ancestors()
        .map(|dir| dir.join("target"))
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| manifest_dir.join("target"));
    if let Some(found) = search_directory(&target_dir) {
        return Ok(found);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "could not find {} under {}",
            mock_library_file_name(),
            target_dir.display()
        ),
    ))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use j2534_0404_sys::bindings::SPARAM;
    use std::sync::Mutex;

    // All tests share the global mock state; serialize them to avoid races.
    static SERIAL: Mutex<()> = Mutex::new(());

    macro_rules! serial_test {
        (fn $name:ident() $body:block) => {
            #[test]
            fn $name() {
                let _lock = SERIAL.lock().unwrap();
                mock_reset();
                $body
            }
        };
    }

    /// `search_directory` must prefer the most recently modified copy: a
    /// stale uplifted `target/debug/lib*.so` must never shadow the freshly
    /// compiled artifact in `deps/` (or vice versa), or tests dynamically
    /// load a mock whose FFI constants disagree with the code under test.
    #[test]
    fn search_directory_prefers_the_newest_copy() {
        fn set_mtime(path: &Path, mtime: std::time::SystemTime) {
            let file = std::fs::File::options().append(true).open(path).unwrap();
            file.set_modified(mtime).unwrap();
        }

        let root = std::env::temp_dir().join(format!(
            "j2534-0404-mock-search-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let deps = root.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let uplifted = root.join(mock_library_file_name());
        let fresh = deps.join(mock_library_file_name());
        std::fs::write(&uplifted, b"stale").unwrap();
        std::fs::write(&fresh, b"fresh").unwrap();

        let base = std::time::SystemTime::now();
        set_mtime(&uplifted, base - std::time::Duration::from_secs(100));
        set_mtime(&fresh, base);
        assert_eq!(search_directory(&root), Some(fresh.clone()));

        // The preference is by mtime, not by location in the tree.
        set_mtime(&uplifted, base + std::time::Duration::from_secs(100));
        assert_eq!(search_directory(&root), Some(uplifted));

        let _ = std::fs::remove_dir_all(&root);
    }

    serial_test!(
        fn open_returns_mock_device_id() {
            let mut device_id: u32 = 0;
            let rc = unsafe { PassThruOpen(std::ptr::null_mut(), &mut device_id) };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_eq!(device_id, MOCK_DEVICE_ID);
            assert_eq!(mock_get_open_count(), 1);
            assert_eq!(
                mock_get_open_pname(),
                None,
                "a NULL pName should record as None"
            );
        }
    );

    serial_test!(
        fn open_records_a_non_null_pname() {
            let pname = std::ffi::CString::new("USB:1").unwrap();
            let mut device_id: u32 = 0;
            let rc = unsafe { PassThruOpen(pname.as_ptr() as *mut c_void, &mut device_id) };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_eq!(mock_get_open_count(), 1);
            assert_eq!(mock_get_open_pname(), Some(b"USB:1".to_vec()));

            // A second call with a NULL pName overwrites the recorded value.
            let rc2 = unsafe { PassThruOpen(std::ptr::null_mut(), &mut device_id) };
            assert_eq!(rc2, STATUS_NOERROR as c_long);
            assert_eq!(mock_get_open_count(), 2);
            assert_eq!(mock_get_open_pname(), None);
        }
    );

    serial_test!(
        fn connect_assigns_sequential_channel_ids() {
            let mut ch1: u32 = 0;
            let mut ch2: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut ch1);
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut ch2);
            }
            assert_ne!(ch1, 0);
            assert_ne!(ch2, 0);
            assert_ne!(ch1, ch2);
            assert_eq!(mock_get_connect_count(), 2);
        }
    );

    // ── ADR-156 Decision 3/4/Phase 2b: `_CHx` (Additional Channels)
    // connect-time simulation ──────────────────────────────────────────────

    serial_test!(
        fn chx_connect_within_default_capacity_succeeds() {
            let mut channel_id: u32 = 0;
            let rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN_CH1 + 3, // _CH4 -- within DEFAULT_CHX_CAPACITY (4)
                    0,
                    500_000,
                    &mut channel_id,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_ne!(channel_id, 0);
        }
    );

    serial_test!(
        fn chx_connect_beyond_default_capacity_is_not_supported() {
            let mut channel_id: u32 = 0;
            let rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN_CH1 + 4, // _CH5 -- beyond DEFAULT_CHX_CAPACITY (4)
                    0,
                    500_000,
                    &mut channel_id,
                )
            };
            assert_eq!(rc, ERR_NOT_SUPPORTED as c_long);
        }
    );

    serial_test!(
        fn chx_connect_beyond_overridden_capacity_is_not_supported() {
            unsafe {
                __mock_set_chx_capacity(2);
            }
            let mut channel_id: u32 = 0;
            let rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN_CH1 + 2, // _CH3 -- beyond the overridden capacity (2)
                    0,
                    500_000,
                    &mut channel_id,
                )
            };
            assert_eq!(rc, ERR_NOT_SUPPORTED as c_long);
        }
    );

    serial_test!(
        fn chx_connect_second_open_of_the_same_index_is_resource_in_use() {
            let mut channel_id_a: u32 = 0;
            let mut channel_id_b: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN_CH1 + 1, // _CH2
                    0,
                    500_000,
                    &mut channel_id_a,
                );
            }
            let rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN_CH1 + 1, // same _CH2 id again -- conflict
                    0,
                    500_000,
                    &mut channel_id_b,
                )
            };
            assert_eq!(rc, ERR_RESOURCE_IN_USE as c_long);
        }
    );

    serial_test!(
        fn chx_connect_a_different_index_does_not_conflict() {
            let mut channel_id_a: u32 = 0;
            let mut channel_id_b: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN_CH1,
                    0,
                    500_000,
                    &mut channel_id_a,
                );
            }
            let rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN_CH1 + 1,
                    0,
                    500_000,
                    &mut channel_id_b,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_ne!(channel_id_a, channel_id_b);
        }
    );

    serial_test!(
        // ADR-213/Round 3 closed the last `_CHx` scope gap: as of this
        // round, every id in the full clause-24/26 `_CHx` vocabulary region
        // (`is_chx_protocol_id`) resolves to an in-scope block
        // (`chx_base_protocol_id`), so there is no longer an example left of
        // an in-region-but-out-of-scope raw `_CHx` id for this test's
        // original premise -- this test used to name a CAN-FD `_CHx` id here
        // (itself a swap-in after ADR-206 moved this test's original J1939
        // example into scope), so it is repurposed to confirm the opposite:
        // a raw CAN-FD `_CHx` connect at this native-mock layer now
        // succeeds, mirroring `chx_connect_a_different_index_does_not_
        // conflict`'s own shape. `resources::is_chx_protocol_id`'s own
        // regression test (`is_chx_protocol_id_covers_the_full_region_not_
        // just_in_scope_families`, `j2534-0404-service`) is the service-side
        // analog confirming the region itself has no gap.
        fn chx_connect_can_fd_chx_id_now_succeeds() {
            let mut channel_id: u32 = 0;
            let rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    j2534_0404_sys::bindings::PROTOCOL_FD_CAN_CH1,
                    0,
                    500_000,
                    &mut channel_id,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
        }
    );

    serial_test!(
        fn chx_channel_is_never_pin_gated() {
            // Clause 7 channels are vendor-connector-based, never
            // J1962-pin-tied -- unlike a `_PS` channel, a `_CHx` channel's
            // pins are "assigned" (i.e. not gated) from the moment it
            // connects, with no IOCTL_SET_CONFIG(CONFIG_J1962_PINS) step.
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN_CH1,
                    0,
                    500_000,
                    &mut channel_id,
                );
            }
            let mut msg = PASSTHRU_MSG {
                ProtocolID: PROTOCOL_CAN_CH1,
                RxStatus: 0,
                TxFlags: 0,
                Timestamp: 0,
                ExtraDataIndex: 0,
                DataSize: 4,
                Data: [0u8; 4128],
            };
            msg.Data[..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
            let mut num_msgs: u32 = 1;
            let rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num_msgs, 0) };
            assert_eq!(
                rc, STATUS_NOERROR as c_long,
                "a _CHx channel must never be pin-gated like a _PS channel"
            );
        }
    );

    // Codex review, PR #25: `IOCTL_GET_PROTOCOL_INFO` advertises
    // `PROTOCOL_INFO_MAX_PASS_FILTER`/`MAX_BLOCK_FILTER` = 10, but
    // `PassThruStartMsgFilter` didn't actually enforce any limit --
    // Discovery's claim disagreed with the mock's real behavior. Pins
    // that the 11th same-type filter is rejected, that `PASS_FILTER` and
    // `BLOCK_FILTER` are counted independently (installing 10 of one type
    // doesn't block the other), and that `FLOW_CONTROL_FILTER` -- for
    // which the mock reports `NOT_SUPPORTED` on
    // `PROTOCOL_INFO_MAX_FLOW_CONTROL_FILTER` -- is left uncapped.
    serial_test!(
        fn start_msg_filter_enforces_the_limit_discovery_advertises() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut channel_id);
            }
            let mut mask: PASSTHRU_MSG = unsafe { std::mem::zeroed() };
            let mut pattern: PASSTHRU_MSG = unsafe { std::mem::zeroed() };
            let mut filter_id: u32 = 0;

            for i in 0..MAX_FILTERS_PER_TYPE {
                let rc = unsafe {
                    PassThruStartMsgFilter(
                        channel_id,
                        PASS_FILTER,
                        &mut mask,
                        &mut pattern,
                        std::ptr::null_mut(),
                        &mut filter_id,
                    )
                };
                assert_eq!(
                    rc, STATUS_NOERROR as c_long,
                    "PASS_FILTER #{i} should succeed"
                );
            }
            let eleventh = unsafe {
                PassThruStartMsgFilter(
                    channel_id,
                    PASS_FILTER,
                    &mut mask,
                    &mut pattern,
                    std::ptr::null_mut(),
                    &mut filter_id,
                )
            };
            assert_eq!(
                eleventh, ERR_EXCEEDED_LIMIT as c_long,
                "an 11th PASS_FILTER must be rejected, matching the advertised max of 10"
            );

            // BLOCK_FILTER has its own independent count.
            let block_rc = unsafe {
                PassThruStartMsgFilter(
                    channel_id,
                    BLOCK_FILTER,
                    &mut mask,
                    &mut pattern,
                    std::ptr::null_mut(),
                    &mut filter_id,
                )
            };
            assert_eq!(
                block_rc, STATUS_NOERROR as c_long,
                "BLOCK_FILTER must not be blocked by PASS_FILTER's count"
            );

            // FLOW_CONTROL_FILTER is uncapped -- the mock claims no specific
            // limit for it (PROTOCOL_INFO_MAX_FLOW_CONTROL_FILTER reports
            // NOT_SUPPORTED), so there is nothing for it to disagree with.
            for i in 0..MAX_FILTERS_PER_TYPE + 1 {
                let rc = unsafe {
                    PassThruStartMsgFilter(
                        channel_id,
                        j2534_0404_sys::bindings::FLOW_CONTROL_FILTER,
                        &mut mask,
                        &mut pattern,
                        std::ptr::null_mut(),
                        &mut filter_id,
                    )
                };
                assert_eq!(
                    rc, STATUS_NOERROR as c_long,
                    "FLOW_CONTROL_FILTER #{i} should not be capped"
                );
            }
        }
    );

    serial_test!(
        fn write_and_read_with_loopback() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut channel_id);
            }

            let mut cfg = SCONFIG {
                Parameter: j2534_0404_sys::bindings::CONFIG_LOOPBACK,
                Value: 1,
            };
            let mut cfg_list = SCONFIG_LIST {
                NumOfParams: 1,
                ConfigPtr: &mut cfg,
            };
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_SET_CONFIG,
                    &mut cfg_list as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = 5;
            msg.DataSize = 4;
            msg.Data[..4].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
            let mut num = 1u32;
            unsafe {
                PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0);
            }
            assert_eq!(mock_get_write_msgs_count(), 1);

            let mut rx = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut rx_count = 1u32;
            let rc = unsafe { PassThruReadMsgs(channel_id, &mut rx, &mut rx_count, 0) };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_eq!(rx_count, 1);
            assert_eq!(rx.DataSize, 4);
            assert_eq!(&rx.Data[..4], &[0xAA, 0xBB, 0xCC, 0xDD]);
        }
    );

    serial_test!(
        fn read_from_empty_queue_returns_buffer_empty() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut channel_id);
            }
            let mut rx = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut count = 1u32;
            let rc = unsafe { PassThruReadMsgs(channel_id, &mut rx, &mut count, 0) };
            assert_eq!(rc, ERR_BUFFER_EMPTY as c_long);
            assert_eq!(count, 0);
        }
    );

    serial_test!(
        fn inject_and_read_rx_message() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut channel_id);
            }
            assert!(mock_inject_rx_message(
                channel_id,
                vec![0x01, 0x02, 0x03],
                5,
                0
            ));
            let mut rx = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut count = 1u32;
            let rc = unsafe { PassThruReadMsgs(channel_id, &mut rx, &mut count, 0) };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_eq!(count, 1);
            assert_eq!(rx.DataSize, 3);
            assert_eq!(&rx.Data[..3], &[0x01, 0x02, 0x03]);
        }
    );

    serial_test!(
        fn set_and_get_config() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut channel_id);
            }

            let mut cfgs = [
                SCONFIG {
                    Parameter: j2534_0404_sys::bindings::CONFIG_DATA_RATE,
                    Value: 500_000,
                },
                SCONFIG {
                    Parameter: j2534_0404_sys::bindings::CONFIG_LOOPBACK,
                    Value: 1,
                },
            ];
            let mut set_list = SCONFIG_LIST {
                NumOfParams: 2,
                ConfigPtr: cfgs.as_mut_ptr(),
            };
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_SET_CONFIG,
                    &mut set_list as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }

            let mut get_cfgs = [
                SCONFIG {
                    Parameter: j2534_0404_sys::bindings::CONFIG_DATA_RATE,
                    Value: 0,
                },
                SCONFIG {
                    Parameter: j2534_0404_sys::bindings::CONFIG_LOOPBACK,
                    Value: 0,
                },
            ];
            let mut get_list = SCONFIG_LIST {
                NumOfParams: 2,
                ConfigPtr: get_cfgs.as_mut_ptr(),
            };
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_GET_CONFIG,
                    &mut get_list as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }
            assert_eq!(get_cfgs[0].Value, 500_000);
            assert_eq!(get_cfgs[1].Value, 1);
            assert_eq!(
                mock_get_config_value(channel_id, j2534_0404_sys::bindings::CONFIG_LOOPBACK),
                Some(1)
            );
        }
    );

    serial_test!(
        fn periodic_msg_lifecycle() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut channel_id);
            }

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = 5;
            msg.DataSize = 2;
            msg.Data[0] = 0x7E;
            msg.Data[1] = 0x00;
            let mut periodic_id: u32 = 0;
            unsafe {
                PassThruStartPeriodicMsg(channel_id, &mut msg, &mut periodic_id, 100);
            }
            assert_ne!(periodic_id, 0);
            assert_eq!(mock_get_start_periodic_count(), 1);
            assert_eq!(mock_get_periodic_msg_count(channel_id), 1);

            unsafe {
                PassThruStopPeriodicMsg(channel_id, periodic_id);
            }
            assert_eq!(mock_get_stop_periodic_count(), 1);
            assert_eq!(mock_get_periodic_msg_count(channel_id), 0);
        }
    );

    serial_test!(
        fn read_version_fills_strings() {
            let mut fw = [0i8; 80];
            let mut dll = [0i8; 80];
            let mut api = [0i8; 80];
            let rc = unsafe {
                PassThruReadVersion(
                    MOCK_DEVICE_ID,
                    fw.as_mut_ptr(),
                    dll.as_mut_ptr(),
                    api.as_mut_ptr(),
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            let api_bytes: Vec<u8> = api.iter().take(5).map(|&b| b as u8).collect();
            assert_eq!(&api_bytes, b"04.04");
        }
    );

    serial_test!(
        fn null_pointer_returns_error() {
            let rc = unsafe { PassThruOpen(std::ptr::null_mut(), std::ptr::null_mut()) };
            assert_eq!(rc, ERR_NULL_PARAMETER as c_long);
        }
    );

    serial_test!(
        fn disconnect_removes_channel() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut channel_id);
            }
            unsafe {
                PassThruDisconnect(channel_id);
            }
            assert_eq!(mock_get_disconnect_count(), 1);
            let written = mock_get_written_messages(channel_id);
            assert!(written.is_empty());
        }
    );

    serial_test!(
        fn five_baud_and_fast_init_reject_non_k_line_protocol() {
            // Protocol 5 == ISO15765 (CAN family) -- not a K-line protocol.
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, 5, 0, 500_000, &mut channel_id);
            }

            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_FIVE_BAUD_INIT,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, ERR_NOT_SUPPORTED as c_long);
            assert_eq!(mock_get_five_baud_init_count(), 0);

            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_FAST_INIT,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, ERR_NOT_SUPPORTED as c_long);
            assert_eq!(mock_get_fast_init_count(), 0);
        }
    );

    serial_test!(
        fn five_baud_init_succeeds_for_k_line_protocols() {
            for protocol_id in [PROTOCOL_ISO9141, PROTOCOL_ISO14230] {
                mock_reset();
                let mut channel_id: u32 = 0;
                unsafe {
                    PassThruConnect(MOCK_DEVICE_ID, protocol_id, 0, 10_400, &mut channel_id);
                }

                let rc = unsafe {
                    PassThruIoctl(
                        channel_id,
                        IOCTL_FIVE_BAUD_INIT,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                assert_eq!(rc, STATUS_NOERROR as c_long);
                assert_eq!(mock_get_five_baud_init_count(), 1);
            }
        }
    );

    serial_test!(
        fn fast_init_succeeds_for_k_line_protocols() {
            // Per J2534-1 v04.04, FAST_INIT is valid on both K-line
            // protocols (ISO9141 and ISO14230), not ISO14230 alone.
            for protocol_id in [PROTOCOL_ISO9141, PROTOCOL_ISO14230] {
                mock_reset();
                let mut channel_id: u32 = 0;
                unsafe {
                    PassThruConnect(MOCK_DEVICE_ID, protocol_id, 0, 10_400, &mut channel_id);
                }

                let rc = unsafe {
                    PassThruIoctl(
                        channel_id,
                        IOCTL_FAST_INIT,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                assert_eq!(rc, STATUS_NOERROR as c_long);
                assert_eq!(mock_get_fast_init_count(), 1);
            }
        }
    );

    // ADR-157: `IOCTL_FIVE_BAUD_INIT`/`IOCTL_FAST_INIT`'s K-line-protocol
    // checks must also accept `ISO9141_PS`/`ISO14230_PS` -- a caller with a
    // `_PS` K-line link can legitimately run either init sequence on it.
    // Before this fix, `base_protocol_id` was not applied and a `_PS` id
    // never matched `ISO9141 | ISO14230`, so both IOCTLs rejected it with
    // `ERR_NOT_SUPPORTED`. Pins are assigned before either IOCTL runs here
    // (`set_j1962_pins`, defined below) -- see
    // `five_baud_init_rejected_until_pins_assigned_for_ps_k_line_protocols`/
    // `fast_init_rejected_until_pins_assigned_for_ps_k_line_protocols` for the
    // unassigned-pins case these tests deliberately don't cover.
    serial_test!(
        fn five_baud_init_succeeds_for_ps_k_line_protocols() {
            for protocol_id in [PROTOCOL_ISO9141_PS, PROTOCOL_ISO14230_PS] {
                mock_reset();
                let mut channel_id: u32 = 0;
                unsafe {
                    PassThruConnect(MOCK_DEVICE_ID, protocol_id, 0, 10_400, &mut channel_id);
                }
                assert_eq!(
                    set_j1962_pins(channel_id, 0x0000_0F07),
                    STATUS_NOERROR as c_long
                );

                let rc = unsafe {
                    PassThruIoctl(
                        channel_id,
                        IOCTL_FIVE_BAUD_INIT,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                assert_eq!(rc, STATUS_NOERROR as c_long, "protocol id {protocol_id:#x}");
                assert_eq!(mock_get_five_baud_init_count(), 1);
            }
        }
    );

    // Regression test (Codex review, PR #28): before this fix, neither
    // `IOCTL_FIVE_BAUD_INIT` nor `IOCTL_FAST_INIT` checked
    // `channel.pins_assigned` at all -- a `_PS` K-line channel could run
    // either init sequence over unassigned (and therefore physically
    // meaningless) pins, bypassing the same `ERR_PIN_INVALID` gate every
    // other I/O IOCTL enforces.
    serial_test!(
        fn five_baud_init_rejected_until_pins_assigned_for_ps_k_line_protocols() {
            for protocol_id in [PROTOCOL_ISO9141_PS, PROTOCOL_ISO14230_PS] {
                mock_reset();
                let mut channel_id: u32 = 0;
                unsafe {
                    PassThruConnect(MOCK_DEVICE_ID, protocol_id, 0, 10_400, &mut channel_id);
                }

                let rc = unsafe {
                    PassThruIoctl(
                        channel_id,
                        IOCTL_FIVE_BAUD_INIT,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                assert_eq!(
                    rc, ERR_PIN_INVALID as c_long,
                    "protocol id {protocol_id:#x}: 5-baud init on unassigned pins must be rejected"
                );
                assert_eq!(mock_get_five_baud_init_count(), 0);
            }
        }
    );

    serial_test!(
        fn fast_init_succeeds_for_ps_k_line_protocols() {
            for protocol_id in [PROTOCOL_ISO9141_PS, PROTOCOL_ISO14230_PS] {
                mock_reset();
                let mut channel_id: u32 = 0;
                unsafe {
                    PassThruConnect(MOCK_DEVICE_ID, protocol_id, 0, 10_400, &mut channel_id);
                }
                assert_eq!(
                    set_j1962_pins(channel_id, 0x0000_0F07),
                    STATUS_NOERROR as c_long
                );

                let rc = unsafe {
                    PassThruIoctl(
                        channel_id,
                        IOCTL_FAST_INIT,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                assert_eq!(rc, STATUS_NOERROR as c_long, "protocol id {protocol_id:#x}");
                assert_eq!(mock_get_fast_init_count(), 1);
            }
        }
    );

    // Regression test (Codex review, PR #28): the `IOCTL_FAST_INIT` sibling
    // of `five_baud_init_rejected_until_pins_assigned_for_ps_k_line_protocols`
    // above.
    serial_test!(
        fn fast_init_rejected_until_pins_assigned_for_ps_k_line_protocols() {
            for protocol_id in [PROTOCOL_ISO9141_PS, PROTOCOL_ISO14230_PS] {
                mock_reset();
                let mut channel_id: u32 = 0;
                unsafe {
                    PassThruConnect(MOCK_DEVICE_ID, protocol_id, 0, 10_400, &mut channel_id);
                }

                let rc = unsafe {
                    PassThruIoctl(
                        channel_id,
                        IOCTL_FAST_INIT,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                assert_eq!(
                    rc, ERR_PIN_INVALID as c_long,
                    "protocol id {protocol_id:#x}: fast init on unassigned pins must be rejected"
                );
                assert_eq!(mock_get_fast_init_count(), 0);
            }
        }
    );

    // ADR-157: `IOCTL_GET_PROTOCOL_INFO`'s protocol-id validation must
    // accept every `_PS` id this mock implements, answering based on the
    // base protocol's capability data -- before this fix, any `_PS` id was
    // rejected outright with `ERR_INVALID_PROTOCOL_ID`.
    serial_test!(
        fn get_protocol_info_accepts_ps_protocol_ids() {
            for &protocol_id in PS_PROTOCOL_IDS.iter() {
                let mut input = protocol_id;
                let mut params = [SPARAM {
                    Parameter: PROTOCOL_INFO_MAX_RX_BUFFER_SIZE,
                    Value: 0,
                    Supported: 0,
                }];
                let mut list = SPARAM_LIST {
                    NumOfParams: 1,
                    ParamPtr: params.as_mut_ptr(),
                };
                let rc = unsafe {
                    PassThruIoctl(
                        0,
                        IOCTL_GET_PROTOCOL_INFO,
                        &mut input as *mut _ as *mut c_void,
                        &mut list as *mut _ as *mut c_void,
                    )
                };
                assert_eq!(rc, STATUS_NOERROR as c_long, "protocol id {protocol_id:#x}");
                assert_eq!(params[0].Value, 4128, "protocol id {protocol_id:#x}");
                assert_eq!(params[0].Supported, 1, "protocol id {protocol_id:#x}");
            }
        }
    );

    // Codex review, PR #64: `IOCTL_GET_PROTOCOL_INFO`'s allow-list must
    // individually name each standalone `_PS`-only protocol id, since
    // `base_protocol_id` self-identifies for all three and none is a
    // Table-1 `_PS` variant `PS_PROTOCOL_IDS` covers. `HONDA_DIAGH_PS`/
    // `J1708_PS` genuinely support Repeat Messaging (clause 13/17 have no
    // exclusion equivalent to clause 12.3.3.1's), so both are now accepted;
    // `UART_ECHO_BYTE_PS` deliberately stays rejected, consistent with
    // clause 12.3.3.1's own explicit Repeat Messaging exclusion for it.
    serial_test!(
        fn get_protocol_info_accepts_honda_diagh_and_j1708_but_not_uart_echo_byte() {
            for (protocol_id, should_succeed) in [
                (PROTOCOL_HONDA_DIAGH_PS, true),
                (PROTOCOL_J1708_PS, true),
                (PROTOCOL_UART_ECHO_BYTE_PS, false),
            ] {
                let mut input = protocol_id;
                let mut params = [SPARAM {
                    Parameter: PROTOCOL_INFO_MAX_RX_BUFFER_SIZE,
                    Value: 0,
                    Supported: 0,
                }];
                let mut list = SPARAM_LIST {
                    NumOfParams: 1,
                    ParamPtr: params.as_mut_ptr(),
                };
                let rc = unsafe {
                    PassThruIoctl(
                        0,
                        IOCTL_GET_PROTOCOL_INFO,
                        &mut input as *mut _ as *mut c_void,
                        &mut list as *mut _ as *mut c_void,
                    )
                };
                if should_succeed {
                    assert_eq!(rc, STATUS_NOERROR as c_long, "protocol id {protocol_id:#x}");
                    assert_eq!(params[0].Value, 4128, "protocol id {protocol_id:#x}");
                    assert_eq!(params[0].Supported, 1, "protocol id {protocol_id:#x}");
                } else {
                    assert_eq!(
                        rc, ERR_INVALID_PROTOCOL_ID as c_long,
                        "protocol id {protocol_id:#x}"
                    );
                }
            }
        }
    );

    // Codex review finding, PR #64 (values updated for ADR-186; J1939
    // reasoning corrected in the third design-advisor round):
    // `PROTOCOL_INFO_MAX_REPEAT_MESSAGING_LENGTH` must report each
    // protocol's real periodic-message TX cap (enforced by
    // `ioctl_start_repeat_message`), not the flat 4128-byte structural
    // buffer max, nor the older (pre-ADR-186) per-protocol ordinary TX size
    // ranges -- ISO15765 gets its own 11-byte cap and every other protocol
    // reachable through this handler's own `base_id` allow-list above (raw
    // CAN, Honda DIAG-H, J1708, SAE J1939, etc.) gets the generic 12-byte
    // cap. SAE J1939 itself is deliberately NOT exercised here:
    // `PROTOCOL_J1939_PS` is not itself in this handler's `base_id`
    // allow-list above, so `IOCTL_GET_PROTOCOL_INFO` already rejects it
    // with `ERR_INVALID_PROTOCOL_ID` before reaching this arm -- a
    // pre-existing gap independent of this fix.
    serial_test!(
        fn get_protocol_info_reports_the_real_repeat_messaging_length_per_protocol() {
            for (protocol_id, expected_max_length) in [
                (PROTOCOL_HONDA_DIAGH_PS, 12),
                (PROTOCOL_J1708_PS, 12),
                (PROTOCOL_CAN, 12),
                (PROTOCOL_ISO15765, 11),
                // ADR-186 follow-up (edge-case-hunter review, PR #95):
                // J1850PWM's own ordinary TX size range (`protocol.rs`'s
                // `3..=10`) is narrower than the flat 12-byte §7.2.7
                // periodic cap every other protocol above falls back to,
                // so its advertised/enforced max is 10, not 12.
                (PROTOCOL_J1850PWM, 10),
            ] {
                let mut input = protocol_id;
                let mut params = [SPARAM {
                    Parameter: PROTOCOL_INFO_MAX_REPEAT_MESSAGING_LENGTH,
                    Value: 0,
                    Supported: 0,
                }];
                let mut list = SPARAM_LIST {
                    NumOfParams: 1,
                    ParamPtr: params.as_mut_ptr(),
                };
                let rc = unsafe {
                    PassThruIoctl(
                        0,
                        IOCTL_GET_PROTOCOL_INFO,
                        &mut input as *mut _ as *mut c_void,
                        &mut list as *mut _ as *mut c_void,
                    )
                };
                assert_eq!(rc, STATUS_NOERROR as c_long, "protocol id {protocol_id:#x}");
                assert_eq!(
                    params[0].Value, expected_max_length,
                    "protocol id {protocol_id:#x}"
                );
                assert_eq!(params[0].Supported, 1, "protocol id {protocol_id:#x}");
            }
        }
    );

    /// Builds a one-entry `SCONFIG_LIST` for `IOCTL_SET_CONFIG`/`IOCTL_GET_CONFIG`
    /// carrying `CONFIG_J1962_PINS = pin_select`, mirroring the layout
    /// `rpc_link.rs`'s pin-`SET_CONFIG` call constructs (ADR-156 Decision 2).
    fn set_j1962_pins(channel_id: u32, pin_select: u32) -> c_long {
        set_single_config(channel_id, CONFIG_J1962_PINS, pin_select)
    }

    /// Builds a one-entry `SCONFIG_LIST` for `IOCTL_SET_CONFIG` carrying an
    /// arbitrary `parameter = value` pair, mirroring the single-param
    /// `SET_CONFIG` shape `apply_j2534_params`/`connect_new_physical_channel`
    /// (`rpc_link.rs`) issue (ADR-158).
    fn set_single_config(channel_id: u32, parameter: u32, value: u32) -> c_long {
        let mut cfg = SCONFIG {
            Parameter: parameter,
            Value: value,
        };
        let mut list = SCONFIG_LIST {
            NumOfParams: 1,
            ConfigPtr: &mut cfg,
        };
        unsafe {
            PassThruIoctl(
                channel_id,
                IOCTL_SET_CONFIG,
                &mut list as *mut _ as *mut c_void,
                std::ptr::null_mut(),
            )
        }
    }

    // ADR-156 Decision 2 / SAE J2534-2 clause 6.3.3.2: a `_PS` channel's
    // pins start unassigned, gating every I/O operation that needs wired
    // pins with `ERR_PIN_INVALID` until `IOCTL_SET_CONFIG(CONFIG_J1962_PINS)`
    // binds them.
    serial_test!(
        fn ps_channel_rejects_io_until_pins_assigned() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN_PS, 0, 500_000, &mut channel_id);
            }

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_CAN_PS;
            msg.DataSize = 4;
            msg.Data[..4].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc, ERR_PIN_INVALID as c_long,
                "write on an unassigned-pins _PS channel must be rejected"
            );

            let mut rx = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut rx_count = 1u32;
            let read_rc = unsafe { PassThruReadMsgs(channel_id, &mut rx, &mut rx_count, 0) };
            assert_eq!(
                read_rc, ERR_PIN_INVALID as c_long,
                "read on an unassigned-pins _PS channel must be rejected, not ERR_BUFFER_EMPTY"
            );

            let mut mask = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut pattern = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut filter_id: u32 = 0;
            let filter_rc = unsafe {
                PassThruStartMsgFilter(
                    channel_id,
                    PASS_FILTER,
                    &mut mask,
                    &mut pattern,
                    std::ptr::null_mut(),
                    &mut filter_id,
                )
            };
            assert_eq!(
                filter_rc, ERR_PIN_INVALID as c_long,
                "filter install on an unassigned-pins _PS channel must be rejected"
            );

            let mut periodic_msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            periodic_msg.ProtocolID = PROTOCOL_CAN_PS;
            periodic_msg.DataSize = 2;
            let mut periodic_id: u32 = 0;
            let periodic_rc = unsafe {
                PassThruStartPeriodicMsg(channel_id, &mut periodic_msg, &mut periodic_id, 100)
            };
            assert_eq!(
                periodic_rc, ERR_PIN_INVALID as c_long,
                "periodic-message start on an unassigned-pins _PS channel must be rejected"
            );
        }
    );

    serial_test!(
        fn ps_channel_io_succeeds_after_pins_assigned() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN_PS, 0, 500_000, &mut channel_id);
            }

            let set_rc = set_j1962_pins(channel_id, 0x0000_030B);
            assert_eq!(set_rc, STATUS_NOERROR as c_long);

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_CAN_PS;
            msg.DataSize = 4;
            msg.Data[..4].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc, STATUS_NOERROR as c_long,
                "write must succeed once pins are assigned"
            );

            // The RX queue is empty, so a lifted gate reports ERR_BUFFER_EMPTY
            // (not ERR_PIN_INVALID) -- proof the pin gate itself lifted, not
            // just that some other check masks it.
            let mut rx = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut rx_count = 1u32;
            let read_rc = unsafe { PassThruReadMsgs(channel_id, &mut rx, &mut rx_count, 0) };
            assert_eq!(read_rc, ERR_BUFFER_EMPTY as c_long);

            let mut mask = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut pattern = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut filter_id: u32 = 0;
            let filter_rc = unsafe {
                PassThruStartMsgFilter(
                    channel_id,
                    PASS_FILTER,
                    &mut mask,
                    &mut pattern,
                    std::ptr::null_mut(),
                    &mut filter_id,
                )
            };
            assert_eq!(filter_rc, STATUS_NOERROR as c_long);

            let mut periodic_msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            periodic_msg.ProtocolID = PROTOCOL_CAN_PS;
            periodic_msg.DataSize = 2;
            let mut periodic_id: u32 = 0;
            let periodic_rc = unsafe {
                PassThruStartPeriodicMsg(channel_id, &mut periodic_msg, &mut periodic_id, 100)
            };
            assert_eq!(periodic_rc, STATUS_NOERROR as c_long);
        }
    );

    // ── SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5) ────────

    /// Connects a `PROTOCOL_J1939_PS` channel and assigns its pins (the
    /// same pin-gated shape `PROTOCOL_CAN_PS`'s own connect helper uses --
    /// see `ps_channel_io_succeeds_after_pins_assigned` above), returning
    /// the channel id.
    fn connect_j1939_channel_with_pins() -> u32 {
        let mut channel_id: u32 = 0;
        unsafe {
            PassThruConnect(
                MOCK_DEVICE_ID,
                PROTOCOL_J1939_PS,
                0,
                250_000,
                &mut channel_id,
            );
        }
        let set_rc = set_j1962_pins(channel_id, 0x0000_0604);
        assert_eq!(set_rc, STATUS_NOERROR as c_long);
        channel_id
    }

    /// Issues `IOCTL_PROTECT_J1939_ADDR` with a 9-byte `[address, name...]`
    /// payload, mirroring the exact `SBYTE_ARRAY` shape
    /// `j2534-0404`'s `protect_j1939_addr`/`cancel_j1939_addr_protect`
    /// build.
    fn protect_j1939_addr(channel_id: u32, address: u8, name: [u8; 8]) -> c_long {
        let mut payload = [0u8; 9];
        payload[0] = address;
        payload[1..9].copy_from_slice(&name);
        let mut input = SBYTE_ARRAY {
            NumOfBytes: 9,
            BytePtr: payload.as_mut_ptr(),
        };
        unsafe {
            PassThruIoctl(
                channel_id,
                IOCTL_PROTECT_J1939_ADDR,
                &mut input as *mut _ as *mut c_void,
                std::ptr::null_mut(),
            )
        }
    }

    fn read_one_msg(channel_id: u32) -> Option<(u32, Vec<u8>)> {
        let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
        let mut count = 1u32;
        let rc = unsafe { PassThruReadMsgs(channel_id, &mut msg, &mut count, 0) };
        if rc != STATUS_NOERROR as c_long || count == 0 {
            return None;
        }
        let len = (msg.DataSize as usize).min(msg.Data.len());
        Some((msg.RxStatus, msg.Data[..len].to_vec()))
    }

    serial_test!(
        fn protect_j1939_addr_rejects_254_and_255() {
            let channel_id = connect_j1939_channel_with_pins();
            assert_eq!(
                protect_j1939_addr(channel_id, 254, [1; 8]),
                ERR_INVALID_IOCTL_VALUE as c_long
            );
            assert_eq!(
                protect_j1939_addr(channel_id, 255, [1; 8]),
                ERR_INVALID_IOCTL_VALUE as c_long
            );
            // Neither rejected attempt should have queued an indication.
            assert!(read_one_msg(channel_id).is_none());
        }
    );

    serial_test!(
        fn protect_j1939_addr_default_success_pushes_claimed_indication() {
            let channel_id = connect_j1939_channel_with_pins();
            let rc = protect_j1939_addr(channel_id, 0x80, [1, 2, 3, 4, 5, 6, 7, 8]);
            assert_eq!(rc, STATUS_NOERROR as c_long);

            let (rx_status, data) =
                read_one_msg(channel_id).expect("a CLAIMED indication should be queued");
            assert_eq!(
                rx_status & RX_FLAG_J1939_ADDRESS_CLAIMED,
                RX_FLAG_J1939_ADDRESS_CLAIMED
            );
            assert_eq!(rx_status & RX_FLAG_J1939_ADDRESS_LOST, 0);
            assert_eq!(data, vec![0x80]);
        }
    );

    serial_test!(
        fn protect_j1939_addr_forced_lost_via_backdoor() {
            let channel_id = connect_j1939_channel_with_pins();
            let backdoor_rc = unsafe { __mock_set_j1939_claim_lost(1) };
            assert_eq!(backdoor_rc, STATUS_NOERROR as c_long);

            let rc = protect_j1939_addr(channel_id, 0x81, [1; 8]);
            assert_eq!(rc, STATUS_NOERROR as c_long);

            let (rx_status, data) =
                read_one_msg(channel_id).expect("a LOST indication should be queued");
            assert_eq!(
                rx_status & RX_FLAG_J1939_ADDRESS_LOST,
                RX_FLAG_J1939_ADDRESS_LOST
            );
            assert_eq!(rx_status & RX_FLAG_J1939_ADDRESS_CLAIMED, 0);
            assert_eq!(data, vec![0x81]);

            // A subsequent write with this address as source must still be
            // rejected -- the forced-LOST attempt must not have recorded it
            // as claimed.
            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_J1939_PS;
            msg.DataSize = 14;
            msg.Data[3] = 0x81;
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(write_rc, ERR_ADDRESS_NOT_CLAIMED as c_long);
        }
    );

    serial_test!(
        fn protect_j1939_addr_cancel_form_removes_claim_with_no_indication() {
            let channel_id = connect_j1939_channel_with_pins();
            assert_eq!(
                protect_j1939_addr(channel_id, 0x82, [1; 8]),
                STATUS_NOERROR as c_long
            );
            // Drain the CLAIMED indication the successful claim above queued.
            assert!(read_one_msg(channel_id).is_some());

            let cancel_rc = protect_j1939_addr(channel_id, 0x82, [0; 8]);
            assert_eq!(cancel_rc, STATUS_NOERROR as c_long);
            // A cancel is not itself an indication -- nothing new queued.
            assert!(read_one_msg(channel_id).is_none());

            // The address is no longer claimed: an oversized write from it
            // is now rejected.
            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_J1939_PS;
            msg.DataSize = 14;
            msg.Data[3] = 0x82;
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(write_rc, ERR_ADDRESS_NOT_CLAIMED as c_long);
        }
    );

    serial_test!(
        fn write_msgs_j1939_allows_claimed_source_address_over_8_bytes() {
            let channel_id = connect_j1939_channel_with_pins();
            assert_eq!(
                protect_j1939_addr(channel_id, 0x83, [1; 8]),
                STATUS_NOERROR as c_long
            );
            assert!(read_one_msg(channel_id).is_some());

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_J1939_PS;
            msg.DataSize = 14; // 5-byte prefix + 9 payload bytes (> 8)
            msg.Data[3] = 0x83; // claimed source address
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(write_rc, STATUS_NOERROR as c_long);
        }
    );

    serial_test!(
        fn write_msgs_j1939_allows_short_payload_regardless_of_claim() {
            // Clause 16.5's ERR_ADDRESS_NOT_CLAIMED only applies to a data
            // payload larger than 8 bytes -- a short (<= 8-byte payload)
            // message must succeed even with no claimed address at all.
            let channel_id = connect_j1939_channel_with_pins();
            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_J1939_PS;
            msg.DataSize = 13; // 5-byte prefix + 8 payload bytes (not > 8)
            msg.Data[3] = 0x99; // never claimed
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(write_rc, STATUS_NOERROR as c_long);
        }
    );

    serial_test!(
        // Codex review finding, PR #117 (ADR-206): before this fix,
        // `PassThruWriteMsgs`'s ERR_ADDRESS_NOT_CLAIMED check used
        // `is_j1939_protocol` (exact `_PS`-only match), so a J1939 `_CHx`
        // channel silently skipped this enforcement entirely -- an
        // unclaimed-source oversized write that clause 16.5 requires to be
        // rejected on the `_PS` channel (see
        // `write_msgs_j1939_allows_claimed_source_address_over_8_bytes`
        // above's negative counterpart) would have wrongly succeeded on a
        // `_CHx` channel instead. No `dlc_pin_data`/`set_j1962_pins` call is
        // needed here -- clause 7's vendor-connector model gives `_CHx`
        // channels no J1962 pin concept at all, mirroring
        // `chx_connect_out_of_scope_family_is_not_supported`'s own
        // pins-free direct `PassThruConnect` shape.
        fn write_msgs_j1939_chx_rejects_unclaimed_source_address_over_8_bytes() {
            let mut channel_id: u32 = 0;
            let connect_rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_J1939_CH1,
                    0,
                    250_000,
                    &mut channel_id,
                )
            };
            assert_eq!(connect_rc, STATUS_NOERROR as c_long);

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_J1939_CH1;
            msg.DataSize = 14; // 5-byte prefix + 9 payload bytes (> 8)
            msg.Data[3] = 0x84; // never claimed on this channel
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(write_rc, ERR_ADDRESS_NOT_CLAIMED as c_long);
        }
    );

    // ── SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a) ────

    /// Connects a `PROTOCOL_TP2_0_PS` channel and assigns pins 6/14 (clause
    /// 19.2.2's own one documented pair), returning the channel id.
    fn connect_tp2_0_channel_with_pins() -> u32 {
        let mut channel_id: u32 = 0;
        unsafe {
            PassThruConnect(
                MOCK_DEVICE_ID,
                PROTOCOL_TP2_0_PS,
                0,
                500_000,
                &mut channel_id,
            );
        }
        let set_rc = set_j1962_pins(channel_id, 0x0000_060E);
        assert_eq!(set_rc, STATUS_NOERROR as c_long);
        channel_id
    }

    /// Issues `IOCTL_REQUEST_CONNECTION` with the clause 19 Table 78
    /// 11-byte `SBYTE_ARRAY` payload, mirroring the exact shape
    /// `j2534-0404`'s `tp20_request_connection` builds.
    fn tp20_request_connection(
        channel_id: u32,
        setup_can_id: u32,
        destination_address: u8,
        tx_id_proposal: u16,
        rx_id_proposal: u16,
        application_type: u8,
    ) -> c_long {
        let mut payload = [0u8; 11];
        payload[0..4].copy_from_slice(&setup_can_id.to_be_bytes());
        payload[4] = destination_address;
        payload[5] = 0xC0;
        payload[6..8].copy_from_slice(&tx_id_proposal.to_be_bytes());
        payload[8..10].copy_from_slice(&rx_id_proposal.to_be_bytes());
        payload[10] = application_type;
        let mut input = SBYTE_ARRAY {
            NumOfBytes: 11,
            BytePtr: payload.as_mut_ptr(),
        };
        unsafe {
            PassThruIoctl(
                channel_id,
                IOCTL_REQUEST_CONNECTION,
                &mut input as *mut _ as *mut c_void,
                std::ptr::null_mut(),
            )
        }
    }

    /// Issues `IOCTL_TEARDOWN_CONNECTION` with the clause 19 Table 79 4-byte
    /// `SBYTE_ARRAY` payload, mirroring `j2534-0404`'s
    /// `tp20_teardown_connection`.
    fn tp20_teardown_connection(channel_id: u32, rx_id: u32) -> c_long {
        let mut payload = rx_id.to_be_bytes();
        let mut input = SBYTE_ARRAY {
            NumOfBytes: 4,
            BytePtr: payload.as_mut_ptr(),
        };
        unsafe {
            PassThruIoctl(
                channel_id,
                IOCTL_TEARDOWN_CONNECTION,
                &mut input as *mut _ as *mut c_void,
                std::ptr::null_mut(),
            )
        }
    }

    serial_test!(
        fn tp20_request_connection_success_pushes_established_indication() {
            let channel_id = connect_tp2_0_channel_with_pins();
            let rc = tp20_request_connection(channel_id, 0x0000_0700, 0x10, 0x300, 0x321, 1);
            assert_eq!(rc, STATUS_NOERROR as c_long);

            let (rx_status, data) = read_one_msg(channel_id)
                .expect("a CONNECTION_ESTABLISHED indication should be queued");
            assert_eq!(
                rx_status & RX_FLAG_CONNECTION_ESTABLISHED,
                RX_FLAG_CONNECTION_ESTABLISHED
            );
            assert_eq!(rx_status & RX_FLAG_CONNECTION_LOST, 0);
            assert_eq!(data.len(), 8);
            assert_eq!(
                u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
                0x321
            );
        }
    );

    serial_test!(
        fn tp20_request_connection_rejected_when_all_four_slots_full() {
            let channel_id = connect_tp2_0_channel_with_pins();
            for rx_id in [0x100u16, 0x101, 0x102, 0x103] {
                let rc = tp20_request_connection(channel_id, 0x700, 0x10, 0x300, rx_id, 1);
                assert_eq!(rc, STATUS_NOERROR as c_long);
                assert!(read_one_msg(channel_id).is_some());
            }

            let rc = tp20_request_connection(channel_id, 0x700, 0x10, 0x300, 0x104, 1);
            assert_eq!(rc, STATUS_NOERROR as c_long);
            let (rx_status, data) =
                read_one_msg(channel_id).expect("a CONNECTION_LOST indication should be queued");
            assert_eq!(rx_status & RX_FLAG_CONNECTION_LOST, RX_FLAG_CONNECTION_LOST);
            assert_eq!(rx_status & RX_FLAG_CONNECTION_ESTABLISHED, 0);
            assert_eq!(data.len(), 5);
            assert_eq!(data[4], 0xD8);
        }
    );

    serial_test!(
        fn tp20_teardown_connection_removes_slot_and_pushes_lost_indication() {
            let channel_id = connect_tp2_0_channel_with_pins();
            assert_eq!(
                tp20_request_connection(channel_id, 0x700, 0x10, 0x300, 0x200, 1),
                STATUS_NOERROR as c_long
            );
            assert!(read_one_msg(channel_id).is_some());

            let rc = tp20_teardown_connection(channel_id, 0x200);
            assert_eq!(rc, STATUS_NOERROR as c_long);
            let (rx_status, data) =
                read_one_msg(channel_id).expect("a CONNECTION_LOST indication should be queued");
            assert_eq!(rx_status & RX_FLAG_CONNECTION_LOST, RX_FLAG_CONNECTION_LOST);
            assert_eq!(data, vec![0, 0, 2, 0, 0]);

            // A second teardown for the same RX-ID no longer matches a slot.
            assert_eq!(
                tp20_teardown_connection(channel_id, 0x200),
                ERR_INVALID_IOCTL_VALUE as c_long
            );
        }
    );

    // Codex review finding (P2, PR #97, 7th round -- ADR-188's own
    // abandoned-RX-ID quarantine mechanism): `tp20_no_indication`'s own
    // field doc extension. With the hook armed, `IOCTL_TEARDOWN_CONNECTION`
    // still genuinely frees the native slot (a following `IOCTL_REQUEST_
    // CONNECTION` for the same `rx_id` succeeds), but does NOT queue its
    // own `CONNECTION_LOST` reason-`0` indication -- unlike `tp20_teardown_
    // connection_removes_slot_and_pushes_lost_indication` above, which pins
    // the un-armed (default) behavior this extends rather than replaces.
    serial_test!(
        fn tp20_teardown_connection_with_no_indication_armed_frees_the_slot_without_queuing() {
            let channel_id = connect_tp2_0_channel_with_pins();
            assert_eq!(
                tp20_request_connection(channel_id, 0x700, 0x10, 0x300, 0x200, 1),
                STATUS_NOERROR as c_long
            );
            assert!(read_one_msg(channel_id).is_some());

            state()
                .lock()
                .expect("mock state poisoned")
                .tp20_no_indication = true;

            let rc = tp20_teardown_connection(channel_id, 0x200);
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert!(
                read_one_msg(channel_id).is_none(),
                "no CONNECTION_LOST indication should be queued while tp20_no_indication is armed"
            );

            // The slot was still genuinely freed: a fresh request for the
            // same rx_id succeeds (armed, so still no indication for it
            // either -- only the slot-freeing half is being pinned here).
            assert_eq!(
                tp20_request_connection(channel_id, 0x700, 0x10, 0x300, 0x200, 1),
                STATUS_NOERROR as c_long
            );
            assert!(read_one_msg(channel_id).is_none());

            state()
                .lock()
                .expect("mock state poisoned")
                .tp20_no_indication = false;
        }
    );

    serial_test!(
        fn tp20_teardown_connection_rejects_wrong_num_of_bytes() {
            let channel_id = connect_tp2_0_channel_with_pins();
            let mut payload = [0u8; 2];
            let mut input = SBYTE_ARRAY {
                NumOfBytes: 2,
                BytePtr: payload.as_mut_ptr(),
            };
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_TEARDOWN_CONNECTION,
                    &mut input as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, ERR_INVALID_IOCTL_VALUE as c_long);
        }
    );

    serial_test!(
        fn write_msgs_tp2_0_rejects_oversized_non_connection_write() {
            let channel_id = connect_tp2_0_channel_with_pins();
            assert_eq!(
                tp20_request_connection(channel_id, 0x700, 0x10, 0x300, 0x321, 1),
                STATUS_NOERROR as c_long
            );
            assert!(read_one_msg(channel_id).is_some());

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_TP2_0_PS;
            msg.DataSize = 13; // 4-byte prefix + 9 payload bytes, > single-CAN-frame size
            // Data[0..4] deliberately does NOT match the established TX-ID
            // (0x1000_0321) above.
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(write_rc, ERR_NO_CONNECTION_ESTABLISHED as c_long);
        }
    );

    serial_test!(
        fn write_msgs_tp2_0_allows_a_write_matching_the_established_tx_id() {
            let channel_id = connect_tp2_0_channel_with_pins();
            assert_eq!(
                tp20_request_connection(channel_id, 0x700, 0x10, 0x300, 0x321, 1),
                STATUS_NOERROR as c_long
            );
            assert!(read_one_msg(channel_id).is_some());

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_TP2_0_PS;
            msg.DataSize = 13;
            msg.Data[0..4].copy_from_slice(&0x1000_0321u32.to_be_bytes());
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(write_rc, STATUS_NOERROR as c_long);
        }
    );

    serial_test!(
        fn write_msgs_tp2_0_broadcast_rejects_data0_outside_broadcast_range() {
            let channel_id = connect_tp2_0_channel_with_pins();
            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_TP2_0_PS;
            msg.TxFlags = j2534_0404_sys::bindings::TX_FLAG_TP2_0_BROADCAST_MSG;
            msg.DataSize = 3;
            msg.Data[0..3].copy_from_slice(&[0xE5, 0x01, 0x02]); // 0xE5 not in 0xF0-0xFF
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc, ERR_INVALID_MSG as c_long,
                "Data[0] outside 0xF0-0xFF with the broadcast flag set must be rejected"
            );
        }
    );

    serial_test!(
        fn write_msgs_tp2_0_broadcast_sends_five_frames_with_alternating_last_two_bytes() {
            let channel_id = connect_tp2_0_channel_with_pins();
            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_TP2_0_PS;
            msg.TxFlags = j2534_0404_sys::bindings::TX_FLAG_TP2_0_BROADCAST_MSG;
            msg.DataSize = 4;
            msg.Data[0..4].copy_from_slice(&[0xF3, 0x01, 0x02, 0x03]);
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(write_rc, STATUS_NOERROR as c_long);

            let written = state()
                .lock()
                .expect("mock state poisoned")
                .channels
                .get(&channel_id)
                .map(|c| c.written_msgs.clone())
                .unwrap_or_default();
            assert_eq!(
                written.len(),
                5,
                "a single PassThruWriteMsgs broadcast call must simulate a 5-frame burst"
            );
            for (i, frame) in written.iter().enumerate() {
                assert_eq!(
                    &frame.data[0..2],
                    &[0xF3, 0x01],
                    "address/first payload byte unchanged"
                );
                let expected = if i % 2 == 0 { 0xAA } else { 0x55 };
                assert_eq!(
                    &frame.data[2..4],
                    &[expected, expected],
                    "last two bytes alternate 0xAA/0x55 each send"
                );
            }
        }
    );

    serial_test!(
        fn start_periodic_msg_tp2_0_broadcast_rejects_data0_outside_broadcast_range() {
            let channel_id = connect_tp2_0_channel_with_pins();
            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_TP2_0_PS;
            msg.TxFlags = j2534_0404_sys::bindings::TX_FLAG_TP2_0_BROADCAST_MSG;
            msg.DataSize = 3;
            msg.Data[0..3].copy_from_slice(&[0x05, 0x01, 0x02]);
            let mut periodic_id = 0u32;
            let rc =
                unsafe { PassThruStartPeriodicMsg(channel_id, &mut msg, &mut periodic_id, 100) };
            assert_eq!(rc, ERR_INVALID_MSG as c_long);
        }
    );

    serial_test!(
        fn start_periodic_msg_tp2_0_broadcast_sends_immediate_burst_then_tracks_periodic_state() {
            let channel_id = connect_tp2_0_channel_with_pins();
            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_TP2_0_PS;
            msg.TxFlags = j2534_0404_sys::bindings::TX_FLAG_TP2_0_BROADCAST_MSG;
            msg.DataSize = 4;
            msg.Data[0..4].copy_from_slice(&[0xFF, 0x10, 0x20, 0x30]);
            let mut periodic_id = 0u32;
            let rc =
                unsafe { PassThruStartPeriodicMsg(channel_id, &mut msg, &mut periodic_id, 100) };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_ne!(periodic_id, 0);

            let guard = state().lock().expect("mock state poisoned");
            let channel = guard.channels.get(&channel_id).expect("channel exists");
            assert_eq!(
                channel.written_msgs.len(),
                5,
                "PassThruStartPeriodicMsg's own immediate burst sends 5 frames, same as \
                 PassThruWriteMsgs's broadcast case"
            );
            assert!(
                channel.periodic_msgs.contains_key(&periodic_id),
                "the periodic message is still tracked for start/stop lifecycle, unchanged"
            );
            assert_eq!(channel.last_start_periodic_time_interval, Some(100));
        }
    );

    serial_test!(
        fn second_j1962_pins_set_config_is_rejected() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN_PS, 0, 500_000, &mut channel_id);
            }

            let first_rc = set_j1962_pins(channel_id, 0x0000_030B);
            assert_eq!(first_rc, STATUS_NOERROR as c_long);

            let second_rc = set_j1962_pins(channel_id, 0x0000_0402);
            assert_eq!(
                second_rc, ERR_CHANNEL_IN_USE as c_long,
                "pins are bound exactly once per channel until teardown (clause 6.3.3.2)"
            );
            // The rejected second call must not have overwritten the
            // already-bound pin value.
            assert_eq!(
                mock_get_config_value(channel_id, CONFIG_J1962_PINS),
                Some(0x0000_030B)
            );
        }
    );

    // Regression test (Codex review, PR #28): `CONFIG_J1962_PINS = 0` is
    // SAE J2534-2 clause 6.3.3.2's own "no selection performed" sentinel,
    // not a real pin assignment -- `compute_pin_select`, the service's only
    // producer of a real value, never packs it. A direct mock client
    // sending the sentinel must not silently mark the channel's pins
    // assigned, defeating the `ERR_PIN_INVALID` I/O gate.
    serial_test!(
        fn zero_j1962_pins_set_config_does_not_assign_pins() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN_PS, 0, 500_000, &mut channel_id);
            }

            let set_rc = set_j1962_pins(channel_id, 0);
            assert_eq!(
                set_rc, STATUS_NOERROR as c_long,
                "SET_CONFIG itself succeeds -- clause 6.3.3.2's sentinel is a valid value to \
                 write, it just doesn't count as a real pin assignment"
            );

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_CAN_PS;
            msg.DataSize = 4;
            msg.Data[..4].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc, ERR_PIN_INVALID as c_long,
                "the zero sentinel must not have marked pins assigned -- I/O still rejected"
            );

            // A follow-up call with a real value is still legitimate, since
            // the sentinel call bound nothing.
            let real_rc = set_j1962_pins(channel_id, 0x0000_030B);
            assert_eq!(
                real_rc, STATUS_NOERROR as c_long,
                "a real pin assignment after the sentinel call must still succeed, not be \
                 rejected as ERR_CHANNEL_IN_USE"
            );
            let write_rc2 = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(write_rc2, STATUS_NOERROR as c_long);
        }
    );

    // A non-`_PS` channel is never gated: pins are meaningless for it, so
    // I/O works immediately (today's pre-Phase-2a behavior, unchanged), and
    // a CONFIG_J1962_PINS SET_CONFIG on it is treated the same as an
    // already-assigned channel (ERR_CHANNEL_IN_USE), never silently
    // accepted.
    serial_test!(
        fn non_ps_channel_is_never_pin_gated() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_CAN;
            msg.DataSize = 4;
            msg.Data[..4].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc, STATUS_NOERROR as c_long,
                "a non-_PS channel must never be pin-gated"
            );

            let mut rx = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut rx_count = 1u32;
            let read_rc = unsafe { PassThruReadMsgs(channel_id, &mut rx, &mut rx_count, 0) };
            assert_eq!(
                read_rc, ERR_BUFFER_EMPTY as c_long,
                "a non-_PS channel must never be pin-gated"
            );

            let set_rc = set_j1962_pins(channel_id, 0x0000_030B);
            assert_eq!(
                set_rc, ERR_CHANNEL_IN_USE as c_long,
                "CONFIG_J1962_PINS on a non-_PS channel is treated as already-assigned"
            );
        }
    );

    // ── ADR-158/Phase 3a: SAE J2534-2 clause 21 CAN FD ──────────────────────

    // Clause 21.3.2.5.1: CONFIG_J1962_PINS on an FD_CAN_PS channel fails
    // ERR_FAILED until CONFIG_FD_CAN_DATA_PHASE_RATE has been SET_CONFIG'd
    // on that channel first -- the sequencing this mock exists to make
    // testable end-to-end (a unit-level check of the ordering, not just the
    // service's own internal call order).
    serial_test!(
        fn fd_can_pins_rejected_before_data_phase_rate_is_set() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_FD_CAN_PS,
                    0,
                    500_000,
                    &mut channel_id,
                );
            }

            let pins_rc = set_j1962_pins(channel_id, 0x0000_060E);
            assert_eq!(
                pins_rc, ERR_FAILED as c_long,
                "CONFIG_J1962_PINS before CONFIG_FD_CAN_DATA_PHASE_RATE must fail ERR_FAILED"
            );

            let rate_rc = set_single_config(channel_id, CONFIG_FD_CAN_DATA_PHASE_RATE, 2_000_000);
            assert_eq!(rate_rc, STATUS_NOERROR as c_long);

            let pins_rc2 = set_j1962_pins(channel_id, 0x0000_060E);
            assert_eq!(
                pins_rc2, STATUS_NOERROR as c_long,
                "CONFIG_J1962_PINS must succeed once CONFIG_FD_CAN_DATA_PHASE_RATE is set"
            );
        }
    );

    // Clause 21.3.2.5.1: BIT_SAMPLE_POINT/SYNC_JUMP_WIDTH are read-only on an
    // FD_CAN_PS channel (ERR_NOT_SUPPORTED); the same two params still
    // succeed, unchanged, on a plain CAN channel.
    serial_test!(
        fn fd_can_bit_timing_params_are_read_only() {
            let mut fd_channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_FD_CAN_PS,
                    0,
                    500_000,
                    &mut fd_channel_id,
                );
            }
            let bit_sample_rc = set_single_config(fd_channel_id, CONFIG_BIT_SAMPLE_POINT, 80);
            assert_eq!(bit_sample_rc, ERR_NOT_SUPPORTED as c_long);
            let sync_jump_rc = set_single_config(fd_channel_id, CONFIG_SYNC_JUMP_WIDTH, 15);
            assert_eq!(sync_jump_rc, ERR_NOT_SUPPORTED as c_long);

            let mut can_channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN,
                    0,
                    500_000,
                    &mut can_channel_id,
                );
            }
            let can_bit_sample_rc = set_single_config(can_channel_id, CONFIG_BIT_SAMPLE_POINT, 80);
            assert_eq!(can_bit_sample_rc, STATUS_NOERROR as c_long);
            let can_sync_jump_rc = set_single_config(can_channel_id, CONFIG_SYNC_JUMP_WIDTH, 15);
            assert_eq!(can_sync_jump_rc, STATUS_NOERROR as c_long);
        }
    );

    // An FD_CAN_PS channel is pin-gated exactly like a `_PS` channel
    // (ADR-158 extends `pins_assigned`'s initial-`false` gate to it), once
    // both the data-phase-rate and pins SET_CONFIG steps have run.
    serial_test!(
        fn fd_can_channel_is_pin_gated_like_a_ps_channel() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_FD_CAN_PS,
                    0,
                    500_000,
                    &mut channel_id,
                );
            }

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_FD_CAN_PS;
            msg.DataSize = 4;
            msg.Data[..4].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc, ERR_PIN_INVALID as c_long,
                "write on an unassigned-pins FD_CAN_PS channel must be rejected"
            );

            let rate_rc = set_single_config(channel_id, CONFIG_FD_CAN_DATA_PHASE_RATE, 2_000_000);
            assert_eq!(rate_rc, STATUS_NOERROR as c_long);
            let pins_rc = set_j1962_pins(channel_id, 0x0000_060E);
            assert_eq!(pins_rc, STATUS_NOERROR as c_long);

            let write_rc2 = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc2, STATUS_NOERROR as c_long,
                "write must succeed once pins are assigned"
            );
        }
    );

    // ── ADR-213/Round 3: SAE J2534-2 clause 7 Additional Channels (`_CHx`)
    // support, extended to CAN FD / ISO15765-on-CAN-FD ──────────────────────

    // Clause 21.3.2.5.1/22.3.2.6.1's own `_CHx`-specific rule (distinct from
    // the `_PS` rate-before-pins ordering check just above, which a `_CHx`
    // channel has no pin-selection step to interact with at all --
    // `is_fd_chx_protocol`'s own doc comment): a `_CHx` FD channel stays
    // electrically inert -- every I/O entry point rejected `ERR_PIN_INVALID`
    // -- until CONFIG_FD_CAN_DATA_PHASE_RATE has been SET_CONFIG'd on it,
    // at which point it attaches and every entry point succeeds normally.
    // Mirrors `ps_channel_rejects_io_until_pins_assigned`'s/
    // `ps_channel_io_succeeds_after_pins_assigned`'s own all-four-entry-point
    // shape, but gated on the data-phase rate instead of pins (a `_CHx` FD
    // channel's `pins_assigned` is already `true` from connect --
    // `chx_channel_is_never_pin_gated` -- so it is genuinely the rate gate,
    // not the pin gate, doing the rejecting here).
    serial_test!(
        fn fd_can_chx_io_rejected_until_data_phase_rate_is_set_then_succeeds() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_FD_CAN_CH1,
                    0,
                    500_000,
                    &mut channel_id,
                );
            }

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_FD_CAN_CH1;
            msg.DataSize = 4;
            msg.Data[..4].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc, ERR_PIN_INVALID as c_long,
                "write on a _CHx FD channel with no data-phase rate set must be rejected"
            );

            let mut rx = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut rx_count = 1u32;
            let read_rc = unsafe { PassThruReadMsgs(channel_id, &mut rx, &mut rx_count, 0) };
            assert_eq!(
                read_rc, ERR_PIN_INVALID as c_long,
                "read on a _CHx FD channel with no data-phase rate set must be rejected, not \
                 ERR_BUFFER_EMPTY"
            );

            let mut mask = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut pattern = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            let mut filter_id: u32 = 0;
            let filter_rc = unsafe {
                PassThruStartMsgFilter(
                    channel_id,
                    PASS_FILTER,
                    &mut mask,
                    &mut pattern,
                    std::ptr::null_mut(),
                    &mut filter_id,
                )
            };
            assert_eq!(
                filter_rc, ERR_PIN_INVALID as c_long,
                "filter install on a _CHx FD channel with no data-phase rate set must be rejected"
            );

            let mut periodic_msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            periodic_msg.ProtocolID = PROTOCOL_FD_CAN_CH1;
            periodic_msg.DataSize = 2;
            let mut periodic_id: u32 = 0;
            let periodic_rc = unsafe {
                PassThruStartPeriodicMsg(channel_id, &mut periodic_msg, &mut periodic_id, 100)
            };
            assert_eq!(
                periodic_rc, ERR_PIN_INVALID as c_long,
                "periodic-message start on a _CHx FD channel with no data-phase rate set must be \
                 rejected"
            );

            let rate_rc = set_single_config(channel_id, CONFIG_FD_CAN_DATA_PHASE_RATE, 2_000_000);
            assert_eq!(rate_rc, STATUS_NOERROR as c_long);

            let write_rc2 = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc2, STATUS_NOERROR as c_long,
                "write must succeed once the data-phase rate is set"
            );
            let read_rc2 = unsafe { PassThruReadMsgs(channel_id, &mut rx, &mut rx_count, 0) };
            assert_eq!(
                read_rc2, ERR_BUFFER_EMPTY as c_long,
                "read must report ERR_BUFFER_EMPTY (not ERR_PIN_INVALID) once the data-phase \
                 rate is set -- proof the rate gate itself lifted, not just that some other check \
                 masks it"
            );
            let filter_rc2 = unsafe {
                PassThruStartMsgFilter(
                    channel_id,
                    PASS_FILTER,
                    &mut mask,
                    &mut pattern,
                    std::ptr::null_mut(),
                    &mut filter_id,
                )
            };
            assert_eq!(filter_rc2, STATUS_NOERROR as c_long);
            let periodic_rc2 = unsafe {
                PassThruStartPeriodicMsg(channel_id, &mut periodic_msg, &mut periodic_id, 100)
            };
            assert_eq!(periodic_rc2, STATUS_NOERROR as c_long);
        }
    );

    // ── ADR-164/Phase 4: SAE J2534-2 clause 9 Single Wire CAN (SWCAN/GMLAN) ──

    // Clause 9.2.1: an SW_CAN_PS/SW_ISO15765_PS channel is pin-gated exactly
    // like a `_PS` channel (`is_sw_protocol` extends `pins_assigned`'s
    // initial-`false` gate to it), unlike `FD_CAN_PS` there is no
    // data-phase-rate-before-pins sequencing rule to satisfy first -- a
    // single `CONFIG_J1962_PINS` call is enough.
    serial_test!(
        fn sw_can_channel_is_pin_gated_like_a_ps_channel() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_SW_CAN_PS,
                    0,
                    500_000,
                    &mut channel_id,
                );
            }

            let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
            msg.ProtocolID = PROTOCOL_SW_CAN_PS;
            msg.DataSize = 4;
            msg.Data[..4].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc, ERR_PIN_INVALID as c_long,
                "write on an unassigned-pins SW_CAN_PS channel must be rejected"
            );

            // Pin 1, PIN_HI -- clause 9.2.1's single default pin, packed
            // 0x0000PPSS with SS = 0 (no secondary pin), matching
            // `names::J2534Service::resolve_pin_selection`'s own SW arm.
            let pins_rc = set_j1962_pins(channel_id, 0x0000_0100);
            assert_eq!(pins_rc, STATUS_NOERROR as c_long);

            let write_rc2 = unsafe { PassThruWriteMsgs(channel_id, &mut msg, &mut num, 0) };
            assert_eq!(
                write_rc2, STATUS_NOERROR as c_long,
                "write must succeed once pins are assigned"
            );
        }
    );

    // Regression test (edge-case-hunter re-verification round 2, follow-up
    // to the `is_ps_protocol`/pin-gate fix): unlike
    // `native_mixed_mode_qualified_sw_link_skips_mixed_format_set_config`
    // (`j2534-0404-service/tests/grpc_mock/mixed_format_can.rs` -- renamed
    // from `..._applies_pins_before_mixed_format` by the ADR-160 Correction,
    // 2026-08-17, part 2, which removed the service's SET_CONFIG for a
    // qualified link outright), which drives the *service* and so no longer
    // observes any CONFIG_CAN_MIXED_FORMAT issuance order at all for a
    // qualified link, this test drives
    // the mock's exported `PassThru*` FFI directly -- it is the one that
    // actually isolates SAE J2534-2 clause 6.3.2.7's "no exception for
    // CONFIG_CAN_MIXED_FORMAT" rule (ADR-160 Correction) on an
    // `SW_ISO15765_PS` channel: a buggy `is_ps_protocol(channel.protocol_id)
    // && !channel.pins_assigned` gate (which `is_ps_protocol` does not cover
    // SW_ISO15765_PS with) would let the pre-pins CONFIG_CAN_MIXED_FORMAT
    // call below wrongly succeed.
    serial_test!(
        fn sw_can_mixed_format_rejected_until_pins_assigned_then_succeeds() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_SW_ISO15765_PS,
                    0,
                    33_300,
                    &mut channel_id,
                );
            }

            // CAN_MIXED_FORMAT_ON (`rpc_link.rs`'s own private constant,
            // clause 8) -- attempted before any CONFIG_J1962_PINS call.
            let mixed_format_rc = set_single_config(channel_id, CONFIG_CAN_MIXED_FORMAT, 1);
            assert_eq!(
                mixed_format_rc, ERR_PIN_INVALID as c_long,
                "clause 6.3.2.7 grants CONFIG_CAN_MIXED_FORMAT no exception -- an SW_ISO15765_PS \
                 channel's pins must be assigned first"
            );

            // Pin 1, PIN_HI -- clause 9.2.1's single default pin, matching
            // `sw_can_channel_is_pin_gated_like_a_ps_channel` above.
            let pins_rc = set_j1962_pins(channel_id, 0x0000_0100);
            assert_eq!(pins_rc, STATUS_NOERROR as c_long);

            let mixed_format_rc2 = set_single_config(channel_id, CONFIG_CAN_MIXED_FORMAT, 1);
            assert_eq!(
                mixed_format_rc2, STATUS_NOERROR as c_long,
                "the gate is ordering-specific, not a permanent rejection -- CONFIG_CAN_MIXED_\
                 FORMAT must succeed once pins are assigned"
            );
        }
    );

    // FT-CAN analog of `sw_can_mixed_format_rejected_until_pins_assigned_
    // then_succeeds` above (ADR-168/Phase 6): `is_ps_protocol` does not
    // cover `FT_ISO15765_PS` either, so the same buggy gate would wrongly
    // let CONFIG_CAN_MIXED_FORMAT succeed before pins are assigned here too.
    serial_test!(
        fn ft_can_mixed_format_rejected_until_pins_assigned_then_succeeds() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_FT_ISO15765_PS,
                    0,
                    125_000,
                    &mut channel_id,
                );
            }

            let mixed_format_rc = set_single_config(channel_id, CONFIG_CAN_MIXED_FORMAT, 1);
            assert_eq!(
                mixed_format_rc, ERR_PIN_INVALID as c_long,
                "clause 6.3.2.7 grants CONFIG_CAN_MIXED_FORMAT no exception -- an FT_ISO15765_PS \
                 channel's pins must be assigned first"
            );

            // Pin 1/HI + pin 9/LOW -- clause 20.2.1's first-listed pin-pair
            // default (`names::resolve_pin_selection`'s FT arm).
            let pins_rc = set_j1962_pins(channel_id, 0x0000_0109);
            assert_eq!(pins_rc, STATUS_NOERROR as c_long);

            let mixed_format_rc2 = set_single_config(channel_id, CONFIG_CAN_MIXED_FORMAT, 1);
            assert_eq!(
                mixed_format_rc2, STATUS_NOERROR as c_long,
                "the gate is ordering-specific, not a permanent rejection -- CONFIG_CAN_MIXED_\
                 FORMAT must succeed once pins are assigned"
            );
        }
    );

    // SW_CAN_HS/SW_CAN_NS (ADR-164 Decision 3) succeed on a channel
    // connected with an SW protocol id, and are rejected `ERR_NOT_SUPPORTED`
    // on a plain CAN channel -- the mock's own "wrong protocol for this
    // IOCTL" answer, mirroring `IOCTL_FIVE_BAUD_INIT`/`IOCTL_FAST_INIT`'s
    // existing non-K-line rejection.
    serial_test!(
        fn sw_can_hs_ns_ioctls_require_an_sw_connected_channel() {
            let mut sw_channel_id: u32 = 0;
            let mut can_channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_SW_ISO15765_PS,
                    0,
                    33_300,
                    &mut sw_channel_id,
                );
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN,
                    0,
                    500_000,
                    &mut can_channel_id,
                );
            }

            let hs_rc = unsafe {
                PassThruIoctl(
                    sw_channel_id,
                    IOCTL_SW_CAN_HS,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(
                hs_rc, STATUS_NOERROR as c_long,
                "SW_CAN_HS should succeed on an SW-connected channel"
            );
            let ns_rc = unsafe {
                PassThruIoctl(
                    sw_channel_id,
                    IOCTL_SW_CAN_NS,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(
                ns_rc, STATUS_NOERROR as c_long,
                "SW_CAN_NS should succeed on an SW-connected channel"
            );

            let hs_wrong_protocol_rc = unsafe {
                PassThruIoctl(
                    can_channel_id,
                    IOCTL_SW_CAN_HS,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(
                hs_wrong_protocol_rc, ERR_NOT_SUPPORTED as c_long,
                "SW_CAN_HS on a plain CAN channel should be rejected ERR_NOT_SUPPORTED"
            );
            let ns_wrong_protocol_rc = unsafe {
                PassThruIoctl(
                    can_channel_id,
                    IOCTL_SW_CAN_NS,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(
                ns_wrong_protocol_rc, ERR_NOT_SUPPORTED as c_long,
                "SW_CAN_NS on a plain CAN channel should be rejected ERR_NOT_SUPPORTED"
            );
        }
    );

    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): connect
    // succeeds with no pin-assignment gating (clause 24 has no `_PS`/`_CHx`
    // pin-selection mechanics), and every message-API call is rejected
    // `ERR_NOT_SUPPORTED` per clause 24.2.5.2-5.5.
    serial_test!(
        fn ethernet_ndis_channel_rejects_every_message_api_call() {
            let mut channel_id: u32 = 0;
            let connect_rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_ETHERNET_NDIS,
                    j2534_0404_sys::bindings::CONNECT_FLAG_NDIS_PINS_OPTION1,
                    0,
                    &mut channel_id,
                )
            };
            assert_eq!(connect_rc, STATUS_NOERROR as c_long);

            let mut rx = PASSTHRU_MSG {
                ProtocolID: PROTOCOL_ETHERNET_NDIS,
                RxStatus: 0,
                TxFlags: 0,
                Timestamp: 0,
                ExtraDataIndex: 0,
                DataSize: 0,
                Data: [0u8; 4128],
            };
            let mut rx_count = 1u32;
            let read_rc = unsafe { PassThruReadMsgs(channel_id, &mut rx, &mut rx_count, 0) };
            assert_eq!(
                read_rc, ERR_NOT_SUPPORTED as c_long,
                "PassThruReadMsgs must always be ERR_NOT_SUPPORTED on Ethernet_NDIS"
            );

            let mut tx = PASSTHRU_MSG {
                ProtocolID: PROTOCOL_ETHERNET_NDIS,
                RxStatus: 0,
                TxFlags: 0,
                Timestamp: 0,
                ExtraDataIndex: 0,
                DataSize: 4,
                Data: [0u8; 4128],
            };
            let mut num = 1u32;
            let write_rc = unsafe { PassThruWriteMsgs(channel_id, &mut tx, &mut num, 0) };
            assert_eq!(
                write_rc, ERR_NOT_SUPPORTED as c_long,
                "PassThruWriteMsgs must always be ERR_NOT_SUPPORTED on Ethernet_NDIS"
            );

            let mut periodic_msg = tx;
            let mut periodic_id = 0u32;
            let periodic_rc = unsafe {
                PassThruStartPeriodicMsg(channel_id, &mut periodic_msg, &mut periodic_id, 100)
            };
            assert_eq!(
                periodic_rc, ERR_NOT_SUPPORTED as c_long,
                "PassThruStartPeriodicMsg must always be ERR_NOT_SUPPORTED on Ethernet_NDIS"
            );

            let mut mask = PASSTHRU_MSG {
                ProtocolID: PROTOCOL_ETHERNET_NDIS,
                RxStatus: 0,
                TxFlags: 0,
                Timestamp: 0,
                ExtraDataIndex: 0,
                DataSize: 0,
                Data: [0u8; 4128],
            };
            let mut pattern = mask;
            let mut filter_id = 0u32;
            let filter_rc = unsafe {
                PassThruStartMsgFilter(
                    channel_id,
                    PASS_FILTER,
                    &mut mask,
                    &mut pattern,
                    std::ptr::null_mut(),
                    &mut filter_id,
                )
            };
            assert_eq!(
                filter_rc, ERR_NOT_SUPPORTED as c_long,
                "PassThruStartMsgFilter must always be ERR_NOT_SUPPORTED on Ethernet_NDIS"
            );
        }
    );

    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
    // `__mock_set_ndis_connect_error` forces the activation-failure connect
    // path, and only for `PROTOCOL_ETHERNET_NDIS` -- an ordinary CAN connect
    // is unaffected.
    serial_test!(
        fn ndis_connect_error_injection_is_scoped_to_ethernet_ndis() {
            let rc =
                unsafe { __mock_set_ndis_connect_error(ERR_NO_CONNECTION_ESTABLISHED as c_long) };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            let mut ndis_channel_id: u32 = 0;
            let ndis_rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_ETHERNET_NDIS,
                    0,
                    0,
                    &mut ndis_channel_id,
                )
            };
            assert_eq!(ndis_rc, ERR_NO_CONNECTION_ESTABLISHED as c_long);

            let mut can_channel_id: u32 = 0;
            let can_rc = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN,
                    0,
                    500_000,
                    &mut can_channel_id,
                )
            };
            assert_eq!(
                can_rc, STATUS_NOERROR as c_long,
                "the injected error must not affect a non-Ethernet_NDIS connect"
            );

            // Clearing the override (STATUS_NOERROR) restores normal success.
            let clear_rc = unsafe { __mock_set_ndis_connect_error(STATUS_NOERROR as c_long) };
            assert_eq!(clear_rc, STATUS_NOERROR as c_long);
            let mut ndis_channel_id2: u32 = 0;
            let ndis_rc2 = unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_ETHERNET_NDIS,
                    0,
                    0,
                    &mut ndis_channel_id2,
                )
            };
            assert_eq!(ndis_rc2, STATUS_NOERROR as c_long);
        }
    );

    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
    // `IOCTL_GET_NDIS_ADAPTER_INFO` returns the canned fixture and is
    // rejected on a non-Ethernet_NDIS channel.
    serial_test!(
        fn get_ndis_adapter_info_returns_canned_struct_and_requires_ethernet_ndis() {
            let mut ndis_channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_ETHERNET_NDIS,
                    0,
                    0,
                    &mut ndis_channel_id,
                );
            }
            let mut info = NDIS_ADAPTER_INFORMATION {
                AdapterUniqueID: [0; 128],
                AdapterName: [0; 64],
                Status: 0,
                MAC_Address: [0; 6],
                IPV6_Address: [0; 16],
                IPV4_Address: [0; 4],
                EthernetPinConfig: 0,
            };
            let rc = unsafe {
                PassThruIoctl(
                    ndis_channel_id,
                    IOCTL_GET_NDIS_ADAPTER_INFO,
                    std::ptr::null_mut(),
                    &mut info as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_eq!(info.Status, MOCK_NDIS_STATUS);
            assert_eq!(info.MAC_Address, MOCK_NDIS_MAC_ADDRESS);
            assert_eq!(info.IPV6_Address, MOCK_NDIS_IPV6_ADDRESS);
            assert_eq!(info.IPV4_Address, MOCK_NDIS_IPV4_ADDRESS);
            assert_eq!(info.EthernetPinConfig, MOCK_NDIS_ETHERNET_PIN_CONFIG);
            assert_eq!(
                &info.AdapterName[..MOCK_NDIS_ADAPTER_NAME.len()]
                    .iter()
                    .map(|&b| b as u8)
                    .collect::<Vec<u8>>(),
                MOCK_NDIS_ADAPTER_NAME
            );

            let mut can_channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN,
                    0,
                    500_000,
                    &mut can_channel_id,
                );
            }
            let wrong_protocol_rc = unsafe {
                PassThruIoctl(
                    can_channel_id,
                    IOCTL_GET_NDIS_ADAPTER_INFO,
                    std::ptr::null_mut(),
                    &mut info as *mut _ as *mut c_void,
                )
            };
            assert_eq!(
                wrong_protocol_rc, ERR_NOT_SUPPORTED as c_long,
                "IOCTL_GET_NDIS_ADAPTER_INFO on a plain CAN channel should be rejected \
                 ERR_NOT_SUPPORTED"
            );
        }
    );

    // Codex review, PR #102 round 9: `EthernetPinConfig` must reflect the
    // channel's own connect-time `CONNECT_FLAG_NDIS_PINS_OPTION2` bit, not
    // always echo the canned fixture's Option 1 value.
    serial_test!(
        fn get_ndis_adapter_info_reports_option_2_when_connected_with_option_2_flag() {
            let mut ndis_channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_ETHERNET_NDIS,
                    j2534_0404_sys::bindings::CONNECT_FLAG_NDIS_PINS_OPTION2,
                    0,
                    &mut ndis_channel_id,
                );
            }
            let mut info = NDIS_ADAPTER_INFORMATION {
                AdapterUniqueID: [0; 128],
                AdapterName: [0; 64],
                Status: 0,
                MAC_Address: [0; 6],
                IPV6_Address: [0; 16],
                IPV4_Address: [0; 4],
                EthernetPinConfig: 0,
            };
            let rc = unsafe {
                PassThruIoctl(
                    ndis_channel_id,
                    IOCTL_GET_NDIS_ADAPTER_INFO,
                    std::ptr::null_mut(),
                    &mut info as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_eq!(
                info.EthernetPinConfig, 2,
                "a channel connected with CONNECT_FLAG_NDIS_PINS_OPTION2 must report \
                 EthernetPinConfig = 2, not the canned Option 1 fixture value"
            );
        }
    );

    // edge-case-hunter finding, PR #102 close-out: a direct FFI caller of
    // this mock (not the real gRPC service, whose `ndis_pin_option_connect_
    // flags` always emits at most one bit) can set both
    // `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2` at once. `rpc_link.rs`'s own
    // comment and ADR-194 document that the native table treats both bits
    // set as equivalent to neither, so this must report `1`, not `2`.
    serial_test!(
        fn get_ndis_adapter_info_reports_option_1_when_both_option_flags_are_set() {
            let mut ndis_channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_ETHERNET_NDIS,
                    j2534_0404_sys::bindings::CONNECT_FLAG_NDIS_PINS_OPTION1
                        | j2534_0404_sys::bindings::CONNECT_FLAG_NDIS_PINS_OPTION2,
                    0,
                    &mut ndis_channel_id,
                );
            }
            let mut info = NDIS_ADAPTER_INFORMATION {
                AdapterUniqueID: [0; 128],
                AdapterName: [0; 64],
                Status: 0,
                MAC_Address: [0; 6],
                IPV6_Address: [0; 16],
                IPV4_Address: [0; 4],
                EthernetPinConfig: 0,
            };
            let rc = unsafe {
                PassThruIoctl(
                    ndis_channel_id,
                    IOCTL_GET_NDIS_ADAPTER_INFO,
                    std::ptr::null_mut(),
                    &mut info as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_eq!(
                info.EthernetPinConfig, 1,
                "both NDIS pin-option flags set together must report EthernetPinConfig = 1 \
                 (equivalent to neither/auto), matching the native table's own convention"
            );
        }
    );

    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
    // `__mock_set_ndis_supported` forces `DEVICE_INFO_ETHERNET_NDIS_SUPPORTED`
    // to report unsupported, the same `DEVICE_INFO_GM_UART_SUPPORTED`
    // toggle shape.
    serial_test!(
        fn ndis_supported_device_info_toggle() {
            let rc = unsafe { __mock_set_ndis_supported(0) };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            let mut param = SPARAM {
                Parameter: DEVICE_INFO_ETHERNET_NDIS_SUPPORTED,
                Value: 0,
                Supported: 0,
            };
            let mut list = SPARAM_LIST {
                NumOfParams: 1,
                ParamPtr: &mut param,
            };
            let rc = unsafe {
                PassThruIoctl(
                    0,
                    IOCTL_GET_DEVICE_INFO,
                    std::ptr::null_mut(),
                    &mut list as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_eq!(param.Supported, 0);

            let restore_rc = unsafe { __mock_set_ndis_supported(1) };
            assert_eq!(restore_rc, STATUS_NOERROR as c_long);
            param.Supported = 0;
            let rc2 = unsafe {
                PassThruIoctl(
                    0,
                    IOCTL_GET_DEVICE_INFO,
                    std::ptr::null_mut(),
                    &mut list as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc2, STATUS_NOERROR as c_long);
            assert_eq!(param.Supported, 1);
        }
    );

    // `CONFIG_SW_CAN_HS_DATA_RATE`/`_SPEEDCHANGE_ENABLE`/`_RES_SWITCH` round
    // -trip through this mock's generic per-channel `SET_CONFIG`/
    // `GET_CONFIG` storage (`channel.params`) -- the same simple
    // stored-value semantics every other numeric CONFIG param not otherwise
    // specially handled already gets (e.g. `CONFIG_BIT_SAMPLE_POINT` on a
    // plain CAN channel), so no SWCAN-specific storage code is needed.
    serial_test!(
        fn sw_can_config_params_round_trip_through_generic_storage() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_SW_CAN_PS,
                    0,
                    500_000,
                    &mut channel_id,
                );
            }
            set_j1962_pins(channel_id, 0x0000_0100);

            let rc = set_single_config(
                channel_id,
                j2534_0404_sys::bindings::CONFIG_SW_CAN_HS_DATA_RATE,
                83_300,
            );
            assert_eq!(rc, STATUS_NOERROR as c_long);
            let rc2 = set_single_config(
                channel_id,
                j2534_0404_sys::bindings::CONFIG_SW_CAN_SPEEDCHANGE_ENABLE,
                1,
            );
            assert_eq!(rc2, STATUS_NOERROR as c_long);
            let rc3 = set_single_config(
                channel_id,
                j2534_0404_sys::bindings::CONFIG_SW_CAN_RES_SWITCH,
                1,
            );
            assert_eq!(rc3, STATUS_NOERROR as c_long);

            let mut cfg_hs = SCONFIG {
                Parameter: j2534_0404_sys::bindings::CONFIG_SW_CAN_HS_DATA_RATE,
                Value: 0,
            };
            let mut cfg_speedchange = SCONFIG {
                Parameter: j2534_0404_sys::bindings::CONFIG_SW_CAN_SPEEDCHANGE_ENABLE,
                Value: 0,
            };
            let mut cfg_res = SCONFIG {
                Parameter: j2534_0404_sys::bindings::CONFIG_SW_CAN_RES_SWITCH,
                Value: 0,
            };
            let mut configs = [cfg_hs, cfg_speedchange, cfg_res];
            let mut list = SCONFIG_LIST {
                NumOfParams: configs.len() as u32,
                ConfigPtr: configs.as_mut_ptr(),
            };
            let get_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_GET_CONFIG,
                    &mut list as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(get_rc, STATUS_NOERROR as c_long);
            cfg_hs = configs[0];
            cfg_speedchange = configs[1];
            cfg_res = configs[2];
            assert_eq!(cfg_hs.Value, 83_300);
            assert_eq!(cfg_speedchange.Value, 1);
            assert_eq!(cfg_res.Value, 1);
        }
    );

    // ── SAE J2534-2 clause 14 Repeat Messaging (ADR-165/Phase 12) ───────────

    /// Builds a `PASSTHRU_MSG` carrying `data`, `ProtocolID = PROTOCOL_CAN`,
    /// for a `REPEAT_MSG_SETUP.RepeatMsgData` slot.
    fn repeat_passthru_msg(data: &[u8]) -> PASSTHRU_MSG {
        let mut msg = unsafe { std::mem::zeroed::<PASSTHRU_MSG>() };
        msg.ProtocolID = PROTOCOL_CAN;
        msg.DataSize = data.len() as u32;
        msg.Data[..data.len()].copy_from_slice(data);
        msg
    }

    fn written_msg_count(channel_id: u32) -> usize {
        let guard = state().lock().expect("mock state poisoned");
        guard
            .channels
            .get(&channel_id)
            .map(|c| c.written_msgs.len())
            .unwrap_or(0)
    }

    /// Polls (short steps, not one fixed sleep) until
    /// `written_msg_count(channel_id) >= min_count` or `max_wait_ms` elapses
    /// -- avoids a flaky exact-timing assumption while still bounding how
    /// long a failing test can block.
    fn wait_for_written_msg_count(channel_id: u32, min_count: usize, max_wait_ms: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(max_wait_ms);
        while written_msg_count(channel_id) < min_count && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Polls (short steps) until `IOCTL_QUERY_REPEAT_MESSAGE` reports
    /// `expected_status` for `msg_id` on `channel_id` (a successful QUERY
    /// with `status == expected_status`), or `max_wait_ms` elapses. Returns
    /// whether that status was observed within the bound. ADR-173's QUERY
    /// polarity: `1` for a still-live slot, `0` for a terminated-but-
    /// unstopped one (both are successful QUERYs against a still-tracked
    /// `MsgId` -- an explicit `IOCTL_STOP_REPEAT_MESSAGE` is required before
    /// a QUERY starts returning `ERR_INVALID_MSG_ID`, clause 14.2.2.3's
    /// `MsgId`-retention rule).
    fn wait_for_repeat_slot_status(
        channel_id: u32,
        msg_id: u32,
        expected_status: u32,
        max_wait_ms: u64,
    ) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(max_wait_ms);
        while std::time::Instant::now() < deadline {
            let mut status: u32 = 0;
            let mut query_msg_id = msg_id;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id as *mut _ as *mut c_void,
                    &mut status as *mut _ as *mut c_void,
                )
            };
            if rc == STATUS_NOERROR as c_long && status == expected_status {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    // ADR-173 Decision 1/coverage item 3: with the mask/pattern set to
    // never match (mask/pattern both `[]` is vacuous-matches-anything, so
    // this uses a non-empty mask/pattern that the never-injected traffic
    // could never satisfy anyway -- but here the bus is simply left fully
    // silent, no frames at all), a `Condition == 0` slot must NOT
    // self-terminate: it keeps retransmitting indefinitely regardless of
    // RX activity (or the lack of it), unlike `Condition == 1`, which DOES
    // self-terminate on silence (see `condition_one_slot_stops_on_a_silent_
    // interval_with_no_frames_at_all` below).
    serial_test!(
        fn start_repeat_message_condition_zero_retransmits_until_stopped() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            let mut setup = REPEAT_MSG_SETUP {
                TimeInterval: 10,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0xAA, 0xBB]),
                    repeat_passthru_msg(&[]),
                    repeat_passthru_msg(&[]),
                ],
            };
            let mut msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut setup as *mut _ as *mut c_void,
                    &mut msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_ne!(msg_id, 0);

            wait_for_written_msg_count(channel_id, 3, 300);
            assert!(
                written_msg_count(channel_id) >= 3,
                "Condition == 0 should retransmit RepeatMsgData[0] indefinitely until stopped, \
                 with a fully silent bus (no frames at all, matching or otherwise) never \
                 self-terminating it"
            );

            let mut status: u32 = 0;
            let mut query_msg_id = msg_id;
            let query_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id as *mut _ as *mut c_void,
                    &mut status as *mut _ as *mut c_void,
                )
            };
            assert_eq!(query_rc, STATUS_NOERROR as c_long);
            assert_eq!(
                status, 1,
                "a still-live slot (never self-terminated, never stopped) reports QUERY status \
                 1 (ADR-173 Table 53 polarity)"
            );

            let mut stop_msg_id = msg_id;
            let stop_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(stop_rc, STATUS_NOERROR as c_long);

            // A stopped slot's MsgId is no longer known ...
            let mut query_msg_id2 = msg_id;
            let mut status2: u32 = 0;
            let query_rc2 = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id2 as *mut _ as *mut c_void,
                    &mut status2 as *mut _ as *mut c_void,
                )
            };
            assert_eq!(query_rc2, ERR_INVALID_MSG_ID as c_long);

            // ... and stopping it again is also ERR_INVALID_MSG_ID.
            let mut stop_msg_id2 = msg_id;
            let stop_rc2 = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id2 as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(stop_rc2, ERR_INVALID_MSG_ID as c_long);
        }
    );

    // ADR-173 Decision 1 (coverage item 1) + Decision 3 (coverage item 9,
    // QUERY status polarity end-to-end): a `Condition == 0` slot's own stop
    // trigger is a MATCHING frame (the inverse of `Condition == 1`'s, tested
    // separately below). Exercises the full lifecycle: live (status 1) ->
    // self-terminated-but-unstopped (status 0, MsgId still valid per clause
    // 14.2.2.3) -> genuinely gone (ERR_INVALID_MSG_ID) only after an
    // explicit STOP.
    serial_test!(
        fn start_repeat_message_condition_zero_stops_on_a_matching_frame_and_retains_msg_id_until_stop()
         {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            // Mask/pattern: byte 0 must equal 0x55 exactly; the byte beyond
            // the 1-byte mask/pattern is don't-care.
            let mut setup = REPEAT_MSG_SETUP {
                TimeInterval: 200,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x02, 0x10]),
                    repeat_passthru_msg(&[0xFF]),
                    repeat_passthru_msg(&[0x55]),
                ],
            };
            let mut msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut setup as *mut _ as *mut c_void,
                    &mut msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            assert!(
                wait_for_repeat_slot_status(channel_id, msg_id, 1, 100),
                "a freshly started slot with no RX yet must report QUERY status 1 (live)"
            );

            // A matching frame (0x55 followed by a don't-care byte) injected
            // shortly after START should end the slot well before the
            // 200ms TimeInterval elapses.
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert!(mock_inject_rx_message(
                channel_id,
                vec![0x55, 0x99],
                PROTOCOL_CAN,
                0
            ));

            assert!(
                wait_for_repeat_slot_status(channel_id, msg_id, 0, 150),
                "a Condition == 0 slot must terminate (QUERY status 0) once a matching frame \
                 arrives, well before TimeInterval elapses"
            );

            // Terminated, but the MsgId is still valid (clause 14.2.2.3) --
            // ERR_INVALID_MSG_ID must NOT appear until an explicit STOP.
            let mut status: u32 = 0;
            let mut query_msg_id = msg_id;
            let query_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id as *mut _ as *mut c_void,
                    &mut status as *mut _ as *mut c_void,
                )
            };
            assert_eq!(query_rc, STATUS_NOERROR as c_long);
            assert_eq!(status, 0);

            let mut stop_msg_id = msg_id;
            let stop_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(stop_rc, STATUS_NOERROR as c_long);

            let mut query_msg_id2 = msg_id;
            let mut status2: u32 = 0;
            let query_rc2 = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id2 as *mut _ as *mut c_void,
                    &mut status2 as *mut _ as *mut c_void,
                )
            };
            assert_eq!(
                query_rc2, ERR_INVALID_MSG_ID as c_long,
                "only the explicit STOP above removes the MsgId outright"
            );
        }
    );

    // ADR-173 Decision 1 (coverage item 2): a `Condition == 0` slot's stop
    // trigger is specifically a MATCH -- a non-matching received frame must
    // never end it.
    serial_test!(
        fn start_repeat_message_condition_zero_non_matching_frame_does_not_stop_retransmission() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            // Mask/pattern: byte 0 must equal 0x55 exactly.
            let mut setup = REPEAT_MSG_SETUP {
                TimeInterval: 10,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x02, 0x10]),
                    repeat_passthru_msg(&[0xFF]),
                    repeat_passthru_msg(&[0x55]),
                ],
            };
            let mut msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut setup as *mut _ as *mut c_void,
                    &mut msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            // A NON-matching frame (byte 0 != 0x55).
            assert!(mock_inject_rx_message(
                channel_id,
                vec![0xAA, 0x99],
                PROTOCOL_CAN,
                0
            ));

            wait_for_written_msg_count(channel_id, 3, 300);
            assert!(
                written_msg_count(channel_id) >= 3,
                "a non-matching frame must never stop a Condition == 0 slot's retransmission"
            );

            let mut status: u32 = 0;
            let mut query_msg_id = msg_id;
            let query_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id as *mut _ as *mut c_void,
                    &mut status as *mut _ as *mut c_void,
                )
            };
            assert_eq!(query_rc, STATUS_NOERROR as c_long);
            assert_eq!(
                status, 1,
                "a non-matching frame must not terminate the slot"
            );

            let mut stop_msg_id = msg_id;
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }
        }
    );

    serial_test!(
        fn query_repeat_message_does_not_resolve_a_sibling_channels_msg_id() {
            let mut channel_a: u32 = 0;
            let mut channel_b: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_a);
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_b);
            }

            let mut setup = REPEAT_MSG_SETUP {
                TimeInterval: 20,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x01]),
                    repeat_passthru_msg(&[]),
                    repeat_passthru_msg(&[]),
                ],
            };
            let mut msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_a,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut setup as *mut _ as *mut c_void,
                    &mut msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            let mut status: u32 = 0;
            let mut query_msg_id = msg_id;
            let cross_channel_rc = unsafe {
                PassThruIoctl(
                    channel_b,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id as *mut _ as *mut c_void,
                    &mut status as *mut _ as *mut c_void,
                )
            };
            assert_eq!(
                cross_channel_rc, ERR_INVALID_MSG_ID as c_long,
                "a MsgId started on channel_a must not resolve against channel_b"
            );

            let mut own_query_msg_id = msg_id;
            let own_rc = unsafe {
                PassThruIoctl(
                    channel_a,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut own_query_msg_id as *mut _ as *mut c_void,
                    &mut status as *mut _ as *mut c_void,
                )
            };
            assert_eq!(own_rc, STATUS_NOERROR as c_long);

            let mut stop_msg_id = msg_id;
            unsafe {
                PassThruIoctl(
                    channel_a,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }
        }
    );

    serial_test!(
        fn start_repeat_message_enforces_max_slots_per_channel() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            let mut msg_ids = Vec::new();
            for _ in 0..MAX_REPEAT_SLOTS_PER_CHANNEL {
                let mut setup = REPEAT_MSG_SETUP {
                    TimeInterval: 5_000,
                    Condition: 0,
                    RepeatMsgData: [
                        repeat_passthru_msg(&[0x01]),
                        repeat_passthru_msg(&[]),
                        repeat_passthru_msg(&[]),
                    ],
                };
                let mut msg_id: u32 = 0;
                let rc = unsafe {
                    PassThruIoctl(
                        channel_id,
                        IOCTL_START_REPEAT_MESSAGE,
                        &mut setup as *mut _ as *mut c_void,
                        &mut msg_id as *mut _ as *mut c_void,
                    )
                };
                assert_eq!(rc, STATUS_NOERROR as c_long);
                msg_ids.push(msg_id);
            }

            let mut overflow_setup = REPEAT_MSG_SETUP {
                TimeInterval: 5_000,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x01]),
                    repeat_passthru_msg(&[]),
                    repeat_passthru_msg(&[]),
                ],
            };
            let mut overflow_msg_id: u32 = 0;
            let overflow_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut overflow_setup as *mut _ as *mut c_void,
                    &mut overflow_msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(overflow_rc, ERR_EXCEEDED_LIMIT as c_long);

            // ADR-173 Decision 3 (coverage item 10): a terminated-but-
            // unstopped slot still occupies its budget slot -- the limit is
            // freed only by an explicit STOP, not by self-termination. Every
            // slot above was started with an empty mask/pattern, which
            // vacuously matches ANY received frame (see
            // `short_frame_never_matches_repeat_slot_mask_pattern`'s own
            // "empty mask/pattern matches anything" case), so a single
            // injected frame terminates all `MAX_REPEAT_SLOTS_PER_CHANNEL`
            // Condition == 0 slots at once.
            assert!(mock_inject_rx_message(
                channel_id,
                vec![0xFF],
                PROTOCOL_CAN,
                0
            ));
            for &msg_id in &msg_ids {
                assert!(
                    wait_for_repeat_slot_status(channel_id, msg_id, 0, 200),
                    "every slot's empty mask/pattern vacuously matches the injected frame and \
                     must terminate under Condition == 0"
                );
            }

            let mut still_overflow_setup = REPEAT_MSG_SETUP {
                TimeInterval: 5_000,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x01]),
                    repeat_passthru_msg(&[]),
                    repeat_passthru_msg(&[]),
                ],
            };
            let mut still_overflow_msg_id: u32 = 0;
            let still_overflow_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut still_overflow_setup as *mut _ as *mut c_void,
                    &mut still_overflow_msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(
                still_overflow_rc, ERR_EXCEEDED_LIMIT as c_long,
                "a terminated-but-unstopped slot must still count toward the per-channel limit \
                 -- the budget is freed only by an explicit STOP"
            );

            for &msg_id in &msg_ids {
                let mut stop_msg_id = msg_id;
                unsafe {
                    PassThruIoctl(
                        channel_id,
                        IOCTL_STOP_REPEAT_MESSAGE,
                        &mut stop_msg_id as *mut _ as *mut c_void,
                        std::ptr::null_mut(),
                    );
                }
            }

            // Now that every slot has been explicitly stopped, the budget
            // is freed and a fresh START succeeds.
            let mut freed_setup = REPEAT_MSG_SETUP {
                TimeInterval: 5_000,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x01]),
                    repeat_passthru_msg(&[]),
                    repeat_passthru_msg(&[]),
                ],
            };
            let mut freed_msg_id: u32 = 0;
            let freed_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut freed_setup as *mut _ as *mut c_void,
                    &mut freed_msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(
                freed_rc, STATUS_NOERROR as c_long,
                "explicit STOP of every slot must free the per-channel budget"
            );
            let mut freed_stop_msg_id = freed_msg_id;
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut freed_stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }
        }
    );

    // Codex review finding, PR #95: `IOCTL_START_REPEAT_MESSAGE` used to
    // accept `RepeatMsgData[0]` at any size, never enforcing the same
    // per-protocol cap `IOCTL_GET_PROTOCOL_INFO` advertises via
    // `PROTOCOL_INFO_MAX_REPEAT_MESSAGING_LENGTH` -- so no test could
    // exercise the device-side `ERR_INVALID_MSG` rejection (clause
    // 14.2.2.1) that advertised value promises. Raw CAN's advertised cap is
    // 12 bytes (`max_repeat_messaging_length`); this proves both the
    // boundary (exactly 12 bytes still accepted) and the rejection (13
    // bytes, one over, rejected `ERR_INVALID_MSG`) in one test, mirroring
    // `start_repeat_message_enforces_max_slots_per_channel`'s own
    // accept-then-reject shape just above.
    serial_test!(
        fn start_repeat_message_rejects_datasize_exceeding_the_advertised_max_repeat_messaging_length()
         {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            let mut ok_setup = REPEAT_MSG_SETUP {
                TimeInterval: 5_000,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0u8; 12]),
                    repeat_passthru_msg(&[]),
                    repeat_passthru_msg(&[]),
                ],
            };
            let mut ok_msg_id: u32 = 0;
            let ok_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut ok_setup as *mut _ as *mut c_void,
                    &mut ok_msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(
                ok_rc, STATUS_NOERROR as c_long,
                "a 12-byte RepeatMsgData[0] is exactly the advertised raw-CAN cap and must be \
                 accepted"
            );
            let mut ok_stop_msg_id = ok_msg_id;
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut ok_stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }

            let mut oversized_setup = REPEAT_MSG_SETUP {
                TimeInterval: 5_000,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0u8; 13]),
                    repeat_passthru_msg(&[]),
                    repeat_passthru_msg(&[]),
                ],
            };
            let mut oversized_msg_id: u32 = 0;
            let oversized_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut oversized_setup as *mut _ as *mut c_void,
                    &mut oversized_msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(
                oversized_rc, ERR_INVALID_MSG as c_long,
                "a 13-byte RepeatMsgData[0] exceeds the advertised raw-CAN cap (12) and must be \
                 rejected with ERR_INVALID_MSG, mirroring what a real conforming adapter would do"
            );
        }
    );

    // ADR-173 Decision 1 (coverage item 4): a `Condition == 1` slot's stop
    // trigger includes an IMMEDIATE non-matching frame -- it does not wait
    // for the interval deadline.
    serial_test!(
        fn condition_one_slot_stops_immediately_on_a_non_matching_frame() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            // Mask/pattern: byte 0 must equal 0x55 exactly.
            let mut setup = REPEAT_MSG_SETUP {
                TimeInterval: 200,
                Condition: 1,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x02, 0x10]),
                    repeat_passthru_msg(&[0xFF]),
                    repeat_passthru_msg(&[0x55]),
                ],
            };
            let mut msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut setup as *mut _ as *mut c_void,
                    &mut msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            // A NON-matching frame (byte 0 != 0x55) injected shortly after
            // START should end the slot well before the 200ms TimeInterval
            // elapses.
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert!(mock_inject_rx_message(
                channel_id,
                vec![0xAA, 0x99],
                PROTOCOL_CAN,
                0
            ));

            assert!(
                wait_for_repeat_slot_status(channel_id, msg_id, 0, 150),
                "a Condition == 1 slot must terminate immediately once a non-matching frame \
                 arrives, well before TimeInterval elapses"
            );

            let mut stop_msg_id = msg_id;
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }
        }
    );

    // ADR-173 Decision 1 (coverage item 7): the silence check resets every
    // interval -- a match in interval N does not immunize interval N+1. A
    // matching frame early in interval 1, followed by silence for the rest
    // of interval 1 and all of interval 2, must still terminate at interval
    // 2's own deadline.
    serial_test!(
        fn condition_one_match_in_one_interval_does_not_immunize_the_next_interval() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            const INTERVAL_MS: u32 = 60;
            let mut setup = REPEAT_MSG_SETUP {
                TimeInterval: INTERVAL_MS,
                Condition: 1,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x02, 0x10]),
                    repeat_passthru_msg(&[0xFF]),
                    repeat_passthru_msg(&[0x55]),
                ],
            };
            let mut msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut setup as *mut _ as *mut c_void,
                    &mut msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            // Inject a MATCHING frame early in interval 1 (well before its
            // ~60ms deadline).
            std::thread::sleep(std::time::Duration::from_millis(10));
            assert!(mock_inject_rx_message(
                channel_id,
                vec![0x55, 0x99],
                PROTOCOL_CAN,
                0
            ));

            // Still live shortly after the match -- confirms the match did
            // not terminate it (same invariant as the dedicated test above).
            assert!(
                wait_for_repeat_slot_status(channel_id, msg_id, 1, 30),
                "a matching frame must not terminate a Condition == 1 slot"
            );

            // No further frames are injected -- interval 1 finishes silent
            // (after the match) and interval 2 is fully silent. The slot
            // must terminate at interval 2's own deadline, not be immunized
            // by interval 1's earlier match.
            assert!(
                wait_for_repeat_slot_status(channel_id, msg_id, 0, 400),
                "a match in interval N must not immunize interval N+1 -- the slot must still \
                 terminate once a subsequent interval elapses in silence"
            );

            let mut stop_msg_id = msg_id;
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }
        }
    );

    // ADR-173 Decision 1 (coverage item 8): the worker's own loopback echo
    // of its transmission is excluded from repeat-slot evaluation
    // (`REPEAT_INELIGIBLE_RX_STATUS_MASK`) -- it must not count as
    // "received" for Condition == 1's silence check, even though its data
    // coincidentally matches the slot's own mask/pattern (constructed here
    // to match the transmitted message exactly, so a bug that let the echo
    // count would make this slot incorrectly immortal).
    serial_test!(
        fn condition_one_slot_own_transmit_echo_does_not_count_as_received_and_still_terminates_on_silence()
         {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            let mut cfg = SCONFIG {
                Parameter: j2534_0404_sys::bindings::CONFIG_LOOPBACK,
                Value: 1,
            };
            let mut cfg_list = SCONFIG_LIST {
                NumOfParams: 1,
                ConfigPtr: &mut cfg,
            };
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_SET_CONFIG,
                    &mut cfg_list as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }

            // Mask/pattern matches the transmitted message's own data
            // exactly -- if the echo were (incorrectly) treated as
            // "received", it would satisfy this match every single
            // interval and the slot would never terminate.
            let mut setup = REPEAT_MSG_SETUP {
                TimeInterval: 30,
                Condition: 1,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x77, 0x88]),
                    repeat_passthru_msg(&[0xFF, 0xFF]),
                    repeat_passthru_msg(&[0x77, 0x88]),
                ],
            };
            let mut msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut setup as *mut _ as *mut c_void,
                    &mut msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            // No externally injected traffic at all -- only the worker's
            // own loopback echoes reach `channel.rx_queue`. The slot must
            // still terminate on silence.
            assert!(
                wait_for_repeat_slot_status(channel_id, msg_id, 0, 300),
                "a Condition == 1 slot's own transmit echo must never count as a received \
                 frame -- the slot must still terminate on silence despite the echo"
            );

            let mut stop_msg_id = msg_id;
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }
        }
    );

    // ADR-173 Decision 1 (coverage item 6): the single most safety-critical
    // semantic reversal in this fix -- under the corrected `Condition == 1`
    // semantics, a MATCHING frame does NOT terminate the slot (only a
    // non-match or a silent interval does, see the dedicated non-match and
    // silence tests elsewhere in this module). This test used to be named
    // `condition_one_slot_stops_on_a_matching_response` and asserted the
    // OPPOSITE (ADR-165's inverted pairing, now superseded) -- fixed in
    // place per ADR-173 rather than deleted, since a real invariant (a
    // Condition == 1 slot's behavior around a matching frame) is still
    // exactly what it exercises, just with the corrected expected outcome.
    // This test fails under the pre-ADR-173 code and passes under the
    // corrected one.
    serial_test!(
        fn condition_one_slot_does_not_stop_on_a_matching_frame_within_the_interval() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            // Mask/pattern: byte 0 must equal 0x55 exactly; bytes beyond the
            // 1-byte mask/pattern are don't-care.
            let mut setup = REPEAT_MSG_SETUP {
                TimeInterval: 300,
                Condition: 1,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x02, 0x10]),
                    repeat_passthru_msg(&[0xFF]),
                    repeat_passthru_msg(&[0x55]),
                ],
            };
            let mut msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut setup as *mut _ as *mut c_void,
                    &mut msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            // A matching frame (0x55 followed by a don't-care byte) injected
            // shortly after START must NOT end the slot.
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert!(mock_inject_rx_message(
                channel_id,
                vec![0x55, 0x99],
                PROTOCOL_CAN,
                0
            ));

            // Give the (wrong, pre-ADR-173) immediate-stop-on-match behavior
            // every opportunity to have fired, then assert the slot is
            // still live -- well before the 300ms TimeInterval, so this is
            // not merely "hasn't reached the silence deadline yet" but
            // genuinely "a match does not terminate it".
            std::thread::sleep(std::time::Duration::from_millis(100));
            let mut status: u32 = 0;
            let mut query_msg_id = msg_id;
            let query_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id as *mut _ as *mut c_void,
                    &mut status as *mut _ as *mut c_void,
                )
            };
            assert_eq!(query_rc, STATUS_NOERROR as c_long);
            assert_eq!(
                status, 1,
                "a matching frame must NOT terminate a Condition == 1 slot (ADR-173 Decision 1 \
                 -- only a non-match or a silent interval does)"
            );

            let mut stop_msg_id = msg_id;
            unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }
        }
    );

    // ADR-173 Decision 1 (coverage item 5): a `Condition == 1` slot with a
    // fully silent bus (zero received frames, matching or otherwise) must
    // still terminate once `TimeInterval` elapses -- fixed in place from
    // its pre-ADR-173 name/assertion (`condition_one_slot_stops_on_timeout_
    // with_no_match`, which asserted the slot was fully REMOVED at timeout;
    // it is now only `terminated`, status 0, retained until an explicit
    // STOP per Decision 3).
    serial_test!(
        fn condition_one_slot_stops_on_a_silent_interval_with_no_frames_at_all() {
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }

            let mut setup = REPEAT_MSG_SETUP {
                TimeInterval: 20,
                Condition: 1,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0x02, 0x10]),
                    repeat_passthru_msg(&[0xFF]),
                    repeat_passthru_msg(&[0x55]),
                ],
            };
            let mut msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut setup as *mut _ as *mut c_void,
                    &mut msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);

            // No frame at all is ever injected -- the slot must still
            // terminate (ADR-173 Decision 1: an interval elapsing with zero
            // eligible received frames is itself a stop trigger) within a
            // generous bound, reporting QUERY status 0 rather than being
            // outright removed.
            assert!(
                wait_for_repeat_slot_status(channel_id, msg_id, 0, 300),
                "a Condition == 1 slot with no received frames at all must terminate once \
                 TimeInterval elapses"
            );

            // Retained, not gone: the MsgId is still valid until an
            // explicit STOP (clause 14.2.2.3).
            let mut status: u32 = 0;
            let mut query_msg_id = msg_id;
            let query_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id as *mut _ as *mut c_void,
                    &mut status as *mut _ as *mut c_void,
                )
            };
            assert_eq!(query_rc, STATUS_NOERROR as c_long);
            assert_eq!(status, 0);

            let mut stop_msg_id = msg_id;
            let stop_rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(stop_rc, STATUS_NOERROR as c_long);

            let mut query_msg_id2 = msg_id;
            let mut status2: u32 = 0;
            let query_rc2 = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_QUERY_REPEAT_MESSAGE,
                    &mut query_msg_id2 as *mut _ as *mut c_void,
                    &mut status2 as *mut _ as *mut c_void,
                )
            };
            assert_eq!(query_rc2, ERR_INVALID_MSG_ID as c_long);
        }
    );

    // Bug 4 regression (edge-case-hunter, ADR-165): a lingering
    // `spawn_repeat_worker` thread from a repeat slot that was never
    // explicitly stopped before `mock_reset()` must not survive into a fresh
    // post-reset slot occupying the identical `(channel_id, msg_id)` key and
    // silently double-write into it.
    serial_test!(
        fn a_lingering_worker_from_before_reset_does_not_corrupt_a_same_keyed_post_reset_slot() {
            // `serial_test!` already called `mock_reset()` once on entry;
            // start a slot here, leave it running (never STOP it -- the
            // exact scenario that produced the bug), then reset again
            // ourselves while it is still alive.
            let mut channel_id: u32 = 0;
            unsafe {
                PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
            }
            assert_eq!(
                channel_id, 1,
                "sanity: first channel post-reset is always id 1"
            );

            let mut old_setup = REPEAT_MSG_SETUP {
                TimeInterval: 10,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0xAA]),
                    repeat_passthru_msg(&[]),
                    repeat_passthru_msg(&[]),
                ],
            };
            let mut old_msg_id: u32 = 0;
            let rc = unsafe {
                PassThruIoctl(
                    channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut old_setup as *mut _ as *mut c_void,
                    &mut old_msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc, STATUS_NOERROR as c_long);
            assert_eq!(
                old_msg_id, 1,
                "sanity: first MsgId on a fresh channel is always id 1"
            );

            // Let the old slot's worker fire at least once before the reset
            // below, so it is definitely mid-loop (past its first state
            // read) rather than not-yet-started.
            wait_for_written_msg_count(channel_id, 1, 300);

            // Reset WITHOUT stopping the old slot first -- `mock_reset` now
            // bumps REPEAT_WORKER_EPOCH, which is the fix under test.
            mock_reset();

            // Reconnect: `next_channel_id`/`next_repeat_msg_id` are both back
            // to 1 after the reset, so this new slot lands on the EXACT same
            // (channel_id, msg_id) = (1, 1) key the old worker is still
            // watching.
            let mut new_channel_id: u32 = 0;
            unsafe {
                PassThruConnect(
                    MOCK_DEVICE_ID,
                    PROTOCOL_CAN,
                    0,
                    500_000,
                    &mut new_channel_id,
                );
            }
            assert_eq!(new_channel_id, channel_id);

            let mut new_setup = REPEAT_MSG_SETUP {
                TimeInterval: 100,
                Condition: 0,
                RepeatMsgData: [
                    repeat_passthru_msg(&[0xBB]),
                    repeat_passthru_msg(&[]),
                    repeat_passthru_msg(&[]),
                ],
            };
            let mut new_msg_id: u32 = 0;
            let rc2 = unsafe {
                PassThruIoctl(
                    new_channel_id,
                    IOCTL_START_REPEAT_MESSAGE,
                    &mut new_setup as *mut _ as *mut c_void,
                    &mut new_msg_id as *mut _ as *mut c_void,
                )
            };
            assert_eq!(rc2, STATUS_NOERROR as c_long);
            assert_eq!(
                new_msg_id, old_msg_id,
                "sanity: same key as the old, lingering slot"
            );

            // Fixed wait (not an early-exit poll): a still-alive OLD worker
            // re-reads the CURRENT (new) slot's own data every loop iteration
            // (it is not pinned to the original slot's stale 10ms interval),
            // so an unfixed lingering worker converges into an
            // indistinguishable SECOND worker for the new slot -- doubling
            // its ~100ms-interval write rate, not retransmitting at the old
            // 10ms cadence forever. 650ms comfortably covers 6 of the new
            // slot's own intervals (t=0,100,...,600 -> 7 writes from a
            // single, correctly-isolated worker); a duplicated second worker
            // roughly doubles that to ~14. The two are far enough apart that
            // this assertion is not sensitive to ordinary scheduling jitter.
            std::thread::sleep(std::time::Duration::from_millis(650));
            let count = written_msg_count(new_channel_id);
            assert!(
                (5..=9).contains(&count),
                "written_msgs count ({count}) should reflect only the NEW slot's own single \
                 ~100ms-interval worker (~7 writes expected in 650ms), not be roughly doubled by \
                 a lingering pre-reset worker that re-attached itself as a second worker for \
                 this same slot"
            );

            let mut stop_msg_id = new_msg_id;
            unsafe {
                PassThruIoctl(
                    new_channel_id,
                    IOCTL_STOP_REPEAT_MESSAGE,
                    &mut stop_msg_id as *mut _ as *mut c_void,
                    std::ptr::null_mut(),
                );
            }
        }
    );

    /// Builds a minimal `RepeatSlot` for the grid-anchored-window tests below
    /// -- only the fields those tests actually vary are parameterized;
    /// everything else is a fixed, timing-irrelevant placeholder (a mask/
    /// pattern of `[0xFF]`/`[0x55]`, matched by a `[0x55]` data frame).
    fn test_repeat_slot(
        condition: u32,
        time_interval_ms: u32,
        interval_deadline: Option<std::time::Instant>,
        rx_seen_this_interval: bool,
    ) -> RepeatSlot {
        RepeatSlot {
            time_interval_ms,
            condition,
            message: StoredMessage {
                protocol_id: PROTOCOL_CAN,
                rx_status: 0,
                tx_flags: 0,
                timestamp: 0,
                data: vec![0x02, 0x10],
                extra_data_index: None,
            },
            mask: vec![0xFF],
            pattern: vec![0x55],
            response_format_rx_bits: 0,
            response_protocol_id: PROTOCOL_CAN,
            mask_pattern_tx_flags: 0,
            terminated: false,
            rx_seen_this_interval,
            interval_deadline,
        }
    }

    /// Round-3 regression (ADR-173, Codex review PR #60 round 3): the exact
    /// scenario found in review -- a worker (or, here, a direct
    /// `advance_repeat_slot_windows` call standing in for one) that only
    /// notices `now` after MORE than one full `TimeInterval` has elapsed
    /// since the stored deadline. `interval_deadline` is 150ms in the past
    /// with a 100ms `TimeInterval`, so two whole windows ([50ms,150ms) and
    /// [150ms,now)-ish, relative to the original anchor) have elapsed:
    /// window 1 (`rx_seen_this_interval == true`) had traffic and correctly
    /// does not terminate, but window 2 is fully silent and must terminate
    /// the slot -- the round-1/round-2 patches' `Instant::now() + interval`
    /// re-anchoring at each transmit would silently skip evaluating window
    /// 2 entirely, the bug this mechanism replaces those patches to fix.
    #[test]
    fn advance_repeat_slot_windows_terminates_on_a_stall_spanning_multiple_intervals() {
        let now = std::time::Instant::now();
        let mut slot = test_repeat_slot(
            1,
            100,
            Some(now - std::time::Duration::from_millis(150)),
            true, // interval 1 (the one ending at the stored deadline) had traffic
        );
        advance_repeat_slot_windows(&mut slot, now);
        assert!(
            slot.terminated,
            "interval 2 (the window immediately following the stored deadline) elapsed in total \
             silence and must terminate the slot -- re-anchoring the grid to `now` at each call \
             (the round-3 bug) would silently skip evaluating it"
        );
    }

    /// A single elapsed window that HAD traffic must not terminate, and the
    /// grid must advance by exactly one whole `TimeInterval` step from its
    /// own prior boundary (not re-anchor to `now`), with
    /// `rx_seen_this_interval` reset fresh for the new window.
    #[test]
    fn advance_repeat_slot_windows_credits_a_single_elapsed_window_and_resets_for_the_next() {
        let now = std::time::Instant::now();
        let original = now - std::time::Duration::from_millis(50);
        let mut slot = test_repeat_slot(1, 100, Some(original), true);
        advance_repeat_slot_windows(&mut slot, now);
        assert!(
            !slot.terminated,
            "the just-closed window had traffic (rx_seen_this_interval was true), so it must \
             not terminate on silence"
        );
        assert!(
            !slot.rx_seen_this_interval,
            "the new window must start fresh, not carry over the prior window's credit (ADR-173: \
             a match in interval N must not immunize interval N+1)"
        );
        assert_eq!(
            slot.interval_deadline,
            Some(original + std::time::Duration::from_millis(100)),
            "the grid must advance by exactly one whole TimeInterval step from its own prior \
             boundary, not re-anchor to `now`"
        );
    }

    /// Round-1 regression, strengthened for the new mechanism: a frame
    /// arriving after a silent window's deadline must trigger termination AT
    /// ARRIVAL (via `note_rx_frame_for_repeat_slots` calling
    /// `advance_repeat_slot_windows` first), not merely fail to rescue the
    /// window.
    #[test]
    fn note_rx_frame_for_repeat_slots_terminates_a_condition_one_slot_on_arrival_after_a_silent_window()
     {
        let mut channel = ChannelState::new(PROTOCOL_CAN, 500_000, 0);
        channel.repeat_slots.insert(
            1,
            test_repeat_slot(
                1,
                20,
                Some(std::time::Instant::now() - std::time::Duration::from_millis(5)),
                false, // the window that just elapsed was silent
            ),
        );

        note_rx_frame_for_repeat_slots(&mut channel, &[0x55], 0, PROTOCOL_CAN);

        assert!(
            channel.repeat_slots.get(&1).unwrap().terminated,
            "a frame arriving strictly after a silent window's deadline must trigger termination \
             at arrival, not merely fail to rescue it"
        );
    }

    /// Round-2 regression, via the new mechanism: a frame arriving after a
    /// window's deadline, where that window HAD traffic, must be credited to
    /// the NEW window it actually falls into (not lost, and not terminated),
    /// and a second call comfortably within that new window must be a no-op
    /// rather than double-advancing the grid.
    #[test]
    fn note_rx_frame_for_repeat_slots_credits_a_late_frame_to_the_next_window() {
        let mut channel = ChannelState::new(PROTOCOL_CAN, 500_000, 0);
        let original = std::time::Instant::now() - std::time::Duration::from_millis(5);
        channel.repeat_slots.insert(
            1,
            test_repeat_slot(1, 100, Some(original), true), // just-elapsed window had traffic
        );
        let expected_deadline = original + std::time::Duration::from_millis(100);

        // A MATCHING frame (a no-op for Condition == 1's own per-frame
        // stop-trigger -- only a non-match or silence terminates it)
        // arriving after the deadline has already elapsed.
        note_rx_frame_for_repeat_slots(&mut channel, &[0x55], 0, PROTOCOL_CAN);
        {
            let slot = channel.repeat_slots.get(&1).unwrap();
            assert!(
                !slot.terminated,
                "a matching frame under Condition == 1 must never terminate the slot by itself, \
                 and the window that just closed had traffic so it must not silence-terminate \
                 either"
            );
            assert_eq!(
                slot.interval_deadline,
                Some(expected_deadline),
                "the grid must advance by exactly one whole TimeInterval step"
            );
            assert!(
                slot.rx_seen_this_interval,
                "the late frame must be credited to the NEW window it actually falls into, not \
                 lost"
            );
        }

        // The identical call again, with the (still comfortably future)
        // deadline unchanged -- confirms this is a no-op, not a
        // double-advance.
        note_rx_frame_for_repeat_slots(&mut channel, &[0x55], 0, PROTOCOL_CAN);
        let slot = channel.repeat_slots.get(&1).unwrap();
        assert!(!slot.terminated);
        assert_eq!(
            slot.interval_deadline,
            Some(expected_deadline),
            "a frame arriving comfortably within the current window must not advance the grid \
             again"
        );
        assert!(slot.rx_seen_this_interval);
    }

    /// `Condition == 0` never silence-terminates, even across a stall
    /// spanning many whole intervals -- but the O(1) fast-forward must land
    /// on the exact correct next grid point, not overshoot or undershoot by
    /// one interval (an off-by-one in the `missed` computation).
    #[test]
    fn advance_repeat_slot_windows_condition_zero_multi_interval_stall_lands_on_correct_grid_point()
    {
        let now = std::time::Instant::now();
        let mut slot = test_repeat_slot(
            0,
            100,
            Some(now - std::time::Duration::from_millis(1000)), // 10 whole intervals elapsed
            false,
        );
        advance_repeat_slot_windows(&mut slot, now);
        let after = std::time::Instant::now();
        assert!(
            !slot.terminated,
            "condition == 0 never silence-terminates, regardless of how many silent intervals \
             elapsed"
        );
        let deadline = slot
            .interval_deadline
            .expect("the grid was already started (interval_deadline was Some on entry)");
        assert!(
            deadline > after,
            "the fast-forwarded grid point must land strictly ahead of the moment this check ran"
        );
        assert!(
            deadline < after + std::time::Duration::from_millis(100),
            "the fast-forward must land on the correct NEXT grid point, not overshoot by a whole \
             extra interval -- an off-by-one in the `missed` computation would push this past the \
             100ms bound"
        );
    }

    /// Latent bug 4 (found alongside the round-3 fix): a frame arriving
    /// before the slot's very first transmit (`interval_deadline` still
    /// `None`, the grid hasn't started) must credit no window -- the round-
    /// 1/round-2 patches' `Instant::now()`-at-creation sentinel wrongly
    /// credited this to "interval 1". Uses `condition == 1` with a MATCHING
    /// frame so that condition's own per-frame stop-trigger (which fires on
    /// a non-match) cannot fire either, isolating this assertion to the
    /// silence machinery alone.
    #[test]
    fn note_rx_frame_for_repeat_slots_credits_nothing_before_the_slots_first_transmit() {
        let mut channel = ChannelState::new(PROTOCOL_CAN, 500_000, 0);
        channel
            .repeat_slots
            .insert(1, test_repeat_slot(1, 100, None, false));

        note_rx_frame_for_repeat_slots(&mut channel, &[0x55], 0, PROTOCOL_CAN);

        let slot = channel.repeat_slots.get(&1).unwrap();
        assert!(
            !slot.rx_seen_this_interval,
            "a frame arriving before the grid has started (interval_deadline still None) must \
             credit no window"
        );
        assert!(
            !slot.terminated,
            "a matching frame under Condition == 1 never terminates via its own per-frame stop- \
             trigger, and the silence machinery must not fire either since the grid hasn't \
             started"
        );
    }

    // Round-4 regression (Codex review PR #60): `IOCTL_QUERY_REPEAT_MESSAGE`
    // must advance the slot's windows itself before deriving its status,
    // exactly like `note_rx_frame_for_repeat_slots`/`spawn_repeat_worker`
    // already do -- otherwise a QUERY landing before either of those two
    // call sites next notices an elapsed deadline reads the stale
    // `terminated == false` and reports a genuinely-silent slot as still
    // live. Deliberately bypasses `IOCTL_START_REPEAT_MESSAGE` (which would
    // spawn a real `spawn_repeat_worker` thread) -- the slot is inserted
    // directly into a real, connected channel's `repeat_slots` map with an
    // already-elapsed `interval_deadline`, so the ONLY code path that could
    // possibly advance it before this test's own `IOCTL_QUERY_REPEAT_
    // MESSAGE` call is the QUERY handler itself, making this fully
    // deterministic rather than racing a background thread.
    // Serialized like the other tests that touch the global mock state; as a plain #[test]
    // it raced mock_reset() from parallel tests on Windows CI.
    serial_test! {
    fn query_repeat_message_advances_windows_before_deriving_status() {
        let mut channel_id: u32 = 0;
        unsafe {
            PassThruConnect(MOCK_DEVICE_ID, PROTOCOL_CAN, 0, 500_000, &mut channel_id);
        }

        const MSG_ID: u32 = 4242;
        {
            let mut guard = state().lock().expect("mock state poisoned");
            let channel = guard
                .channels
                .get_mut(&channel_id)
                .expect("channel was just connected");
            channel.repeat_slots.insert(
                MSG_ID,
                test_repeat_slot(
                    1, // Condition == 1
                    20,
                    Some(std::time::Instant::now() - std::time::Duration::from_millis(50)),
                    false, // the window that just elapsed was silent
                ),
            );
        }

        let mut status: u32 = 0;
        let mut query_msg_id = MSG_ID;
        let query_rc = unsafe {
            PassThruIoctl(
                channel_id,
                IOCTL_QUERY_REPEAT_MESSAGE,
                &mut query_msg_id as *mut _ as *mut c_void,
                &mut status as *mut _ as *mut c_void,
            )
        };
        assert_eq!(query_rc, STATUS_NOERROR as c_long);
        assert_eq!(
            status, 0,
            "QUERY must itself close the already-elapsed, silent window and report status 0 -- \
             no worker thread exists for this manually-inserted slot to have done it instead"
        );

        // Retained, not gone -- a second QUERY still resolves the same
        // MsgId (clause 14.2.2.3), now correctly reporting the
        // already-advanced state.
        let mut status2: u32 = 0;
        let mut query_msg_id2 = MSG_ID;
        let query_rc2 = unsafe {
            PassThruIoctl(
                channel_id,
                IOCTL_QUERY_REPEAT_MESSAGE,
                &mut query_msg_id2 as *mut _ as *mut c_void,
                &mut status2 as *mut _ as *mut c_void,
            )
        };
        assert_eq!(query_rc2, STATUS_NOERROR as c_long);
        assert_eq!(status2, 0);
    }
    }

    #[test]
    fn short_frame_never_matches_repeat_slot_mask_pattern() {
        assert!(!repeat_mask_pattern_matches(
            &[0xFF, 0xFF],
            &[0x55, 0x00],
            &[0x55]
        ));
        assert!(repeat_mask_pattern_matches(&[0xFF], &[0x55], &[0x55, 0xAA]));
        assert!(!repeat_mask_pattern_matches(&[0xFF], &[0x55], &[0x54]));
        // Vacuous (empty mask/pattern) matches anything, including empty data.
        assert!(repeat_mask_pattern_matches(&[], &[], &[]));
    }

    /// Codex-review Finding 3 (PR #42): a mismatched-length mask/pattern must
    /// never silently "match" on just the shorter array's prefix -- the
    /// longer array's own tail bytes must still be compared (against an
    /// implied zero on the shorter side), never dropped.
    #[test]
    fn mismatched_length_mask_pattern_never_matches_on_a_truncated_prefix() {
        // pattern shorter than mask: with the old `.min()` comparison length,
        // only mask[0]/pattern[0] would be checked and this would incorrectly
        // report a match, silently ignoring mask[1] entirely.
        assert!(!repeat_mask_pattern_matches(
            &[0xFF, 0xFF],
            &[0x55],
            &[0x55, 0xAA]
        ));
        // mask shorter than pattern: with the old `.min()` comparison length,
        // only mask[0]/pattern[0] would be checked and this would incorrectly
        // report a match, silently ignoring pattern[1] entirely.
        assert!(!repeat_mask_pattern_matches(
            &[0xFF],
            &[0x55, 0xAA],
            &[0x55, 0xAA]
        ));
        // A data frame shorter than the LONGER of mask/pattern still never
        // matches, even when it is long enough to cover the shorter one.
        assert!(!repeat_mask_pattern_matches(
            &[0xFF],
            &[0x55, 0xAA],
            &[0x55]
        ));
    }

    /// Codex-review round 20 (PR #42): the round-14 `.max()` fix above was
    /// itself insufficient to reject every mismatched mask/pattern length --
    /// when `data` is long enough to cover `cmp_len`, the SHORTER array's
    /// missing tail bytes default to `0` via `.unwrap_or(0)` in the compare
    /// loop rather than being rejected, so a `data` byte that happens to be
    /// `0` after masking can spuriously "match" a position the shorter array
    /// never specified. Before the round-20 explicit length-equality check,
    /// this exact case returned `true`: `cmp_len = 2`, `data.len() = 2` (not
    /// `< cmp_len`, so no short-frame rejection); `i=0` compares
    /// `data[0] & 0xFF == 0xAA` (true); `i=1` compares
    /// `data[1] & 0xFF == pattern.get(1).unwrap_or(0) == 0`, and
    /// `data[1] == 0x00` so that also held, giving an incorrect overall
    /// match. The explicit length check now rejects this pair outright.
    #[test]
    fn mismatched_length_mask_pattern_is_rejected_even_when_data_is_long_enough() {
        assert!(!repeat_mask_pattern_matches(
            &[0xFF, 0xFF],
            &[0xAA],
            &[0xAA, 0x00]
        ));
    }
}
