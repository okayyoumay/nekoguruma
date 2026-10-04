//! Service-level `ComParam` ID constants (0x8000-0x80FF) and the
//! `LockResource`/`UnlockResource` mask bits.
//!
//! These are the D-PDU API `CP_*` params that have no direct J2534 `SET_CONFIG`
//! equivalent — they exist only at the service level (tester-present,
//! error-handling/RC21/RC23/RC78, ISO 15765-2 transport timing, CAN/ECU
//! addressing, INIT, J1939 NAME, and physical-layer params). Native J2534
//! params (`DATA_RATE`, `LOOPBACK`, etc.) live in `j2534_0404` and are looked
//! up by name in `names::map_comparam_name_native`.
//!
//! Moved out of `service.rs` (the module root) to keep that file focused on
//! service state and RPC wiring rather than a ~400-line constant table.

use super::ComParamId;

/// Service-specific `SetComParam` ID for the tester-present message bytes (Bytefield).
///
/// Write a `Bytefield` with this ID before `CoptStartcomm` to enable periodic
/// tester-present transmission.  The bytes are forwarded verbatim as the
/// payload of a J2534 periodic message on the channel.
pub(super) const PARAM_TESTER_PRESENT_MSG: ComParamId = ComParamId(0x8001);

/// Service-specific `SetComParam` ID for the tester-present interval in microseconds (Unum32).
///
/// A value of 0 (default) disables tester-present even if a message is configured.
pub(super) const PARAM_TESTER_PRESENT_INTERVAL_US: ComParamId = ComParamId(0x8002);

// ── Tester-present configuration (0x8003-0x8009) ─────────────────────────────

/// Addressing mode for tester-present: 0 = physical, 1 = functional (D-PDU CP_TesterPresentAddrMode).
pub(super) const PARAM_TESTER_PRESENT_ADDR_MODE: ComParamId = ComParamId(0x8003);

/// Whether a positive response is expected for tester-present (D-PDU CP_TesterPresentExpPosResp).
pub(super) const PARAM_TESTER_PRESENT_EXP_POS_RESP: ComParamId = ComParamId(0x8004);

/// Whether a negative response is expected for tester-present (D-PDU CP_TesterPresentExpNegResp).
pub(super) const PARAM_TESTER_PRESENT_EXP_NEG_RESP: ComParamId = ComParamId(0x8005);

/// Tester-present error-handling mode (D-PDU CP_TesterPresentHandling).
pub(super) const PARAM_TESTER_PRESENT_HANDLING: ComParamId = ComParamId(0x8006);

/// Tester-present request/response mode (D-PDU CP_TesterPresentReqRsp).
pub(super) const PARAM_TESTER_PRESENT_REQ_RSP: ComParamId = ComParamId(0x8007);

/// Tester-present send type — physical or functional (D-PDU CP_TesterPresentSendType).
pub(super) const PARAM_TESTER_PRESENT_SEND_TYPE: ComParamId = ComParamId(0x8008);

/// ECU-side tester-present inter-message timing in ms (D-PDU CP_TesterPresentTime_Ecu).
pub(super) const PARAM_TESTER_PRESENT_TIME_ECU: ComParamId = ComParamId(0x8009);

// ── Timing params (0x8010-0x8019) ────────────────────────────────────────────

/// Timeout for cyclic-response primitives (D-PDU CP_CyclicRespTimeout): used
/// by a tier-2 (Receive Only, ADR-100 Decision §2) registrant created with
/// `NumReceiveCycles == -1` or a finite `N > 0` (ADR-100 Decision §4, scope
/// widened from `-1`-only by ADR-182; `-2` is unaffected) --
/// `PDU_COPST_FINISHED` is reached when no matching response arrives within
/// this timeout of the last one (ISO 22900-2 §9.2.6.3.4 RECEIVE ONLY NOTE 1)
/// -- for the finite-`N` subtype, that expiry with the target count still
/// unmet is additionally a `PduErrEvtRxTimeout` error path (ADR-182).
/// Despite this constant's
/// own historical "in ms" wording, the stored value is microseconds,
/// converted to ms by `ComParamSet::cyclic_resp_timeout_ms()`
/// (`get_us_as_ms`-style, matching `CP_P2Max`/the RC21/23/78 timing family,
/// ADR-053) -- not literal milliseconds. `0` (absent or explicit) means
/// "disabled, no cyclic timeout."
pub(super) const PARAM_CYCLIC_RESP_TIMEOUT: ComParamId = ComParamId(0x8010);

/// Extended tester-side response-waiting time P2* in ms (D-PDU CP_P2Star).
/// The per-occurrence reload window after receiving a ResponsePending
/// indication (e.g. RC 0x78): `RcHandlingConfig::from_params` reads this
/// as `rc78_p2_star_ms`, reloading the deadline on every 0x78 (ISO 14229-2
/// §7.3 P2*client semantics, ADR-102 -- supersedes ADR-056/057's
/// anchor-once treatment of this value). See `PARAM_RC78_COMPLETION_TIMEOUT`
/// for the independent total-duration ceiling.
pub(super) const PARAM_P2_STAR: ComParamId = ComParamId(0x8011);

/// ECU-side extended response-waiting time P2* in ms (D-PDU CP_P2Star_Ecu).
pub(super) const PARAM_P2_STAR_ECU: ComParamId = ComParamId(0x8012);

/// ECU-side P2Max timing in ms (D-PDU CP_P2Max_Ecu).
pub(super) const PARAM_P2_MAX_ECU: ComParamId = ComParamId(0x8013);

/// Enable/disable dynamic timing modification (D-PDU CP_ModifyTiming).
pub(super) const PARAM_MODIFY_TIMING: ComParamId = ComParamId(0x8014);

/// ECU-side session timing in ms (D-PDU CP_SessionTiming_Ecu).
pub(super) const PARAM_SESSION_TIMING_ECU: ComParamId = ComParamId(0x8015);

/// Session timing override in ms (D-PDU CP_SessionTimingOverride).
pub(super) const PARAM_SESSION_TIMING_OVERRIDE: ComParamId = ComParamId(0x8016);

/// CAN inter-frame transmission time in µs (D-PDU CP_CanTransmissionTime).
pub(super) const PARAM_CAN_TRANSMISSION_TIME: ComParamId = ComParamId(0x8017);

/// J1939 message indication rate in ms (D-PDU CP_MessageIndicationRate).
pub(super) const PARAM_MESSAGE_INDICATION_RATE: ComParamId = ComParamId(0x8018);

/// Delay before the speed-change message is transmitted in ms (D-PDU CP_ChangeSpeedTxDelay).
/// Accepted-but-unmapped (ADR-164/Phase 4): SAE J2534-2 clause 9 now covers
/// SWCAN (superseding ADR-017's rejection), but neither ISO 22900-2:2009
/// Annex A.1.2 Table A.3 nor clause 9's automatic-detection model documents
/// a native SET_CONFIG target for this ComParam -- it is get/settable
/// (`comparam_support::is_can_param`) but never translated to hardware
/// (`comparam_id::to_j2534_config_id` always returns `None` for it).
pub(super) const PARAM_CHANGE_SPEED_TX_DELAY: ComParamId = ComParamId(0x8019);

// ── Error-handling params (0x8020-0x802A) ────────────────────────────────────

/// Timeout for response-code 0x21 (ResponsePending) completion in ms (D-PDU CP_RC21CompletionTimeout).
pub(super) const PARAM_RC21_COMPLETION_TIMEOUT: ComParamId = ComParamId(0x8020);

/// Response-code 0x21 handling mode: 0 = pass through, 1 = auto re-request (D-PDU CP_RC21Handling).
pub(super) const PARAM_RC21_HANDLING: ComParamId = ComParamId(0x8021);

/// Delay before re-requesting after RC 0x21 in ms (D-PDU CP_RC21RequestTime).
pub(super) const PARAM_RC21_REQUEST_TIME: ComParamId = ComParamId(0x8022);

/// Timeout for response-code 0x23 completion in ms (D-PDU CP_RC23CompletionTimeout).
pub(super) const PARAM_RC23_COMPLETION_TIMEOUT: ComParamId = ComParamId(0x8023);

/// Response-code 0x23 handling mode (D-PDU CP_RC23Handling).
pub(super) const PARAM_RC23_HANDLING: ComParamId = ComParamId(0x8024);

/// Delay before re-requesting after RC 0x23 in ms (D-PDU CP_RC23RequestTime).
pub(super) const PARAM_RC23_REQUEST_TIME: ComParamId = ComParamId(0x8025);

