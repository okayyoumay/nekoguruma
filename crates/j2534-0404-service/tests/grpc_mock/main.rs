//! Verifies, through the `j2534-0404-service` gRPC interface, that per-protocol
//! communication parameter settings (`SetComParam`) and transmit data
//! composition (`StartComPrimitive` / `CoptSendrecv`) are forwarded to the
//! underlying J2534 library exactly as expected — and, in the reverse
//! direction, that responses read back from the J2534 library are distributed
//! to the right CLLs based on each CLL's `UniqueRespIdTable` (ADR-007 /
//! ADR-014).
//!
//! `J2534Service::new` loads its J2534 library by dynamically opening a
//! shared-library file path resolved from the shared `config.toml` via
//! `j2534-0404-registry::find_library_path` (see `harness::TestServer`), so
//! these tests point that resolution at the compiled `j2534-0404-mock` cdylib
//! and then read back what the service actually sent to it.
//!
//! One module per theme; the shared server/backdoor scaffolding lives in
//! [`harness`]:
//!
//! - [`comparam_tx`] — per-protocol ComParam forwarding and TX message
//!   composition/size validation (ADR-050)
//! - [`cop_ctrl_cycles`] — PDU_COP_CTRL_DATA cycle control on `CoptSendrecv`:
//!   `Time`, `NumSendCycles`, `NumReceiveCycles` (ADR-053)
//! - [`lifecycle`] — the full "Typical gRPC Client Flow" call sequence
//! - [`locks_and_param_classes`] — ComParam class boundaries (ADR-042) and
//!   `LOCK_PHYSICAL_COM_PARAMS` enforcement (ADR-043/045; `CoptUpdateparam`/
//!   `temp_param_update`/`SetComParam` moved to ADR-110's event-based model,
//!   superseding ADR-044's synchronous rejection)
//! - [`flow_control_filters`] — `FLOW_CONTROL_FILTER` installation from the
//!   `UniqueRespIdTable` (ADR-039/040/041/048)
//! - [`rx_header_split`] — RX header/footer split into
//!   `ResultData.extra_info` (ADR-051)
//! - [`can_mode`] — `can_channel_mode` behaviours: software-ISO-TP,
//!   dual-channel, auto (ADR-046)
//! - [`response_distribution`] — `UniqueRespIdTable`-based routing of
//!   responses to CLLs (ADR-007) and the `unique_resp_ids`
//!   expected-response gate (ADR-014)
//! - [`rc_handling`] — response-pending/re-request NRC (0x21/0x23/0x78)
//!   auto-handling deadline behavior (ADR-018, ADR-057)
//! - [`queue_error_suspend`] — `CP_SuspendQueueOnError`: a timed-out COP or
//!   an unhandled negative response suspending the TX queue, and its four
//!   resume paths (ADR-147)
//! - [`p3_gap`] — `CP_P3Func`/`CP_P3Phys` minimum inter-request gap
//!   enforcement on CAN (ADR-060)
//! - [`connect_flags`] — `PassThruConnect` `Flags` derivation from Working
//!   ComParams / `UniqueRespIdTable` (ADR-065)
//! - [`clear_msg_filters`] — `CLEAR_MSG_FILTERS` IoCtl filter rebuild on
//!   non-ISO15765 channels, reusing the connect-time per-ID-type `PASS_FILTER`
//!   set (ADR-065)
//! - [`startcomm_comparam`] — ComParam resolution and `temp_param_update` for
//!   `CoptStartcomm`/`CoptStopcomm` (ADR-066, superseded by ADR-067)
//! - [`param_binding`] — call-time ComParam snapshot binding, the
//!   `temp_param_update` Working writeback, and the `PDU_PC_BUSTYPE` guard
//!   (ADR-067)
//! - [`unique_resp_id_table_binding`] — the UniqueRespIdTable Working/Active
//!   split: `SetUniqueRespIdTable` stages Working only, promotion happens at
//!   Connect/`CoptUpdateparam`, a COP always snapshots ACTIVE regardless of
//!   `temp_param_update`, `CoptRestoreParam` copies Active back into
//!   Working, and the promotion helper's filter-diff gate (ADR-068)
//! - [`resources`] — the ISO 22900-2-style static resource table:
//!   `GetResourceIds` filtering by protocol/bus type/pin selectors and
//!   `CreateComLogicalLink` resolution of table resource IDs
//! - [`j1850_autodetect`] — the `SAE_J1850` bus's VPW/PWM auto-detect probe
//!   at `ConnectComLogicalLink` (ADR-070)
//! - [`pdu_ioctl`] — the 17 `PDU_IOCTL_*` adapter commands (ADR-079):
//!   `GetObjectId(OBJT_IO_CTRL, ...)` name resolution, TX-suspend/resume
//!   FIFO ordering, and the four commands rejected as unsupported
//! - [`pin_selection`] — SAE J2534-2 clause 6 Pin Selection end-to-end
//!   (ADR-156 Decision 2, Phase 2a): `_PS` resolution via the `ProtocolId`
//!   route, the J2534-2 opt-in gate (clause 5), the widened `ChannelKey`'s
//!   pin-select-based physical channel sharing/separation, and Table 3's
//!   pin-legality validation
//! - [`tester_present_send_type`] — `CP_TesterPresentTime` us-to-ms
//!   conversion, `CP_TesterPresentSendType`'s periodic (0) vs. idle-triggered
//!   (1) dispatch modes, and the mode-0 periodic start's `CP_P3Func`/
//!   `CP_P3Phys` gate (ADR-083)
//! - [`tester_present_reqrsp`] — `CP_TesterPresentReqRsp = 1` discarding the
//!   ECU's tester-present response instead of delivering it as an unsolicited
//!   `ResultData` (ADR-088)
//! - [`tester_present_addr_mode`] — `CP_TesterPresentAddrMode` selecting
//!   tester-present's own functional-vs-physical addressing, independently
//!   of `CP_RequestAddrMode` (ADR-138)
//! - [`modules`] — config-declared multi-module device selection: `GetModuleIds`/
//!   `ModuleConnect` over N configured entries, `pname` reaching `PassThruOpen`,
//!   single-open-device reject-on-switch, and `modules = []` startup failure
//!   (ADR-106)
//! - [`concat`] — `CP_EnableConcatenation` multi-segment response merging on
//!   the KWP/J1850 family (ISO 22900-2:2022 Table B.11; see ADR-148)
//! - [`fd_can`] — SAE J2534-2 clause 21 CAN FD's connect-time protocol
//!   substitution (ADR-158, Phase 3a): the `TX_DL > 8 || CP_CANFDBaudrate !=
//!   0` FD-mode trigger, the mandatory `CONFIG_FD_CAN_DATA_PHASE_RATE` ->
//!   `CONFIG_J1962_PINS` SET_CONFIG ordering (clause 21.3.2.5.1), the
//!   connect-latched (never-sticky) mode flip on reconnect, and the
//!   opt-in/`_CHx`/software-ISO-TP/direct-id-naming rejections
//! - [`mixed_format_can`] — `can_channel_mode = "native-mixed"` (SAE
//!   J2534-2 clause 8 Mixed Format Frames on a CAN Network, ADR-160/Phase
//!   3c): the connect-time `SET_CONFIG(CAN_MIXED_FORMAT, ON)` step and its
//!   `ERR_NOT_SUPPORTED` rollback, the per-UUDT-id `PASS_FILTER` replacing
//!   the ADR-041 `FLOW_CONTROL_FILTER` workaround (including the
//!   `CLEAR_MSG_FILTERS` reinstall path), per-frame native `ProtocolID`
//!   routing on a single physical channel, and the `FD_ISO15765_PS`
//!   exclusion (ADR-159)
//! - [`sw_can`] — SAE J2534-2 clause 9 Single Wire CAN (SWCAN/GMLAN,
//!   ADR-164/Phase 4): connecting an SW resource row's mandatory internal
//!   `SET_CONFIG(CONFIG_J1962_PINS)`, the `CP_ChangeSpeed*` ComParam
//!   family's accept-all/translate-3 scope, `CP_SwCan_HighVoltage`'s
//!   `TX_FLAG_SW_CAN_HV_TX` wiring gated on the link's SW hw id,
//!   `SW_CAN_HS`/`SW_CAN_NS` IOCTL exposure (including the shared-channel
//!   no-op gate), and the SWCAN-vs-CAN-FD mutual-exclusion guard
//! - [`repeat_message`] — SAE J2534-2 clause 14 Repeat Messaging
//!   (ADR-165/Phase 12): `START`/`QUERY`/`STOP_REPEAT_MESSAGE` forwarding,
//!   the J2534-2 opt-in gate, per-CLL `MsgId` ownership across sibling CLLs
//!   sharing a physical channel, and slot teardown on
//!   `DestroyComLogicalLink`/`DisconnectComLogicalLink`
//! - [`ft_can`] — SAE J2534-2 clause 20 Fault-Tolerant CAN (ISO 11898-3,
//!   ADR-168/Phase 6): connecting an FT resource row's mandatory internal
//!   `SET_CONFIG(CONFIG_J1962_PINS)`, the row's own two-pin (1/HI, 9/LOW)
//!   default vs. an explicit caller override to the second documented
//!   pin-pair (3/11), and the FTCAN-vs-CAN-FD mutual-exclusion guard
//! - [`uart_echo_byte`] — SAE J2534-2 clause 12 UART Echo Byte Protocol
//!   (ADR-170/Phase 9): connecting the new standalone `UART_ECHO_BYTE_PS`
//!   resource row's mandatory internal `SET_CONFIG(CONFIG_J1962_PINS)`
//!   (default pin 7), the raw-id-without-pins rejection, the closed
//!   ComParam allowlist (clause 12.3.4.1), and the Repeat Messaging
//!   START/QUERY/STOP rejection (clause 12.3.3.1)
//! - [`j1939`] — SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5):
//!   the address-claim/defend state machine driven by `CoptStartcomm`
//!   (initial claim success, retry-over-the-`CP_J1939PreferredAddress`-list
//!   on a lost claim, and list-exhaustion failure), the
//!   `CP_J1939TargetAddress == 0xFFFF` StartComPrimitive-time rejection, the
//!   native `ERR_ADDRESS_NOT_CLAIMED` write failure surfacing as
//!   `PduErrEvtTxError`, and `tx_header::j1939_header_bytes`'s composed
//!   5-byte prefix reaching the wire
//! - [`ethernet_ndis`] — SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase
//!   16): connecting with each `CP_NdisPinOption` pin option, the native
//!   `ERR_NO_CONNECTION_ESTABLISHED` activation-failure mapping, the
//!   unconditional (no receive-only exemption) COP-type gate, the poll
//!   task's `rx_supported` RX-pass skip, `PDU_IOCTL_GET_NDIS_ADAPTER_INFO`,
//!   and the ADR-185 Stage 1 Discovery-gated connect rejection
//! - [`raw_mode`] — `CllCreateFlag` RawMode (ADR-196/Phase 1): TX header
//!   construction skipped and `TX_FLAG_CAN_29BIT_ID`/
//!   `TX_FLAG_ISO15765_ADDR_TYPE` made client-authoritative on base CAN/
//!   hardware ISO15765, the RX header/footer split skipped, and the Phase 1
//!   protocol-allowlist/malformed-flag rejections at `CreateComLogicalLink`

// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]

mod harness;

mod additional_channels;
mod analog_inputs;
mod can_mode;
mod clear_msg_filters;
mod comparam_tx;
mod concat;
mod connect_flags;
mod cop_ctrl_cycles;
mod cop_tag;
mod ethernet_ndis;
mod fault_tolerant_can;
mod fd_can;
mod fd_iso15765;
mod flow_control_filters;
mod ft_can;
mod gm_uart;
mod honda_diagh;
mod j1708;
mod j1850_autodetect;
mod j1850_raw_mode;
mod j1939;
mod j1939_raw_mode;
mod kline_raw_mode;
mod kline_source_address_routing;
mod lifecycle;
mod locks_and_param_classes;
mod mixed_format_can;
mod modules;
mod p3_gap;
mod param_binding;
mod pdu_ioctl;
mod pin_selection;
mod queue_error_suspend;
mod raw_mode;
mod rc_handling;
mod repeat_message;
mod resources;
mod response_distribution;
mod rx_header_split;
mod startcomm_comparam;
mod startcomm_optional_message_tx;
mod stopcomm_data_tx;
mod sw_can;
mod tester_present_addr_mode;
mod tester_present_message_length;
mod tester_present_reqrsp;
mod tester_present_send_type;
mod tp20;
mod uart_echo_byte;
mod unique_resp_id_table_binding;
mod vendor_passthrough;
