//! Demonstrates SAE J2534-2 clause 16 SAE J1939 Protocol access
//! (`ChannelProtocol::J1939_PS`, native `SAE_J1939_11_DWCAN` bus,
//! ADR-179/Phase 5) through the `j2534-0404-service` gRPC interface:
//! resolves the `ISO_OBD_on_SAE_J1939_73` resource (of the two
//! J1939-derived presets `resources.rs` defines --
//! `ISO_OBD_on_SAE_J1939_73`/`SAE_J1939_73_on_SAE_J1939_21`, both sharing
//! one `ChannelProtocol::J1939_PS` channel -- this example uses the
//! ISO-OBD-flavored one since it maps onto a concrete, widely-testable
//! request: an SAE J1939-21 Request PGN asking for PGN 0xFECA, Active
//! Diagnostic Trouble Codes/DM1, a message every OBD-on-J1939-compliant
//! ECU supports), drives `CoptStartcomm`'s address-claim, and a single
//! `CoptSendrecv`.
//!
//! SAE J1939 is a heavy-duty-truck/off-highway CAN-based bus; this example
//! targets a single engine-class ECU (source/target address `0x00`), the
//! most common single-ECU convention for these buses.
//!
//! This is a **pure gRPC client** -- it connects to an already-running
//! `j2534-0404-service` process (see `crates/j2534-0404-service/docs/startup-spec.md`); it does not
//! start a server itself. To see a real response you need a real
//! J2534-compatible interface wired to a live SAE J1939 bus.
//!
//! # Address claim
//!
//! Verified against `events_j1939_claim.rs`: address claim happens
//! AUTOMATICALLY inside `CoptStartcomm` -- `CP_J1939AddressNegotiationRule`
//! (`comparam_defaults.rs`'s seeded default, `0`) requests a claim by
//! default, so no separate explicit claim step is needed before
//! `CoptStartcomm`. What IS required beforehand: `CP_J1939PreferredAddress`
//! (the ordered candidate address list, a Bytefield ComParam) and
//! `CP_J1939Name` (this node's 8-byte SAE J1939 NAME) must both be staged,
//! since an absent/all-zero NAME is the wire-level claim-CANCEL form and
//! fails the claim closed (`run_j1939_claim_loop`'s own dedicated check) --
//! this example stages both before `ConnectComLogicalLink`.
//!
//! `CP_J1939TargetAddress` -- the destination ECU's address, a SEPARATE
//! ComParam from the claim's own candidate list -- must be a real address
//! (`0x00`-`0xFF`), never the `0xFFFF` "not configured" sentinel:
//! `rpc_primitive.rs` rejects `CoptStartcomm` synchronously with that
//! sentinel still staged (ADR-179 Decision 4, verified against
//! `tests/grpc_mock/j1939.rs`'s own `startcomm_rejects_the_0xffff_target_address_sentinel`-class
//! coverage).
//!
//! `CP_J1939SourceAddress` is deliberately NOT settable via `SetComParam`
//! (ADR-184): it is `PDU_PC_UNIQUE_ID`-class, the same restriction
//! `CP_CanPhysReqId`/`CP_CanRespUSDTId` have on the CAN family -- this
//! example does not need to set it via `SetComParam` at all, since the
//! claimed address is resolved automatically by the claim loop and readable
//! back via `CP_TesterSourceAddress`, not staged by the client. A
//! `SetUniqueRespIdTable(CP_J1939SourceAddress, ...)` entry exists for
//! multi-ECU RX classification on a shared physical channel (ADR-184), and
//! this example DOES use it below to constrain the `CoptSendrecv` response
//! to frames whose SAE J1939 source address matches `target_address` --
//! without it, this example's non-RawMode CLL strips the CAN-ID/source-
//! address header before mask/pattern matching (see `events.rs`), so the
//! zero-length `mask_data`/`pattern_data` used here would otherwise accept
//! the first J1939 frame from ANY node on the bus. This SOURCE constraint
//! alone does not select the specific requested PGN (0xFECA/DM1): the target
//! ECU's other periodic broadcasts (e.g. a live engine's EEC1 message, sent
//! every 10-50ms) also match on source address alone. This example therefore
//! layers a second, client-side PGN filter on top: the `CoptSendrecv` below
//! sets `num_receive_cycles: -2` (IS-MULTIPLE), which collects every
//! source-matched frame as its own `ResultData` event without a fixed count
//! that could under-fill and resets its receive window on each match rather
//! than closing after a set number of cycles (`rpc_primitive.rs`/`events.rs`).
//! The wait loop inspects each `ResultData`'s `extra_info.header_bytes` --
//! populated because this CLL is non-RawMode (`events.rs`) -- for a
//! PF/PS/data-page match against the Request PGN just sent, and stops on the
//! FIRST frame whose header actually carries PGN 0xFECA (the data-page/EDP
//! comparison distinguishes it from a same-PF/PS periodic frame on a
//! different page). Because IS-MULTIPLE never self-finishes while periodic
//! source-matched traffic keeps arriving, this example explicitly cancels
//! the primitive afterward (`CancelComPrimitive`) whenever the wait ended by
//! PGN match or client-side timeout rather than by the primitive's own
//! terminal `CopStatus`, and drains the resulting terminal event before
//! moving on to `CoptStopcomm`.
//! This split is structural, not a shortcut: the ISO 22900-2 expected-
//! response descriptor (`mask_data`/`pattern_data`) matches payload bytes
//! only, since the header is split off into `ResultData.extra_info` before
//! matching ever sees the data (ADR-051), and no descriptor mode that can
//! see the header exists outside RawMode, a different, out-of-scope CLL
//! configuration (ADR-200).
//!
//! A DM1 response with enough active DTCs to exceed one CAN frame is
//! transported as multi-packet SAE J1939-21 TP.CM_BAM, reassembled
//! transparently by the vendor adapter -- this service only sees the final
//! assembled frame once reassembly completes, so `CoptSendrecv`'s `CP_P2Max`
//! (the receive-window deadline this reassembly must fit inside) is staged
//! explicitly to a value with real multi-packet headroom rather than left at
//! this preset's single-frame-response default; see `SENDRECV_P2MAX_US`'s
//! own doc comment below for the derivation.
//!
//! # Usage
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_j1939 -- <service-addr> \
//!     [target-address] [preferred-address] [request-data]
//! ```
//!
//! ```sh
//! cargo run -p j2534-0404-service --example grpc_j1939 -- http://127.0.0.1:60124
//! ```
//!
//! Defaults: `target-address=0x00` (engine-class ECU), `preferred-address
//! =0x80` (this example's own tester-address candidate), `request-data
//! =ca,fe,00` (SAE J1939-21 Request PGN payload requesting PGN 0xFECA,
//! DM1/Active DTCs, little-endian per SAE J1939-21's own Request PGN
//! encoding -- not SAE J2534-2 clause 16.4.3, which is an unrelated
//! wire-level concept, the native 5-byte CAN-ID/destination-address message
//! prefix `PassThruWriteMsgs`/`ReadMsgs` require; SAE J1939-21 itself isn't
//! among this workspace's locally available specs, so no clause number is
//! cited here -- an earlier round of this file's own comments conflated the
//! two, corrected per an `edge-case-hunter` review).