/// Timeout for response-code 0x78 (CAN ResponsePending) completion in ms (D-PDU CP_RC78CompletionTimeout).
/// Reinstated (ADR-102) as an independent total-duration anti-stall
/// ceiling for the whole RC78 sequence, anchored once at the first 0x78:
/// `RcHandlingConfig::from_params` reads this as `rc78_total_ceiling_ms`
/// (`0`/absent disables the ceiling). Distinct from `PARAM_P2_STAR`, which
/// drives the per-occurrence deadline reload.
pub(super) const PARAM_RC78_COMPLETION_TIMEOUT: ComParamId = ComParamId(0x8026);

/// Response-code 0x78 handling mode (D-PDU CP_RC78Handling).
pub(super) const PARAM_RC78_HANDLING: ComParamId = ComParamId(0x8027);

/// Byte offset of the response code within a message (D-PDU CP_RCByteOffset).
pub(super) const PARAM_RC_BYTE_OFFSET: ComParamId = ComParamId(0x8028);

/// Maximum number of application-layer request retries (D-PDU CP_RepeatReqCountApp).
pub(super) const PARAM_REPEAT_REQ_COUNT_APP: ComParamId = ComParamId(0x8029);

/// Suspend TX queue on error flag (D-PDU CP_SuspendQueueOnError).
pub(super) const PARAM_SUSPEND_QUEUE_ON_ERROR: ComParamId = ComParamId(0x802A);

// ── COM params (0x8030-0x8037) ────────────────────────────────────────────────
// CP_ChangeSpeed* (0x8030-0x8033): ADR-017's original "not supported, J2534-1
// does not include SWCAN" rejection is superseded by ADR-164/Phase 4 -- SAE
// J2534-2 clause 9 now brings SWCAN into this service's scope, and all 5
// CP_ChangeSpeed* ComParams (this group plus CP_ChangeSpeedTxDelay, 0x8019)
// are accepted family-wide on the CAN family (`comparam_support::is_can_param`),
// matching a client's conformant SWCAN MDF round-tripping all 5 together.
// CP_ChangeSpeedCtrl/Rate/ResCtrl additionally translate to native SWCAN
// CONFIG ids on an SW link (`comparam_id::to_j2534_config_id`);
// CP_ChangeSpeedMsg/TxDelay have no documented native mapping and stay
// accepted-but-unmapped (see PARAM_CHANGE_SPEED_TX_DELAY's own doc comment).

/// Speed-change control flags (D-PDU CP_ChangeSpeedCtrl). ADR-164/Phase 4:
/// translates to `CONFIG_SW_CAN_SPEEDCHANGE_ENABLE` on an SW link.
pub(super) const PARAM_CHANGE_SPEED_CTRL: ComParamId = ComParamId(0x8030);

/// Speed-change message payload (D-PDU CP_ChangeSpeedMessage). Bytefield
/// param. ADR-164/Phase 4: accepted-but-unmapped (see
/// PARAM_CHANGE_SPEED_TX_DELAY's own doc comment) -- clause 9's
/// automatic-detection model has no configurable native target for this
/// ComParam.
pub(super) const PARAM_CHANGE_SPEED_MSG: ComParamId = ComParamId(0x8031);

/// Target baud rate for speed-change (D-PDU CP_ChangeSpeedRate). ADR-164/Phase
/// 4: translates to `CONFIG_SW_CAN_HS_DATA_RATE` on an SW link.
pub(super) const PARAM_CHANGE_SPEED_RATE: ComParamId = ComParamId(0x8032);

/// Speed-change response-control flags (D-PDU CP_ChangeSpeedResCtrl).
/// ADR-164/Phase 4: translates to `CONFIG_SW_CAN_RES_SWITCH` on an SW link.
pub(super) const PARAM_CHANGE_SPEED_RES_CTRL: ComParamId = ComParamId(0x8033);

/// Enable VCI performance-test mode (D-PDU CP_EnablePerformanceTest).
pub(super) const PARAM_ENABLE_PERFORMANCE_TEST: ComParamId = ComParamId(0x8034);

/// Enable start-of-message indications (D-PDU CP_StartMsgIndEnable).
pub(super) const PARAM_START_MSG_IND_ENABLE: ComParamId = ComParamId(0x8035);

/// Enable transmit indications (D-PDU CP_TransmitIndEnable).
pub(super) const PARAM_TRANSMIT_IND_ENABLE: ComParamId = ComParamId(0x8036);

/// SW-CAN high-voltage mode enable (D-PDU CP_SwCan_HighVoltage).
pub(super) const PARAM_SW_CAN_HIGH_VOLTAGE: ComParamId = ComParamId(0x8037);

// ── ISO 15765-2 / TP frame timing (0x8040-0x804F) ────────────────────────────

/// ISO 15765-2 N_Ar timer in ms — tester side (D-PDU CP_Ar).
pub(super) const PARAM_N_AR: ComParamId = ComParamId(0x8040);
/// ISO 15765-2 N_Ar timer in ms — ECU side (D-PDU CP_Ar_Ecu).
pub(super) const PARAM_N_AR_ECU: ComParamId = ComParamId(0x8041);
/// ISO 15765-2 N_As timer in ms — tester side (D-PDU CP_As).
pub(super) const PARAM_N_AS: ComParamId = ComParamId(0x8042);
/// ISO 15765-2 N_As timer in ms — ECU side (D-PDU CP_As_Ecu).
pub(super) const PARAM_N_AS_ECU: ComParamId = ComParamId(0x8043);
/// ISO 15765-2 N_Br timer in ms — tester side (D-PDU CP_Br).
pub(super) const PARAM_N_BR: ComParamId = ComParamId(0x8044);
/// ISO 15765-2 N_Br timer in ms — ECU side (D-PDU CP_Br_Ecu).
pub(super) const PARAM_N_BR_ECU: ComParamId = ComParamId(0x8045);
/// ISO 15765-2 N_Bs flow-control wait timer — tester side (D-PDU CP_Bs).
/// Distinct from `ISO15765_BS` which is the block-size count. Despite this
/// constant's own historical "in ms" wording, the stored value is
/// microseconds, converted to ms by `ComParamSet::isotp_n_bs_timeout_ms()`
/// (same `div_ceil`-based conversion as `CP_P2Max`/`PARAM_CYCLIC_RESP_
/// TIMEOUT`'s family) -- not literal milliseconds.
pub(super) const PARAM_N_BS: ComParamId = ComParamId(0x8046);
/// ISO 15765-2 N_Bs timer in ms — ECU side (D-PDU CP_Bs_Ecu).
pub(super) const PARAM_N_BS_ECU: ComParamId = ComParamId(0x8047);
/// ISO 15765-2 N_Cr consecutive-frame reception timer — tester side (D-PDU
/// CP_Cr). Despite this constant's own historical "in ms" wording, the
/// stored value is microseconds, converted to ms by `ComParamSet::
/// isotp_n_cr_timeout_ms()` (same `div_ceil`-based conversion as
/// `CP_P2Max`/`PARAM_CYCLIC_RESP_TIMEOUT`'s family) -- not literal
/// milliseconds.
pub(super) const PARAM_N_CR: ComParamId = ComParamId(0x8048);
/// ISO 15765-2 N_Cr timer in ms — ECU side (D-PDU CP_Cr_Ecu).
pub(super) const PARAM_N_CR_ECU: ComParamId = ComParamId(0x8049);
/// ISO 15765-2 N_Cs consecutive-frame send timer in ms — tester side (D-PDU CP_Cs).
pub(super) const PARAM_N_CS: ComParamId = ComParamId(0x804A);
/// ISO 15765-2 N_Cs timer in ms — ECU side (D-PDU CP_Cs_Ecu).
pub(super) const PARAM_N_CS_ECU: ComParamId = ComParamId(0x804B);
/// ECU-side STmin in ms (D-PDU CP_StMin_Ecu). Informational; not applied to hardware.
pub(super) const PARAM_ST_MIN_ECU: ComParamId = ComParamId(0x804C);
/// ECU-side block size (D-PDU CP_BlockSize_Ecu). Informational; not applied to hardware.
pub(super) const PARAM_BLOCK_SIZE_ECU: ComParamId = ComParamId(0x804D);
/// ECU-side access timing in ms (D-PDU CP_AccessTiming_Ecu).
pub(super) const PARAM_ACCESS_TIMING_ECU: ComParamId = ComParamId(0x804E);
/// Override for ECU access timing in ms (D-PDU CP_AccessTimingOverride).
pub(super) const PARAM_ACCESS_TIMING_OVERRIDE: ComParamId = ComParamId(0x804F);

// ── General transport timing (0x8050-0x8052) ──────────────────────────────────

