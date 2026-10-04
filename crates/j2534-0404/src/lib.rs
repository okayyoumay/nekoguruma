#![allow(unsafe_code)]
//! Safe-ish Rust wrapper around the SAE J2534 v04.04 API.
//!
//! This crate loads a vendor J2534 shared library and exposes typed wrappers
//! for device, channel, message, filter, and common IOCTL operations.

use std::{
    ffi::CStr,
    os::raw::{c_char, c_void},
    path::Path,
};

use tracing::{debug, info, trace};

use j2534_0404_sys::bindings::{
    self,
    IOCTL_ADD_TO_FUNCT_MSG_LOOKUP_TABLE,
    // SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8): the
    // ChannelID-scoped `become_master` below issues this blocking (~2s
    // native-side worst case) bus-mastership handshake primitive.
    IOCTL_BECOME_MASTER,
    IOCTL_DELETE_FROM_FUNCT_MSG_LOOKUP_TABLE,
    IOCTL_GET_CONFIG,
    // SAE J2534-2 clause 18 Device Configuration (ADR-176/Phase 14): reads/
    // writes the device's non-volatile parameter store; `get_device_config`/
    // `set_device_config` below issue these.
    IOCTL_GET_DEVICE_CONFIG,
    IOCTL_GET_DEVICE_INFO,
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): the
    // ChannelID-scoped `get_ndis_adapter_info` below issues this
    // channel-scoped, no-input, struct-output IOCTL.
    IOCTL_GET_NDIS_ADAPTER_INFO,
    IOCTL_GET_PROTOCOL_INFO,
    // SAE J2534-2 clause 14 Repeat Messaging (ADR-165/Phase 12): the three
    // ChannelID-scoped IOCTLs `start_repeat_message`/`query_repeat_message`/
    // `stop_repeat_message` below issue.
    // SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5): the
    // ChannelID-scoped `protect_j1939_addr`/`cancel_j1939_addr_protect`
    // below issue this non-blocking claim/defend-or-cancel primitive
    // (clause 16.3.3.2).
    IOCTL_PROTECT_J1939_ADDR,
    IOCTL_QUERY_REPEAT_MESSAGE,
    // SAE J2534-2 clause 23 J1962 Pin Voltage Read (Phase 13): reads a
    // single J1962 pin's voltage (millivolts) via PassThruIoctl.
    IOCTL_READ_J1962PIN_VOLTAGE,
    IOCTL_READ_PROG_VOLTAGE,
    IOCTL_READ_VBATT,
    // SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a): the
    // ChannelID-scoped `request_connection`/`teardown_connection` below
    // issue these, invoked internally from `PDU_COPT_STARTCOMM`/
    // `PDU_COPT_STOPCOMM` -- never a client-visible `PDU_IOCTL_*` (ADR-188
    // Decision item 3).
    IOCTL_REQUEST_CONNECTION,
    IOCTL_SET_CONFIG,
    IOCTL_SET_DEVICE_CONFIG,
    // SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8): the
    // ChannelID-scoped `set_poll_response` below issues this thin
    // passthrough primitive.
    IOCTL_SET_POLL_RESPONSE,
    IOCTL_START_REPEAT_MESSAGE,
    IOCTL_STOP_REPEAT_MESSAGE,
    // SAE J2534-2 clause 9 Single Wire CAN (ADR-164/Phase 4): the two
    // ChannelID-scoped IOCTLs `sw_can_hs`/`sw_can_ns` below issue.
    IOCTL_SW_CAN_HS,
    IOCTL_SW_CAN_NS,
    IOCTL_TEARDOWN_CONNECTION,
};
// The sys facade converts `unsigned long` width at the FFI boundary.
use j2534_0404_sys::J2534Api0404 as SysJ2534Api0404;

mod discovery;
mod error;
mod ioctl;
pub mod iso15765;
mod message;

pub use discovery::{DiscoveryParam, DiscoveryResult};
pub use error::{Error, StatusCode, VersionInfo};
pub use ioctl::IoCtlCommand;
pub use message::{BorrowedPassThruMessage, NdisAdapterInfo, PassThruMessage, RepeatMsgSetup};