#[path = "common/mod.rs"]
mod common;

use vci_service_interface::{
    CancelComPrimitiveRequest, ComOperationType, ComPrimitiveCtrlData,
    ConnectComLogicalLinkRequest, CreateComLogicalLinkRequest, EcuUniqueRespData,
    ExpectedResponseData, GetResourceIdsRequest, ParamItem, PduComPrimitiveStatus, PduParamClass,
    PinData, ResourceData, SetComParamRequest, SetUniqueRespIdTableRequest,
    StartComPrimitiveRequest, UniqueRespIdTableItem, create_com_logical_link_request, event_item,
    param_item, pin_data, resource_data,
};

const PROTOCOL_NAME: &str = "ISO_OBD_on_SAE_J1939_73";
const BUS_TYPE_NAME: &str = "SAE_J1939_11_DWCAN";
// SAE J1939-21 Request PGN's own PDU Format byte (234, PDU1/peer-to-peer
// addressing -- see tx_header.rs's `j1939_header_bytes` doc comment).
const REQUEST_PGN_PDU_FORMAT: u32 = 0xEA;

/// Wait timeout for the `CoptStartcomm` SAE J1939 address claim, in place of
/// `common::DEFAULT_RESPONSE_TIMEOUT_MS` (2000ms) -- mirrors
/// `grpc_iso9141.rs`'s own `INIT_TIMEOUT_MS` rationale. Verified against
/// `events_j1939_claim.rs`: the claim loop bounds each candidate address to
/// `CP_J1939AddrClaimTimeout` (default 1_250_000us = 1.25s, seeded by
/// `comparam_defaults.rs`), up to `candidate_count * timeout` total -- this
/// example stages exactly one preferred-address candidate, so at most one
/// 1.25s attempt. As with `grpc_tp2_0.rs`'s own `STARTCOMM_TIMEOUT_MS`, that
/// 1.25s deadline only starts once the poll task dequeues this COP, a real
/// additional delay `common::DEFAULT_RESPONSE_TIMEOUT_MS` (2000ms) leaves
/// too little headroom over (only ~750ms). 4000ms gives generous headroom
/// instead.
const STARTCOMM_TIMEOUT_MS: u32 = 4000;