/// Extended timing mode enable (D-PDU CP_ExtendedTiming).
pub(super) const PARAM_EXTENDED_TIMING: ComParamId = ComParamId(0x8050);
/// J1939 address-claim timeout in ms (D-PDU CP_J1939AddrClaimTimeout).
pub(super) const PARAM_J1939_ADDR_CLAIM_TIMEOUT: ComParamId = ComParamId(0x8051);
/// Maximum transport-layer request retries (D-PDU CP_RepeatReqCountTrans).
pub(super) const PARAM_REPEAT_REQ_COUNT_TRANS: ComParamId = ComParamId(0x8052);

// ── CAN addressing COM params (0x8060-0x806F) ────────────────────────────────

/// Physical request extended address (D-PDU CP_CanPhysReqExtAddr, UNIQUE_ID).
pub(super) const PARAM_CAN_PHYS_REQ_EXT_ADDR: ComParamId = ComParamId(0x8060);
/// Physical request frame format (D-PDU CP_CanPhysReqFormat, UNIQUE_ID).
pub(super) const PARAM_CAN_PHYS_REQ_FORMAT: ComParamId = ComParamId(0x8061);
/// Physical request CAN ID (D-PDU CP_CanPhysReqId, UNIQUE_ID).
pub(super) const PARAM_CAN_PHYS_REQ_ID: ComParamId = ComParamId(0x8062);
/// USDT response extended address (D-PDU CP_CanRespUSDTExtAddr, UNIQUE_ID).
pub(super) const PARAM_CAN_RESP_USDT_EXT_ADDR: ComParamId = ComParamId(0x8063);
/// USDT response frame format (D-PDU CP_CanRespUSDTFormat, UNIQUE_ID).
pub(super) const PARAM_CAN_RESP_USDT_FORMAT: ComParamId = ComParamId(0x8064);
/// USDT response CAN ID (D-PDU CP_CanRespUSDTId, UNIQUE_ID).
pub(super) const PARAM_CAN_RESP_USDT_ID: ComParamId = ComParamId(0x8065);
/// UUDT response extended address (D-PDU CP_CanRespUUDTExtAddr, UNIQUE_ID).
pub(super) const PARAM_CAN_RESP_UUDT_EXT_ADDR: ComParamId = ComParamId(0x8066);
/// UUDT response frame format (D-PDU CP_CanRespUUDTFormat, UNIQUE_ID).
pub(super) const PARAM_CAN_RESP_UUDT_FORMAT: ComParamId = ComParamId(0x8067);
/// UUDT response CAN ID (D-PDU CP_CanRespUUDTId, UNIQUE_ID).
pub(super) const PARAM_CAN_RESP_UUDT_ID: ComParamId = ComParamId(0x8068);
/// Functional request extended address (D-PDU CP_CanFuncReqExtAddr).
pub(super) const PARAM_CAN_FUNC_REQ_EXT_ADDR: ComParamId = ComParamId(0x8069);
/// Functional request frame format (D-PDU CP_CanFuncReqFormat).
pub(super) const PARAM_CAN_FUNC_REQ_FORMAT: ComParamId = ComParamId(0x806A);
/// Functional request CAN ID (D-PDU CP_CanFuncReqId).
pub(super) const PARAM_CAN_FUNC_REQ_ID: ComParamId = ComParamId(0x806B);
/// CAN data-size byte offset for non-ISO-TP frames (D-PDU CP_CanDataSizeOffset).
pub(super) const PARAM_CAN_DATA_SIZE_OFFSET: ComParamId = ComParamId(0x806C);
/// CAN padding/filler byte value (D-PDU CP_CanFillerByte).
pub(super) const PARAM_CAN_FILLER_BYTE: ComParamId = ComParamId(0x806D);
/// CAN filler-byte handling mode (D-PDU CP_CanFillerByteHandling).
pub(super) const PARAM_CAN_FILLER_BYTE_HANDLING: ComParamId = ComParamId(0x806E);
/// Initial SN value of the first consecutive frame (D-PDU CP_CanFirstConsecutiveFrameValue).
pub(super) const PARAM_CAN_FIRST_CF_VALUE: ComParamId = ComParamId(0x806F);

// ── ECU addressing COM params (0x8070-0x808C) ────────────────────────────────

/// ECU response source address (D-PDU CP_EcuRespSourceAddress, UNIQUE_ID).
pub(super) const PARAM_ECU_RESP_SOURCE_ADDR: ComParamId = ComParamId(0x8070);
/// Functional request format/priority type (D-PDU CP_FuncReqFormatPriorityType).
pub(super) const PARAM_FUNC_REQ_FORMAT_PRIORITY: ComParamId = ComParamId(0x8071);
/// Functional request target address (D-PDU CP_FuncReqTargetAddr).
pub(super) const PARAM_FUNC_REQ_TARGET_ADDR: ComParamId = ComParamId(0x8072);
/// Functional response format/priority type (D-PDU CP_FuncRespFormatPriorityType, UNIQUE_ID).
pub(super) const PARAM_FUNC_RESP_FORMAT_PRIORITY: ComParamId = ComParamId(0x8073);
/// Functional response target address (D-PDU CP_FuncRespTargetAddr, UNIQUE_ID).
pub(super) const PARAM_FUNC_RESP_TARGET_ADDR: ComParamId = ComParamId(0x8074);
/// Physical request format/priority type (D-PDU CP_PhysReqFormatPriorityType).
pub(super) const PARAM_PHYS_REQ_FORMAT_PRIORITY: ComParamId = ComParamId(0x8075);
/// Physical request target address (D-PDU CP_PhysReqTargetAddr).
pub(super) const PARAM_PHYS_REQ_TARGET_ADDR: ComParamId = ComParamId(0x8076);
/// Physical response format/priority type (D-PDU CP_PhysRespFormatPriorityType, UNIQUE_ID).
pub(super) const PARAM_PHYS_RESP_FORMAT_PRIORITY: ComParamId = ComParamId(0x8077);
/// Request addressing mode — physical or functional (D-PDU CP_RequestAddrMode).
pub(super) const PARAM_REQUEST_ADDR_MODE: ComParamId = ComParamId(0x8078);
/// J1850 header format (D-PDU CP_HeaderFormatJ1850).
pub(super) const PARAM_HEADER_FORMAT_J1850: ComParamId = ComParamId(0x8079);
/// KW header format (D-PDU CP_HeaderFormatKW).
pub(super) const PARAM_HEADER_FORMAT_KW: ComParamId = ComParamId(0x807A);
/// Enable message concatenation (D-PDU CP_EnableConcatenation).
pub(super) const PARAM_ENABLE_CONCATENATION: ComParamId = ComParamId(0x807B);
/// K-line/J1850 padding/filler byte value (D-PDU CP_FillerByte).
pub(super) const PARAM_FILLER_BYTE: ComParamId = ComParamId(0x807C);
/// Filler-byte handling mode (D-PDU CP_FillerByteHandling).
pub(super) const PARAM_FILLER_BYTE_HANDLING: ComParamId = ComParamId(0x807D);
/// Number of filler bytes to append (D-PDU CP_FillerByteLength).
pub(super) const PARAM_FILLER_BYTE_LENGTH: ComParamId = ComParamId(0x807E);
/// 5-baud init functional address (D-PDU CP_5BaudAddressFunc).
pub(super) const PARAM_5BAUD_ADDR_FUNC: ComParamId = ComParamId(0x807F);
/// 5-baud init physical address (D-PDU CP_5BaudAddressPhys).
pub(super) const PARAM_5BAUD_ADDR_PHYS: ComParamId = ComParamId(0x8080);
/// Enable CAN remote-frame transmission (D-PDU CP_SendRemoteFrame).
pub(super) const PARAM_SEND_REMOTE_FRAME: ComParamId = ComParamId(0x8081);
/// TP connection-management mode (D-PDU CP_TPConnectionManagement).
pub(super) const PARAM_TP_CONNECTION_MGMT: ComParamId = ComParamId(0x8082);
/// Message priority for J1939/J1708 (D-PDU CP_MessagePriority).
pub(super) const PARAM_MESSAGE_PRIORITY: ComParamId = ComParamId(0x8083);
/// J1708 MID for request frames (D-PDU CP_MidReqId).
pub(super) const PARAM_MID_REQ_ID: ComParamId = ComParamId(0x8084);
/// J1708 MID for response frames (D-PDU CP_MidRespId, UNIQUE_ID).
pub(super) const PARAM_MID_RESP_ID: ComParamId = ComParamId(0x8085);
/// J1939 address-negotiation rule (D-PDU CP_J1939AddressNegotiationRule).
pub(super) const PARAM_J1939_ADDR_NEG_RULE: ComParamId = ComParamId(0x8086);
/// J1939 data page bit (D-PDU CP_J1939DataPage).
pub(super) const PARAM_J1939_DATA_PAGE: ComParamId = ComParamId(0x8087);
/// Maximum J1939 packets per transmission (D-PDU CP_J1939MaxPacketTx).
pub(super) const PARAM_J1939_MAX_PACKET_TX: ComParamId = ComParamId(0x8088);
/// J1939 PDU format byte (D-PDU CP_J1939PDUFormat).
pub(super) const PARAM_J1939_PDU_FORMAT: ComParamId = ComParamId(0x8089);
/// J1939 PDU specific byte (D-PDU CP_J1939PDUSpecific).
pub(super) const PARAM_J1939_PDU_SPECIFIC: ComParamId = ComParamId(0x808A);
/// J1939 source address (D-PDU CP_J1939SourceAddress, UNIQUE_ID).
pub(super) const PARAM_J1939_SOURCE_ADDRESS: ComParamId = ComParamId(0x808B);
/// J1939 target address (D-PDU CP_J1939TargetAddress).
pub(super) const PARAM_J1939_TARGET_ADDRESS: ComParamId = ComParamId(0x808C);