pub use j2534_0404_sys::bindings::{
    BLOCK_FILTER,
    // SAE J2534-2 clause 10.3.3.2 Analog Inputs (ADR-177/Phase 15; revised by
    // ADR-178, then ADR-216): `CONFIG_SAMPLE_RATE` was the one native
    // SET_CONFIG/GET_CONFIG acquisition parameter ADR-177/178 exposed to
    // D-PDU clients (as the project-minted `CP_AnalogSampleRate` ComParam,
    // staged via `SetComParam`, applied at `ConnectComLogicalLink` time
    // immediately after `PassThruConnect` succeeds). ADR-216 exposes the
    // remaining seven: `CONFIG_ACTIVE_CHANNELS`/`_SAMPLES_PER_READING`/
    // `_READINGS_PER_MSG`/`_AVERAGING_METHOD` (writable, `SetComParam`-staged
    // like `CONFIG_SAMPLE_RATE`) and `_SAMPLE_RESOLUTION`/`_INPUT_RANGE_LOW`/
    // `_INPUT_RANGE_HIGH` (read-only, populated only by a connect-time
    // `GET_CONFIG` readback).
    CONFIG_ACTIVE_CHANNELS,
    CONFIG_AVERAGING_METHOD,
    CONFIG_BIT_SAMPLE_POINT as BIT_SAMPLE_POINT, // 0x17 – CAN bit sample point (%)
    CONFIG_BS_TX as BS_TX,                       // 0x22 – ISO15765 BS from tester side
    // SAE J2534-2 clause 8 (Mixed Format Frames on a CAN Network, Phase 3
    // Stage 3c/ADR-160): enables an ISO15765-family channel to also deliver
    // unformatted CAN frames, distinguished per-frame by native ProtocolID.
    // No native value constants exist for the OFF/ON/ALL_FRAMES payload —
    // see `j2534-0404-service`'s local `CAN_MIXED_FORMAT_*` constants.
    CONFIG_CAN_MIXED_FORMAT,       // 0x8000
    CONFIG_DATA_BITS as DATA_BITS, // 0x20
    // Config parameters – ordered by parameter ID value
    CONFIG_DATA_RATE as DATA_RATE, // 0x01
    // SAE J2534-2 clause 21 CAN FD (ADR-158/Phase 3a): must be SET_CONFIG'd
    // before `CONFIG_J1962_PINS` on an FD_CAN_PS channel (clause 21.3.2.5.1),
    // else the native connect fails ERR_FAILED.
    CONFIG_FD_CAN_DATA_PHASE_RATE,
    // SAE J2534-2 clause 22 ISO15765-on-CAN-FD (ADR-159/Phase 3b): the
    // native `SET_CONFIG` targets `CP_CANFDTxMaxDataLength`/`CP_Cr`/
    // `CP_CanFillerByte` translate to on an FD_ISO15765_PS link
    // (`comparam_id::to_j2534_config_id`). `CONFIG_HS_CAN_TERMINATION` is
    // deferred (ADR-159 Decision 6), not re-exported here.
    CONFIG_FD_ISO15765_TX_DATA_LENGTH,
    CONFIG_FIVE_BAUD_MOD as FIVE_BAUD_MOD, // 0x21
    CONFIG_INPUT_RANGE_HIGH,
    CONFIG_INPUT_RANGE_LOW,
    CONFIG_ISO15765_BS as ISO15765_BS, // 0x1E
    CONFIG_ISO15765_PAD_VALUE,
    CONFIG_ISO15765_STMIN as ISO15765_STMIN,     // 0x1F
    CONFIG_ISO15765_WFT_MAX as ISO15765_WFT_MAX, // 0x25
    // SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5): the five
    // native SET_CONFIG targets `CP_Cr`/`CP_T5Max`/`CP_T4Max`/`CP_Bs`/`CP_Cs`
    // translate to (non-ordinally -- see ADR-179's Context section) on a
    // J1939_PS link (`comparam_id::to_j2534_config_id`). `CONFIG_J1939_PINS`
    // (the SAE J1939-13 connector) stays deferred (ADR-179 Decision 2), not
    // re-exported here.
    CONFIG_J1939_BRDCST_MIN_DELAY,
    CONFIG_J1939_T1,
    CONFIG_J1939_T2,
    CONFIG_J1939_T3,
    CONFIG_J1939_T4,
    // SAE J2534-2 clause 6 Pin Selection (ADR-156 Phase 2a): the
    // `CONFIG_J1962_PINS` SET_CONFIG parameter and the seven clause 6.3.1
    // Table 1 `_PS` hardware protocol ids this phase supports.
    CONFIG_J1962_PINS,
    CONFIG_LOOPBACK as LOOPBACK, // 0x03
    // SAE J2534-2 clause 22 ISO15765-on-CAN-FD (ADR-159/Phase 3b): the
    // native target `CP_Cr` translates to on an FD_ISO15765_PS link.
    CONFIG_N_CR_MAX,
    CONFIG_NETWORK_LINE as NETWORK_LINE, // 0x05
    // SAE J2534-2 clause 18 Device Configuration (ADR-176/Phase 14): the ten
    // DeviceID-scoped GET/SET_DEVICE_CONFIG parameter ids `get_device_config`/
    // `set_device_config` accept -- re-exported (Codex review, PR #65) so a
    // downstream crate depending only on `j2534-0404` can name them without
    // adding `j2534-0404-sys` as a separate dependency, matching every other
    // config-parameter constant in this list.
    CONFIG_NODE_ADDRESS as NODE_ADDRESS, // 0x04
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
    CONFIG_P1_MAX as P1_MAX, // 0x07 – KWP inter-byte time (ECU response)
    CONFIG_P1_MIN as P1_MIN, // 0x06 – KWP inter-byte time (tester)
    CONFIG_P2_MAX as P2_MAX, // 0x09
    CONFIG_P2_MIN as P2_MIN, // 0x08 – KWP gap: last tester byte → first ECU byte
    CONFIG_P3_MAX as P3_MAX, // 0x0B
    CONFIG_P3_MIN as P3_MIN, // 0x0A – KWP gap: end of ECU response → next tester request
    CONFIG_P4_MAX as P4_MAX, // 0x0D
    CONFIG_P4_MIN as P4_MIN, // 0x0C – KWP inter-byte time for tester
    CONFIG_PARITY as PARITY, // 0x16 – 0=none, 1=odd, 2=even
    CONFIG_READINGS_PER_MSG,
    CONFIG_SAMPLE_RATE,
    CONFIG_SAMPLE_RESOLUTION,
    CONFIG_SAMPLES_PER_READING,
    CONFIG_STMIN_TX as STMIN_TX, // 0x23 – ISO15765 ST_MIN from tester side
    // SAE J2534-2 clause 9 Single Wire CAN (ADR-164/Phase 4): the three
    // native SET_CONFIG/GET_CONFIG targets `CP_ChangeSpeedRate`/`Ctrl`/
    // `ResCtrl` translate to on an SW_CAN_PS/SW_ISO15765_PS link
    // (`comparam_id::to_j2534_config_id`).
    CONFIG_SW_CAN_HS_DATA_RATE,
    CONFIG_SW_CAN_RES_SWITCH,
    CONFIG_SW_CAN_SPEEDCHANGE_ENABLE,
    CONFIG_SYNC_JUMP_WIDTH as SYNC_JUMP_WIDTH, // 0x18 – CAN sync jump width (%)
    CONFIG_T1_MAX as T1_MAX,                   // 0x1A – SCI timing
    CONFIG_T2_MAX as T2_MAX,                   // 0x1B
    CONFIG_T3_MAX as T3_MAX,                   // 0x24 – SCI timing
    CONFIG_T4_MAX as T4_MAX,                   // 0x1C
    CONFIG_T5_MAX as T5_MAX,                   // 0x1D
    CONFIG_TIDLE as TIDLE,                     // 0x13 – ISO9141 bus-idle time
    CONFIG_TINIL as TINIL,                     // 0x14 – ISO9141 bus-nil time
    // SAE J2534-2 clause 19.3.1/Table 77 TP2.0 passive connections
    // (ADR-190/Phase 7 Stage 7b): the two passive-listener enablers.
    // `j2534-0404-service`'s minted `PARAM_TP20_PASSIVE_IDENTIFIER`/
    // `PARAM_TP20_PASSIVE_RX_ID` ComParams are deliberately NOT routed
    // through the generic `comparam_id::to_j2534_config_id` translation
    // pipeline (ADR-190 section 1) -- `events_tp20_connection.rs`'s
    // passive arm/disarm applies these two directly via `set_config`.
    CONFIG_TP2_0_IDENTIFER,
    CONFIG_TP2_0_RXIDPASSIVE,
    // SAE J2534-2 clause 19.3.1/Table 77 TP2.0 broadcast interval (ADR-192/
    // Phase 7 Stage 7c): the spacing between the five frames that make up
    // one broadcast burst. Unlike the two passive-listener enablers just
    // above, this one *is* routed through the generic `comparam_id::
    // to_j2534_config_id` translation pipeline, since it is a genuine
    // physical-layer, connect-time-resolvable setting rather than a
    // lifecycle-gated one.
    CONFIG_TP2_0_T_BR_INT,
    CONFIG_TWUP as TWUP, // 0x15 – ISO9141 wake-up time
    // SAE J2534-2 clause 12.3.4.1 UART Echo Byte (ADR-216): the ten
    // remaining native SET_CONFIG/GET_CONFIG timing parameters, exposed as
    // the project-minted `PARAM_UEB_T0_MIN`-`PARAM_UEB_T9_MIN` ComParams
    // (`j2534-0404-service`'s `comparam_id::to_j2534_config_id`, keyed on
    // the UART Echo Byte family-wide protocol id predicate). Native-verbatim
    // whole milliseconds, not the 1 us ISO 22900-2 `CP_*` timing convention.
    CONFIG_UEB_T0_MIN,
    CONFIG_UEB_T1_MAX,
    CONFIG_UEB_T2_MAX,
    CONFIG_UEB_T3_MAX,
    CONFIG_UEB_T4_MIN,
    CONFIG_UEB_T5_MAX,
    CONFIG_UEB_T6_MAX,
    CONFIG_UEB_T7_MAX,
    CONFIG_UEB_T7_MIN,
    CONFIG_UEB_T9_MIN,
    CONFIG_W0 as W0, // 0x19 – ISO9141 W0 timing
    CONFIG_W1 as W1, // 0x0E – ISO9141 timing
    CONFIG_W2 as W2, // 0x0F
    CONFIG_W3 as W3, // 0x10
    CONFIG_W4 as W4, // 0x11
    CONFIG_W5 as W5, // 0x12
    // Connect flags
    CONNECT_FLAG_CAN_29BIT_ID as CAN_29BIT_ID,
    CONNECT_FLAG_CAN_ID_BOTH as CAN_ID_BOTH,
    CONNECT_FLAG_ISO9141_K_LINE_ONLY as ISO9141_K_LINE_ONLY,
    CONNECT_FLAG_ISO9141_NO_CHECKSUM as ISO9141_NO_CHECKSUM,
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): the two
    // mutually-exclusive pin-option connect flags `CP_NdisPinOption`
    // (`j2534-0404-service`'s `comparam_id`/`rpc_link.rs`) resolves into.
    CONNECT_FLAG_NDIS_PINS_OPTION1,
    CONNECT_FLAG_NDIS_PINS_OPTION2,
    // ADR-185 Stage 1 Discovery-cache connect-time enforcement: the six
    // `DEVICE_INFO_*_SUPPORTED` flags `resources::connect_discovery_check`
    // consults for the SWCAN, FT-CAN, UART Echo Byte, Honda DIAG-H, J1708,
    // and Analog Inputs `DeviceFlag` connect-path rows.
    DEVICE_INFO_ANALOG_IN_SUPPORTED,
    // SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 4/Phase
    // 2b): the `DEVICE_INFO_<PROTOCOL>_SUPPORTED` GET_DEVICE_INFO
    // parameters whose packed value's QQ byte (bits 16-23, Codex review
    // PR #29) carries the available _CHx count for that family
    // (`resources::chx_device_info_supported_parameter`).
    DEVICE_INFO_CAN_SUPPORTED,
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
    // `resources::connect_discovery_check`'s ninth Stage-1-wired
    // `DeviceFlag` connect-path row (the original six families below plus
    // TP2.0 and GM UART, each added in a later phase), the same ADR-185
    // Stage-1 flat-bit pattern the six families above already use.
    DEVICE_INFO_ETHERNET_NDIS_SUPPORTED,
    // SAE J2534-2 clause 21/22 CAN FD / ISO15765-on-CAN-FD (ADR-213/Round 3
    // of the `_CHx` series): the `DEVICE_INFO_<PROTOCOL>_SUPPORTED`
    // GET_DEVICE_INFO parameters whose packed value's QQ byte carries the
    // available `_CHx` count for each family, mirroring ADR-211's/ADR-212's
    // own FT-CAN/SW-CAN pattern (`resources::chx_device_info_supported_
    // parameter`, `resources::connect_discovery_check`).
    DEVICE_INFO_FD_CAN_SUPPORTED,
    DEVICE_INFO_FD_ISO15765_SUPPORTED,
    DEVICE_INFO_FT_CAN_SUPPORTED,
    DEVICE_INFO_FT_ISO15765_SUPPORTED,
    // SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8):
    // `resources::connect_discovery_check`'s GM_UART_PS/GM_UART_CHx
    // `DeviceFlag` connect-path row, the same ADR-185 Stage-1 flat-bit
    // pattern the families above already use. `DEVICE_INFO_GM_UART_
    // SIMULTANEOUS`/`_PS_J1962` stay deferred, matching every ADR-185
    // Stage-1 family's own `_SIMULTANEOUS` residual.
    DEVICE_INFO_GM_UART_SUPPORTED,
    DEVICE_INFO_HONDA_DIAGH_SUPPORTED,
    DEVICE_INFO_ISO9141_SUPPORTED,
    DEVICE_INFO_ISO14230_SUPPORTED,
    DEVICE_INFO_ISO15765_SUPPORTED,
    DEVICE_INFO_J1708_SUPPORTED,
    DEVICE_INFO_J1850PWM_SUPPORTED,
    DEVICE_INFO_J1850VPW_SUPPORTED,
    // SAE J2534-2 clause 16 SAE J1939 (mechanical extension of the
    // `chx_device_info_supported_parameter` fix that closed this gap for
    // UART Echo Byte, Honda DIAG-H, J1708, and TP2.0; see
    // `j2534-0404-service/docs/implementation-notes.md`'s Additional Channels
    // Prioritized Backlog entry): `resources::chx_device_info_supported_
    // parameter`'s `_CHx` channel-count-cap flag for this family, mirroring
    // ADR-211's/ADR-212's own FT-CAN/SW-CAN pattern. This family has no
    // `connect_discovery_check` row of its own (unlike the ADR-185 Stage 1
    // families above) -- it is used solely for the Additional Channels
    // capacity check.
    DEVICE_INFO_J1939_SUPPORTED,
    DEVICE_INFO_J2610_SUPPORTED,
    // SAE J2534-2 clause 18 Device Configuration (ADR-176/Phase 14):
    // `ioctl_get_device_config`/`ioctl_set_device_config`'s ADR-185 Stage 2
    // `DeviceCapacity` Discovery-cache precheck.
    DEVICE_INFO_MAX_NON_VOLATILE_STORAGE,
    // SAE J2534-2 clause 23/25.3.2.2 (ADR-185 Stage 2, spec-accuracy
    // correction): `ioctl_read_j1962_pin_voltage`'s `DeviceFlag`
    // Discovery-cache precheck -- per-pin, not flat (Table 111, clause
    // 25.3.2.2, matches the two per-pin rows just below).
    DEVICE_INFO_READ_J1962PIN_VOLTAGE_SUPPORTED,
    // SAE J2534-2 clause 15/25.3.2.2 (ADR-185 Stage 2): `ioctl_set_prog_voltage`'s
    // pin-9 Short-to-Ground `DeviceFlag` Discovery-cache precheck.
    DEVICE_INFO_SHORT_TO_GND_J1962,
    DEVICE_INFO_SW_CAN_SUPPORTED,
    DEVICE_INFO_SW_ISO15765_SUPPORTED,
    // SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a):
    // `resources::connect_discovery_check`'s TP2_0_PS `DeviceFlag` connect-
    // path row, the same ADR-185 Stage-1 flat-bit pattern the six families
    // above already use. `DEVICE_INFO_TP2_0_SIMULTANEOUS`/`_PS_J1962` stay
    // deferred, matching every ADR-185 Stage-1 family's own `_SIMULTANEOUS`
    // residual (ADR-188 §7).
    DEVICE_INFO_TP2_0_SUPPORTED,
    DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED,
    // Error codes
    ERR_ADDRESS_NOT_CLAIMED,
    ERR_BUFFER_EMPTY,
    ERR_BUFFER_FULL,
    ERR_BUFFER_OVERFLOW,
    ERR_CHANNEL_IN_USE,
    ERR_DEVICE_IN_USE,
    ERR_DEVICE_NOT_CONNECTED,
    ERR_EXCEEDED_LIMIT,
    ERR_FAILED,
    ERR_INVALID_BAUDRATE,
    ERR_INVALID_CHANNEL_ID,
    ERR_INVALID_DEVICE_ID,
    ERR_INVALID_FILTER_ID,
    ERR_INVALID_FLAGS,
    ERR_INVALID_IOCTL_ID,
    ERR_INVALID_IOCTL_PARAM_ID,
    ERR_INVALID_IOCTL_VALUE,
    ERR_INVALID_MSG,
    ERR_INVALID_MSG_ID,
    ERR_INVALID_PROTOCOL_ID,
    ERR_INVALID_TIME_INTERVAL,
    ERR_MSG_PROTOCOL_ID,
    ERR_NO_CONNECTION_ESTABLISHED,
    ERR_NO_FLOW_CONTROL,
    ERR_NOT_SUPPORTED,
    ERR_NOT_UNIQUE,
    ERR_NULL_PARAMETER,
    ERR_PIN_IN_USE,
    ERR_PIN_INVALID,
    ERR_RESOURCE_IN_USE,
    ERR_TIMEOUT,
    ERR_VOLTAGE_IN_USE,
    FLOW_CONTROL_FILTER,
    IOCTL_CLEAR_FUNCT_MSG_LOOKUP_TABLE as CLEAR_FUNCT_MSG_LOOKUP_TABLE,
    IOCTL_CLEAR_MSG_FILTERS as CLEAR_MSG_FILTERS,
    IOCTL_CLEAR_PERIODIC_MSGS as CLEAR_PERIODIC_MSGS,
    IOCTL_CLEAR_RX_BUFFER as CLEAR_RX_BUFFER,
    IOCTL_CLEAR_TX_BUFFER as CLEAR_TX_BUFFER,
    IOCTL_FAST_INIT as FAST_INIT,
    // IOCTL IDs
    IOCTL_FIVE_BAUD_INIT as FIVE_BAUD_INIT,
    // Filter types (used with PassThruStartMsgFilter)
    PASS_FILTER,
    // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): the 32
    // independent, read-only analog-acquisition protocol ids (Phase 0
    // already added these constants and regenerated bindings -- no bindgen
    // work this phase). Each connects independently; `resources.rs`'s 32
    // new rows each override onto its own id here via `hw_protocol_override`.
    PROTOCOL_ANALOG_IN_1,
    PROTOCOL_ANALOG_IN_2,
    PROTOCOL_ANALOG_IN_3,
    PROTOCOL_ANALOG_IN_4,
    PROTOCOL_ANALOG_IN_5,
    PROTOCOL_ANALOG_IN_6,
    PROTOCOL_ANALOG_IN_7,
    PROTOCOL_ANALOG_IN_8,
    PROTOCOL_ANALOG_IN_9,
    PROTOCOL_ANALOG_IN_10,
    PROTOCOL_ANALOG_IN_11,
    PROTOCOL_ANALOG_IN_12,
    PROTOCOL_ANALOG_IN_13,
    PROTOCOL_ANALOG_IN_14,
    PROTOCOL_ANALOG_IN_15,
    PROTOCOL_ANALOG_IN_16,
    PROTOCOL_ANALOG_IN_17,
    PROTOCOL_ANALOG_IN_18,
    PROTOCOL_ANALOG_IN_19,
    PROTOCOL_ANALOG_IN_20,
    PROTOCOL_ANALOG_IN_21,
    PROTOCOL_ANALOG_IN_22,
    PROTOCOL_ANALOG_IN_23,
    PROTOCOL_ANALOG_IN_24,
    PROTOCOL_ANALOG_IN_25,
    PROTOCOL_ANALOG_IN_26,
    PROTOCOL_ANALOG_IN_27,
    PROTOCOL_ANALOG_IN_28,
    PROTOCOL_ANALOG_IN_29,
    PROTOCOL_ANALOG_IN_30,
    PROTOCOL_ANALOG_IN_31,
    PROTOCOL_ANALOG_IN_32,
    // Protocol IDs
    PROTOCOL_CAN as CAN,
    PROTOCOL_CAN_CH1,
    PROTOCOL_CAN_PS,
    // SAE J2534-2 clause 12 UART Echo Byte Protocol (ADR-170/Phase 9), clause
    // 7 Additional Channels (ADR-207): `PROTOCOL_ECHO_BYTE_CH1`/`_CH128` (the
    // block's own base id and upper bound -- `resources::
    // is_uart_echo_byte_family_protocol_id`'s own range check needs both,
    // mirroring `is_gm_uart_protocol_id`'s identical shape, even though
    // `resources::is_uart_echo_byte_protocol_id` itself stays an exact
    // `_PS`-only match for the one call site -- `names.rs`'s own arm gate --
    // that must never widen) is re-exported for `resources::chx_block_base`/
    // `chx_base_protocol_id`/`is_uart_echo_byte_family_protocol_id`. The
    // vendor header names this family's `_CHx` block without the "UART_"
    // prefix its `_PS`/predicate names use (`PROTOCOL_UART_ECHO_BYTE_PS`/
    // `resources::is_uart_echo_byte_protocol_id`, below) -- a naming
    // inconsistency in the header itself, not a mistake here (ADR-207
    // Context).
    PROTOCOL_ECHO_BYTE_CH1,
    PROTOCOL_ECHO_BYTE_CH128,
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): the one
    // native protocol id this phase supports -- like clause 12 UART Echo
    // Byte/clause 13 Honda DIAG-H/clause 17 SAE J1708/clause 16 SAE J1939/
    // clause 19 TP2.0/clause 11 GM UART, clause 24 defines no unqualified
    // base id of its own beyond this raw value, and it IS the
    // service-level `ChannelProtocol` identity directly (no
    // `hw_protocol_override`/CAN-family relationship at all).
    PROTOCOL_ETHERNET_NDIS,
    // SAE J2534-2 clause 21 CAN FD (ADR-158/Phase 3a): the one `_PS`-only
    // CAN FD protocol id this phase supports -- CAN FD has no unqualified
    // base id of its own (Table 89), unlike every `_PS` family Phase 2
    // covers. Clause 7 Additional Channels (ADR-213/Round 3) is now also in
    // scope for this family: `PROTOCOL_FD_CAN_CH1`/`_CH128` (the block's own
    // bounds) are re-exported for `resources::chx_block_base`/
    // `chx_base_protocol_id`/the widened `resources::is_fd_protocol_id`. The
    // vendor header names this family's `_CHx` block consistently with its
    // `_PS` id -- no naming inconsistency here.
    PROTOCOL_FD_CAN_CH1,
    PROTOCOL_FD_CAN_CH128,
    PROTOCOL_FD_CAN_PS,
    // SAE J2534-2 clause 7 (Additional Channels, ADR-213/Round 3): the
    // lower bound of this family's own `_CHx` block, its `_CH128` sibling
    // already re-exported above for the full-region membership test.
    PROTOCOL_FD_ISO15765_CH1,
    // SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 3/Phase
    // 2b): the full `_CHx` region's upper bound (`FD_ISO15765_CH128`, the
    // last of all 19 native `_CHx` blocks, clause-24) alongside
    // `PROTOCOL_CAN_CH1` above (the region's lower bound) for
    // `resources::is_chx_protocol_id`'s full-region membership test.
    PROTOCOL_FD_ISO15765_CH128,
    // SAE J2534-2 clause 22 ISO15765-on-CAN-FD (ADR-159/Phase 3b): the one
    // `_PS`-only ISO15765-on-CAN-FD protocol id this phase supports -- like
    // clause 21's FD_CAN_PS, it has no unqualified base id of its own
    // (Table 96). Clause 7 Additional Channels (ADR-213/Round 3) is now also
    // in scope for this family, via `PROTOCOL_FD_ISO15765_CH1`/`_CH128`
    // above.
    PROTOCOL_FD_ISO15765_PS,
    // SAE J2534-2 clause 20 Fault-Tolerant CAN (ISO 11898-3, ADR-168/Phase
    // 6): the one `_PS`-only Fault-Tolerant CAN protocol id this phase
    // supports -- like clause 9 SWCAN, it has no unqualified base id of its
    // own. Clause 7 Additional Channels (ADR-211) is now also in scope for
    // this family: `PROTOCOL_FT_CAN_CH1`/`_CH128` (the block's own bounds)
    // are re-exported for `resources::chx_block_base`/`chx_base_protocol_id`/
    // `resources::is_ft_family_protocol_id`. Like clause 13 Honda DIAG-H/
    // clause 17 SAE J1708, the vendor header names this family's `_CHx`
    // block consistently with its `_PS` id -- no naming inconsistency here.
    PROTOCOL_FT_CAN_CH1,
    PROTOCOL_FT_CAN_CH128,
    PROTOCOL_FT_CAN_PS,
    // SAE J2534-2 clause 20 Fault-Tolerant CAN (ISO 11898-3, ADR-168/Phase
    // 6): the one `_PS`-only ISO15765-on-Fault-Tolerant-CAN protocol id this
    // phase supports. Clause 7 Additional Channels (ADR-211) is now also in
    // scope for this family: `PROTOCOL_FT_ISO15765_CH1`/`_CH128` (the
    // block's own bounds) are re-exported for
    // `resources::chx_block_base`/`chx_base_protocol_id`/
    // `resources::is_ft_family_protocol_id`. No naming inconsistency here
    // either.
    PROTOCOL_FT_ISO15765_CH1,
    PROTOCOL_FT_ISO15765_CH128,
    PROTOCOL_FT_ISO15765_PS,
    // SAE J2534-2 clause 11 GM UART Protocol (SAE J2740, ADR-189/Phase 8):
    // the one `_PS`-only protocol id this phase supports -- like clause 12
    // UART Echo Byte/clause 13 Honda DIAG-H, clause 11 defines no
    // unqualified base id of its own, and this `_PS` id IS the
    // service-level `ChannelProtocol` identity directly (no
    // `hw_protocol_override`/CAN-family relationship at all --
    // `resources::is_gm_uart_protocol_id`). Unlike those two, though, GM
    // UART also participates in the clause 7 Additional Channels `_CHx`
    // arithmetic mapping (ADR-189 Decision 2) -- `PROTOCOL_GM_UART_CH1`/
    // `_CH128` (the block's own bounds) are re-exported for
    // `resources::chx_block_base`/`chx_base_protocol_id`/
    // `resources::is_gm_uart_protocol_id`.
    PROTOCOL_GM_UART_CH1,
    PROTOCOL_GM_UART_CH128,
    PROTOCOL_GM_UART_PS,
    // SAE J2534-2 clause 13 Honda DIAG-H Protocol (ADR-174/Phase 10): the
    // one `_PS`-only protocol id this phase supports -- like clause 12 UART
    // Echo Byte just above, clause 13 defines no unqualified base id of its
    // own, and this `_PS` id IS the service-level `ChannelProtocol` identity
    // directly (no `hw_protocol_override`/CAN-family relationship at all --
    // `resources::is_honda_diagh_protocol_id`). Clause 7 Additional Channels
    // (ADR-208) is now also in scope for this family:
    // `PROTOCOL_HONDA_DIAGH_CH1`/`_CH128` (the block's own bounds) are
    // re-exported for `resources::chx_block_base`/`chx_base_protocol_id`/
    // `resources::is_honda_diagh_family_protocol_id`. Unlike clause 12 UART
    // Echo Byte, the vendor header names this family's `_CHx` block
    // consistently with its `_PS` id -- no naming inconsistency here.
    PROTOCOL_HONDA_DIAGH_CH1,
    PROTOCOL_HONDA_DIAGH_CH128,
    PROTOCOL_HONDA_DIAGH_PS,
    // SAE J2534-2 clause 14/25.3.2.3 Repeat Messaging (ADR-185 Stage 2):
    // `ioctl_start_repeat_message`'s `ProtocolCapacity` Discovery-cache
    // precheck against this physical channel's live repeat-message slot
    // count (ADR-165 Decision 4's shared-slot-budget design).
    PROTOCOL_INFO_MAX_REPEAT_MESSAGING,
    PROTOCOL_ISO9141 as ISO9141,
    PROTOCOL_ISO9141_CH1,
    PROTOCOL_ISO9141_PS,
    PROTOCOL_ISO14230 as ISO14230,
    PROTOCOL_ISO14230_CH1,
    PROTOCOL_ISO14230_PS,
    PROTOCOL_ISO15765 as ISO15765,
    PROTOCOL_ISO15765_CH1,
    PROTOCOL_ISO15765_PS,
    // SAE J2534-2 clause 17 SAE J1708 Protocol (ADR-175/Phase 11): the one
    // `_PS`-only protocol id this phase supports -- like clause 12 UART Echo
    // Byte and clause 13 Honda DIAG-H, clause 17 defines no unqualified base
    // id of its own, and this `_PS` id IS the service-level `ChannelProtocol`
    // identity directly (no `hw_protocol_override`/CAN-family relationship at
    // all -- `resources::is_j1708_protocol_id`). Clause 7 Additional Channels
    // (ADR-209) is now also in scope for this family:
    // `PROTOCOL_J1708_CH1`/`_CH128` (the block's own bounds) are re-exported
    // for `resources::chx_block_base`/`chx_base_protocol_id`/
    // `resources::is_j1708_family_protocol_id`. Like clause 13 Honda DIAG-H,
    // the vendor header names this family's `_CHx` block consistently with
    // its `_PS` id -- no naming inconsistency here.
    PROTOCOL_J1708_CH1,
    PROTOCOL_J1708_CH128,
    PROTOCOL_J1708_PS,
    PROTOCOL_J1850PWM as J1850PWM,
    PROTOCOL_J1850PWM_CH1,
    PROTOCOL_J1850PWM_PS,
    PROTOCOL_J1850VPW as J1850VPW,
    PROTOCOL_J1850VPW_CH1,
    PROTOCOL_J1850VPW_PS,
    // SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5): the one
    // `_PS`-only protocol id this phase supports -- like clause 12 UART Echo
    // Byte, clause 13 Honda DIAG-H, and clause 17 SAE J1708, clause 16
    // defines no unqualified base id of its own, and this `_PS` id IS the
    // service-level `ChannelProtocol` identity directly (no
    // `hw_protocol_override`/CAN-family relationship at all --
    // `resources::is_j1939_protocol_id`). `PROTOCOL_J1939_CH1`/`_CH128` (the
    // clause-16 `_CHx` region's endpoints -- Additional Channels themselves
    // stay deferred this phase, ADR-179 Decision 1) are re-exported
    // alongside it purely so `is_j1939_protocol_id`'s own `_CH1..128` range
    // check can be expressed here without a direct `j2534-0404-sys`
    // dependency (this crate depends on that crate only as a
    // dev-dependency).
    PROTOCOL_J1939_CH1,
    PROTOCOL_J1939_CH128,
    PROTOCOL_J1939_PS,
    PROTOCOL_J2610_CH1,
    PROTOCOL_J2610_PS,
    PROTOCOL_SCI_A_ENGINE as SCI_A_ENGINE,
    PROTOCOL_SCI_A_TRANS as SCI_A_TRANS,
    PROTOCOL_SCI_B_ENGINE as SCI_B_ENGINE,
    PROTOCOL_SCI_B_TRANS as SCI_B_TRANS,
    // SAE J2534-2 clause 9 Single Wire CAN (ADR-164/Phase 4): the two
    // `_PS`-only SWCAN protocol ids -- like clause 21/22 CAN FD, clause 9
    // defines no unqualified base id of its own, so a resource-table row's
    // `hw_protocol_override` stores one of these two ids directly
    // (`resources::is_sw_protocol_id`). Clause 7 Additional Channels
    // (ADR-212/Round 2) is now also in scope for this family:
    // `PROTOCOL_SW_CAN_CAN_CH1`/`_CH128` and `PROTOCOL_SW_CAN_ISO15765_CH1`/
    // `_CH128` (each block's own bounds) are re-exported for
    // `resources::chx_block_base`/`chx_base_protocol_id`/
    // `resources::is_sw_family_protocol_id`. Unlike clause 13 Honda DIAG-H/
    // clause 17 SAE J1708, the vendor header names BOTH of this family's
    // `_CHx` blocks with a `SW_CAN_` prefix regardless of which `_PS` id they
    // extend -- a confirmed naming inconsistency (ADR-212 Context item 2),
    // unlike clause 20 Fault-Tolerant CAN just above.
    PROTOCOL_SW_CAN_CAN_CH1,
    PROTOCOL_SW_CAN_CAN_CH128,
    PROTOCOL_SW_CAN_ISO15765_CH1,
    PROTOCOL_SW_CAN_ISO15765_CH128,
    PROTOCOL_SW_CAN_PS,
    PROTOCOL_SW_ISO15765_PS,
    // SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a): the
    // one `_PS`-only protocol id this stage supports -- like clause 12/13/17
    // UART Echo Byte/Honda DIAG-H/SAE J1708 and clause 16 SAE J1939, clause
    // 19 defines no unqualified base id of its own, and this `_PS` id IS the
    // service-level `ChannelProtocol` identity directly (no
    // `hw_protocol_override`/CAN-family relationship at all --
    // `resources::is_tp2_0_protocol_id`). Clause 7 Additional Channels
    // (ADR-210) is now also in scope for this family: `PROTOCOL_TP2_0_CH1`/
    // `_CH128` (the block's own bounds) are re-exported for
    // `resources::chx_block_base`/`chx_base_protocol_id`/
    // `resources::is_tp2_0_family_protocol_id`. Like clause 13 Honda DIAG-H
    // and clause 17 SAE J1708, the vendor header names this family's `_CHx`
    // block consistently with its `_PS` id -- no naming inconsistency here.
    PROTOCOL_TP2_0_CH1,
    PROTOCOL_TP2_0_CH128,
    PROTOCOL_TP2_0_PS,
    // SAE J2534-2 clause 12 UART Echo Byte Protocol (ADR-170/Phase 9): the
    // one `_PS`-only protocol id this phase supports -- like clause 9/20's
    // SW/FT-CAN, clause 12 defines no unqualified base id of its own (clause
    // 12.3.3.1.4's table lists only `_PS`/`_CHx`), but unlike SW/FT-CAN this
    // `_PS` id IS the service-level `ChannelProtocol` identity directly (no
    // `hw_protocol_override`/CAN-family relationship at all --
    // `resources::is_uart_echo_byte_protocol_id`).
    PROTOCOL_UART_ECHO_BYTE_PS,
    // RX flags
    RX_FLAG_CAN_29BIT_ID as CAN_29BIT_ID_STATUS,
    // SAE J2534-2 clause 14 round-15 regression coverage (ADR-165 PR #42):
    // the RX-direction counterparts of `TX_FD_CAN_FORMAT`/`TX_FD_CAN_BRS`
    // below, mirroring this same re-export pattern, so a test can inject an
    // RX frame with `RxStatus` FD bits set without reaching into
    // `j2534_0404_sys::bindings` directly.
    RX_FLAG_FD_CAN_BRS as FD_CAN_BRS_STATUS,
    RX_FLAG_FD_CAN_FORMAT as FD_CAN_FORMAT_STATUS,
    RX_FLAG_ISO15765_ADDR_TYPE as ISO15765_ADDR_TYPE_STATUS,
    RX_FLAG_ISO15765_PADDING_ERROR as ISO15765_PADDING_ERROR,
    RX_FLAG_START_OF_MESSAGE as START_OF_MESSAGE,
    // ADR-219 Decision item 2: `j2534-0404-service`'s vendor IOCTL
    // passthrough builds its own `SBYTE_ARRAY`-wrapped native calls (via the
    // `IoCtlCommand` extension point below) for the wrapped-mode half of its
    // two-mode header -- re-exported here since `j2534-0404-sys` is only a
    // dev-dependency of `j2534-0404-service`, not usable in its production
    // code.
    SBYTE_ARRAY,
    // TX flags
    TX_FLAG_CAN_29BIT_ID as TX_EXTENDED_ID,
    // SAE J2534-2 clause 21.4.4 (ADR-158/Phase 3a): a module must return
    // ERR_INVALID_MSG for any FD-sized TX message whose FD_CAN_FORMAT flag
    // is unset -- distinct from CP_CANFDBaudrate's data-phase bit-rate
    // switch, which FD_CAN_BRS signals separately.
    TX_FLAG_FD_CAN_BRS as TX_FD_CAN_BRS,
    TX_FLAG_FD_CAN_FORMAT as TX_FD_CAN_FORMAT,
    TX_FLAG_ISO15765_ADDR_TYPE as ISO15765_ADDR_TYPE,
    TX_FLAG_ISO15765_FRAME_PAD as TX_ISO15765_FRAME_PAD,
    // SAE J2534-2 clause 17.4.5 (ADR-175/Phase 11): `MSG_PRIORITY_VALUE`,
    // bits 16-19, a 4-bit priority value (1-8) carried per-message in
    // TxFlags -- first used by `ChannelProtocol::J1708_PS`
    // (`ComParamSet::msg_priority_tx_flags`).
    TX_FLAG_MSG_PRIORITY_VALUE,
    TX_FLAG_SCI_MODE as SCI_MODE,
    TX_FLAG_SCI_TX_VOLTAGE as SCI_TX_VOLTAGE,
    // SAE J2534-2 clause 9 Single Wire CAN (ADR-164/Phase 4): the
    // per-message high-voltage TX flag `sw_can_tx_flags` (ADR-062's
    // `sci_tx_flags` pattern, cloned) ORs in when `CP_SwCan_HighVoltage` is
    // nonzero on an SW link.
    TX_FLAG_SW_CAN_HV_TX,
    // SAE J2534-2 clause 19.3.2.2 TP2.0 broadcast send (ADR-192/Phase 7
    // Stage 7c; Phase 0/ADR-152 already added this to the generated
    // bindings for all 5 target triples). ORs in when
    // `CP_TP20BroadcastAddress` resolves to a valid broadcast address
    // (`ComParamSet::tp20_broadcast_address`).
    TX_FLAG_TP2_0_BROADCAST_MSG,
    TX_FLAG_WAIT_P3_MIN_ONLY as TX_WAIT_P3_MIN_ONLY,
};