/// `CoptSendrecv`'s `CP_P2Max`, staged explicitly instead of left at this
/// preset's default. SAE J1939-21 DM1 responses with enough active DTCs to
/// exceed one CAN frame are transported as multi-packet TP.CM_BAM, which the
/// vendor adapter reassembles transparently -- this service only sees the
/// final assembled frame in `ResultData` once reassembly completes (see the
/// module doc comment above), so the receive window this governs
/// (`events.rs`'s `response_timeout_ms = params.p2_max_timeout_ms()`, the
/// same deadline the IS-MULTIPLE `CoptSendrecv` below runs under) must span
/// the WHOLE multi-packet transfer, not just a single frame's turnaround.
/// `j1939_can_common`'s default (`comparam_defaults.rs`, 200_000us/200ms) is
/// this workspace's plain "ECU response delay" assumption (ISO 22900-2's
/// `CP_P2Max` description; for J1939 this maps to Tr) with no such margin --
/// verified this preset has no separate BAM inter-packet ComParam to lean on
/// instead: `j1939_can_common`'s own `CP_T4Max`/`CP_T5Max` (1_250_000us/1.25s
/// each) map, per ISO 22900-2, to J1939's T3/T2 (RTS/CTS-mode timers), not to
/// BAM. 5s gives real headroom over a small-DTC-count DM1's likely transfer
/// time (mirrors `grpc_can.rs`'s own explicit `CP_P2Max` staging, which this
/// example lacked entirely until now) without an impractically long wait for
/// a bench/real-device example.
const SENDRECV_P2MAX_US: u32 = 5_000_000;

/// Client-side wait for the `CoptSendrecv` below, with headroom over
/// `SENDRECV_P2MAX_US`'s server-side deadline so the client doesn't give up
/// before the server-side window itself would -- mirrors THIS file's own
/// `STARTCOMM_TIMEOUT_MS` headroom-over-server-deadline pattern above (not
/// `grpc_tp2_0.rs`'s, an earlier draft of this comment cited the wrong
/// file). Matching that same reasoning -- `STARTCOMM_TIMEOUT_MS` judged
/// 750ms of margin over a 1.25s deadline "too little" and widened to
/// ~2750ms of margin instead, since the deadline only starts once the poll
/// task dequeues this COP, a real additional delay on top of the deadline
/// itself -- this uses the same ~3000ms extra margin here, not a fixed 1s,
/// so the client doesn't again race ahead of the (now much longer)
/// server-side window it exists to wait out.
const SENDRECV_TIMEOUT_MS: u32 = SENDRECV_P2MAX_US.div_ceil(1000) + 3000;