// ── INIT params (0x8090-0x8093) ───────────────────────────────────────────────

/// ISO 9141/14230 initialization settings byte (D-PDU CP_InitializationSettings).
pub(super) const PARAM_INIT_SETTINGS: ComParamId = ComParamId(0x8090);
/// SCI transmit mode (D-PDU CP_SCITransmitMode).
pub(super) const PARAM_SCI_TRANSMIT_MODE: ComParamId = ComParamId(0x8091);
/// Preferred J1939 source address (D-PDU CP_J1939PreferredAddress).
pub(super) const PARAM_J1939_PREFERRED_ADDRESS: ComParamId = ComParamId(0x8092);
/// ECU preferred J1939 source address (D-PDU CP_J1939PreferredAddress_Ecu).
pub(super) const PARAM_J1939_PREFERRED_ADDRESS_ECU: ComParamId = ComParamId(0x8093);

// ── J1939 NAME Bytefield params (0x8094-0x8097) ───────────────────────────────
// 64-bit J1939 NAME values; stored in ComParamSet::bytes (8 bytes each).

/// J1939 NAME of the tester node (D-PDU CP_J1939Name). Bytefield (8 bytes).
pub(super) const PARAM_J1939_NAME: ComParamId = ComParamId(0x8094);
/// J1939 NAME of the ECU node (D-PDU CP_J1939Name_Ecu). Bytefield (8 bytes).
pub(super) const PARAM_J1939_NAME_ECU: ComParamId = ComParamId(0x8095);
/// J1939 source NAME for response matching (D-PDU CP_J1939SourceName, UNIQUE_ID). Bytefield.
pub(super) const PARAM_J1939_SOURCE_NAME: ComParamId = ComParamId(0x8096);
/// J1939 target NAME (D-PDU CP_J1939TargetName). Bytefield (8 bytes).
pub(super) const PARAM_J1939_TARGET_NAME: ComParamId = ComParamId(0x8097);

// ── Physical layer service params (0x80A0-0x80A9) ────────────────────────────
// Physical layer params with no direct J2534 SET_CONFIG equivalent are stored
// at the service level.  Params that DO have a J2534 equivalent (CP_Baudrate →
// DATA_RATE, CP_BitSamplePoint → BIT_SAMPLE_POINT, CP_SyncJumpWidth →
// SYNC_JUMP_WIDTH, CP_NetworkLine → NETWORK_LINE, CP_UartConfig → DATA_BITS)
// are forwarded directly to the adapter and are NOT stored here.

/// ECU-side CAN bit sample point in % (D-PDU CP_BitSamplePoint_Ecu).
pub(super) const PARAM_BIT_SAMPLE_POINT_ECU: ComParamId = ComParamId(0x80A0);
/// Number of samples per CAN bit period for the tester (D-PDU CP_SamplesPerBit).
/// J2534 has no equivalent; stored at service level only.
pub(super) const PARAM_SAMPLES_PER_BIT: ComParamId = ComParamId(0x80A1);
/// ECU-side number of samples per CAN bit period (D-PDU CP_SamplesPerBit_Ecu).
pub(super) const PARAM_SAMPLES_PER_BIT_ECU: ComParamId = ComParamId(0x80A2);
/// ECU-side CAN sync jump width in % (D-PDU CP_SyncJumpWidth_Ecu).
pub(super) const PARAM_SYNC_JUMP_WIDTH_ECU: ComParamId = ComParamId(0x80A3);
/// CAN listen-only mode: 1 = listen only, 0 = normal (D-PDU CP_ListenOnly).
/// Different from J2534 LOOPBACK (which echoes TX into the RX queue for testing).
pub(super) const PARAM_LISTEN_ONLY: ComParamId = ComParamId(0x80A4);
/// Recorded CAN bus timing parameters (D-PDU CP_CanBaudrateRecord). Bytefield.
pub(super) const PARAM_CAN_BAUDRATE_RECORD: ComParamId = ComParamId(0x80A5);
/// K/L line initialization control (D-PDU CP_K_L_LineInit).
pub(super) const PARAM_K_L_LINE_INIT: ComParamId = ComParamId(0x80A6);
/// K line pull-up resistor enable (D-PDU CP_K_LinePullup).
pub(super) const PARAM_K_LINE_PULLUP: ComParamId = ComParamId(0x80A7);
/// Bus termination type for the tester (D-PDU CP_TerminationType).
pub(super) const PARAM_TERMINATION_TYPE: ComParamId = ComParamId(0x80A8);
/// ECU-side bus termination type (D-PDU CP_TerminationType_Ecu).
pub(super) const PARAM_TERMINATION_TYPE_ECU: ComParamId = ComParamId(0x80A9);

// ── CAN FD physical layer (service-level; J2534-0404 has no CAN FD channel) ──

/// CAN FD data phase baud rate in bps (D-PDU CP_CANFDBaudrate). Service-level only.
pub(super) const PARAM_CANFD_BAUDRATE: ComParamId = ComParamId(0x80AA);
/// CAN FD data phase bit sample point in % (D-PDU CP_CANFDBitSamplePoint). Service-level only.
pub(super) const PARAM_CANFD_BIT_SAMPLE_POINT: ComParamId = ComParamId(0x80AB);
/// CAN FD data phase sync jump width in % (D-PDU CP_CANFDSyncJumpWidth). Service-level only.
pub(super) const PARAM_CANFD_SYNC_JUMP_WIDTH: ComParamId = ComParamId(0x80AC);

// ── J1850 IFR control (service-level) ────────────────────────────────────────

/// J1850 In-Frame Response enable: 1 = IFR enabled, 0 = disabled (D-PDU CP_J1850IFRCtrl).
/// Service-level only; J2534-0404 has no IFR control parameter.
pub(super) const PARAM_J1850_IFR_CTRL: ComParamId = ComParamId(0x80AD);

// ── Protocol-layer service params (0x80AE-0x80C3) ────────────────────────────

/// Disable transport-layer checksum verification (D-PDU CP_DisableTransportChecksumCheck).
pub(super) const PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK: ComParamId = ComParamId(0x80AE);

/// Protocol test mode (D-PDU CP_TestMode).
pub(super) const PARAM_TEST_MODE: ComParamId = ComParamId(0x80AF);

/// Enable repetition of the initialization sequence (D-PDU CP_EnableInitSeqRepetition).
pub(super) const PARAM_ENABLE_INIT_SEQ_REPETITION: ComParamId = ComParamId(0x80B0);

/// Send tester-present immediately at CoptStartcomm (D-PDU CP_TesterPresentImmed).
pub(super) const PARAM_TESTER_PRESENT_IMMED: ComParamId = ComParamId(0x80B1);

/// Number of header bytes at start-comm for KWP protocols (D-PDU CP_NumHeaderBytesStartCommKW).
pub(super) const PARAM_NUM_HEADER_BYTES_START_COMM_KW: ComParamId = ComParamId(0x80B2);

/// P3 functional inter-frame gap (service-level; CAN context, D-PDU CP_P3Func).
/// KWP protocols forward P3 via j2534_0404::P3_MIN/P3_MAX.
pub(super) const PARAM_P3_FUNC: ComParamId = ComParamId(0x80B3);

/// P3 physical inter-frame gap (service-level; CAN context, D-PDU CP_P3Phys).
pub(super) const PARAM_P3_PHYS: ComParamId = ComParamId(0x80B4);

/// 5-baud communication baud-rate override (D-PDU CP_5BaudCommBaudrateOverride).
pub(super) const PARAM_5BAUD_COMM_BAUDRATE_OVERRIDE: ComParamId = ComParamId(0x80B5);

/// 5-baud initialization baud rate (D-PDU CP_5BaudInitBaudrate).
pub(super) const PARAM_5BAUD_INIT_BAUDRATE: ComParamId = ComParamId(0x80B6);

/// ISO key-byte count for KWP initialization (D-PDU CP_ISOKeybyteCount).
pub(super) const PARAM_ISO_KEYBYTE_COUNT: ComParamId = ComParamId(0x80B7);