pub const TX_NORMAL_TRANSMIT: u32 = 0;

pub const MAX_MESSAGE_DATA: usize = 4128;
const LAST_ERROR_BUFFER_SIZE: usize = 80;

// ── Handle types ──────────────────────────────────────────────────────────────

/// Opaque identifier returned by `PassThruOpen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceId(pub u32);

/// Opaque identifier for an opened protocol channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelId(pub u32);

/// Opaque identifier for a periodic message slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeriodicMessageId(pub u32);

/// Opaque identifier for an installed message filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageFilterId(pub u32);

// ── API wrapper ───────────────────────────────────────────────────────────────

/// Loaded J2534 v04.04 API handle.
pub struct J2534Api0404 {
    api: SysJ2534Api0404,
}

impl J2534Api0404 {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Loads a vendor J2534 shared library from the given filesystem path.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        info!(path = %path.display(), "loading J2534 library");
        let api = unsafe { SysJ2534Api0404::new(path.as_os_str()) }.map_err(Error::LibraryLoad)?;
        debug!(path = %path.display(), "J2534 library loaded");
        Ok(Self { api })
    }

    /// Checks a raw PassThru* status code, fetching `PassThruGetLastError`
    /// text and attaching it to the error on failure.
    ///
    /// The description fetch is best-effort: if `PassThruGetLastError`
    /// itself fails or returns an empty string, the original error is
    /// returned unchanged (with `description: None`) rather than masking
    /// the real failure.
    fn check(&self, status: ::std::os::raw::c_long) -> Result<(), Error> {
        error::check(status).map_err(|err| match err {
            Error::ApiStatus {
                code,
                description: None,
            } => {
                let description = self.last_error_text().ok().filter(|s| !s.is_empty());
                Error::ApiStatus { code, description }
            }
            other => other,
        })
    }

    // ── Device lifecycle ──────────────────────────────────────────────────────

    /// Opens a device context.
    ///
    /// `name` is normally `None`, opening the first available device context
    /// per the J2534-1 v04.04 spec (§7.2.1 reserves `pName` and requires it
    /// be NULL). When `Some`, `name`'s bytes are passed verbatim as
    /// `PassThruOpen`'s `pName` argument instead — an out-of-spec
    /// vendor-extension some J2534 DLLs use to select a connection target
    /// among multiple physical devices (ADR-106). The `&CStr` borrow ensures
    /// the caller's owned `CString` outlives this call; no pointer is
    /// retained past `PassThruOpen`'s return.
    pub fn open(&self, name: Option<&CStr>) -> Result<DeviceId, Error> {
        let p_name = name.map_or(std::ptr::null_mut(), |n| n.as_ptr() as *mut c_void);
        let mut device_id = 0_u32;
        self.check(unsafe { self.api.PassThruOpen(p_name, &mut device_id) })?;
        info!(device_id, "PassThruOpen");
        Ok(DeviceId(device_id))
    }

    /// Closes a previously opened device context.
    pub fn close(&self, device_id: DeviceId) -> Result<(), Error> {
        info!(device_id = device_id.0, "PassThruClose");
        self.check(unsafe { self.api.PassThruClose(device_id.0) })
    }

    /// Opens a protocol channel using the provided parameters.
    pub fn connect(
        &self,
        device_id: DeviceId,
        protocol_id: u32,
        flags: u32,
        baud_rate: u32,
    ) -> Result<ChannelId, Error> {
        let mut channel_id = 0_u32;
        self.check(unsafe {
            self.api
                .PassThruConnect(device_id.0, protocol_id, flags, baud_rate, &mut channel_id)
        })?;
        info!(
            device_id = device_id.0,
            channel_id, protocol_id, baud_rate, "PassThruConnect"
        );
        Ok(ChannelId(channel_id))
    }

    /// Disconnects an open channel.
    pub fn disconnect(&self, channel_id: ChannelId) -> Result<(), Error> {
        info!(channel_id = channel_id.0, "PassThruDisconnect");
        self.check(unsafe { self.api.PassThruDisconnect(channel_id.0) })
    }

    /// Reads firmware, DLL, and API version strings from the device.
    pub fn read_version(&self, device_id: DeviceId) -> Result<VersionInfo, Error> {
        let mut firmware = [0 as c_char; LAST_ERROR_BUFFER_SIZE];
        let mut dll = [0 as c_char; LAST_ERROR_BUFFER_SIZE];
        let mut api_ver = [0 as c_char; LAST_ERROR_BUFFER_SIZE];
        self.check(unsafe {
            self.api.PassThruReadVersion(
                device_id.0,
                firmware.as_mut_ptr(),
                dll.as_mut_ptr(),
                api_ver.as_mut_ptr(),
            )
        })?;
        Ok(VersionInfo {
            firmware: error::c_buf_to_string(&firmware)?,
            dll: error::c_buf_to_string(&dll)?,
            api: error::c_buf_to_string(&api_ver)?,
        })
    }

    /// Sets programming voltage for a specific pin.
    pub fn set_programming_voltage(
        &self,
        device_id: DeviceId,
        pin_number: u32,
        voltage: u32,
    ) -> Result<(), Error> {
        self.check(unsafe {
            self.api
                .PassThruSetProgrammingVoltage(device_id.0, pin_number, voltage)
        })
    }

    /// Returns the implementation-defined last error text.
    pub fn last_error_text(&self) -> Result<String, Error> {
        let mut message = [0 as c_char; LAST_ERROR_BUFFER_SIZE];
        error::check(unsafe { self.api.PassThruGetLastError(message.as_mut_ptr()) })?;
        error::c_buf_to_string(&message)
    }

    // ── Messaging ─────────────────────────────────────────────────────────────

    /// Writes one or more messages and returns the number accepted.
    pub fn write_messages(
        &self,
        channel_id: ChannelId,
        messages: &mut [PassThruMessage],
        timeout_ms: u32,
    ) -> Result<u32, Error> {
        for message in messages.iter() {
            let _ = message.data_size()?;
        }
        let mut num_messages = messages.len() as u32;
        self.check(unsafe {
            self.api.PassThruWriteMsgs(
                channel_id.0,
                messages.as_mut_ptr().cast(),
                &mut num_messages,
                timeout_ms,
            )
        })?;
        trace!(channel_id = channel_id.0, num_messages, "PassThruWriteMsgs");
        Ok(num_messages)
    }

    /// Reads up to `max_messages` from a channel.
    pub fn read_messages(
        &self,
        channel_id: ChannelId,
        max_messages: usize,
        timeout_ms: u32,
    ) -> Result<Vec<PassThruMessage>, Error> {
        if max_messages == 0 {
            return Ok(Vec::new());
        }
        let mut raw_messages = vec![PassThruMessage::zeroed(); max_messages];
        let mut num_messages = max_messages as u32;
        self.check(unsafe {
            self.api.PassThruReadMsgs(
                channel_id.0,
                raw_messages.as_mut_ptr().cast(),
                &mut num_messages,
                timeout_ms,
            )
        })?;
        raw_messages.truncate(num_messages as usize);
        for message in &raw_messages {
            let _ = message.data_size()?;
        }
        if num_messages > 0 {
            trace!(channel_id = channel_id.0, num_messages, "PassThruReadMsgs");
        }
        Ok(raw_messages)
    }

    // ── Periodic messages ─────────────────────────────────────────────────────

    /// Starts periodic transmission for a message.
    pub fn start_periodic_message(
        &self,
        channel_id: ChannelId,
        message: &mut PassThruMessage,
        interval_ms: u32,
    ) -> Result<PeriodicMessageId, Error> {
        let _ = message.data_size()?;
        let mut message_id = 0_u32;
        self.check(unsafe {
            self.api.PassThruStartPeriodicMsg(
                channel_id.0,
                message.as_raw_mut_ptr(),
                &mut message_id,
                interval_ms,
            )
        })?;
        debug!(
            channel_id = channel_id.0,
            message_id, interval_ms, "PassThruStartPeriodicMsg"
        );
        Ok(PeriodicMessageId(message_id))
    }

    /// Stops a periodic message by ID.
    pub fn stop_periodic_message(
        &self,
        channel_id: ChannelId,
        id: PeriodicMessageId,
    ) -> Result<(), Error> {
        debug!(
            channel_id = channel_id.0,
            message_id = id.0,
            "PassThruStopPeriodicMsg"
        );
        self.check(unsafe { self.api.PassThruStopPeriodicMsg(channel_id.0, id.0) })
    }

    // ── Message filters ───────────────────────────────────────────────────────

    /// Installs a message filter and returns the filter ID.
    pub fn start_message_filter(
        &self,
        channel_id: ChannelId,
        filter_type: u32,
        mask: &mut PassThruMessage,
        pattern: &mut PassThruMessage,
        flow_control: Option<&mut PassThruMessage>,
    ) -> Result<MessageFilterId, Error> {
        let _ = mask.data_size()?;
        let _ = pattern.data_size()?;
        let flow_control_ptr = match flow_control {
            Some(message) => {
                let _ = message.data_size()?;
                message.as_raw_mut_ptr()
            }
            None => std::ptr::null_mut(),
        };
        let mut filter_id = 0_u32;
        self.check(unsafe {
            self.api.PassThruStartMsgFilter(
                channel_id.0,
                filter_type,
                mask.as_raw_mut_ptr(),
                pattern.as_raw_mut_ptr(),
                flow_control_ptr,
                &mut filter_id,
            )
        })?;
        Ok(MessageFilterId(filter_id))
    }

    /// Removes a message filter by ID.
    pub fn stop_message_filter(
        &self,
        channel_id: ChannelId,
        id: MessageFilterId,
    ) -> Result<(), Error> {
        self.check(unsafe { self.api.PassThruStopMsgFilter(channel_id.0, id.0) })
    }

    // ── IOCTL: buffer management ──────────────────────────────────────────────

    /// Clears a channel receive buffer.
    pub fn clear_rx_buffer(&self, channel_id: ChannelId) -> Result<(), Error> {
        self.ioctl_no_data(channel_id, CLEAR_RX_BUFFER)
    }

    /// Clears a channel transmit buffer.
    pub fn clear_tx_buffer(&self, channel_id: ChannelId) -> Result<(), Error> {
        self.ioctl_no_data(channel_id, CLEAR_TX_BUFFER)
    }

    /// Stops and clears all periodic messages for a channel.
    pub fn clear_periodic_messages(&self, channel_id: ChannelId) -> Result<(), Error> {
        self.ioctl_no_data(channel_id, CLEAR_PERIODIC_MSGS)
    }

    /// Clears all configured message filters for a channel.
    pub fn clear_message_filters(&self, channel_id: ChannelId) -> Result<(), Error> {
        self.ioctl_no_data(channel_id, CLEAR_MSG_FILTERS)
    }

    /// Clears the J1850 functional message lookup table for a channel.
    pub fn clear_functional_msg_lookup_table(&self, channel_id: ChannelId) -> Result<(), Error> {
        self.ioctl_no_data(channel_id, CLEAR_FUNCT_MSG_LOOKUP_TABLE)
    }

    // ── IOCTL: SAE J2534-2 clause 9 Single Wire CAN (ADR-164/Phase 4) ─────────

    /// Switches an SW_CAN_PS/SW_ISO15765_PS channel to high-speed mode via
    /// `PassThruIoctl(SW_CAN_HS)` -- no input/output parameters, per clause
    /// 9's own command definition.
    pub fn sw_can_hs(&self, channel_id: ChannelId) -> Result<(), Error> {
        self.ioctl_no_data(channel_id, IOCTL_SW_CAN_HS)
    }

    /// Switches an SW_CAN_PS/SW_ISO15765_PS channel to normal-speed mode via
    /// `PassThruIoctl(SW_CAN_NS)` -- no input/output parameters.
    pub fn sw_can_ns(&self, channel_id: ChannelId) -> Result<(), Error> {
        self.ioctl_no_data(channel_id, IOCTL_SW_CAN_NS)
    }

    // ── IOCTL: SAE J2534-2 clause 14 Repeat Messaging (ADR-165/Phase 12) ─────

    /// Starts autonomous device-side retransmission of `setup` via
    /// `PassThruIoctl(START_REPEAT_MESSAGE)`. Returns the device-assigned
    /// `MsgId` used by [`Self::query_repeat_message`]/
    /// [`Self::stop_repeat_message`] to reference this slot.
    pub fn start_repeat_message(
        &self,
        channel_id: ChannelId,
        setup: &mut RepeatMsgSetup,
    ) -> Result<u32, Error> {
        let mut msg_id = 0_u32;
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                IOCTL_START_REPEAT_MESSAGE,
                setup.as_raw_mut_ptr().cast(),
                std::ptr::addr_of_mut!(msg_id).cast(),
            )
        })?;
        debug!(
            channel_id = channel_id.0,
            msg_id, "PassThruIoctl START_REPEAT_MESSAGE"
        );
        Ok(msg_id)
    }

    /// Polls a running repeat slot's status via
    /// `PassThruIoctl(QUERY_REPEAT_MESSAGE)`. `msg_id` is the value
    /// [`Self::start_repeat_message`] returned.
    pub fn query_repeat_message(&self, channel_id: ChannelId, msg_id: u32) -> Result<u32, Error> {
        let mut msg_id = msg_id;
        let mut status = 0_u32;
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                IOCTL_QUERY_REPEAT_MESSAGE,
                std::ptr::addr_of_mut!(msg_id).cast(),
                std::ptr::addr_of_mut!(status).cast(),
            )
        })?;
        Ok(status)
    }

    /// Cancels a running repeat slot via `PassThruIoctl(STOP_REPEAT_MESSAGE)`.
    /// `msg_id` is the value [`Self::start_repeat_message`] returned.
    pub fn stop_repeat_message(&self, channel_id: ChannelId, msg_id: u32) -> Result<(), Error> {
        let mut msg_id = msg_id;
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                IOCTL_STOP_REPEAT_MESSAGE,
                std::ptr::addr_of_mut!(msg_id).cast(),
                std::ptr::null_mut(),
            )
        })?;
        debug!(
            channel_id = channel_id.0,
            msg_id, "PassThruIoctl STOP_REPEAT_MESSAGE"
        );
        Ok(())
    }

    /// Adds one or more functional addresses to the J1850 lookup table for a channel.
    pub fn add_to_functional_msg_lookup_table(
        &self,
        channel_id: ChannelId,
        addresses: &[u8],
    ) -> Result<(), Error> {
        self.ioctl_sbyte_array_input(channel_id, IOCTL_ADD_TO_FUNCT_MSG_LOOKUP_TABLE, addresses)
    }

    /// Removes one or more functional addresses from the J1850 lookup table for a channel.
    pub fn delete_from_functional_msg_lookup_table(
        &self,
        channel_id: ChannelId,
        addresses: &[u8],
    ) -> Result<(), Error> {
        self.ioctl_sbyte_array_input(
            channel_id,
            IOCTL_DELETE_FROM_FUNCT_MSG_LOOKUP_TABLE,
            addresses,
        )
    }

    // ── IOCTL: SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5) ────

    /// Directs the adapter to claim and defend `address` under SAE J1939-81,
    /// using `name` as this node's SAE J1939 NAME, via
    /// `PassThruIoctl(PROTECT_J1939_ADDR)` (clause 16.3.3.2). Non-blocking:
    /// the outcome (`RX_FLAG_J1939_ADDRESS_CLAIMED`/`_LOST`) arrives later
    /// as an ordinary RxStatus-flagged message through the normal read path
    /// (`j2534-0404-service`'s `events.rs` consumes it), not as this call's
    /// return value.
    ///
    /// `address` must be `0..=253` -- clause 16.3.3.2 defines `254` as
    /// always invalid and `255` as the conceptual "no address claimed"
    /// power-on default only, never a value this call may explicitly
    /// target; the native call returns `ERR_INVALID_IOCTL_VALUE` for
    /// either. This wrapper does not pre-validate `address` itself (the
    /// native call is the authoritative check, mirroring every other IOCTL
    /// wrapper in this file); callers that need a synchronous rejection
    /// before reaching hardware should check first.
    ///
    /// Builds the clause 16.3.3.2 9-byte `SBYTE_ARRAY` payload (`address`
    /// then `name`, LSB-first per SAE J1939-81) and reuses
    /// [`Self::ioctl_sbyte_array_input`], the same helper
    /// [`Self::add_to_functional_msg_lookup_table`] uses for its own
    /// `SBYTE_ARRAY`-shaped IOCTL.
    pub fn protect_j1939_addr(
        &self,
        channel_id: ChannelId,
        address: u8,
        name: [u8; 8],
    ) -> Result<(), Error> {
        let mut payload = [0u8; 9];
        payload[0] = address;
        payload[1..9].copy_from_slice(&name);
        self.ioctl_sbyte_array_input(channel_id, IOCTL_PROTECT_J1939_ADDR, &payload)
    }

    /// Cancels a previously-protected SAE J1939 address via
    /// `PassThruIoctl(PROTECT_J1939_ADDR)`'s cancel form (clause 16.3.3.2):
    /// `address` in byte 0, all-zero NAME bytes. Same underlying IOCTL as
    /// [`Self::protect_j1939_addr`], distinguished only by the all-zero
    /// NAME -- matches this crate's existing convention of separate named
    /// methods for a dual-purpose IOCTL's two forms (compare
    /// [`Self::start_repeat_message`]/[`Self::stop_repeat_message`],
    /// [`Self::sw_can_hs`]/[`Self::sw_can_ns`]) rather than a boolean flag
    /// parameter.
    pub fn cancel_j1939_addr_protect(
        &self,
        channel_id: ChannelId,
        address: u8,
    ) -> Result<(), Error> {
        self.protect_j1939_addr(channel_id, address, [0u8; 8])
    }

    // ── IOCTL: SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a) ─

    /// Requests establishment of a TP2.0 channel/connection via
    /// `PassThruIoctl(REQUEST_CONNECTION)` (clause 19.3.3.2), invoked
    /// internally from `PDU_COPT_STARTCOMM` -- never a client-visible
    /// `PDU_IOCTL_*` (ADR-188 Decision item 3). Non-blocking: the outcome
    /// (`RX_FLAG_CONNECTION_ESTABLISHED`/`_LOST`) arrives later as an
    /// ordinary RxStatus-flagged message through the normal read path
    /// (`j2534-0404-service`'s `events_tp20_connection.rs` consumes it), not
    /// as this call's return value -- the same non-blocking shape
    /// [`Self::protect_j1939_addr`] already uses.
    ///
    /// Builds the clause 19 Table 78 11-byte `SBYTE_ARRAY` payload:
    /// `setup_can_id` (bytes 0-3, MSB first), `destination_address` (byte
    /// 4), the fixed opcode `0xC0` (byte 5), `tx_id_proposal` (bytes 6-7,
    /// MSB first), `rx_id_proposal` (bytes 8-9, MSB first), and
    /// `application_type` (byte 10).
    pub fn tp20_request_connection(
        &self,
        channel_id: ChannelId,
        setup_can_id: u32,
        destination_address: u8,
        tx_id_proposal: u16,
        rx_id_proposal: u16,
        application_type: u8,
    ) -> Result<(), Error> {
        let mut payload = [0u8; 11];
        payload[0..4].copy_from_slice(&setup_can_id.to_be_bytes());
        payload[4] = destination_address;
        payload[5] = 0xC0;
        payload[6..8].copy_from_slice(&tx_id_proposal.to_be_bytes());
        payload[8..10].copy_from_slice(&rx_id_proposal.to_be_bytes());
        payload[10] = application_type;
        self.ioctl_sbyte_array_input(channel_id, IOCTL_REQUEST_CONNECTION, &payload)
    }

    /// Tears down an established TP2.0 connection via
    /// `PassThruIoctl(TEARDOWN_CONNECTION)` (clause 19.3.3.3), invoked
    /// internally from `PDU_COPT_STOPCOMM` and every existing
    /// `DestroyComLogicalLink`/`DisconnectComLogicalLink` cleanup site
    /// (ADR-188 Decision item 2, mirroring Repeat Messaging's own Decision
    /// 4 cleanup sites). Non-blocking, same shape as
    /// [`Self::tp20_request_connection`].
    ///
    /// `rx_id` is the connection's own established receiving CAN address
    /// (clause 19.3.3.3: teardown is keyed on the original request's
    /// RX-ID), packed as the clause 19 Table 79 4-byte `SBYTE_ARRAY`
    /// payload, MSB first.
    pub fn tp20_teardown_connection(&self, channel_id: ChannelId, rx_id: u32) -> Result<(), Error> {
        self.ioctl_sbyte_array_input(channel_id, IOCTL_TEARDOWN_CONNECTION, &rx_id.to_be_bytes())
    }

    // ── IOCTL: SAE J2534-2 clause 11 GM UART Protocol (ADR-189/Phase 8) ──────

    /// Defines the bytes of the poll-response message the interface should
    /// transmit once bus mastership is granted, via
    /// `PassThruIoctl(SET_POLL_RESPONSE)` (clause 11.3.3.1) -- no output.
    /// `poll_response` is Table 28's `PollResponseMsg[100]`, at most 100
    /// bytes; this wrapper does not pre-validate the length itself (the
    /// native call is the authoritative check, mirroring every other IOCTL
    /// wrapper in this file).
    pub fn set_poll_response(
        &self,
        channel_id: ChannelId,
        poll_response: &[u8],
    ) -> Result<(), Error> {
        self.ioctl_sbyte_array_input(channel_id, IOCTL_SET_POLL_RESPONSE, poll_response)
    }

    /// Requests bus mastership via `PassThruIoctl(BECOME_MASTER)` (clause
    /// 11.3.3.2): waits for a poll message matching `poll_id` (or grants
    /// immediately if `poll_id` is zero), transmits the poll response
    /// [`Self::set_poll_response`] previously staged, and reports
    /// success/failure -- a single blocking native call with an ~2 second
    /// native-side timeout, not a state machine this crate or its callers
    /// drive (ADR-189 Context/Decision item 4). No output; `Table 32`'s
    /// single `Poll_ID` byte is the only input.
    pub fn become_master(&self, channel_id: ChannelId, poll_id: u8) -> Result<(), Error> {
        self.ioctl_sbyte_array_input(channel_id, IOCTL_BECOME_MASTER, &[poll_id])
    }

    // ── IOCTL: SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16) ────────

    /// Reads adapter identity/status via `PassThruIoctl(GET_NDIS_ADAPTER_INFO)`
    /// (clause 24.2.5.6) -- a single non-blocking native call (an ordinary
    /// synchronous forward, unlike [`Self::become_master`]'s spawn_blocking-
    /// dispatched ~2 second native-side wait), no input, channel-scoped (the
    /// whole `NDIS_ADAPTER_INFORMATION` struct is returned at once; clause
    /// 24's own IOCTL definition requires a live `ChannelID`, so this is only
    /// ever meaningful post-`PassThruConnect`).
    pub fn get_ndis_adapter_info(&self, channel_id: ChannelId) -> Result<NdisAdapterInfo, Error> {
        let mut info = NdisAdapterInfo::zeroed();
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                IOCTL_GET_NDIS_ADAPTER_INFO,
                std::ptr::null_mut(),
                info.as_raw_mut_ptr().cast(),
            )
        })?;
        Ok(info)
    }

    // ── IOCTL: diagnostics ────────────────────────────────────────────────────

    /// Reads the battery voltage via `PassThruIoctl(READ_VBATT)`.
    ///
    /// Returns the voltage in millivolts as reported by the adapter.
    pub fn read_vbatt(&self, device_id: DeviceId) -> Result<u32, Error> {
        self.ioctl_read_u32(device_id.0, IOCTL_READ_VBATT)
    }

    /// Reads the J1962 programming voltage via `PassThruIoctl(READ_PROG_VOLTAGE)`.
    ///
    /// Returns the voltage in millivolts as reported by the adapter.
    pub fn read_prog_voltage(&self, device_id: DeviceId) -> Result<u32, Error> {
        self.ioctl_read_u32(device_id.0, IOCTL_READ_PROG_VOLTAGE)
    }

    /// Reads a single J1962 connector pin's voltage via
    /// `PassThruIoctl(READ_J1962PIN_VOLTAGE)` (SAE J2534-2 clause 23).
    ///
    /// `pin_number` is the J1962 pin (1-16) to read; the ground reference is
    /// J1962 pin 5, and pin 16 reports the same value [`read_vbatt`] does.
    /// Returns the voltage in millivolts as reported by the adapter.
    ///
    /// [`read_vbatt`]: Self::read_vbatt
    pub fn read_j1962_pin_voltage(
        &self,
        device_id: DeviceId,
        pin_number: u32,
    ) -> Result<u32, Error> {
        self.ioctl_write_read_u32(device_id.0, IOCTL_READ_J1962PIN_VOLTAGE, pin_number)
    }

    // ── IOCTL: configuration ──────────────────────────────────────────────────

    /// Reads one or more IOCTL config parameters in a single call.
    ///
    /// `parameters` is a slice of parameter IDs (e.g. [`DATA_RATE`]).  Returns a
    /// `Vec<u32>` of the corresponding values in the same order.
    pub fn get_config(&self, channel_id: ChannelId, parameters: &[u32]) -> Result<Vec<u32>, Error> {
        let mut configs: Vec<bindings::SCONFIG> = parameters
            .iter()
            .map(|&p| bindings::SCONFIG {
                Parameter: p,
                Value: 0,
            })
            .collect();
        let mut config_list = bindings::SCONFIG_LIST {
            NumOfParams: configs.len() as u32,
            ConfigPtr: configs.as_mut_ptr(),
        };
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                IOCTL_GET_CONFIG,
                std::ptr::addr_of_mut!(config_list).cast(),
                std::ptr::null_mut(),
            )
        })?;
        Ok(configs.iter().map(|c| c.Value).collect())
    }

    /// Reads a single IOCTL config parameter as `u32`.
    pub fn get_config_u32(&self, channel_id: ChannelId, parameter: u32) -> Result<u32, Error> {
        Ok(self.get_config(channel_id, &[parameter])?[0])
    }

    /// Writes one or more IOCTL config parameters in a single call.
    ///
    /// `configs` is a slice of `(parameter_id, value)` pairs.
    pub fn set_config(&self, channel_id: ChannelId, configs: &[(u32, u32)]) -> Result<(), Error> {
        let mut sconfigs: Vec<bindings::SCONFIG> = configs
            .iter()
            .map(|&(p, v)| bindings::SCONFIG {
                Parameter: p,
                Value: v,
            })
            .collect();
        let mut config_list = bindings::SCONFIG_LIST {
            NumOfParams: sconfigs.len() as u32,
            ConfigPtr: sconfigs.as_mut_ptr(),
        };
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                IOCTL_SET_CONFIG,
                std::ptr::addr_of_mut!(config_list).cast(),
                std::ptr::null_mut(),
            )
        })
    }

    /// Writes a single IOCTL config parameter as `u32`.
    pub fn set_config_u32(
        &self,
        channel_id: ChannelId,
        parameter: u32,
        value: u32,
    ) -> Result<(), Error> {
        self.set_config(channel_id, &[(parameter, value)])
    }

    /// Reads one or more Device Configuration parameters (SAE J2534-2 clause 18,
    /// ADR-176/Phase 14) via `PassThruIoctl(GET_DEVICE_CONFIG)`. `device_id` is
    /// passed as the native call's handle directly -- this IOCTL is
    /// `DeviceID`-scoped, not `ChannelID`-scoped, the same shape
    /// `get_device_info`/`get_protocol_info` already use. `parameters` is a
    /// slice of parameter IDs (e.g. `CONFIG_NON_VOLATILE_STORE_1`). Returns a
    /// `Vec<u32>` of the corresponding values in the same order.
    pub fn get_device_config(
        &self,
        device_id: DeviceId,
        parameters: &[u32],
    ) -> Result<Vec<u32>, Error> {
        let mut configs: Vec<bindings::SCONFIG> = parameters
            .iter()
            .map(|&p| bindings::SCONFIG {
                Parameter: p,
                Value: 0,
            })
            .collect();
        let mut config_list = bindings::SCONFIG_LIST {
            NumOfParams: configs.len() as u32,
            ConfigPtr: configs.as_mut_ptr(),
        };
        self.check(unsafe {
            self.api.PassThruIoctl(
                device_id.0,
                IOCTL_GET_DEVICE_CONFIG,
                std::ptr::addr_of_mut!(config_list).cast(),
                std::ptr::null_mut(),
            )
        })?;
        Ok(configs.iter().map(|c| c.Value).collect())
    }

    /// Writes one or more Device Configuration parameters (SAE J2534-2 clause 18,
    /// ADR-176/Phase 14) via `PassThruIoctl(SET_DEVICE_CONFIG)`. `device_id` is
    /// passed as the native call's handle directly, the same `DeviceID`-scoped
    /// shape [`get_device_config`] uses. `configs` is a slice of
    /// `(parameter_id, value)` pairs.
    ///
    /// [`get_device_config`]: Self::get_device_config
    pub fn set_device_config(
        &self,
        device_id: DeviceId,
        configs: &[(u32, u32)],
    ) -> Result<(), Error> {
        let mut sconfigs: Vec<bindings::SCONFIG> = configs
            .iter()
            .map(|&(p, v)| bindings::SCONFIG {
                Parameter: p,
                Value: v,
            })
            .collect();
        let mut config_list = bindings::SCONFIG_LIST {
            NumOfParams: sconfigs.len() as u32,
            ConfigPtr: sconfigs.as_mut_ptr(),
        };
        self.check(unsafe {
            self.api.PassThruIoctl(
                device_id.0,
                IOCTL_SET_DEVICE_CONFIG,
                std::ptr::addr_of_mut!(config_list).cast(),
                std::ptr::null_mut(),
            )
        })
    }

    // ── IOCTL: protocol initialisation ────────────────────────────────────────

    /// Performs a fast (wakeup) initialisation on the channel.
    ///
    /// Calls `PassThruIoctl` with `FAST_INIT`.  `input` is the optional start
    /// communication request transmitted after the wakeup pattern; pass `None`
    /// to transmit no request message (a null input to the native API).
    /// Returns the init response frame written into the output buffer by the
    /// adapter.
    pub fn fast_init(
        &self,
        channel_id: ChannelId,
        input: Option<&PassThruMessage>,
    ) -> Result<PassThruMessage, Error> {
        // The C API declares pInput as `void*` but only reads it; cast the
        // const raw pointer to *mut so the FFI signature is satisfied without
        // requiring a mutable borrow from the caller.
        let input_ptr = match input {
            Some(msg) => std::ptr::addr_of!(msg.0) as *mut bindings::PASSTHRU_MSG,
            None => std::ptr::null_mut(),
        };
        let mut output = PassThruMessage::zeroed();
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                FAST_INIT,
                input_ptr.cast(),
                output.as_raw_mut_ptr().cast(),
            )
        })?;
        Ok(output)
    }

    /// Performs a 5-baud (slow) initialisation on the channel.
    ///
    /// `target_address` is the single-byte ECU address sent at 5 baud during
    /// the wakeup sequence.  Returns the keyword bytes `[KB1, KB2]` received
    /// from the ECU after the init sequence.
    pub fn five_baud_init(
        &self,
        channel_id: ChannelId,
        target_address: u8,
    ) -> Result<[u8; 2], Error> {
        let mut addr_byte = target_address;
        let mut keywords = [0u8; 2];
        let mut input = bindings::SBYTE_ARRAY {
            NumOfBytes: 1,
            BytePtr: &mut addr_byte,
        };
        let mut output = bindings::SBYTE_ARRAY {
            NumOfBytes: 2,
            BytePtr: keywords.as_mut_ptr(),
        };
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                FIVE_BAUD_INIT,
                std::ptr::addr_of_mut!(input).cast(),
                std::ptr::addr_of_mut!(output).cast(),
            )
        })?;
        Ok(keywords)
    }

    // ── IOCTL: capability discovery (SAE J2534-2 clause 25) ──────────────────

    /// Queries device-wide capabilities via `PassThruIoctl(GET_DEVICE_INFO)`
    /// (SAE J2534-2 §25.3.2.2). `device_id` is passed as the native call's
    /// handle directly (this IOCTL is `DeviceID`-scoped, not
    /// `ChannelID`-scoped). Returns one [`DiscoveryResult`] per input
    /// `params` entry, in the same order.
    pub fn get_device_info(
        &self,
        device_id: DeviceId,
        params: &[DiscoveryParam],
    ) -> Result<Vec<DiscoveryResult>, Error> {
        self.discovery_ioctl(
            device_id.0,
            IOCTL_GET_DEVICE_INFO,
            std::ptr::null_mut(),
            params,
        )
    }

    /// Queries capabilities for a single protocol via
    /// `PassThruIoctl(GET_PROTOCOL_INFO)` (SAE J2534-2 §25.3.2.3).
    /// `device_id` is passed as the native call's handle directly, and
    /// `protocol_id` as the `InputPtr` payload. Returns one
    /// [`DiscoveryResult`] per input `params` entry, in the same order.
    pub fn get_protocol_info(
        &self,
        device_id: DeviceId,
        protocol_id: u32,
        params: &[DiscoveryParam],
    ) -> Result<Vec<DiscoveryResult>, Error> {
        let mut protocol_id = protocol_id;
        self.discovery_ioctl(
            device_id.0,
            IOCTL_GET_PROTOCOL_INFO,
            std::ptr::addr_of_mut!(protocol_id).cast(),
            params,
        )
    }

    /// Shared `SPARAM_LIST` marshaling for [`Self::get_device_info`] and
    /// [`Self::get_protocol_info`] — mirrors [`Self::get_config`]'s
    /// `SCONFIG_LIST` marshaling, but over `SPARAM` (which additionally
    /// carries a `Supported` output flag per parameter).
    fn discovery_ioctl(
        &self,
        handle: u32,
        ioctl_id: u32,
        input_ptr: *mut c_void,
        params: &[DiscoveryParam],
    ) -> Result<Vec<DiscoveryResult>, Error> {
        // No early return on an empty `params`: for `GET_PROTOCOL_INFO`,
        // `input_ptr` still carries the protocol ID to validate even with a
        // zero-length parameter list, and skipping the native call would
        // silently swallow that validation (edge-case-hunter finding). An
        // empty `SPARAM_LIST` (`NumOfParams: 0`) is well-defined either way:
        // `ParamPtr` from an empty `Vec` is non-null and aligned even though
        // it is never dereferenced.
        let mut sparams: Vec<bindings::SPARAM> = params
            .iter()
            .map(|p| bindings::SPARAM {
                Parameter: p.parameter,
                Value: p.value,
                Supported: 0,
            })
            .collect();
        let mut param_list = bindings::SPARAM_LIST {
            NumOfParams: sparams.len() as u32,
            ParamPtr: sparams.as_mut_ptr(),
        };
        self.check(unsafe {
            self.api.PassThruIoctl(
                handle,
                ioctl_id,
                input_ptr,
                std::ptr::addr_of_mut!(param_list).cast(),
            )
        })?;
        Ok(sparams
            .iter()
            .map(|p| DiscoveryResult {
                parameter: p.Parameter,
                value: p.Value,
                supported: p.Supported != 0,
            })
            .collect())
    }

    // ── IOCTL: extension point ────────────────────────────────────────────────

    /// Executes a custom `PassThruIoctl` command described by `command`.
    ///
    /// This is the extension point for IOCTL IDs not covered by the named
    /// methods on [`J2534Api0404`].  `handle` is forwarded directly to
    /// `PassThruIoctl` — use [`DeviceId`]`.0` for device-level commands and
    /// [`ChannelId`]`.0` for channel-level commands.
    ///
    /// # Safety
    /// All safety requirements documented on [`IoCtlCommand`] must be upheld by
    /// the `command` value passed to this function.
    pub unsafe fn ioctl<C: IoCtlCommand>(&self, handle: u32, command: &mut C) -> Result<(), Error> {
        self.check(unsafe {
            self.api.PassThruIoctl(
                handle,
                command.ioctl_id(),
                command.input_ptr(),
                command.output_ptr(),
            )
        })
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// `PassThruIoctl` with no input or output data.
    fn ioctl_no_data(&self, channel_id: ChannelId, ioctl_id: u32) -> Result<(), Error> {
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                ioctl_id,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        })
    }

    /// `PassThruIoctl` with a `u32` output and no input.
    fn ioctl_read_u32(&self, handle: u32, ioctl_id: u32) -> Result<u32, Error> {
        let mut value: u32 = 0;
        self.check(unsafe {
            self.api.PassThruIoctl(
                handle,
                ioctl_id,
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(value).cast(),
            )
        })?;
        Ok(value)
    }

    /// `PassThruIoctl` with a `u32` input and a `u32` output.
    fn ioctl_write_read_u32(
        &self,
        handle: u32,
        ioctl_id: u32,
        mut input: u32,
    ) -> Result<u32, Error> {
        let mut value: u32 = 0;
        self.check(unsafe {
            self.api.PassThruIoctl(
                handle,
                ioctl_id,
                std::ptr::addr_of_mut!(input).cast(),
                std::ptr::addr_of_mut!(value).cast(),
            )
        })?;
        Ok(value)
    }

    /// `PassThruIoctl` with a byte-slice `SBYTE_ARRAY` as input and no output.
    fn ioctl_sbyte_array_input(
        &self,
        channel_id: ChannelId,
        ioctl_id: u32,
        data: &[u8],
    ) -> Result<(), Error> {
        let mut bytes = data.to_vec();
        let mut input = bindings::SBYTE_ARRAY {
            NumOfBytes: bytes.len() as u32,
            BytePtr: bytes.as_mut_ptr(),
        };
        self.check(unsafe {
            self.api.PassThruIoctl(
                channel_id.0,
                ioctl_id,
                std::ptr::addr_of_mut!(input).cast(),
                std::ptr::null_mut(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip_preserves_fields() {
        let original = PassThruMessage::new(
            ISO15765,
            START_OF_MESSAGE,
            TX_ISO15765_FRAME_PAD,
            42,
            0,
            &[0x02, 0x10, 0x03],
        )
        .expect("message should build");

        assert_eq!(original.protocol_id(), ISO15765);
        assert_eq!(original.rx_status(), START_OF_MESSAGE);
        assert_eq!(original.tx_flags(), TX_ISO15765_FRAME_PAD);
        assert_eq!(original.timestamp(), 42);
        assert_eq!(original.extra_data_index(), 0);
        assert_eq!(
            original.data().expect("data should be valid"),
            &[0x02, 0x10, 0x03]
        );

        let borrowed = original.borrowed();
        let cloned = borrowed.to_owned();
        assert_eq!(cloned, original);
    }

    #[test]
    fn to_raw_rejects_oversized_payload() {
        let err = PassThruMessage::new(CAN, 0, 0, 0, 0, &[0_u8; MAX_MESSAGE_DATA + 1])
            .expect_err("oversized payload must fail");
        match err {
            Error::MessageTooLarge { len, max } => {
                assert_eq!(len, MAX_MESSAGE_DATA + 1);
                assert_eq!(max, MAX_MESSAGE_DATA);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn status_display_has_symbolic_name() {
        let code = StatusCode(ERR_TIMEOUT);
        let text = code.to_string();
        assert!(text.contains("ERR_TIMEOUT"));
    }
}