async fn run(bin: &str, mut args: std::env::Args) -> common::Result<()> {
    let extra_usage = "[target-address] [preferred-address] [request-data]";
    let common_args = common::parse_common_args(bin, &mut args, extra_usage);

    let target_address = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "target-address", &raw))
        .unwrap_or(0x00);
    if target_address > 0xFF {
        return Err(
            format!("target-address {target_address:#x} must fit in one byte (0x00-0xFF)").into(),
        );
    }
    if target_address >= 0xFE {
        // clause 16.3.3.2 (PROTECT_J1939_ADDR) governs the address-CLAIM
        // byte (CP_J1939PreferredAddress's own candidates, not
        // CP_J1939TargetAddress): 254 is "NOT allowed" as a claim target,
        // and 255 is defined as the power-on default meaning "no address
        // claimed" rather than a real claimed address (ADR-180 Decision 8
        // already established this pair for exactly that claim-byte path).
        // CP_J1939TargetAddress itself has no clause-16.3.3.2-imposed range
        // -- this guard instead follows from that fact: since no node can
        // ever CLAIM address 254, and 255 by definition means no address is
        // claimed, no operational ECU's diagnostic response is EVER
        // transmitted with source address 0xFE or 0xFF. This example stages
        // SetUniqueRespIdTable(CP_J1939SourceAddress = target_address) to
        // constrain the CoptSendrecv response's SOURCE, so either value
        // staged there would never match any real response and this
        // example would always report no match. Rejected explicitly rather
        // than silently producing a request no reply can ever satisfy.
        let reason = if target_address == 0xFE {
            "0xfe (the NULL address -- clause 16.3.3.2 forbids any node from ever claiming it)"
        } else {
            "0xff (global/broadcast -- clause 16.3.3.2 defines it as \"no address claimed\", not \
             a real claimed address)"
        };
        return Err(format!(
            "target-address {target_address:#x} is not supported by this example -- {reason}, so \
             it is never a real ECU's own transmit source, and this example's response filtering \
             is built around a single addressed ECU's own source address; supply the specific \
             ECU's address instead (0x00-0xfd)"
        )
        .into());
    }
    let preferred_address = args
        .next()
        .map(|raw| common::parse_u32_arg(bin, extra_usage, "preferred-address", &raw))
        .unwrap_or(0x80);
    if preferred_address > 0xFF {
        return Err(format!(
            "preferred-address {preferred_address:#x} must fit in one byte (0x00-0xFF)"
        )
        .into());
    }
    if preferred_address >= 0xFE {
        // Unlike CP_J1939TargetAddress above, clause 16.3.3.2 DOES directly
        // govern this ComParam's own address-claim candidate byte: 254 is
        // "NOT allowed", 255 means "no address claimed" rather than a real
        // claim target. Without this check, the service's own claim loop
        // (ADR-180 Decision 8) silently skips this sole candidate, exhausts
        // the candidate list, and the caller sees only the generic
        // "address claim did not complete successfully" message below --
        // rejecting here up front gives a specific, actionable reason
        // instead.
        return Err(format!(
            "preferred-address {preferred_address:#x} is not a valid SAE J1939 address-claim \
             candidate -- clause 16.3.3.2 forbids claiming 0xfe (the NULL address) and defines \
             0xff as \"no address claimed\" rather than a real claim target; supply a real \
             address instead (0x00-0xfd)"
        )
        .into());
    }
    let request_data = args
        .next()
        .map(|raw| common::parse_data_bytes_arg(bin, extra_usage, "request-data", &raw))
        .unwrap_or_else(|| vec![0xCA, 0xFE, 0x00]);
    if request_data.len() != 3 {
        return Err(format!(
            "request-data {request_data:02x?} must be EXACTLY 3 bytes (SAE J1939-21's own \
             Request PGN encoding: byte 0 = PS, byte 1 = PF, byte 2 = data page/reserved -- not \
             to be confused with SAE J2534-2 clause 16.4.3's unrelated native 5-byte wire \
             CAN-ID/destination-address message prefix) -- the Request PGN payload \
             CP_J1939PDUFormat=0xEA selects has an exact 3-byte PGN; a shorter payload is \
             incomplete and a longer one transmits extra bytes as application data, and either \
             way a conforming ECU will not answer it"
        )
        .into());
    }
    // SAE J1939-21's own little-endian Request PGN encoding (distinct from
    // SAE J2534-2 clause 16.4.3's unrelated native wire CAN-ID/destination-
    // address prefix -- see the length check's error message above; SAE
    // J1939-21 itself isn't among this workspace's locally available specs,
    // so no clause number is cited for it): byte 0 is PS, byte 1 is PF, byte
    // 2 carries the Data Page bit (bit 0) plus the Extended Data
    // Page/Reserved bit (bit 1) of the 18-bit PGN value -- the same two low
    // bits `j1939_header_bytes` (tx_header.rs) uses in its own byte 0, since
    // both encode the same underlying SAE J1939-21 PGN field. Both bits are
    // compared together below (not just DP alone) so a frame sharing PF/PS
    // but differing in either bit is never mistaken for the requested PGN.
    // Used below to PGN-filter the CoptSendrecv response client-side.
    if request_data[2] & !0x03 != 0 {
        // The 18-bit PGN value SAE J1939-21 encodes across these 3 bytes
        // uses only bits 0-1 of byte 2 (Data Page + Extended Data
        // Page/Reserved); the upper 6 bits are reserved and must be zero.
        // Rejecting this here (rather than silently transmitting it and
        // matching responses via `request_dp_edp`'s own `& 0x03` mask below)
        // avoids a real mismatch: the wire request would carry whatever
        // garbage the caller passed in those reserved bits -- which a
        // conforming ECU may reject outright, or interpret as a different
        // PGN than the one this example's own PGN-match predicate ends up
        // filtering for after normalizing the same byte down to its low 2
        // bits.
        return Err(format!(
            "request-data {request_data:02x?} byte 2 (data page/reserved) has reserved bits set \
             ({:#04x}) -- SAE J1939-21's 18-bit PGN encoding uses only its low 2 bits (Data \
             Page + Extended Data Page/Reserved); the upper 6 bits must be zero",
            request_data[2]
        )
        .into());
    }
    let request_ps = request_data[0];
    let request_pf = request_data[1];
    let request_dp_edp = request_data[2] & 0x03;
    if request_pf < 240 && request_ps != 0 {
        // PDU1 (request_pf < 240, mirroring the wait predicate's own
        // PDU1/PDU2 split below): for a PDU1 PGN, byte 0's wire position is
        // NOT part of the PGN's own identity -- it is the DESTINATION
        // ADDRESS the PGN carries once actually transmitted (SAE J1939-21;
        // not among this workspace's locally available specs, so no clause
        // is cited). Requesting a specific PGN via the Request PGN
        // mechanism means asking for that PGN in general, not a copy
        // already addressed to some destination, so this byte must be 0.
        // The wait predicate below already ignores this field for PDU1
        // matching (it checks the RESPONSE's own destination address
        // against `preferred_address`/`0xff` instead, never against this
        // value) -- so a nonzero value here would silently transmit a
        // different, ill-formed request while the predicate still reports
        // success for the PGN this example actually asked to match.
        return Err(format!(
            "request-data {request_data:02x?} byte 0 (PS) is {request_ps:#04x}, but PGN \
             {request_pf:#04x}xx is PDU1 (SAE J1939-21) -- byte 0 there is the destination \
             address, not part of the PGN identity, and must be 0 when requesting the PGN in \
             general rather than a copy already addressed to a specific destination"
        )
        .into());
    }

    println!(
        "config: protocol={PROTOCOL_NAME} target_address={target_address:#x} \
         preferred_address={preferred_address:#x} request={request_data:02x?}"
    );

    let (mut client, module_handle) =
        common::connect_and_open_module(&common_args.service_addr).await?;

    let cp_target_address =
        common::comparam_object_id(&mut client, "CP_J1939TargetAddress").await?;
    let cp_pdu_format = common::comparam_object_id(&mut client, "CP_J1939PDUFormat").await?;
    let cp_preferred_address =
        common::comparam_object_id(&mut client, "CP_J1939PreferredAddress").await?;
    let cp_name = common::comparam_object_id(&mut client, "CP_J1939Name").await?;
    let cp_source_address =
        common::comparam_object_id(&mut client, "CP_J1939SourceAddress").await?;
    let cp_p2max = common::comparam_object_id(&mut client, "CP_P2Max").await?;

    // ── GetResourceIds(protocol="ISO_OBD_on_SAE_J1939_73", \
    //    bus="SAE_J1939_11_DWCAN") -- clause 16.3.2.1 keeps the pin \
    //    assignment unresolved by default (ADR-179 Decision 2), so this \
    //    protocol's resource row carries no default `dlc_pins` at all. \
    //    That means a *matching* GetResourceIds row-filter must NOT be \
    //    given any dlc_pin_data either: the row-match logic drops any row \
    //    whose own dlc_pins is empty as soon as the request supplies a \
    //    non-empty dlc_pin_data (there is nothing on the row's side left \
    //    to match against), so resolution would come back empty. \
    //    protocol_name + bus_type_name alone already resolve uniquely to \
    //    one resource id here -- the pin assignment is only supplied \
    //    later, to CreateComLogicalLink's RscData below. ─────────────────
    let pins = vec![
        PinData {
            dlc_pin_number: 6,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName("HI".to_string())),
        },
        PinData {
            dlc_pin_number: 14,
            dlc_pin_type: Some(pin_data::DlcPinType::DlcPinTypeName("LOW".to_string())),
        },
    ];
    let resource_ids = client
        .get_resource_ids(GetResourceIdsRequest {
            module_handle: Some(module_handle),
            resource_data: Some(ResourceData {
                dlc_pin_data: Vec::new(),
                bus_type: Some(resource_data::BusType::BusTypeName(
                    BUS_TYPE_NAME.to_string(),
                )),
                protocol: Some(resource_data::Protocol::ProtocolName(
                    PROTOCOL_NAME.to_string(),
                )),
            }),
        })
        .await?
        .into_inner();
    let resource_id = resource_ids
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .and_then(|data| data.resource_id_array.into_iter().next())
        .ok_or(format!(
            "no resource id resolved for protocol {PROTOCOL_NAME:?}"
        ))?;
    println!("resource_id = {resource_id:#x}");

    // ── CreateComLogicalLink (via RscData, so dlc_pin_data travels with \
    //    the create call too -- J1939 has no resource-default pins) ────
    let cll_response = client
        .create_com_logical_link(CreateComLogicalLinkRequest {
            module_handle: Some(module_handle),
            resource: Some(create_com_logical_link_request::Resource::RscData(
                ResourceData {
                    dlc_pin_data: pins,
                    bus_type: None,
                    protocol: Some(resource_data::Protocol::ProtocolId(resource_id)),
                },
            )),
            cll_create_flag: None,
        })
        .await?
        .into_inner();
    let cll_handle = cll_response
        .cll_handle
        .ok_or("cll_handle missing from CreateComLogicalLink response")?;
    println!("cll_handle = {}", cll_handle.cll_handle);

    // ── SetComParam: destination addressing + CP_P2Max, unum32 fields ──
    for (id, value) in [
        (cp_target_address, target_address),
        (cp_pdu_format, REQUEST_PGN_PDU_FORMAT),
        (cp_p2max, SENDRECV_P2MAX_US),
    ] {
        client
            .set_com_param(SetComParamRequest {
                cll_handle: Some(cll_handle),
                param_item: Some(ParamItem {
                    id: Some(param_item::Id::ParamId(id)),
                    com_param_class: PduParamClass::PduPcCom as i32,
                    param_data: Some(param_item::ParamData::Unum32(value)),
                }),
            })
            .await?;
    }

    // ── SetComParam: address-claim inputs (Bytefield ComParams) ────────
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(cp_preferred_address)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Bytefield(vec![
                    preferred_address as u8,
                ])),
            }),
        })
        .await?;
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(param_item::Id::ParamId(cp_name)),
                com_param_class: PduParamClass::PduPcCom as i32,
                param_data: Some(param_item::ParamData::Bytefield(vec![
                    1, 2, 3, 4, 5, 6, 7, 8,
                ])),
            }),
        })
        .await?;

    // ── SetUniqueRespIdTable(CP_J1939SourceAddress) ─────────────────────
    client
        .set_unique_resp_id_table(SetUniqueRespIdTableRequest {
            cll_handle: Some(cll_handle),
            unique_resp_id_table: Some(UniqueRespIdTableItem {
                unique_data: vec![EcuUniqueRespData {
                    unique_resp_identifier: 1,
                    params: vec![ParamItem {
                        id: Some(param_item::Id::ParamId(cp_source_address)),
                        com_param_class: PduParamClass::PduPcUniqueId as i32,
                        param_data: Some(param_item::ParamData::Unum32(target_address)),
                    }],
                }],
            }),
        })
        .await?;

    // ── ConnectComLogicalLink → SubscribeEvent ──────────────────────────
    client
        .connect_com_logical_link(ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await?;
    println!("connected");
    let mut events = common::subscribe(&mut client, cll_handle).await?;

    // ── CoptStartcomm (drives the address claim) ────────────────────────
    println!("starting communication (address claim, preferred_address={preferred_address:#x})");
    client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: Some(b"startcomm".to_vec()),
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptStartcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await?;
    if !common::wait_for_cop_finished(&mut events, STARTCOMM_TIMEOUT_MS, b"startcomm").await {
        return Err(
            "SAE J1939 address claim did not complete successfully -- aborting before \
             CoptSendrecv"
                .into(),
        );
    }

    // ── CoptSendrecv (SAE J1939-21 Request PGN) ─────────────────────────
    println!("sending Request PGN payload {request_data:02x?}");
    let sendrecv_cop_handle = client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: Some(b"sendrecv".to_vec()),
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptSendrecv as i32,
            cop_data: request_data,
            cop_ctrl_data: Some(ComPrimitiveCtrlData {
                time: 0,
                num_send_cycles: 1,
                // -2 (IS-MULTIPLE), not a finite count: a finite count can
                // under-fill (a real DM1 response arriving as the Nth
                // source-matched frame is lost if fewer than N total arrive
                // before the window closes) and reports a spurious
                // PduErrEvtRxTimeout even when the client-side PGN filter
                // below already captured a real match. IS-MULTIPLE instead
                // collects every source-matched frame as its own `ResultData`
                // event and resets its receive window on each match, so
                // RxTimeout can only occur when ZERO frames arrived at all
                // (`events.rs`/`rpc_primitive.rs`). Because IS-MULTIPLE never
                // self-finishes while periodic source-matched traffic keeps
                // resetting the window, this example ends the wait itself
                // (first PGN match, or a client-side timeout) and explicitly
                // cancels the primitive afterward -- see the wait below.
                num_receive_cycles: -2,
                temp_param_update: 0,
                expected_response_array: vec![ExpectedResponseData {
                    acceptance_id: 0,
                    response_type: 0,
                    mask_data: vec![0x00],
                    pattern_data: vec![0x00],
                    unique_resp_ids: vec![1],
                }],
                tx_flag: None,
            }),
        })
        .await?
        .into_inner()
        .cop_handle;
    // The service-layer `SetUniqueRespIdTable` above already constrains
    // responses to `target_address`'s SOURCE; it cannot see the PGN (see the
    // module doc comment above). This local wait applies that second,
    // client-side filter: it inspects each `ResultData`'s
    // `extra_info.header_bytes` (present because this CLL is non-RawMode --
    // see `events.rs`) for a PF/PS/data-page match against the Request PGN
    // just sent, and stops the wait IMMEDIATELY on the first match (IS-
    // MULTIPLE never self-finishes on its own under continuous matching
    // traffic, so waiting for more would just wait for the client-side
    // timeout instead). A terminal `CopStatus` (Finished or Cancelled) also
    // ends the wait if it arrives first; `Cancelled` is recorded via
    // `terminal_status` but is never treated as a success path.
    let mut pgn_matched_response: Option<Vec<u8>> = None;
    let mut saw_error: Option<i32> = None;
    let mut terminal_status: Option<i32> = None;
    common::wait_for_event(&mut events, SENDRECV_TIMEOUT_MS, |item| {
        if item.cop_tag.as_deref() != Some(b"sendrecv") {
            return false;
        }
        match &item.data {
            Some(event_item::Data::ResultData(result)) => {
                let Some(info) = &result.extra_info else {
                    return false;
                };
                let pf_matches = info.header_bytes.get(1) == Some(&request_pf);
                // PDU2 (request_pf >= 240, per SAE J1939-21's own PF/PS
                // encoding -- the requested target PGN this file defaults to,
                // 0xFECA, has PF 0xFE and is PDU2; this is distinct from
                // REQUEST_PGN_PDU_FORMAT/0xEA above, which is the
                // Request-PGN TRANSPORT message's own always-PDU1 PF,
                // not the target PGN's): PS is the PGN's own
                // group-extension byte, so it's compared directly
                // against the requested PGN's PS byte. PDU1 (request_pf
                // < 240): PS is instead the DESTINATION address, not
                // part of the PGN, so this MUST still be checked
                // (against this node's own claimed address or the
                // global destination 0xFF) rather than skipped --
                // otherwise a same-PF frame addressed to a different
                // node on the bus would false-match.
                let ps_matches = if request_pf >= 240 {
                    info.header_bytes.get(2) == Some(&request_ps)
                } else {
                    info.header_bytes
                        .get(2)
                        .is_some_and(|&ps| ps == preferred_address as u8 || ps == 0xFF)
                };
                // Data Page + Extended Data Page/Reserved bits (bits 0-1)
                // of the response's own byte 0 (see `j1939_header_bytes`
                // in tx_header.rs) must match the requested PGN's own two
                // bits -- otherwise a periodic frame sharing PF/PS but
                // differing in either bit would be misreported as the
                // requested PGN.
                let dp_matches = info
                    .header_bytes
                    .first()
                    .is_some_and(|byte0| byte0 & 0x03 == request_dp_edp);
                if pf_matches && ps_matches && dp_matches {
                    pgn_matched_response = Some(result.data_bytes.clone());
                    return true;
                }
                false
            }
            Some(event_item::Data::ErrorData(err)) => {
                saw_error = Some(*err);
                false
            }
            Some(event_item::Data::CopStatus(status))
                if *status == PduComPrimitiveStatus::PduCopstFinished as i32
                    || *status == PduComPrimitiveStatus::PduCopstCancelled as i32 =>
            {
                terminal_status = Some(*status);
                true
            }
            _ => false,
        }
    })
    .await;

    // IS-MULTIPLE never self-finishes while matching traffic keeps arriving,
    // so unless the wait above already ended via the primitive's own
    // terminal CopStatus, the primitive is still Executing here and must be
    // cancelled explicitly before CoptStopcomm is issued against it.
    if terminal_status.is_none() {
        if let Some(cop_handle) = sendrecv_cop_handle
            && let Err(e) = client
                .cancel_com_primitive(CancelComPrimitiveRequest {
                    cop_handle: Some(cop_handle),
                })
                .await
        {
            eprintln!("WARNING: cancel_com_primitive(CoptSendrecv) failed: {e}");
        }
        common::wait_for_event(&mut events, common::DEFAULT_RESPONSE_TIMEOUT_MS, |item| {
            item.cop_tag.as_deref() == Some(b"sendrecv")
                && matches!(item.data, Some(event_item::Data::CopStatus(status))
                    if status == PduComPrimitiveStatus::PduCopstFinished as i32
                        || status == PduComPrimitiveStatus::PduCopstCancelled as i32)
        })
        .await;
    }

    match &pgn_matched_response {
        Some(bytes) => {
            println!("ECU response received: {bytes:02x?}");
            if let Some(err) = saw_error {
                println!(
                    "NOTE: the CoptSendrecv also reported an async PduErrorEvent({err}) \
                     before/alongside the matched response"
                );
            }
        }
        None => {
            if terminal_status == Some(PduComPrimitiveStatus::PduCopstCancelled as i32) {
                println!(
                    "WARNING: CoptSendrecv was cancelled by the service before any PGN-matched \
                     response -- typically a hard channel failure (its own error event carries \
                     no cop_tag)"
                );
            } else if let Some(err) = saw_error {
                println!(
                    "WARNING: no PGN-matched response; CoptSendrecv reported \
                     PduErrorEvent({err}) -- a receive timeout here means NO frame from \
                     target_address={target_address:#x} arrived at all"
                );
            } else {
                println!(
                    "WARNING: no PGN-matched response received -- expected if no J1939 ECU \
                     answers at target_address={target_address:#x} on this bus, or if it \
                     answered with a J1939 NACK (PGN 0xE800) instead of the requested PGN"
                );
            }
        }
    }

    match client
        .start_com_primitive(StartComPrimitiveRequest {
            cop_tag: Some(b"stopcomm".to_vec()),
            cll_handle: Some(cll_handle),
            cop_type: ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
        .await
    {
        Ok(_) => println!("start_com_primitive(CoptStopcomm) succeeded"),
        Err(e) => eprintln!("WARNING: start_com_primitive(CoptStopcomm) failed: {e}"),
    }
    let _ = common::wait_for_cop_finished(
        &mut events,
        common::DEFAULT_RESPONSE_TIMEOUT_MS,
        b"stopcomm",
    )
    .await;

    // ── Teardown ─────────────────────────────────────────────────────────
    common::teardown(&mut client, cll_handle, module_handle).await?;

    println!("flow completed successfully");
    Ok(())
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args();
    let bin = args.next().unwrap_or_else(|| "grpc_j1939".to_string());
    if let Err(err) = run(&bin, args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