/// Ignore checksum errors (D-PDU CP_IgnoreChecksum).
pub(super) const PARAM_IGNORE_CHECKSUM: ComParamId = ComParamId(0x80B8);

/// CAN mixed-frame format mode (D-PDU CP_CanMixedFormat).
pub(super) const PARAM_CAN_MIXED_FORMAT: ComParamId = ComParamId(0x80B9);

/// CAN FD maximum TX data length (D-PDU CP_CANFDTxMaxDataLength).
pub(super) const PARAM_CANFD_TX_MAX_DATA_LENGTH: ComParamId = ComParamId(0x80BA);

/// ISO 15765-2 escape-sequence handling mode (D-PDU CP_EscapeSequenceHandling).
pub(super) const PARAM_ESCAPE_SEQUENCE_HANDLING: ComParamId = ComParamId(0x80BB);

/// Maximum ECU data length in bytes (D-PDU CP_MaxDataLength_Ecu).
pub(super) const PARAM_MAX_DATA_LENGTH_ECU: ComParamId = ComParamId(0x80BC);

/// Maximum CTS requests per J1587 TP session (D-PDU CP_MaxCTSReq).
pub(super) const PARAM_MAX_CTS_REQ: ComParamId = ComParamId(0x80BD);

/// J1587 collision-test mode enable (D-PDU CP_CollisionTestMode).
pub(super) const PARAM_COLLISION_TEST_MODE: ComParamId = ComParamId(0x80BE);

// ADR-179 (Phase 5, closing the gap PR #71 left open): `CP_T3Max`/`CP_T4Max`/
// `CP_T5Max` are each a single ISO 22900-2 ComParam with a per-protocol
// default (SAE_J2610_SCI vs. SAE_J1939_21), not two distinct constants --
// this file previously minted separate `PARAM_T3_MAX`/`_T4_MAX`/`_T5_MAX`
// (0x80BF-0x80C1) for the J1939/J1587 transport-timer context, duplicating
// the pre-existing native `j2534_0404::T3_MAX`/`T4_MAX`/`T5_MAX` SCI-context
// mapping `names::map_comparam_name_native` already used under the same
// D-PDU shortnames (`cp_t3max`/`cp_t4max`/`cp_t5max`). Every former
// reference to these three constants now uses
// `ComParamId(j2534_0404::T3_MAX/T4_MAX/T5_MAX)` directly instead.

/// SCI programming-voltage setting (D-PDU CP_SCISetProgVoltage).
pub(super) const PARAM_SCI_SET_PROG_VOLTAGE: ComParamId = ComParamId(0x80C2);

/// SCI ECU-simulator mode (D-PDU CP_SCIEcuSimulator).
pub(super) const PARAM_SCI_ECU_SIMULATOR: ComParamId = ComParamId(0x80C3);

/// SAE J2534-2 clause 10 Analog Inputs acquisition sample rate (ADR-178) --
/// project-invented, no ISO 22900-2 source (see ADR-178's Decision section
/// for why this is the one case where minting a new service-level ComParam
/// id was necessary rather than reusing/deferring, per ADR-170/176/177's
/// prior "first mint" deferrals). Staged via SetComParam, resolved at
/// ConnectComLogicalLink time (replaces the removed
/// `CreateComLogicalLinkRequest.analog_sample_rate` request field).
pub(super) const PARAM_ANALOG_SAMPLE_RATE: ComParamId = ComParamId(0x80C4);

/// ISO9141 W1 timer, MINIMUM side (D-PDU CP_W1Min) -- ADR-181. SAE
/// J2534-1's Figure 30 defines only a single MAX-side native `W1` register
/// (`ComParamId(j2534_0404::W1)`, already used by `CP_W1Max`); ISO 22900-2
/// defines `CP_W1Min` as its own independent ComParam with no native
/// counterpart at all. Store-only: `ComParamId::to_j2534_config_id`'s
/// catch-all never forwards it.
pub(super) const PARAM_W1_MIN: ComParamId = ComParamId(0x80C5);

/// ISO9141 W2 timer, MINIMUM side (D-PDU CP_W2Min) -- ADR-181. Same
/// rationale as [`PARAM_W1_MIN`]: no native `W2` MIN-side register exists,
/// so this is store-only.
pub(super) const PARAM_W2_MIN: ComParamId = ComParamId(0x80C6);

/// ISO9141 W3 timer, MINIMUM side (D-PDU CP_W3Min) -- ADR-181. Same
/// rationale as [`PARAM_W1_MIN`]: no native `W3` MIN-side register exists,
/// so this is store-only.
pub(super) const PARAM_W3_MIN: ComParamId = ComParamId(0x80C7);

/// ISO9141 W4 timer, MAXIMUM side (D-PDU CP_W4Max) -- ADR-181. SAE
/// J2534-1's Figure 30 defines only a single MIN-side native `W4` register
/// (`ComParamId(j2534_0404::W4)`, already used by `CP_W4Min`); ISO 22900-2
/// defines `CP_W4Max` as its own independent ComParam with no native
/// counterpart at all. Store-only: `ComParamId::to_j2534_config_id`'s
/// catch-all never forwards it.
pub(super) const PARAM_W4_MAX: ComParamId = ComParamId(0x80C8);

/// SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a): the setup CAN ID
/// a `CoptStartcomm` request packs into Table 78 bytes 0-3 of the
/// `IOCTL_REQUEST_CONNECTION` SBYTE_ARRAY -- project-minted, no ISO 22900-2
/// source (the `CP_AnalogSampleRate` naming/id-range convention, ADR-178).
/// Store-only (`ComParamId::to_j2534_config_id` returns `None`): consumed
/// directly by `events_tp20_connection.rs`, never forwarded to a native
/// `SET_CONFIG`. No spec-mandated default -- a client must stage this before
/// `CoptStartcomm`.
pub(super) const PARAM_TP20_CHANNEL_SETUP_CAN_ID: ComParamId = ComParamId(0x80C9);

/// SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a): the destination
/// address packed into Table 78 byte 4 of the `IOCTL_REQUEST_CONNECTION`
/// SBYTE_ARRAY. Same store-only/no-default shape as
/// [`PARAM_TP20_CHANNEL_SETUP_CAN_ID`].
pub(super) const PARAM_TP20_DESTINATION_ADDRESS: ComParamId = ComParamId(0x80CA);

/// SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a): the proposed
/// TX-ID packed into Table 78 bytes 6-7 of the `IOCTL_REQUEST_CONNECTION`
/// SBYTE_ARRAY. Same store-only/no-default shape as
/// [`PARAM_TP20_CHANNEL_SETUP_CAN_ID`].
pub(super) const PARAM_TP20_TX_ID_PROPOSAL: ComParamId = ComParamId(0x80CB);

/// SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a): the proposed
/// RX-ID packed into Table 78 bytes 8-9 of the `IOCTL_REQUEST_CONNECTION`
/// SBYTE_ARRAY -- also the key `handle_start_comm` registers this CLL's
/// per-physical-channel routing-map entry under. Same store-only/no-default
/// shape as [`PARAM_TP20_CHANNEL_SETUP_CAN_ID`].
pub(super) const PARAM_TP20_RX_ID_PROPOSAL: ComParamId = ComParamId(0x80CC);

/// SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a): the application
/// type packed into Table 78 byte 10 of the `IOCTL_REQUEST_CONNECTION`
/// SBYTE_ARRAY. Same store-only/no-default shape as
/// [`PARAM_TP20_CHANNEL_SETUP_CAN_ID`].
pub(super) const PARAM_TP20_APPLICATION_TYPE: ComParamId = ComParamId(0x80CD);

/// SAE J2534-2 clause 19.3.1/Table 77 TP2.0 passive connections (ADR-190/
/// Phase 7 Stage 7b): the CAN ID the adapter uses to recognize an inbound
/// connection request addressed to it -- project-minted, no ISO 22900-2
/// source (the `CP_TP20*` Stage-7a naming/id-range convention, ADR-188
/// section 4). Service-level-only: `ComParamId::to_j2534_config_id` returns
/// `None` for this id -- deliberately NOT routed through the generic
/// ComParam-to-`SET_CONFIG` pipeline despite the native
/// `CONFIG_TP2_0_IDENTIFER` (`0x804E`) constant already existing, since
/// ordinary ComParam application (connect-time, or a `CoptUpdateparam`)
/// would otherwise arm/re-arm the device-side listener outside this ADR's
/// own arm/disarm lifecycle and exclusivity gate. `handle_start_comm`'s
/// passive arm applies this directly via its own native `SET_CONFIG` call
/// when arming (ADR-190 section 1/3). No spec-mandated default -- a client
/// must stage this (and [`PARAM_TP20_PASSIVE_RX_ID`]) before `CoptStartcomm`
/// to arm the passive listener; staging neither means "don't arm passive."
pub(super) const PARAM_TP20_PASSIVE_IDENTIFIER: ComParamId = ComParamId(0x80CE);

/// SAE J2534-2 clause 19.3.1/Table 77 TP2.0 passive connections (ADR-190/
/// Phase 7 Stage 7b): the CAN ID that becomes the passive connection's own
/// RX-ID once established -- also the key the passive arm registers this
/// interface's persistent routing-map entry under (ADR-190 section 3). Same
/// service-level-only/never-forwarded-to-native-`SET_CONFIG` shape as
/// [`PARAM_TP20_PASSIVE_IDENTIFIER`] (corresponding native constant
/// `CONFIG_TP2_0_RXIDPASSIVE`, `0x804F`), for the identical reason: routing
/// it through the generic pipeline would arm/re-arm the listener outside
/// ADR-190's own arm/disarm lifecycle.
pub(super) const PARAM_TP20_PASSIVE_RX_ID: ComParamId = ComParamId(0x80CF);

/// SAE J2534-2 clause 19.3.2.2 TP2.0 broadcast send (ADR-192/Phase 7 Stage
/// 7c): the broadcast address for this send -- `0` (default) = normal send,
/// `0xF0`-`0xFF` = send as a TP2.0 broadcast to that address. Project-minted,
/// no ISO 22900-2 source. Service-level-only: `ComParamId::to_j2534_config_id`
/// returns `None` -- there is no native `SET_CONFIG` parameter for a
/// per-message address. Deliberately per-send-scoped via the existing ADR-067
/// `temp_param_update` mechanism (stage into Working, then a `CoptSendrecv`
/// with `temp_param_update` set) rather than link-invariant like every other
/// ComParam-derived TxFlags bit this service resolves (`sci_tx_flags`/
/// `sw_can_tx_flags`/`msg_priority_tx_flags`) -- see ADR-192 Context for why
/// this one is not a link-wide fact.
pub(super) const PARAM_TP20_BROADCAST_ADDRESS: ComParamId = ComParamId(0x80D0);

/// SAE J2534-2 clause 19.3.1/Table 77 TP2.0 broadcast interval (ADR-192/
/// Phase 7 Stage 7c): native `CONFIG_TP2_0_T_BR_INT` (`0x8044`), the gap
/// separating the five messages that make up one broadcast burst.
/// Genuinely a physical-layer, connect-time-resolvable setting (applies
/// uniformly to every broadcast burst this physical channel ever sends) --
/// unlike `PARAM_TP20_BROADCAST_ADDRESS` above, this one *is* routed through
/// the generic ComParam-to-`SET_CONFIG` pipeline, like `DATA_RATE`/
/// `LOOPBACK`/etc. Default 20 (Table 77's own default, ms).
pub(super) const PARAM_TP20_BROADCAST_INTERVAL: ComParamId = ComParamId(0x80D1);

/// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): the J1962 pin
/// option to connect with -- `0` (default) = auto (neither
/// `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2` bit set), `1` = Option 1
/// (`CONNECT_FLAG_NDIS_PINS_OPTION1`), `2` = Option 2
/// (`CONNECT_FLAG_NDIS_PINS_OPTION2`). Project-minted, no ISO 22900-2
/// source (the `CP_AnalogSampleRate`/`CP_TP20*` naming/id-range convention,
/// ADR-178/ADR-188). Unlike those, though, `0` is spec-functional (the
/// native table treats an auto/neither-bit connect as valid, clause 24's own
/// Option 1/2 wording), so this ComParam is genuinely optional -- staged via
/// `SetComParam`, resolved at `ConnectComLogicalLink` time
/// (`rpc_link.rs::connect_flags`) directly into the native connect flags;
/// `ComParamId::to_j2534_config_id` returns `None` for it (store-only, never
/// forwarded to a native `SET_CONFIG`, the same shape TP2.0's Stage 7a setup
/// ComParams use).
pub(super) const PARAM_NDIS_PIN_OPTION: ComParamId = ComParamId(0x80D2);

/// SAE J2534-2 clause 10 Analog Inputs (ADR-216): the active-channel bitmask
/// -- project-invented, no ISO 22900-2 source (the `CP_AnalogSampleRate`
/// naming/id-range convention, ADR-178). Native `CONFIG_ACTIVE_CHANNELS`
/// (`0x8020`). Unlike `PARAM_ANALOG_SAMPLE_RATE`, this one is populated by a
/// one-time connect-time `GET_CONFIG` readback (ADR-216 Decision item 6) in
/// addition to being client-writable via `SetComParam` -- clause 10.3.3.2.1's
/// own default is device-dependent, so a fresh connect's readback overwrites
/// whatever was staged (or left at its `0`/not-yet-known seed) beforehand.
pub(super) const PARAM_ANALOG_ACTIVE_CHANNELS: ComParamId = ComParamId(0x80D3);

/// SAE J2534-2 clause 10 Analog Inputs (ADR-216): samples averaged per
/// reading -- project-invented, no ISO 22900-2 source. Native
/// `CONFIG_SAMPLES_PER_READING` (`0x8022`). Cannot be re-staged via
/// `CoptUpdateparam` on an already-connected analog link (ADR-216 Decision
/// item 10) -- clause 10.3.3.2.3's own rate-must-be-zero requirement means
/// the device rejects any post-connect change while `CP_AnalogSampleRate`
/// stays connect-time-latched nonzero.
pub(super) const PARAM_ANALOG_SAMPLES_PER_READING: ComParamId = ComParamId(0x80D4);

/// SAE J2534-2 clause 10 Analog Inputs (ADR-216): readings packed per
/// message -- project-invented, no ISO 22900-2 source. Native
/// `CONFIG_READINGS_PER_MSG` (`0x8023`). Same `CoptUpdateparam` re-stage
/// restriction as [`PARAM_ANALOG_SAMPLES_PER_READING`] (ADR-216 Decision
/// item 10), for the identical clause 10.3.3.2.4 reason.
pub(super) const PARAM_ANALOG_READINGS_PER_MSG: ComParamId = ComParamId(0x80D5);

/// SAE J2534-2 clause 10 Analog Inputs (ADR-216): the averaging method
/// applied before a reading is reported -- project-invented, no ISO 22900-2
/// source. Native `CONFIG_AVERAGING_METHOD` (`0x8024`).
pub(super) const PARAM_ANALOG_AVERAGING_METHOD: ComParamId = ComParamId(0x80D6);

/// SAE J2534-2 clause 10 Analog Inputs (ADR-216): the device's fixed sample
/// resolution in bits -- project-invented, no ISO 22900-2 source. Native
/// `CONFIG_SAMPLE_RESOLUTION` (`0x8025`), read-only: rejects any
/// `SetComParam` attempt outright (ADR-216 Decision item 5) and is populated
/// only by the connect-time `GET_CONFIG` readback (ADR-216 Decision item 6)
/// -- reads `0` before this CLL's first successful `ConnectComLogicalLink`.
pub(super) const PARAM_ANALOG_SAMPLE_RESOLUTION: ComParamId = ComParamId(0x80D7);

/// SAE J2534-2 clause 10 Analog Inputs (ADR-216): the low end of the input
/// voltage range, in millivolts, native two's-complement 32-bit encoding --
/// project-invented, no ISO 22900-2 source. Native `CONFIG_INPUT_RANGE_LOW`
/// (`0x8026`), read-only (same rejection/readback-only shape as
/// [`PARAM_ANALOG_SAMPLE_RESOLUTION`]) and signed: `GetComParam` reports this
/// one via the proto's `ParamData::Snum32` oneof arm instead of `Unum32`
/// (ADR-216 Decision item 8) -- the first signed-value-reporting ComParam in
/// this codebase.
pub(super) const PARAM_ANALOG_INPUT_RANGE_LOW: ComParamId = ComParamId(0x80D8);

/// SAE J2534-2 clause 10 Analog Inputs (ADR-216): the high end of the input
/// voltage range, in millivolts, native two's-complement 32-bit encoding --
/// project-invented, no ISO 22900-2 source. Native `CONFIG_INPUT_RANGE_HIGH`
/// (`0x8027`). Same read-only/readback-only/signed-reporting shape as
/// [`PARAM_ANALOG_INPUT_RANGE_LOW`].
pub(super) const PARAM_ANALOG_INPUT_RANGE_HIGH: ComParamId = ComParamId(0x80D9);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T0_MIN` (ADR-216) --
/// project-invented, no ISO 22900-2 source (the `CP_AnalogSampleRate`
/// naming/id-range convention, ADR-178). Native `CONFIG_UEB_T0_MIN`
/// (`0x8028`). Unit is native-verbatim whole milliseconds -- NOT the 1 us
/// ISO 22900-2 `CP_*` timing convention every other timing ComParam in this
/// service uses (ADR-216 Decision item 2); documented in
/// `docs/rpc-api-guide.md` so a client does not assume the ISO convention
/// applies here.
pub(super) const PARAM_UEB_T0_MIN: ComParamId = ComParamId(0x80DA);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T1_MAX` (ADR-216). Same
/// project-invented/native-millisecond shape as [`PARAM_UEB_T0_MIN`]. Native
/// `CONFIG_UEB_T1_MAX` (`0x8029`).
pub(super) const PARAM_UEB_T1_MAX: ComParamId = ComParamId(0x80DB);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T2_MAX` (ADR-216). Same
/// project-invented/native-millisecond shape as [`PARAM_UEB_T0_MIN`]. Native
/// `CONFIG_UEB_T2_MAX` (`0x802A`).
pub(super) const PARAM_UEB_T2_MAX: ComParamId = ComParamId(0x80DC);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T3_MAX` (ADR-216). Same
/// project-invented/native-millisecond shape as [`PARAM_UEB_T0_MIN`]. Native
/// `CONFIG_UEB_T3_MAX` (`0x802B`).
pub(super) const PARAM_UEB_T3_MAX: ComParamId = ComParamId(0x80DD);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T4_MIN` (ADR-216). Same
/// project-invented/native-millisecond shape as [`PARAM_UEB_T0_MIN`]. Native
/// `CONFIG_UEB_T4_MIN` (`0x802C`).
pub(super) const PARAM_UEB_T4_MIN: ComParamId = ComParamId(0x80DE);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T5_MAX` (ADR-216). Same
/// project-invented/native-millisecond shape as [`PARAM_UEB_T0_MIN`]. Native
/// `CONFIG_UEB_T5_MAX` (`0x802D`).
pub(super) const PARAM_UEB_T5_MAX: ComParamId = ComParamId(0x80DF);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T6_MAX` (ADR-216). Same
/// project-invented/native-millisecond shape as [`PARAM_UEB_T0_MIN`]. Native
/// `CONFIG_UEB_T6_MAX` (`0x802E`).
pub(super) const PARAM_UEB_T6_MAX: ComParamId = ComParamId(0x80E0);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T7_MIN` (ADR-216). Same
/// project-invented/native-millisecond shape as [`PARAM_UEB_T0_MIN`]. Native
/// `CONFIG_UEB_T7_MIN` (`0x802F`).
pub(super) const PARAM_UEB_T7_MIN: ComParamId = ComParamId(0x80E1);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T7_MAX` (ADR-216). Same
/// project-invented/native-millisecond shape as [`PARAM_UEB_T0_MIN`]. Native
/// `CONFIG_UEB_T7_MAX` (`0x8030`).
pub(super) const PARAM_UEB_T7_MAX: ComParamId = ComParamId(0x80E2);

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte, `UEB_T9_MIN` (ADR-216). Same
/// project-invented/native-millisecond shape as [`PARAM_UEB_T0_MIN`]. Native
/// `CONFIG_UEB_T9_MIN` (`0x8031`).
pub(super) const PARAM_UEB_T9_MIN: ComParamId = ComParamId(0x80E3);

/// LockMask bit 0: exclusive privilege to modify physical ComParams.
///
/// While this CLL holds the lock, no other CLL sharing the same physical
/// resource (same `protocol_id` on the same device) may call `SetComParam`
/// with a physical-layer param (any param for which
/// `ComParamId::to_j2534_config_id` returns `Some`).
pub(super) const LOCK_PHYSICAL_COM_PARAMS: u32 = 0x01;

/// LockMask bit 1: exclusive privilege to transmit on the physical resource.
///
/// While this CLL holds the lock, every other CLL sharing the same physical
/// resource has its ComPrimitive queue suspended (`LogicalLinkState::
/// tx_suspended_by_lock`, ADR-123, ISO 22900-2 §9.4.13.3 use case 1): a
/// transmitting ComPrimitive (`CoptSendrecv`/`CoptStartcomm`/non-empty-data
/// `CoptStopcomm`) is accepted and queued rather than rejected, then
/// dispatched once the lock releases. Receive-only monitoring is unaffected
/// throughout.
pub(super) const LOCK_PHYSICAL_TX_QUEUE: u32 = 0x02;

// ── PDU_IOCTL adapter command IDs (ADR-079) ──────────────────────────────────
//
// ISO 22900-2's IoCtl surface has no universal numeric ID space of its own --
// `GetObjectId(OBJT_IO_CTRL, shortname)` is how a D-PDU client discovers the
// numeric `io_ctrl_command_id` to pass to `IoCtl`. These 22 values (17
// pre-ADR-164, SW_CAN_HS/SW_CAN_NS added by ADR-164 Decision 3/Phase 4, and
// START/QUERY/STOP_REPEAT_MESSAGE added by ADR-165/Phase 12) are a private
// namespace owned entirely by this adapter: they only round-trip
// through this service's own `GetObjectId` -> `io_ctrl_command_id` ->
// `rpc_io_ctl` path and never reach the underlying J2534 DLL, so the exact
// numbering carries no cross-adapter meaning. Distinct from the legacy raw
// J2534 IOCTL IDs (`j2534_0404::CLEAR_RX_BUFFER` et al., all <= 0x14), which
// remain supported in `rpc_io_ctl` as recognized aliases for their 4
// pre-existing commands.
pub(super) const PDU_IOCTL_BASE: u32 = 0x2900_0000;

/// Soft state reset (module-level): does NOT `PassThruClose`/reopen the
/// device (would drop live channels) -- clears each CLL's RX buffer, held TX
/// queue, and client filters, and resets `tx_suspended_by_ioctl` (leaving a
/// sibling-held `LOCK_PHYSICAL_TX_QUEUE`'s `tx_suspended_by_lock` untouched,
/// ADR-123).
pub(super) const PDU_IOCTL_RESET: u32 = PDU_IOCTL_BASE + 0x01;

/// Drops this CLL's held TX queue (`tx_held`) and cancels its queued COPs;
/// clears the hardware TX buffer only when this CLL's channel is not shared.
pub(super) const PDU_IOCTL_CLEAR_TX_QUEUE: u32 = PDU_IOCTL_BASE + 0x02;

/// Suspends TX dispatch for this CLL: newly dispatched items are held in
/// `tx_held` instead of executing.
pub(super) const PDU_IOCTL_SUSPEND_TX_QUEUE: u32 = PDU_IOCTL_BASE + 0x03;

/// Resumes TX dispatch for this CLL, draining `tx_held` in FIFO order.
pub(super) const PDU_IOCTL_RESUME_TX_QUEUE: u32 = PDU_IOCTL_BASE + 0x04;

/// Clears this CLL's RX ring buffer (`rx_buf`).
pub(super) const PDU_IOCTL_CLEAR_RX_QUEUE: u32 = PDU_IOCTL_BASE + 0x05;

/// Reads battery voltage (module-level; `PDU_IT_IO_UNUM32` output, mV).
pub(super) const PDU_IOCTL_READ_VBATT: u32 = PDU_IOCTL_BASE + 0x06;

/// Sets J1962 programming voltage on a pin (module-level; `PDU_IT_IO_PROG_VOLTAGE` input).
pub(super) const PDU_IOCTL_SET_PROG_VOLTAGE: u32 = PDU_IOCTL_BASE + 0x07;

/// Reads J1962 programming voltage (module-level; `PDU_IT_IO_UNUM32` output, mV).
pub(super) const PDU_IOCTL_READ_PROG_VOLTAGE: u32 = PDU_IOCTL_BASE + 0x08;

/// Generic vendor passthrough IOCTL (module-level) -- not supported by this
/// adapter (no invented byte-array passthrough convention).
pub(super) const PDU_IOCTL_GENERIC: u32 = PDU_IOCTL_BASE + 0x09;

/// Caps the per-item byte size of `GetComPrimitiveData` result items for this
/// CLL (`PDU_IT_IO_UNUM32` input); distinct from the event queue size (see
/// `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`).
pub(super) const PDU_IOCTL_SET_BUFFER_SIZE: u32 = PDU_IOCTL_BASE + 0x0A;

/// Installs one or more client message filters on this CLL (`PDU_IT_IO_FILTER` input).
pub(super) const PDU_IOCTL_START_MSG_FILTER: u32 = PDU_IOCTL_BASE + 0x0B;

/// Removes a single client message filter by `FilterNumber` (`PDU_IT_IO_UNUM32` input).
pub(super) const PDU_IOCTL_STOP_MSG_FILTER: u32 = PDU_IOCTL_BASE + 0x0C;

/// Removes every client message filter installed on this CLL via
/// `PDU_IOCTL_START_MSG_FILTER` (does not touch this service's own ADR-005/
/// 008/039 filters, and does not call the legacy `IOCTL_CLEAR_MSG_FILTERS`).
pub(super) const PDU_IOCTL_CLEAR_MSG_FILTER: u32 = PDU_IOCTL_BASE + 0x0D;

/// Configures this CLL's RX event queue capacity/overflow behavior
/// (`PDU_IT_IO_EVENT_QUEUE_PROPERTY` input).
pub(super) const PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES: u32 = PDU_IOCTL_BASE + 0x0E;

/// Reads VCI cable identification (module-level) -- not supported by this
/// adapter (no underlying hardware capability).
pub(super) const PDU_IOCTL_GET_CABLE_ID: u32 = PDU_IOCTL_BASE + 0x0F;

/// Sends a UART break condition (CLL-level) -- not supported by this adapter;
/// J2534 v04.04 only exposes an inbound `RX_FLAG_BREAK`, no outbound API.
pub(super) const PDU_IOCTL_SEND_BREAK: u32 = PDU_IOCTL_BASE + 0x10;

/// Reads the ignition-sense pin state (module-level; `PDU_IT_IO_UNUM32`
/// input+output) -- not supported by this adapter (no underlying hardware
/// capability).
pub(super) const PDU_IOCTL_READ_IGNITION_SENSE_STATE: u32 = PDU_IOCTL_BASE + 0x11;

/// SAE J2534-2 clause 9 Single Wire CAN (ADR-164 Decision 3/Phase 4):
/// switches an SW_CAN_PS/SW_ISO15765_PS ComLogicalLink's channel to
/// high-speed mode via the native `IOCTL_SW_CAN_HS` (no input/output
/// parameters, per clause 9's own command definition) -- `ChannelID`-scoped
/// (L), resolved the same way the 6 pre-existing L-scoped commands are
/// (`require_cll_handle_for_ioctl`). Rejected on a non-SW link.
pub(super) const PDU_IOCTL_SW_CAN_HS: u32 = PDU_IOCTL_BASE + 0x12;

/// SAE J2534-2 clause 9 Single Wire CAN (ADR-164 Decision 3/Phase 4): the
/// `IOCTL_SW_CAN_NS` (normal-speed mode) analog of `PDU_IOCTL_SW_CAN_HS`.
pub(super) const PDU_IOCTL_SW_CAN_NS: u32 = PDU_IOCTL_BASE + 0x13;

/// SAE J2534-2 clause 14 Repeat Messaging (ADR-165/Phase 12): starts
/// autonomous device-side retransmission of a `REPEAT_MSG_SETUP` via the
/// native `IOCTL_START_REPEAT_MESSAGE` -- `ChannelID`-scoped (L), resolved
/// the same way as every other L-scoped command
/// (`require_cll_handle_for_ioctl`). Input is `bytearray_data`
/// (`IOBytearray`), a hand-packed byte payload decoded by
/// `unpack_repeat_message_setup` (ADR-178); output is the device-assigned
/// `MsgId` in `unum32_value`.
pub(super) const PDU_IOCTL_START_REPEAT_MESSAGE: u32 = PDU_IOCTL_BASE + 0x14;

/// SAE J2534-2 clause 14 Repeat Messaging (ADR-165/Phase 12): polls a
/// running repeat slot's status via the native `IOCTL_QUERY_REPEAT_MESSAGE`.
/// Input is the `MsgId` (`unum32_value`) returned by a prior
/// `PDU_IOCTL_START_REPEAT_MESSAGE`; output is the device-reported status in
/// `unum32_value`.
pub(super) const PDU_IOCTL_QUERY_REPEAT_MESSAGE: u32 = PDU_IOCTL_BASE + 0x15;

/// SAE J2534-2 clause 14 Repeat Messaging (ADR-165/Phase 12): cancels a
/// running repeat slot via the native `IOCTL_STOP_REPEAT_MESSAGE`. Input is
/// the `MsgId` (`unum32_value`) returned by a prior
/// `PDU_IOCTL_START_REPEAT_MESSAGE`; no output.
pub(super) const PDU_IOCTL_STOP_REPEAT_MESSAGE: u32 = PDU_IOCTL_BASE + 0x16;

/// SAE J2534-2 clause 23 J1962 Pin Voltage Read (Phase 13): reads a single
/// J1962 connector pin's voltage via the native `IOCTL_READ_J1962PIN_VOLTAGE`
/// -- module-scoped (M), like `PDU_IOCTL_SET_PROG_VOLTAGE`/
/// `PDU_IOCTL_READ_PROG_VOLTAGE`/`PDU_IOCTL_READ_VBATT`, since J1962 pin
/// voltage is a device-level property. Input is the pin number (1-16) in
/// `unum32_value`; output is the voltage in millivolts, also in
/// `unum32_value`.
pub(super) const PDU_IOCTL_READ_J1962PIN_VOLTAGE: u32 = PDU_IOCTL_BASE + 0x17;

/// SAE J2534-2 clause 18 Device Configuration (ADR-176/Phase 14; byte layout
/// per ADR-178): reads one or more of the device's non-volatile parameter
/// store slots (`NON_VOLATILE_STORE_1`..`_10`) via the native
/// `IOCTL_GET_DEVICE_CONFIG` -- module-scoped (M), like
/// `PDU_IOCTL_READ_J1962PIN_VOLTAGE`, since this is a device-level property
/// with no `ChannelID`/CLL involved. Input and output both use a
/// hand-packed little-endian byte payload in `bytearray_data`
/// (`u32 entry_count`, then `entry_count` × `{u32 parameter_id, u32
/// value}`): input carries the requested `parameter_id`s (`value` ignored),
/// output carries the same entries with `value` populated.
pub(super) const PDU_IOCTL_GET_DEVICE_CONFIG: u32 = PDU_IOCTL_BASE + 0x18;

/// SAE J2534-2 clause 18 Device Configuration (ADR-176/Phase 14; byte layout
/// per ADR-178): writes one or more of the device's non-volatile parameter
/// store slots via the native `IOCTL_SET_DEVICE_CONFIG` -- module-scoped
/// (M), matching `PDU_IOCTL_GET_DEVICE_CONFIG` above. Input is the same
/// hand-packed byte payload in `bytearray_data`, with both `parameter_id`
/// and `value` set; no output.
pub(super) const PDU_IOCTL_SET_DEVICE_CONFIG: u32 = PDU_IOCTL_BASE + 0x19;

/// SAE J2534-2 clause 11 GM UART Protocol (SAE J2740, ADR-189/Phase 8):
/// defines the bytes of the poll-response message the interface should
/// transmit once bus mastership is granted, via the native
/// `IOCTL_SET_POLL_RESPONSE` -- `ChannelID`-scoped (L), resolved the same
/// way `PDU_IOCTL_SW_CAN_HS`/`_NS` are (`require_cll_handle_for_ioctl`).
/// Rejected on a non-GM-UART link. Input is Table 28's `PollResponseMsg[100]`
/// (at most 100 bytes) in `bytearray_data`; no output.
pub(super) const PDU_IOCTL_SET_POLL_RESPONSE: u32 = PDU_IOCTL_BASE + 0x1A;

/// SAE J2534-2 clause 11 GM UART Protocol (SAE J2740, ADR-189/Phase 8):
/// requests bus mastership via the native `IOCTL_BECOME_MASTER` --
/// `ChannelID`-scoped (L), resolved the same way `PDU_IOCTL_SW_CAN_HS`/`_NS`
/// are (`require_cll_handle_for_ioctl`). Rejected on a non-GM-UART link.
/// Input is Table 32's single `Poll_ID` byte in `unum32_value`; no output.
/// The native call blocks up to ~2 seconds (clause 11.3.3.2) inside the
/// ordinary synchronous `PassThruIoctl` forwarding path this service already
/// uses for every native IOCTL -- no service-side wait loop or new
/// timeout/cancellation surface (ADR-189 Decision item 4/Consequences).
pub(super) const PDU_IOCTL_BECOME_MASTER: u32 = PDU_IOCTL_BASE + 0x1B;

/// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): reads adapter
/// identity/status via the native `IOCTL_GET_NDIS_ADAPTER_INFO` --
/// `ChannelID`-scoped (L), resolved the same way `PDU_IOCTL_SW_CAN_HS`/`_NS`
/// are (`require_cll_handle_for_ioctl`). Rejected on a non-Ethernet_NDIS
/// link. No input; output is the hand-packed native `NDIS_ADAPTER_INFORMATION`
/// struct in `bytearray_data` (byte layout documented in
/// `docs/rpc-api-guide.md`).
pub(super) const PDU_IOCTL_GET_NDIS_ADAPTER_INFO: u32 = PDU_IOCTL_BASE + 0x1C;
