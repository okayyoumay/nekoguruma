use tonic::Status;

use super::{
    ChannelProtocol,
    ComParamId,
    J2534Service,
    PARAM_5BAUD_ADDR_FUNC,
    PARAM_5BAUD_ADDR_PHYS,
    PARAM_5BAUD_COMM_BAUDRATE_OVERRIDE,
    PARAM_5BAUD_INIT_BAUDRATE,
    PARAM_ACCESS_TIMING_ECU,
    PARAM_ACCESS_TIMING_OVERRIDE,
    PARAM_ANALOG_ACTIVE_CHANNELS,
    PARAM_ANALOG_AVERAGING_METHOD,
    PARAM_ANALOG_INPUT_RANGE_HIGH,
    PARAM_ANALOG_INPUT_RANGE_LOW,
    PARAM_ANALOG_READINGS_PER_MSG,
    PARAM_ANALOG_SAMPLE_RATE,
    PARAM_ANALOG_SAMPLE_RESOLUTION,
    PARAM_ANALOG_SAMPLES_PER_READING,
    // Physical layer ComParams (service-level; no J2534 SET_CONFIG equivalent)
    PARAM_BIT_SAMPLE_POINT_ECU,
    PARAM_BLOCK_SIZE_ECU,
    PARAM_CAN_BAUDRATE_RECORD,
    PARAM_CAN_DATA_SIZE_OFFSET,
    PARAM_CAN_FILLER_BYTE,
    PARAM_CAN_FILLER_BYTE_HANDLING,
    PARAM_CAN_FIRST_CF_VALUE,
    PARAM_CAN_FUNC_REQ_EXT_ADDR,
    PARAM_CAN_FUNC_REQ_FORMAT,
    PARAM_CAN_FUNC_REQ_ID,
    PARAM_CAN_MIXED_FORMAT,
    // Transport layer ComParams — CAN addressing
    PARAM_CAN_PHYS_REQ_EXT_ADDR,
    PARAM_CAN_PHYS_REQ_FORMAT,
    PARAM_CAN_PHYS_REQ_ID,
    PARAM_CAN_RESP_USDT_EXT_ADDR,
    PARAM_CAN_RESP_USDT_FORMAT,
    PARAM_CAN_RESP_USDT_ID,
    PARAM_CAN_RESP_UUDT_EXT_ADDR,
    PARAM_CAN_RESP_UUDT_FORMAT,
    PARAM_CAN_RESP_UUDT_ID,
    PARAM_CAN_TRANSMISSION_TIME,
    PARAM_CANFD_BAUDRATE,
    PARAM_CANFD_BIT_SAMPLE_POINT,
    PARAM_CANFD_SYNC_JUMP_WIDTH,
    PARAM_CANFD_TX_MAX_DATA_LENGTH,
    PARAM_CHANGE_SPEED_CTRL,
    PARAM_CHANGE_SPEED_MSG,
    PARAM_CHANGE_SPEED_RATE,
    PARAM_CHANGE_SPEED_RES_CTRL,
    PARAM_CHANGE_SPEED_TX_DELAY,
    PARAM_COLLISION_TEST_MODE,
    PARAM_CYCLIC_RESP_TIMEOUT,
    PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK,
    // Transport layer ComParams — ECU addressing / COM
    PARAM_ECU_RESP_SOURCE_ADDR,
    PARAM_ENABLE_CONCATENATION,
    PARAM_ENABLE_INIT_SEQ_REPETITION,
    PARAM_ENABLE_PERFORMANCE_TEST,
    PARAM_ESCAPE_SEQUENCE_HANDLING,
    PARAM_EXTENDED_TIMING,
    PARAM_FILLER_BYTE,
    PARAM_FILLER_BYTE_HANDLING,
    PARAM_FILLER_BYTE_LENGTH,
    PARAM_FUNC_REQ_FORMAT_PRIORITY,
    PARAM_FUNC_REQ_TARGET_ADDR,
    PARAM_FUNC_RESP_FORMAT_PRIORITY,
    PARAM_FUNC_RESP_TARGET_ADDR,
    PARAM_HEADER_FORMAT_J1850,
    PARAM_HEADER_FORMAT_KW,
    PARAM_IGNORE_CHECKSUM,
    // Transport layer ComParams — INIT
    PARAM_INIT_SETTINGS,
    PARAM_ISO_KEYBYTE_COUNT,
    PARAM_J1850_IFR_CTRL,
    PARAM_J1939_ADDR_CLAIM_TIMEOUT,
    PARAM_J1939_ADDR_NEG_RULE,
    PARAM_J1939_DATA_PAGE,
    PARAM_J1939_MAX_PACKET_TX,
    // Transport layer ComParams — J1939 NAME (Bytefield)
    PARAM_J1939_NAME,
    PARAM_J1939_NAME_ECU,
    PARAM_J1939_PDU_FORMAT,
    PARAM_J1939_PDU_SPECIFIC,
    PARAM_J1939_PREFERRED_ADDRESS,
    PARAM_J1939_PREFERRED_ADDRESS_ECU,
    PARAM_J1939_SOURCE_ADDRESS,
    PARAM_J1939_SOURCE_NAME,
    PARAM_J1939_TARGET_ADDRESS,
    PARAM_J1939_TARGET_NAME,
    PARAM_K_L_LINE_INIT,
    PARAM_K_LINE_PULLUP,
    PARAM_LISTEN_ONLY,
    PARAM_MAX_CTS_REQ,
    PARAM_MAX_DATA_LENGTH_ECU,
    PARAM_MESSAGE_INDICATION_RATE,
    PARAM_MESSAGE_PRIORITY,
    PARAM_MID_REQ_ID,
    PARAM_MID_RESP_ID,
    PARAM_MODIFY_TIMING,
    // Transport layer ComParams — ISO 15765-2 frame timing
    PARAM_N_AR,
    PARAM_N_AR_ECU,
    PARAM_N_AS,
    PARAM_N_AS_ECU,
    PARAM_N_BR,
    PARAM_N_BR_ECU,
    PARAM_N_BS,
    PARAM_N_BS_ECU,
    PARAM_N_CR,
    PARAM_N_CR_ECU,
    PARAM_N_CS,
    PARAM_N_CS_ECU,
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16).
    PARAM_NDIS_PIN_OPTION,
    PARAM_NUM_HEADER_BYTES_START_COMM_KW,
    PARAM_P2_MAX_ECU,
    PARAM_P2_STAR,
    PARAM_P2_STAR_ECU,
    PARAM_P3_FUNC,
    PARAM_P3_PHYS,
    PARAM_PHYS_REQ_FORMAT_PRIORITY,
    PARAM_PHYS_REQ_TARGET_ADDR,
    PARAM_PHYS_RESP_FORMAT_PRIORITY,
    PARAM_RC_BYTE_OFFSET,
    PARAM_RC21_COMPLETION_TIMEOUT,
    PARAM_RC21_HANDLING,
    PARAM_RC21_REQUEST_TIME,
    PARAM_RC23_COMPLETION_TIMEOUT,
    PARAM_RC23_HANDLING,
    PARAM_RC23_REQUEST_TIME,
    PARAM_RC78_COMPLETION_TIMEOUT,
    PARAM_RC78_HANDLING,
    PARAM_REPEAT_REQ_COUNT_APP,
    PARAM_REPEAT_REQ_COUNT_TRANS,
    PARAM_REQUEST_ADDR_MODE,
    PARAM_SAMPLES_PER_BIT,
    PARAM_SAMPLES_PER_BIT_ECU,
    PARAM_SCI_ECU_SIMULATOR,
    PARAM_SCI_SET_PROG_VOLTAGE,
    PARAM_SCI_TRANSMIT_MODE,
    PARAM_SEND_REMOTE_FRAME,
    PARAM_SESSION_TIMING_ECU,
    PARAM_SESSION_TIMING_OVERRIDE,
    PARAM_ST_MIN_ECU,
    PARAM_START_MSG_IND_ENABLE,
    PARAM_SUSPEND_QUEUE_ON_ERROR,
    PARAM_SW_CAN_HIGH_VOLTAGE,
    PARAM_SYNC_JUMP_WIDTH_ECU,
    PARAM_TERMINATION_TYPE,
    PARAM_TERMINATION_TYPE_ECU,
    PARAM_TEST_MODE,
    PARAM_TESTER_PRESENT_ADDR_MODE,
    PARAM_TESTER_PRESENT_EXP_NEG_RESP,
    PARAM_TESTER_PRESENT_EXP_POS_RESP,
    PARAM_TESTER_PRESENT_HANDLING,
    PARAM_TESTER_PRESENT_IMMED,
    PARAM_TESTER_PRESENT_INTERVAL_US,
    // Application layer ComParams
    PARAM_TESTER_PRESENT_MSG,
    PARAM_TESTER_PRESENT_REQ_RSP,
    PARAM_TESTER_PRESENT_SEND_TYPE,
    PARAM_TESTER_PRESENT_TIME_ECU,
    PARAM_TP_CONNECTION_MGMT,
    PARAM_TRANSMIT_IND_ENABLE,
    // SAE J2534-2 clause 12.3.4.1 UART Echo Byte timing ComParams (ADR-216)
    PARAM_UEB_T0_MIN,
    PARAM_UEB_T1_MAX,
    PARAM_UEB_T2_MAX,
    PARAM_UEB_T3_MAX,
    PARAM_UEB_T4_MIN,
    PARAM_UEB_T5_MAX,
    PARAM_UEB_T6_MAX,
    PARAM_UEB_T7_MAX,
    PARAM_UEB_T7_MIN,
    PARAM_UEB_T9_MIN,
    // ISO9141 W-timer MIN/MAX sides with no native J2534 register (ADR-181)
    PARAM_W1_MIN,
    PARAM_W2_MIN,
    PARAM_W3_MIN,
    PARAM_W4_MAX,
    PDU_IOCTL_BECOME_MASTER,
    PDU_IOCTL_CLEAR_MSG_FILTER,
    PDU_IOCTL_CLEAR_RX_QUEUE,
    PDU_IOCTL_CLEAR_TX_QUEUE,
    PDU_IOCTL_GENERIC,
    PDU_IOCTL_GET_CABLE_ID,
    PDU_IOCTL_GET_DEVICE_CONFIG,
    PDU_IOCTL_GET_NDIS_ADAPTER_INFO,
    PDU_IOCTL_QUERY_REPEAT_MESSAGE,
    PDU_IOCTL_READ_IGNITION_SENSE_STATE,
    PDU_IOCTL_READ_J1962PIN_VOLTAGE,
    PDU_IOCTL_READ_PROG_VOLTAGE,
    PDU_IOCTL_READ_VBATT,
    PDU_IOCTL_RESET,
    PDU_IOCTL_RESUME_TX_QUEUE,
    PDU_IOCTL_SEND_BREAK,
    PDU_IOCTL_SET_BUFFER_SIZE,
    PDU_IOCTL_SET_DEVICE_CONFIG,
    PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES,
    PDU_IOCTL_SET_POLL_RESPONSE,
    PDU_IOCTL_SET_PROG_VOLTAGE,
    PDU_IOCTL_START_MSG_FILTER,
    PDU_IOCTL_START_REPEAT_MESSAGE,
    PDU_IOCTL_STOP_MSG_FILTER,
    PDU_IOCTL_STOP_REPEAT_MESSAGE,
    PDU_IOCTL_SUSPEND_TX_QUEUE,
    PDU_IOCTL_SW_CAN_HS,
    PDU_IOCTL_SW_CAN_NS,
    resources,
};

/// Return type of [`J2534Service::parse_protocol_id_from_resource`] (clippy
/// `type_complexity` fix): the resolved `ChannelProtocol`, the matched
/// `resources` table row (`None` for the legacy path), ADR-156 Decision 2's
/// Pin Selection outcome (`Some((base_hw_protocol_id, ps_protocol_id,
/// pin_select))` or `None`), and ADR-156 Decision 3/Phase 2b's Additional
/// Channels outcome (`Some((base_hw_protocol_id, chx_protocol_id,
/// channel_index))` or `None`, mutually exclusive with the Pin Selection
/// outcome -- see that method's own doc comment for the full contract).
/// Each `base_hw_protocol_id` (ADR-157/Bug 1 fix, generalized to `_CHx` by
/// ADR-156 Decision 3 addendum) is the correctly-resolved base hardware
/// protocol id (preserving, e.g., the exact SAE J2610 SCI variant) that
/// `LogicalLinkState::base_hw_protocol_override` is set from.
type ParsedProtocolResourceResult = Result<
    (
        ChannelProtocol,
        Option<&'static resources::ResourceDef>,
        Option<(u32, u32, u32)>,
        Option<(u32, u32, u32)>,
    ),
    Status,
>;

/// Return type of [`J2534Service::resolve_protocol_name_with_chx_suffix`]
/// (clippy `type_complexity` fix): the resolved `ChannelProtocol`, the
/// matched `resources` table row (`None` for the legacy path), the
/// compound-name grammar's requested Additional Channels index (`Some` only
/// when the `"_CH<n>"` suffix split engaged, `None` when the whole string
/// resolved directly), and the exact name actually resolved against the
/// table/alias/numeric routes -- the full original name when it resolved
/// directly, or the split head when the compound path engaged (see that
/// method's own doc comment for why callers must use this, not the
/// original possibly-compound input).
type ProtocolNameWithChxSuffixResult<'a> = Result<
    Option<(
        ChannelProtocol,
        Option<&'static resources::ResourceDef>,
        Option<u32>,
        &'a str,
    )>,
    Status,
>;

impl J2534Service {
    /// Resolves a numeric resource/protocol ID: a table row's `ChannelProtocol`
    /// when `id` is a `resources` table resource ID, else the legacy
    /// `ChannelProtocol::from_raw` fallback (unchanged behavior for a raw or
    /// extended `ChannelProtocol` value passed directly).
    fn resolve_protocol_id(id: u32) -> (ChannelProtocol, Option<&'static resources::ResourceDef>) {
        match resources::find_by_resource_id(id) {
            Some(row) => (row.protocol, Some(row)),
            None => (ChannelProtocol::from_raw(id), None),
        }
    }

    /// Resolves a name-based resource/protocol selector: first a
    /// case-insensitive match against the table's `protocol_name`/
    /// `config_name`, then `map_protocol_name`, then (if numeric) a re-entry
    /// through `resolve_protocol_id` -- so a numeric string still prefers a
    /// table hit over the legacy `from_raw` interpretation.
    ///
    /// `dlc_pin_data` narrows a multi-row table match by typed pin (see
    /// `find_table_row_by_name`); pass `&[]` when no pin data is available
    /// (e.g. the `ResourceName` request variant, which cannot carry pins).
    ///
    /// Returns `Err` when the name (after any pin narrowing) still matches
    /// multiple table rows that differ in `ChannelProtocol` or
    /// `hw_protocol_override` (see `find_table_row_by_name`) -- an ambiguous
    /// `CreateComLogicalLink` request that must be rejected rather than
    /// silently picking one.
    fn resolve_protocol_name(
        name: &str,
        dlc_pin_data: &[vci_service_interface::PinData],
    ) -> Result<Option<(ChannelProtocol, Option<&'static resources::ResourceDef>)>, Status> {
        if let Some(row) = Self::find_table_row_by_name(name, dlc_pin_data)? {
            return Ok(Some((row.protocol, Some(row))));
        }
        if let Some(proto) = Self::map_protocol_name(name) {
            return Ok(Some((proto, None)));
        }
        Ok(name.parse::<u32>().ok().map(Self::resolve_protocol_id))
    }

    /// SAE J2534-2 clause 7 (Additional Channels) compound-name grammar
    /// (ADR-156 Decision 3/Phase 2b; reinstated in this form after ADR-178
    /// removed the `channel_index` proto field route it replaces --
    /// design-advisor's answer to the resulting SAE J2610/SCI regression:
    /// the four SCI variants collapse onto one `_CHx` numeric block, so a
    /// bare numeric `_CHx` id can never express which variant a caller
    /// means, unlike every other in-scope family). Only ever called from the
    /// two string-typed input routes (`ResourceName`, `RscData::ProtocolName`)
    /// -- never from a numeric `ResourceId`/`protocol_id` route, which stays
    /// purely numeric and can only ever reach the directly-named `_CHx` id
    /// path in [`resolve_channel_selection`].
    ///
    /// First attempts whole-string resolution via [`resolve_protocol_name`],
    /// unchanged (table row -> alias -> numeric parse) -- a real name or
    /// numeric id, even one that happens to look compound-shaped, always
    /// takes precedence and is never split (no known table/alias name
    /// currently ends in a `_CH<digits>`-shaped suffix, but this ordering
    /// means one could be added later without a silent misparse). Only when
    /// that whole-string resolution reports "not found" (`Ok(None)`, not an
    /// `Err` -- an ambiguous whole-string match still propagates its own
    /// error unchanged) does this attempt exactly one fallback split via
    /// [`split_chx_suffix`] (rightmost `_CH<digits>` suffix, case-insensitive
    /// on the whole string, no recursion into this same split for the head):
    /// the head is resolved through the SAME [`resolve_protocol_name`] call,
    /// and on success the parsed trailing digits are returned as the
    /// requested Additional Channel index.
    ///
    /// Returns `Ok(None)` when neither the whole string nor a split head
    /// resolves (the caller reports its own "not a valid protocol name"
    /// error, matching every pre-existing route's behavior). The third tuple
    /// element is the requested index (`Some` only when the compound-name
    /// split engaged); the fourth is the exact name that was actually
    /// resolved against the table/alias/numeric routes -- the full original
    /// `name` when the whole string resolved directly, or the split head
    /// when the compound path engaged. Callers that separately re-derive
    /// "did this name match a resource-table row" (`find_table_rows_by_name`,
    /// for `RscData::ProtocolName`'s row-identification-vs-Pin-Selection
    /// scoping) must query that same resolved name, not the original
    /// possibly-compound `name` -- the compound `name` itself never matches
    /// any table row.
    fn resolve_protocol_name_with_chx_suffix<'a>(
        name: &'a str,
        dlc_pin_data: &[vci_service_interface::PinData],
    ) -> ProtocolNameWithChxSuffixResult<'a> {
        if let Some((protocol, row)) = Self::resolve_protocol_name(name, dlc_pin_data)? {
            return Ok(Some((protocol, row, None, name)));
        }
        let Some((head, index)) = Self::split_chx_suffix(name) else {
            return Ok(None);
        };
        let Some((protocol, row)) = Self::resolve_protocol_name(head, dlc_pin_data)? else {
            return Ok(None);
        };
        Ok(Some((protocol, row, Some(index), head)))
    }

    /// The one-shot suffix split [`resolve_protocol_name_with_chx_suffix`]
    /// attempts: the rightmost case-insensitive `"_CH"` substring, followed
    /// by one or more decimal digits running to the end of the string, with
    /// a non-empty head before it. Returns `(head, index)` -- `index` is
    /// parsed as a plain `u32` with no range check here (out-of-range
    /// `1..=128` handling belongs to [`resources::chx_protocol_id`], the
    /// single source of truth for that range, via
    /// [`resolve_channel_selection`]). `None` when the rightmost `"_CH"`
    /// (if any) isn't followed by only digits, or has an empty head --
    /// deliberately not backtracking to search for an earlier `"_CH"` in
    /// that case, since this is a one-shot fallback, not a general grammar.
    fn split_chx_suffix(name: &str) -> Option<(&str, u32)> {
        let upper = name.to_ascii_uppercase();
        let idx = upper.rfind("_CH")?;
        let head = &name[..idx];
        let suffix = &name[idx + 3..];
        if head.is_empty() || suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let index = suffix.parse::<u32>().ok()?;
        Some((head, index))
    }

    /// Case-insensitive lookup of every table row whose `protocol_name` or
    /// `config_name` matches `name` (e.g. `"SCI_A_ENGINE"` matches the
    /// `SAE_J2610_SCI` row configured for SCI-A Engine), in table order.
    ///
    /// A name can match more than one row -- e.g. `"SAE_J2610_SCI"` matches
    /// all four SCI configuration rows (0x021F-0x0222), which differ only by
    /// `config_name` and `ChannelProtocol`; every other table name matches at
    /// most one row (by construction: `protocol_name`/`config_name` pairs are
    /// otherwise unique). Callers decide how to handle more than one match --
    /// see `find_table_row_by_name` (errors on an ambiguous `ChannelProtocol`)
    /// and `rpc_link::rpc_get_conflicting_resources` (treats it as "match
    /// any" for the static conflict scan).
    pub(super) fn find_table_rows_by_name(name: &str) -> Vec<&'static resources::ResourceDef> {
        resources::resource_table()
            .iter()
            .filter(|row| {
                row.protocol_name.eq_ignore_ascii_case(name)
                    || row
                        .config_name
                        .is_some_and(|c| c.eq_ignore_ascii_case(name))
            })
            .collect()
    }

    /// True for a resource-table row belonging to one of the standalone
    /// `_PS`-only protocol families (SW/FT/UART Echo Byte/Honda DIAG-H/
    /// SAE J1708/SAE J1939/TP2.0/GM UART) whose pin binding is resolved dynamically via
    /// `resolve_pin_selection`
    /// rather than being a single already-fixed hardware configuration --
    /// `dlc_pin_data` for these rows selects among the *protocol's own*
    /// documented pin set (clause 6-style), it does not disambiguate
    /// between multiple resource-table configurations sharing one name the
    /// way it does for e.g. `SAE_J2610_SCI`'s four rows. Shared by
    /// `find_table_row_by_name` (skips fixed-row pin narrowing) and
    /// `parse_protocol_id_from_resource`'s `matched_row_needs_pin_selection`
    /// (routes the match through `resolve_pin_selection`) so the two checks
    /// cannot drift apart.
    fn row_needs_dynamic_pin_selection(row: &resources::ResourceDef) -> bool {
        let hw_id = row
            .hw_protocol_override
            .unwrap_or_else(|| row.protocol.j2534_protocol_id());
        resources::is_sw_protocol_id(hw_id)
            || resources::is_ft_protocol_id(hw_id)
            || resources::is_uart_echo_byte_protocol_id(hw_id)
            || resources::is_honda_diagh_protocol_id(hw_id)
            || resources::is_j1708_protocol_id(hw_id)
            || resources::is_j1939_protocol_id(hw_id)
            || resources::is_tp2_0_protocol_id(hw_id)
            || resources::is_gm_uart_protocol_id(hw_id)
    }

    /// Case-insensitive lookup of a *single* table row by `protocol_name` or
    /// `config_name`, for `CreateComLogicalLink` (and `GetObjectId(OBJT_RESOURCE, ...)`,
    /// which passes `&[]` for `dlc_pin_data` since it has no pin data at all).
    ///
    /// When `find_table_rows_by_name` returns more than one row (e.g.
    /// `"SAE_J2610_SCI"`'s four configurations, or
    /// `"SAE_J2610_on_SAE_J2610_SCI"`'s four `hw_protocol_override` variants)
    /// and `dlc_pin_data` is non-empty, it first narrows the matches by typed
    /// pin (`retain_rows_matching_pin`, the same row-based check
    /// `resolve_resource_ids_from_data` uses) -- narrowing to zero rows is an
    /// error (the pin set matches no configuration of this name), narrowing
    /// to exactly one resolves it unambiguously regardless of differing
    /// `ChannelProtocol`/`hw_protocol_override`.
    ///
    /// Otherwise (no pin data, or pins didn't narrow to one row): if every
    /// remaining row shares one `ChannelProtocol` *and* one
    /// `hw_protocol_override` (they are behaviorally identical, e.g. two
    /// alias rows), any of them is returned -- the specific one picked
    /// doesn't matter. When remaining rows differ in either field, there is
    /// no single correct choice, so this returns `Err` naming the available
    /// configurations/resource IDs instead of silently picking the first
    /// (table order) -- this is what stops an unqualified
    /// `"SAE_J2610_on_SAE_J2610_SCI"` from silently connecting as
    /// `SCI_A_ENGINE`.
    fn find_table_row_by_name(
        name: &str,
        dlc_pin_data: &[vci_service_interface::PinData],
    ) -> Result<Option<&'static resources::ResourceDef>, Status> {
        let mut rows = Self::find_table_rows_by_name(name);
        if rows.is_empty() {
            return Ok(None);
        }

        // Codex review, PR #63 round 4: a canonical name matching exactly
        // ONE standalone `_PS` row whose own pin binding is resolved
        // dynamically via `resolve_pin_selection` (SW/FT/UART Echo Byte/
        // Honda DIAG-H -- `row_needs_dynamic_pin_selection`, the same
        // predicate `parse_protocol_id_from_resource`'s
        // `matched_row_needs_pin_selection` uses) must not have
        // `dlc_pin_data` narrowed against that row's own fixed
        // resource-table pin default here. Unlike `SAE_J2610_SCI`'s four
        // same-named rows, there is nothing to disambiguate among a single
        // match -- the caller's `dlc_pin_data` instead selects among the
        // *protocol's own* documented pin set (e.g. Honda DIAG-H's pin 1 or
        // pin 14, clause 13.2.4), which `resolve_pin_selection` validates
        // downstream once `matched_row_needs_pin_selection` routes this row
        // through it. Narrowing here first would incorrectly reject a
        // legitimate non-default pin (Honda DIAG-H's pin 1, when row
        // 0x023B's own convenience default is pin 14) with a generic "no
        // configuration" error before that protocol-aware check is ever
        // reached.
        if let [only] = rows.as_slice()
            && Self::row_needs_dynamic_pin_selection(only)
        {
            return Ok(Some(only));
        }

        if !dlc_pin_data.is_empty() {
            Self::retain_rows_matching_all_pins(&mut rows, dlc_pin_data)?;
            if rows.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "resource name {name:?}'s dlc_pin_data matches no configuration"
                )));
            }
        }

        let first = rows[0];
        if rows.iter().all(|row| {
            row.protocol == first.protocol && row.hw_protocol_override == first.hw_protocol_override
        }) {
            return Ok(Some(first));
        }

        let choices = rows
            .iter()
            .map(|row| {
                // The J2610_on family has no config_name (reserved for the
                // bare SAE_J2610_SCI rows); fall back to naming the resource
                // id, since protocol_name is identical for all of them and
                // would not distinguish the choices.
                let label = row.config_name.unwrap_or(row.protocol_name);
                format!("{label} (resource_id 0x{:04X})", row.resource_id)
            })
            .collect::<Vec<_>>()
            .join(", ");
        Err(Status::invalid_argument(format!(
            "resource name {name:?} is ambiguous across {} configurations; select a specific one \
             (e.g. by resource_id, or by supplying dlc_pin_data): {choices}",
            rows.len()
        )))
    }

    /// Resolves a name to a single `ChannelProtocol` via the resources
    /// table, for `OBJT_PROTOCOL` lookups specifically -- which only care
    /// about protocol *identity*, unlike `find_table_row_by_name`
    /// (`OBJT_RESOURCE`/`CreateComLogicalLink`), which must pick one
    /// specific hardware configuration and so treats differing
    /// `hw_protocol_override`s as ambiguous too. That stricter check is
    /// wrong here: the four `"SAE_J2610_on_SAE_J2610_SCI"` rows share one
    /// `ChannelProtocol` (`0x0160`) and differ only in `hw_protocol_override`
    /// (which configuration's pins/hardware id to connect with) -- for a
    /// protocol-identity query that is not an ambiguity at all, so this
    /// helper only rejects a name whose matching rows differ in `protocol`
    /// itself (e.g. the bare `"SAE_J2610_SCI"` name, which spans 4 distinct
    /// `ChannelProtocol`s and stays genuinely ambiguous).
    fn find_protocol_for_name(name: &str) -> Result<Option<ChannelProtocol>, Status> {
        let rows = Self::find_table_rows_by_name(name);
        if rows.is_empty() {
            return Ok(None);
        }

        let first = rows[0];
        if rows.iter().all(|row| row.protocol == first.protocol) {
            return Ok(Some(first.protocol));
        }

        let choices = rows
            .iter()
            .map(|row| {
                let label = row.config_name.unwrap_or(row.protocol_name);
                format!("{label} (resource_id 0x{:04X})", row.resource_id)
            })
            .collect::<Vec<_>>()
            .join(", ");
        Err(Status::invalid_argument(format!(
            "protocol name {name:?} is ambiguous across {} distinct protocols; select a specific \
             one (e.g. by resource_id, or a config-specific name): {choices}",
            rows.len()
        )))
    }

    /// Case-insensitive lookup of a resource-table row by `bus_type_name`,
    /// returning its `bus_type_id`. Unlike `protocol_name`/
    /// `find_table_row_by_name`, `bus_type_name` is 1:1 with `bus_type_id`
    /// for every row in the table by construction (grouped by the
    /// `BUSTYPE_*` constants in `resources.rs`), so the first matching row's
    /// `bus_type_id` is authoritative regardless of how many rows share that
    /// bus type -- no ambiguity handling like `find_table_row_by_name`'s is
    /// needed here.
    fn find_bustype_id_by_name(name: &str) -> Option<u32> {
        resources::resource_table()
            .iter()
            .find(|row| row.bus_type_name.eq_ignore_ascii_case(name))
            .map(|row| row.bus_type_id)
    }

    /// Resolves a single `PinData`'s `dlc_pin_type` to a numeric logical pin
    /// type id -- `None` when no type was supplied at all (a bare pin
    /// number), `Err` only for an unrecognized type name. Shared by
    /// `retain_rows_matching_pin` (row narrowing) and
    /// `compute_pin_select` (ADR-156 Decision 2's primary/secondary
    /// determination) so the two never resolve a type name differently.
    fn resolve_pin_type_id(pin: &vci_service_interface::PinData) -> Result<Option<u32>, Status> {
        match &pin.dlc_pin_type {
            None => Ok(None),
            Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeId(id)) => Ok(Some(*id)),
            Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(name)) => {
                if let Some(id) = Self::map_pintype_name(name) {
                    Ok(Some(id))
                } else if let Ok(id) = name.parse::<u32>() {
                    Ok(Some(id))
                } else {
                    Err(Status::invalid_argument(
                        "resource_data.dlc_pin_type_name is not recognized",
                    ))
                }
            }
        }
    }

    /// Narrows `candidates` in place to rows matching a single requested
    /// `PinData`, per the row-based semantics
    /// `resolve_resource_ids_from_data`/`find_table_row_by_name` share:
    /// - `dlc_pin_number > 0`: the row must contain that pin number; if a pin
    ///   type is also supplied, it must equal that specific pin's type.
    /// - `dlc_pin_number == 0` with a pin type: matches rows having *any* pin
    ///   of that type (no specific number requested).
    /// - Neither present: no constraint (this `PinData` entry is a no-op).
    ///
    /// Returns `Err` only for an unrecognized pin-type name -- a recognized
    /// type/number combination that matches no candidate simply narrows to
    /// an empty `Vec`, it is not an error (callers surface that as "no
    /// results"/"pin set matches no configuration" as appropriate).
    fn retain_rows_matching_pin(
        candidates: &mut Vec<&'static resources::ResourceDef>,
        pin: &vci_service_interface::PinData,
    ) -> Result<(), Status> {
        let pin_type_id = Self::resolve_pin_type_id(pin)?;

        if pin.dlc_pin_number > 0 {
            candidates.retain(|row| {
                row.dlc_pins.iter().any(|&(num, ty)| {
                    num == pin.dlc_pin_number && pin_type_id.is_none_or(|want| want == ty)
                })
            });
        } else if let Some(want_ty) = pin_type_id {
            candidates.retain(|row| row.dlc_pins.iter().any(|&(_, ty)| ty == want_ty));
        }
        Ok(())
    }

    /// Narrows `candidates` in place against the FULL `dlc_pin_data` set --
    /// the shared entry point both `find_table_row_by_name` and
    /// `resolve_resource_ids_from_data` use instead of looping
    /// `retain_rows_matching_pin` per entry directly.
    ///
    /// SAE J2534-2 clause 24 Table 103 (Codex review, PR #102 round 6): a
    /// per-entry loop calling `retain_rows_matching_pin` once per `PinData`
    /// is correct for every ordinary row (each entry is independently
    /// conjunctive against that row's ONE fixed `dlc_pins`), but wrong for
    /// Ethernet_NDIS specifically once Option 2's alternate Tx pins (1/9)
    /// are considered a valid match (the sibling fix to
    /// `resources::ethernet_ndis_alternate_pin_overlap`, closing the same
    /// class of gap for resource lookup instead of conflict detection):
    /// evaluating each entry independently against "Option 1 OR Option 2"
    /// would let an impossible MIXED selection -- e.g. `(1, PLUS)` (Option
    /// 2's Tx(+)) combined with `(11, MINUS)` (Option 1's Tx(-)) -- survive
    /// both individual retains even though no single real wiring satisfies
    /// both at once. So the Ethernet_NDIS row is set aside before the
    /// ordinary per-entry loop runs (which only matches Option 1, via the
    /// row's own `dlc_pins`, same as it always did) and separately
    /// re-admitted only if [`Self::ethernet_ndis_row_matches_full_pin_set`]
    /// confirms the WHOLE `dlc_pin_data` set is simultaneously satisfiable
    /// under Option 1 or simultaneously satisfiable under Option 2 -- never
    /// a mix of both.
    fn retain_rows_matching_all_pins(
        candidates: &mut Vec<&'static resources::ResourceDef>,
        dlc_pin_data: &[vci_service_interface::PinData],
    ) -> Result<(), Status> {
        if dlc_pin_data.is_empty() {
            return Ok(());
        }

        let ndis_row = candidates
            .iter()
            .position(|row| row.protocol == ChannelProtocol::ETHERNET_NDIS)
            .map(|idx| candidates.remove(idx));

        for pin in dlc_pin_data {
            Self::retain_rows_matching_pin(candidates, pin)?;
        }

        if let Some(row) = ndis_row
            && Self::ethernet_ndis_row_matches_full_pin_set(row, dlc_pin_data)?
        {
            candidates.push(row);
        }
        Ok(())
    }

    /// Evaluates `dlc_pin_data` as ONE atomic wiring choice against the
    /// Ethernet_NDIS row: true iff every entry matches Option 1's wiring
    /// (`row.dlc_pins` itself, pins 3/8/11/12/13), OR every entry matches
    /// Option 2's wiring (pins 1/8/9/12/13 -- Option 1's Tx pins 3/11
    /// substituted for their Table 103 alternates 1/9, same logical
    /// `PIN_PLUS`/`PIN_MINUS` types, everything else unchanged) -- checked
    /// as two separate, mutually exclusive whole-set evaluations, never a
    /// pin-by-pin mix of both. Reuses `row.dlc_pins` directly for Option 1
    /// rather than a duplicated constant, so this can never drift from
    /// whatever `PINS_ETHERNET_NDIS` (`resources.rs`) actually contains.
    fn ethernet_ndis_row_matches_full_pin_set(
        row: &'static resources::ResourceDef,
        dlc_pin_data: &[vci_service_interface::PinData],
    ) -> Result<bool, Status> {
        let option1 = row.dlc_pins;
        let plus = Self::map_pintype_name("PLUS");
        let minus = Self::map_pintype_name("MINUS");
        let option2: Vec<(u32, u32)> = option1
            .iter()
            .map(|&(num, ty)| match num {
                3 => (1, plus.unwrap_or(ty)),
                11 => (9, minus.unwrap_or(ty)),
                other => (other, ty),
            })
            .collect();

        let matches_wiring = |wiring: &[(u32, u32)]| -> Result<bool, Status> {
            for pin in dlc_pin_data {
                let pin_type_id = Self::resolve_pin_type_id(pin)?;
                let matched = if pin.dlc_pin_number > 0 {
                    wiring.iter().any(|&(num, ty)| {
                        num == pin.dlc_pin_number && pin_type_id.is_none_or(|want| want == ty)
                    })
                } else if let Some(want_ty) = pin_type_id {
                    wiring.iter().any(|&(_, ty)| ty == want_ty)
                } else {
                    true
                };
                if !matched {
                    return Ok(false);
                }
            }
            Ok(true)
        };

        Ok(matches_wiring(option1)? || matches_wiring(&option2)?)
    }

    /// SAE J2534-2 clause 6's Pin Selection packs a caller's two DLC pins
    /// into one `pin_select: u32` bitmask as `0x0000PPSS` (ADR-156 Decision
    /// 2), where `PP` is whichever pin is logically "primary" (HI/K/TX/PLUS)
    /// and `SS` is whichever is "secondary" (LOW/L/RX/MINUS). These two
    /// arrays are `map_pintype_name`'s 2000-range values grouped by that
    /// primary/secondary split (mirrors `resources.rs`'s own `PIN_HI`-family
    /// constants, duplicated there for the same reason: this module already
    /// owns `map_pintype_name`, so these need to stay in sync with it, not
    /// with `resources.rs`).
    const PIN_TYPE_PRIMARY: [u32; 4] = [2000, 2002, 2004, 2006]; // HI, K, TX, PLUS
    const PIN_TYPE_SECONDARY: [u32; 4] = [2001, 2003, 2005, 2007]; // LOW, L, RX, MINUS

    /// SAE J2534-2 clause 6 Pin Selection resolution (ADR-156 Decision 2),
    /// layered on top of an already-resolved `(protocol, resource_row)`
    /// pair. Compares `dlc_pin_data` structurally (number *and*, where
    /// specified, type) against the resolved default DLC pins for this
    /// protocol via `dlc_pin_data_matches_defaults` -- `resource_row`'s own
    /// `dlc_pins` when a table row matched, else
    /// `resources::default_dlc_pins_for_hw_protocol` keyed off the same
    /// hardware id a table-less resolution would connect with.
    ///
    /// The raw hardware id (`resource_row`'s `hw_protocol_override`, else
    /// `protocol.j2534_protocol_id()`) is normalized via
    /// `resources::base_protocol_id` *before* any of that -- a caller can
    /// name a `_PS` hardware variant directly (e.g. `protocol_id =
    /// PROTOCOL_CAN_PS`), which the legacy `ChannelProtocol::from_raw`
    /// fallback passes through unchanged since no `resources` table row
    /// exists for a `_PS` id (ADR-156 Decision 1). Without normalizing
    /// first, both the default-pins lookup and `ps_protocol_id` below are
    /// called with a `_PS` id neither is keyed by, silently returning
    /// `None`/rejecting a request that should have succeeded (found by
    /// Codex review, PR #28, a later round).
    ///
    /// - `dlc_pin_data` empty: `Ok(None)` when the raw id was already a base
    ///   (non-`_PS`) id, matching clause 6.3.3.2's auto-connect-to-
    ///   default-pins behavior. Rejected with `invalid_argument` when the
    ///   raw id was already `_PS` -- a `_PS` channel connects with its pins
    ///   *unassigned* until `SET_CONFIG(CONFIG_J1962_PINS)` (clause 6.3.3.2),
    ///   which this service only ever issues from `pin_select` computed
    ///   here; a raw `_PS` id with no `dlc_pin_data` to compute it from
    ///   would connect a channel this service can never assign pins to.
    /// - An exact structural match against the default set is treated as
    ///   "default, no Pin Selection" and short-circuits to `Ok(None)` --
    ///   regardless of whether the raw id was already `_PS` (Correction,
    ///   design-advisor, PR #28, a later round: an earlier version of this
    ///   function always computed a real `pin_select` for a directly-named
    ///   `_PS` id even when its pins matched the base defaults, reasoning
    ///   that the hardware channel starts pin-unassigned regardless -- true
    ///   per clause 6.3.3.2's sequencing, but it gave one physical wiring
    ///   two different `ChannelKey`s: `(PROTOCOL_CAN_PS, baud, 0x060E)` for
    ///   the directly-named request vs. `(CAN, baud, 0)` for an ordinary
    ///   default-pins connect, splitting `same_physical_resource`'s lock
    ///   scope across them and modeling a two-channel coexistence clause
    ///   6.3.3.2's own pin-conflict rule forbids -- Codex review, PR #28).
    ///   `already_ps` with `default_pins == None` cannot arise in practice
    ///   (every `_PS`-scoped hardware id has a `default_dlc_pins_for_hw_protocol`
    ///   entry, kept in sync with `ps_protocol_id` by construction; no
    ///   `resources` table row's `hw_protocol_override` is ever a `_PS` id,
    ///   so `already_ps` only arises via the table-less `from_raw` fallback)
    ///   -- if that invariant is ever broken by a future `_PS`-scoped
    ///   addition, this falls through to the non-default-pins path below
    ///   rather than silently canonicalizing against the wrong defaults.
    /// - Otherwise, the caller is asking for non-default pins: rejected
    ///   with `invalid_argument` unless `j2534_2_opted_in` (ADR-156 Decision
    ///   4/clause 5 -- Pin Selection is a J2534-2 feature, never silently
    ///   honored for a module that hasn't opted in). A directly-named `_PS`
    ///   id is checked against this same opt-in gate even when its pins
    ///   canonicalize to `Ok(None)` above (checked earlier, right after the
    ///   empty-`dlc_pin_data` case) -- naming a `_PS` id at all is using
    ///   J2534-2 vocabulary (clause 6.3.1 Table 1), regardless of what pins
    ///   the resulting connect ends up equivalent to.
    /// - When opted in, `resources::ps_protocol_id` (called with the
    ///   already-normalized base id) decides whether this hardware protocol
    ///   has a `_PS` variant at all (ADR-156 Decision 2's scope narrowing to
    ///   clause 6.3.1 Table 1's seven protocols) -- `Some(ps_id)` computes
    ///   `pin_select` via `compute_pin_select` and returns
    ///   `Ok(Some((hw_protocol_id, ps_id, pin_select)))`; `None` rejects
    ///   with `invalid_argument` (an otherwise-supported protocol this phase
    ///   doesn't extend to Pin Selection). The returned `hw_protocol_id`
    ///   (ADR-157/Bug 1 fix) is this function's own correctly-resolved base
    ///   hardware protocol id, so callers no longer need to lossily
    ///   re-derive it from `protocol` alone (which collapses every SCI
    ///   variant onto `SCI_MODE`).
    fn resolve_pin_selection(
        protocol: ChannelProtocol,
        resource_row: Option<&'static resources::ResourceDef>,
        dlc_pin_data: &[vci_service_interface::PinData],
        j2534_2_opted_in: bool,
        requested_index: Option<u32>,
    ) -> Result<Option<(u32, u32, u32)>, Status> {
        let raw_hw_protocol_id = resource_row
            .and_then(|row| row.hw_protocol_override)
            .unwrap_or_else(|| protocol.j2534_protocol_id());
        // ADR-158/Phase 3a: a directly-named SAE J2534-2 clause 21 CAN FD
        // (`_PS` or, as of ADR-213/Round 3, `_CHx`) hardware protocol id is
        // rejected outright here, before any
        // other resolution below -- unlike clause 6 Pin Selection's `_PS`
        // ids, there is no valid direct-naming route for CAN FD at all: it is
        // a ComParam-driven mode (staged `CP_CANFDTxMaxDataLength`/
        // `CP_CANFDBaudrate`), inferred from the Working set on an ordinary
        // `CAN` resource at `ConnectComLogicalLink` time
        // (`rpc_link::J2534Service::apply_fd_mode`). Critically, this must
        // NOT silently normalize through `resources::base_protocol_id`'s new
        // FD arm below to a plain `CAN` connect -- that would drop the
        // caller's FD intent entirely, the exact bug shape ADR-156/ADR-157
        // have repeatedly found and fixed for `_PS`/`_CHx` (e.g.
        // `already_ps`'s own rejection just below). Runs unconditionally,
        // regardless of `dlc_pin_data`/`j2534_2_opted_in` -- naming the id at
        // all is the error, not something an opt-in module could legitimize.
        if resources::is_fd_protocol_id(raw_hw_protocol_id) {
            return Err(Status::invalid_argument(
                "resource_data names a SAE J2534-2 clause 21 CAN FD or clause 22 \
                 ISO15765-on-CAN-FD (_PS or _CHx) hardware protocol id directly -- FD mode is \
                 derived from staged Working ComParams (CP_CANFDTxMaxDataLength / \
                 CP_CANFDBaudrate) at ConnectComLogicalLink time, not from a directly-named \
                 protocol id; stage those ComParams via SetComParam and connect to the ordinary \
                 CAN or ISO15765 resource (optionally with a SAE J2534-2 clause 7 Additional \
                 Channel index) instead",
            ));
        }
        // ADR-164/Phase 4: SAE J2534-2 clause 9 Single Wire CAN's two
        // `_PS`-only hardware ids, positioned right after the FD guard above
        // for the same "naming this vocabulary at all is decisive" placement
        // -- but structurally the OPPOSITE case from FD: FD is a
        // ComParam-driven *substitution* with no valid direct-naming route at
        // all (rejected above), whereas an SW resource row's
        // `hw_protocol_override` (added in `resources.rs`) IS this row's own
        // native connect id -- there is no unqualified SW base id to
        // substitute from (clause 9 defines only `SW_CAN_PS`/
        // `SW_ISO15765_PS`). Clause 9.2.1: the physical layer stays
        // pin-unassigned until an explicit SET_CONFIG(CONFIG_J1962_PINS) is
        // issued -- unlike every base-id'd row, there is no "implicit,
        // already-pinned default" connect for SW at all, so this always
        // returns a real `pin_select` triple (never canonicalizes to
        // `Ok(None)` the way the defaults-match short-circuit below does for
        // an ordinary base connect).
        if resources::is_sw_protocol_id(raw_hw_protocol_id) {
            // ADR-212/Round 2: Single Wire CAN is now also in scope for
            // clause 7 Additional Channels (`resources::chx_block_base`'s new
            // `PROTOCOL_SW_CAN_PS => Some(PROTOCOL_SW_CAN_CAN_CH1)`/
            // `PROTOCOL_SW_ISO15765_PS => Some(PROTOCOL_SW_CAN_ISO15765_CH1)`
            // entries), so -- mirroring the FT arm's own
            // `requested_index.is_some()` bypass below -- a compound
            // `_CHx`-suffixed name (e.g. "SW_CAN_PS_CH1") resolves its head to
            // this same row/protocol via `resolve_protocol_name_with_chx_suffix`,
            // reaching this arm with `raw_hw_protocol_id` equal to `SW_CAN_PS`/
            // `SW_ISO15765_PS` but `requested_index` set. Clause 7 Additional
            // Channels have no J1962 pin concept at all, so a caller combining
            // a `_CHx` suffix with explicit dlc_pin_data must be rejected
            // here, not silently accepted with the pins discarded -- and
            // without this bypass, an empty-`dlc_pin_data` compound-name
            // connect fell through to the `dlc_pin_data.is_empty()` branch
            // below, matched the resource-table row, and returned the row's
            // own default single-pin selection (`Some(pin_select)`) instead
            // of `Ok(None)`, which `resolve_channel_selection`'s own
            // clause-6/clause-7 mutual-exclusion check then rejected as
            // combining Pin Selection with an Additional Channel -- the exact
            // masking gap ADR-212's Context section describes.
            if requested_index.is_some() {
                if !dlc_pin_data.is_empty() {
                    return Err(Status::invalid_argument(
                        "resource_data requests both SAE J2534-2 clause 6 Pin Selection \
                         (non-empty dlc_pin_data) and a clause 7 Additional Channel (a compound \
                         `_CHx`-suffixed resource name) for a SAE J2534-2 clause 9 Single Wire \
                         CAN (SW_CAN_PS/SW_ISO15765_PS) resource -- these are mutually exclusive: \
                         clause 7 channels live on vendor connectors, never on J1962 pins",
                    ));
                }
                return Ok(None);
            }
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 9 Single Wire CAN \
                     (SW_CAN_PS/SW_ISO15765_PS) resource, which is a SAE J2534-2 feature -- \
                     this module has not opted into J2534-2 (its pname lacks the \"J2534-2:\" \
                     prefix, clause 5)",
                ));
            }
            let base_id = resources::base_protocol_id(raw_hw_protocol_id);
            let pin_select = if dlc_pin_data.is_empty() {
                // The row's own single default pin (1, PIN_HI per clause
                // 9.2.1) is only a real default when an actual SW resource
                // row was matched (`resource_row.is_some()`) -- a caller
                // naming `SW_CAN_PS`/`SW_ISO15765_PS` directly, bypassing the
                // resource table entirely (`resource_row: None`, e.g. a bare
                // `ProtocolId` selector `resolve_protocol_id` couldn't match
                // to a row), has made no such choice at all: clause 9.2.1
                // identifies no default pin, so silently assigning one here
                // would connect the caller to wiring it never selected
                // (edge-case-hunter finding, Codex review round 2). Packed
                // as `0x0000PPSS` with SS = 0 ("no secondary pin") -- matches
                // `compute_pin_select`'s own format for a single typed pin,
                // computed directly here rather than via
                // `resources::default_pin_select_for_base` (ADR-164 Decision
                // 1: that helper's two-pin -- one primary, one secondary --
                // contract does not apply to a single-pin bus).
                if resource_row.is_none() {
                    return Err(Status::invalid_argument(
                        "resource_data names the raw SW_CAN_PS/SW_ISO15765_PS hardware protocol \
                         id directly, bypassing the resource table -- SAE J2534-2 clause 9.2.1 \
                         identifies no default pin, so dlc_pin_data must explicitly select pin 1 \
                         (clause 9.2.1's one and only supported pin) when naming this id outside \
                         a resource-table row",
                    ));
                }
                0x0000_0100
            } else if dlc_pin_data.len() > 1 {
                // Codex review finding, PR #57 (surfaced while adding the
                // sibling UART Echo Byte arm below, which copied this same
                // shape): clause 9's SW bus is single-wire -- its resource
                // row defines exactly one pin, with no secondary role at
                // all -- so `compute_pin_select`'s general 2-pin packing
                // (built for genuine dual-wire buses like CAN/FT-CAN) must
                // not be reached here; a caller-supplied secondary pin would
                // otherwise be silently packed into `CONFIG_J1962_PINS` and
                // configure wiring this bus has no second conductor for.
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data supplies more than one pin for a SAE J2534-2 \
                     clause 9 Single Wire CAN (SW_CAN_PS/SW_ISO15765_PS) connection -- this bus \
                     is single-wire and has no secondary pin role to pair a second entry with",
                ));
            } else {
                let computed = Self::compute_pin_select(dlc_pin_data)?;
                // Sibling gap to the FT-CAN closed-set check below (ADR-168's
                // "Seventh correction") -- clause 9.2.1 attaches SW_CAN to
                // J1962 pin 1 only, a closed set of exactly one pin, but this
                // arm previously accepted any single well-formed pin
                // `compute_pin_select` packed, silently forwarding a
                // physically meaningless pin (e.g. pin 6) as
                // `SET_CONFIG(J1962_PINS)`. A wrong pin is rejected here
                // instead, mirroring the FT arm's own
                // `computed != 0x0000_0109 && computed != 0x0000_030B` check.
                if computed != 0x0000_0100 {
                    return Err(Status::invalid_argument(
                        "resource_data.dlc_pin_data does not select the one pin SAE J2534-2 \
                         clause 9.2.1 defines for Single Wire CAN (SW_CAN_PS/SW_ISO15765_PS) \
                         connections -- pin 1/HI -- unlike the general clause 6 Pin Selection \
                         mechanism, SW_CAN_PS/SW_ISO15765_PS accept no other pin",
                    ));
                }
                computed
            };
            return Ok(Some((base_id, raw_hw_protocol_id, pin_select)));
        }
        // ADR-168/Phase 6: SAE J2534-2 clause 20 Fault-Tolerant CAN's two
        // `_PS`-only hardware ids, positioned right after the SW arm above
        // for the same reasoning -- an FT resource row's `hw_protocol_override`
        // IS this row's own native connect id (clause 20 defines no
        // unqualified base FTCAN id either). Unlike SW, FTCAN is a genuine
        // CAN-high/CAN-low differential pair (like DWCAN), so its own default
        // pin selection is a two-pin (primary + secondary) pair rather than a
        // single pin -- clause 20.2.1 still identifies no default pin at all,
        // so this always returns a real `pin_select` triple, exactly
        // mirroring the SW arm's "never canonicalizes to Ok(None)" behavior.
        //
        // ADR-211: Fault-Tolerant CAN is now also in scope for clause 7
        // Additional Channels (`resources::chx_block_base`'s new
        // `PROTOCOL_FT_CAN_PS => Some(PROTOCOL_FT_CAN_CH1)`/
        // `PROTOCOL_FT_ISO15765_PS => Some(PROTOCOL_FT_ISO15765_CH1)`
        // entries), so -- mirroring UART Echo Byte's/Honda DIAG-H's/SAE
        // J1708's own `requested_index.is_some()` bypass above -- a compound
        // `_CHx`-suffixed name (e.g. "FT_CAN_PS_CH1") resolves its head to
        // this same row/protocol via `resolve_protocol_name_with_chx_suffix`,
        // reaching this arm with `raw_hw_protocol_id` equal to `FT_CAN_PS`/
        // `FT_ISO15765_PS` but `requested_index` set. Clause 7 Additional
        // Channels have no J1962 pin concept at all, so a caller combining a
        // `_CHx` suffix with explicit dlc_pin_data must be rejected here, not
        // silently accepted with the pins discarded -- and without this
        // bypass, an empty-`dlc_pin_data` compound-name connect fell through
        // to the `dlc_pin_data.is_empty()` branch below, matched the
        // resource-table row, and returned the row's own default two-pin
        // selection (`Some(pin_select)`) instead of `Ok(None)`, which
        // `resolve_channel_selection`'s own clause-6/clause-7
        // mutual-exclusion check then rejected as combining Pin Selection
        // with an Additional Channel -- the same masking gap ADR-211's
        // Context section documents (its own removal is why this fix and the
        // `resolve_channel_selection`/`base_protocol_id` infrastructure fixes
        // below must land together). A directly-named `_CHx` id (e.g.
        // `PROTOCOL_FT_CAN_CH1`) never reaches this arm at all -- it fails
        // the outer `is_ft_protocol_id` match (an exact `_PS`-only check,
        // never widened to the `_CHx` ranges) and falls through to this
        // function's generic tail's `Ok(None)`, then on to
        // `resolve_channel_selection`'s own `already_chx`/
        // `chx_base_protocol_id` route, the same as every other in-scope
        // family's directly-named `_CHx` id already does.
        if resources::is_ft_protocol_id(raw_hw_protocol_id) {
            if requested_index.is_some() {
                if !dlc_pin_data.is_empty() {
                    return Err(Status::invalid_argument(
                        "resource_data requests both SAE J2534-2 clause 6 Pin Selection \
                         (non-empty dlc_pin_data) and a clause 7 Additional Channel (a compound \
                         `_CHx`-suffixed resource name) for a SAE J2534-2 clause 20 Fault-Tolerant \
                         CAN (FT_CAN_PS/FT_ISO15765_PS) resource -- these are mutually exclusive: \
                         clause 7 channels live on vendor connectors, never on J1962 pins",
                    ));
                }
                return Ok(None);
            }
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 20 Fault-Tolerant CAN \
                     (FT_CAN_PS/FT_ISO15765_PS) resource, which is a SAE J2534-2 feature -- this \
                     module has not opted into J2534-2 (its pname lacks the \"J2534-2:\" prefix, \
                     clause 5)",
                ));
            }
            let base_id = resources::base_protocol_id(raw_hw_protocol_id);
            let pin_select = if dlc_pin_data.is_empty() {
                // The row's own two-pin default (pin 1/PIN_HI primary, pin
                // 9/PIN_LOW secondary per clause 20.2.1's first-listed
                // pin-pair option) is only a real default when an actual FT
                // resource row was matched (`resource_row.is_some()`) -- same
                // reasoning, and same Codex-review finding, as the SW arm
                // above: a caller naming `FT_CAN_PS`/`FT_ISO15765_PS`
                // directly, bypassing the resource table, has made no pin
                // choice at all, and clause 20.2.1 identifies no default
                // pin-pair either. Packed as `0x0000PPSS` -- computed
                // directly here rather than via
                // `resources::default_pin_select_for_base`, for the same
                // "this table already owns the pin data, no base id to
                // derive it from" reason the SW arm above avoids that helper.
                if resource_row.is_none() {
                    return Err(Status::invalid_argument(
                        "resource_data names the raw FT_CAN_PS/FT_ISO15765_PS hardware protocol \
                         id directly, bypassing the resource table -- SAE J2534-2 clause 20.2.1 \
                         identifies no default pin-pair, so dlc_pin_data must explicitly select \
                         one of the two documented pin-pairs (1/9 or 3/11) when naming this id \
                         outside a resource-table row",
                    ));
                }
                0x0000_0109
            } else {
                let computed = Self::compute_pin_select(dlc_pin_data)?;
                // Codex review round 6/design-advisor: unlike the general
                // clause-6 mechanism (shared by CAN_PS/ISO15765_PS/FD
                // variants, and deliberately left untouched here -- its own
                // per-bus completeness check is
                // `resources::secondary_pin_requirement`, enforced further
                // down in the fallback path below, ADR-201), clause 20.2.1
                // defines FT-CAN (ISO 11898-3) as connectable on exactly two
                // pin-pairs: (1,HI)+(9,LOW) or (3,HI)+(11,LOW). A single
                // explicit pin (physically incomplete) or a mismatched pair
                // (e.g. 1/HI + 11/LOW, well-formed per the generic pin-typing
                // rules but not one of the two documented pairs) must be
                // rejected here, not silently accepted just because
                // `compute_pin_select` packed it into a valid-looking
                // bitmask.
                if computed != 0x0000_0109 && computed != 0x0000_030B {
                    return Err(Status::invalid_argument(
                        "resource_data.dlc_pin_data does not select one of the two pin-pairs \
                         SAE J2534-2 clause 20.2.1 defines for Fault-Tolerant CAN (ISO 11898-3) \
                         connections -- (1/HI, 9/LOW) or (3/HI, 11/LOW) -- unlike the general \
                         clause 6 Pin Selection mechanism, FT_CAN_PS/FT_ISO15765_PS accept no \
                         other pin or pin pair",
                    ));
                }
                computed
            };
            return Ok(Some((base_id, raw_hw_protocol_id, pin_select)));
        }
        // ADR-170/Phase 9: SAE J2534-2 clause 12 UART Echo Byte's one
        // `_PS`-only hardware id, positioned right after the FT arm above --
        // it shares FD/SW/FT's "no unqualified base id to normalize onto"
        // shape (`ChannelProtocol::UART_ECHO_BYTE_PS` already self-identifies
        // via `j2534_protocol_id()`, so there is nothing for
        // `resources::base_protocol_id` below to collapse it to), but unlike
        // FD/SW/FT it has no CAN-family relationship at all: `base_id` here
        // equals `raw_hw_protocol_id` itself, not a different family's base.
        // Row 0x023A's own default pin (7, VW/Audi convention) is only a
        // real default when an actual resource-table row was matched -- same
        // reasoning as the SW/FT arms above: clause 12.2.2 identifies no
        // default pin at all, so a caller naming `UART_ECHO_BYTE_PS`
        // directly, bypassing the resource table, has made no pin choice
        // (ADR-170 Decision 2, mirroring ADR-164 Decision 1's "always issue
        // explicit SET_CONFIG" framing). Unlike FT, clause 12 documents no
        // second pin option, so (like the SW arm) any `dlc_pin_data` the
        // caller does supply must resolve to exactly one pin -- more than
        // one is rejected below rather than silently packed via the general
        // `compute_pin_select` two-pin path (Codex review finding, PR #57).
        //
        // ADR-207 Decision item 8: UART Echo Byte is now also in scope for
        // clause 7 Additional Channels (`resources::chx_block_base`'s new
        // `PROTOCOL_UART_ECHO_BYTE_PS => Some(PROTOCOL_ECHO_BYTE_CH1)` entry),
        // so -- mirroring GM UART's own `requested_index.is_some()` bypass
        // below and SAE J1939's own bypass further up -- a compound
        // `_CHx`-suffixed name (e.g. "UART_ECHO_BYTE_CH1") resolves its head
        // to this same row/protocol via `resolve_protocol_name_with_chx_suffix`,
        // reaching this arm with `raw_hw_protocol_id ==
        // PROTOCOL_UART_ECHO_BYTE_PS` but `requested_index` set. Clause 7
        // Additional Channels have no J1962 pin concept at all, so a caller
        // combining a `_CHx` suffix with explicit dlc_pin_data must be
        // rejected here, not silently accepted with the pins discarded (same
        // rationale as GM UART's own PR #98 round 3 fix) -- and without this
        // bypass, an empty-`dlc_pin_data` compound-name connect fell through
        // to the `dlc_pin_data.is_empty()` branch below, matched the
        // resource-table row, and returned the row's own default pin
        // (`Some(pin_select)`) instead of `Ok(None)`, which
        // `resolve_channel_selection`'s own clause-6/clause-7
        // mutual-exclusion check then rejected as combining Pin Selection
        // with an Additional Channel -- a real gap ADR-207's own initial
        // investigation missed (confirmed by a live repro before this fix
        // landed). A directly-named `_CHx` id (e.g. `PROTOCOL_ECHO_BYTE_CH1`)
        // never reaches this arm at all -- it fails the outer
        // `is_uart_echo_byte_protocol_id` match (an exact `_PS`-only check,
        // never widened to the `_CH1..128` range) and falls through to this
        // function's generic tail's `Ok(None)`, then on to
        // `resolve_channel_selection`'s own `already_chx`/
        // `chx_base_protocol_id` route, the same as every other in-scope
        // family's directly-named `_CHx` id already does.
        if resources::is_uart_echo_byte_protocol_id(raw_hw_protocol_id) {
            if requested_index.is_some() {
                if !dlc_pin_data.is_empty() {
                    return Err(Status::invalid_argument(
                        "resource_data requests both SAE J2534-2 clause 6 Pin Selection \
                         (non-empty dlc_pin_data) and a clause 7 Additional Channel (a compound \
                         `_CHx`-suffixed resource name) for a SAE J2534-2 clause 12 UART Echo \
                         Byte Protocol (UART_ECHO_BYTE_PS) resource -- these are mutually \
                         exclusive: clause 7 channels live on vendor connectors, never on J1962 \
                         pins",
                    ));
                }
                return Ok(None);
            }
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 12 UART Echo Byte Protocol \
                     (UART_ECHO_BYTE_PS) resource, which is a SAE J2534-2 feature -- this module \
                     has not opted into J2534-2 (its pname lacks the \"J2534-2:\" prefix, clause \
                     5)",
                ));
            }
            let pin_select = if dlc_pin_data.is_empty() {
                if resource_row.is_none() {
                    return Err(Status::invalid_argument(
                        "resource_data names the raw UART_ECHO_BYTE_PS hardware protocol id \
                         directly, bypassing the resource table -- SAE J2534-2 clause 12.2.2 \
                         identifies no default pin, so dlc_pin_data must explicitly select pin 7 \
                         (or another supported pin) when naming this id outside a resource-table \
                         row",
                    ));
                }
                0x0000_0700
            } else if dlc_pin_data.len() > 1 {
                // Codex review finding, PR #57: clause 12's K-line is
                // single-wire, and Row 0x023A defines exactly one pin, so
                // `compute_pin_select`'s general 2-pin packing (built for
                // genuine dual-wire buses) must not be reached here -- a
                // caller-supplied secondary pin would otherwise be silently
                // packed into `CONFIG_J1962_PINS`, configuring wiring this
                // protocol has no second conductor for. Mirrors the SW arm's
                // identical, same-shaped guard above (a pre-existing gap
                // this new arm copied and Codex's review surfaced; fixed
                // there too in this same PR).
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data supplies more than one pin for a SAE J2534-2 \
                     clause 12 UART Echo Byte Protocol (UART_ECHO_BYTE_PS) connection -- this \
                     protocol is single-wire and has no secondary pin role to pair a second \
                     entry with",
                ));
            } else {
                Self::compute_pin_select(dlc_pin_data)?
            };
            return Ok(Some((raw_hw_protocol_id, raw_hw_protocol_id, pin_select)));
        }
        // ADR-174/Phase 10: SAE J2534-2 clause 13 Honda DIAG-H's one
        // `_PS`-only hardware id, positioned right after the UART Echo Byte
        // arm above -- like UART Echo Byte, it has no CAN-family
        // relationship at all and no unqualified base id to normalize onto
        // (`base_id` here equals `raw_hw_protocol_id` itself). Unlike UART
        // Echo Byte/SWCAN's own single-value closed sets, clause 13.2.4
        // documents TWO valid single pins (1 and 14, not a pair) -- the
        // structural shape is otherwise identical to the SWCAN arm above
        // (`is_empty()`/`len() > 1`/else), only the accepted value set in
        // the `else` branch's closed-set check widens from one to two.
        //
        // ADR-208 Decision item 5: Honda DIAG-H is now also in scope for
        // clause 7 Additional Channels (`resources::chx_block_base`'s new
        // `PROTOCOL_HONDA_DIAGH_PS => Some(PROTOCOL_HONDA_DIAGH_CH1)` entry),
        // so -- mirroring UART Echo Byte's own `requested_index.is_some()`
        // bypass above -- a compound `_CHx`-suffixed name (e.g.
        // "HONDA_DIAGH_CH1") resolves its head to this same row/protocol via
        // `resolve_protocol_name_with_chx_suffix`, reaching this arm with
        // `raw_hw_protocol_id == PROTOCOL_HONDA_DIAGH_PS` but
        // `requested_index` set. Clause 7 Additional Channels have no J1962
        // pin concept at all, so a caller combining a `_CHx` suffix with
        // explicit dlc_pin_data must be rejected here, not silently accepted
        // with the pins discarded (same rationale as UART Echo Byte's own
        // fix) -- and without this bypass, an empty-`dlc_pin_data`
        // compound-name connect would fall through to the
        // `dlc_pin_data.is_empty()` branch below, match the resource-table
        // row, and return the row's own default pin (`Some(pin_select)`)
        // instead of `Ok(None)`, which `resolve_channel_selection`'s own
        // clause-6/clause-7 mutual-exclusion check would then reject as
        // combining Pin Selection with an Additional Channel. Found by
        // up-front investigation this time (ADR-208 Context), not by a live
        // failing-test repro. A directly-named `_CHx` id (e.g.
        // `PROTOCOL_HONDA_DIAGH_CH1`) never reaches this arm at all -- it
        // fails the outer `is_honda_diagh_protocol_id` match (an exact
        // `_PS`-only check, never widened to the `_CH1..128` range) and
        // falls through to this function's generic tail's `Ok(None)`, then
        // on to `resolve_channel_selection`'s own `already_chx`/
        // `chx_base_protocol_id` route, the same as every other in-scope
        // family's directly-named `_CHx` id already does.
        if resources::is_honda_diagh_protocol_id(raw_hw_protocol_id) {
            if requested_index.is_some() {
                if !dlc_pin_data.is_empty() {
                    return Err(Status::invalid_argument(
                        "resource_data requests both SAE J2534-2 clause 6 Pin Selection \
                         (non-empty dlc_pin_data) and a clause 7 Additional Channel (a compound \
                         `_CHx`-suffixed resource name) for a SAE J2534-2 clause 13 Honda DIAG-H \
                         Protocol (HONDA_DIAGH_PS) resource -- these are mutually exclusive: \
                         clause 7 channels live on vendor connectors, never on J1962 pins",
                    ));
                }
                return Ok(None);
            }
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 13 Honda DIAG-H Protocol \
                     (HONDA_DIAGH_PS) resource, which is a SAE J2534-2 feature -- this module \
                     has not opted into J2534-2 (its pname lacks the \"J2534-2:\" prefix, clause \
                     5)",
                ));
            }
            let pin_select = if dlc_pin_data.is_empty() {
                // Row 0x023B's own default pin (14, the Honda diagnostic
                // application's own convention) is only a real default when
                // an actual resource-table row was matched -- same reasoning
                // as the SW/FT/UART-Echo-Byte arms above: clause 13.2.3
                // identifies no default pin at all, so a caller naming
                // `HONDA_DIAGH_PS` directly, bypassing the resource table,
                // has made no pin choice.
                if resource_row.is_none() {
                    return Err(Status::invalid_argument(
                        "resource_data names the raw HONDA_DIAGH_PS hardware protocol id \
                         directly, bypassing the resource table -- SAE J2534-2 clause 13.2.3 \
                         identifies no default pin, so dlc_pin_data must explicitly select pin 1 \
                         or pin 14 (clause 13.2.4's two documented pins) when naming this id \
                         outside a resource-table row",
                    ));
                }
                0x0000_0E00
            } else if dlc_pin_data.len() > 1 {
                // Same reasoning as the SW/UART-Echo-Byte arms above: this
                // protocol is single-wire, with no secondary pin role at
                // all, so `compute_pin_select`'s general 2-pin packing must
                // not be reached here.
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data supplies more than one pin for a SAE J2534-2 \
                     clause 13 Honda DIAG-H Protocol (HONDA_DIAGH_PS) connection -- this \
                     protocol is single-wire and has no secondary pin role to pair a second \
                     entry with",
                ));
            } else {
                let computed = Self::compute_pin_select(dlc_pin_data)?;
                // The two-single-pin analog of the SWCAN (one pin) and
                // FT-CAN (two pin-pairs) closed-set checks above -- clause
                // 13.2.4 documents exactly two valid single pins (1 and 14),
                // not an open choice.
                if computed != 0x0000_0100 && computed != 0x0000_0E00 {
                    return Err(Status::invalid_argument(
                        "resource_data.dlc_pin_data does not select one of the two pins SAE \
                         J2534-2 clause 13.2.4 defines for Honda DIAG-H Protocol \
                         (HONDA_DIAGH_PS) connections -- pin 1 or pin 14 -- unlike the general \
                         clause 6 Pin Selection mechanism, HONDA_DIAGH_PS accepts no other pin",
                    ));
                }
                computed
            };
            return Ok(Some((raw_hw_protocol_id, raw_hw_protocol_id, pin_select)));
        }
        // ADR-175/Phase 11: SAE J2534-2 clause 17 SAE J1708's one `_PS`-only
        // hardware id, positioned right after the Honda DIAG-H arm above --
        // like UART Echo Byte/Honda DIAG-H, it has no CAN-family
        // relationship at all and no unqualified base id to normalize onto
        // (`base_id` here equals `raw_hw_protocol_id` itself). Structurally
        // simpler than every predecessor: unlike SWCAN/FT-CAN/Honda DIAG-H,
        // clause 17 documents no valid-pin table on the J1962 connector at
        // all, so this arm has no `len() > 1` rejection and no closed-set
        // check on the non-empty branch -- whatever `compute_pin_select`'s
        // generic 1-or-2-pin validation allows is accepted as-is (ADR-175
        // Decision 3).
        //
        // ADR-209 Decision item 5: SAE J1708 is now also in scope for
        // clause 7 Additional Channels (`resources::chx_block_base`'s new
        // `PROTOCOL_J1708_PS => Some(PROTOCOL_J1708_CH1)` entry), so --
        // mirroring Honda DIAG-H's own `requested_index.is_some()` bypass
        // above -- a compound `_CHx`-suffixed name (e.g. "SAE_J1708_CH1")
        // resolves its head to this same row/protocol via
        // `resolve_protocol_name_with_chx_suffix`, reaching this arm with
        // `raw_hw_protocol_id == PROTOCOL_J1708_PS` but `requested_index`
        // set. Clause 7 Additional Channels have no J1962 pin concept at
        // all, so a caller combining a `_CHx` suffix with explicit
        // dlc_pin_data must be rejected here, not silently accepted with the
        // pins discarded (same rationale as Honda DIAG-H's own fix) -- and
        // without this bypass, an empty-`dlc_pin_data` compound-name connect
        // would fall through to the `dlc_pin_data.is_empty()` branch below,
        // match the resource-table row, and return the row's own default
        // pin (`Some(pin_select)`) instead of `Ok(None)`, which
        // `resolve_channel_selection`'s own clause-6/clause-7
        // mutual-exclusion check would then reject as combining Pin
        // Selection with an Additional Channel. Found by up-front
        // investigation this time (ADR-209 Context), not by a live
        // failing-test repro. A directly-named `_CHx` id (e.g.
        // `PROTOCOL_J1708_CH1`) never reaches this arm at all -- it fails
        // the outer `is_j1708_protocol_id` match (an exact `_PS`-only check,
        // never widened to the `_CH1..128` range) and falls through to this
        // function's generic tail's `Ok(None)`, then on to
        // `resolve_channel_selection`'s own `already_chx`/
        // `chx_base_protocol_id` route, the same as every other in-scope
        // family's directly-named `_CHx` id already does.
        if resources::is_j1708_protocol_id(raw_hw_protocol_id) {
            if requested_index.is_some() {
                if !dlc_pin_data.is_empty() {
                    return Err(Status::invalid_argument(
                        "resource_data requests both SAE J2534-2 clause 6 Pin Selection \
                         (non-empty dlc_pin_data) and a clause 7 Additional Channel (a compound \
                         `_CHx`-suffixed resource name) for a SAE J2534-2 clause 17 SAE J1708 \
                         Protocol (J1708_PS) resource -- these are mutually exclusive: clause 7 \
                         channels live on vendor connectors, never on J1962 pins",
                    ));
                }
                return Ok(None);
            }
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 17 SAE J1708 Protocol (J1708_PS) \
                     resource, which is a SAE J2534-2 feature -- this module has not opted into \
                     J2534-2 (its pname lacks the \"J2534-2:\" prefix, clause 5)",
                ));
            }
            let pin_select = if dlc_pin_data.is_empty() {
                // Row 0x023C's own default pins (3/11, an editorial
                // convenience default with no textual basis in clause 17 or
                // clause 6 -- see `PINS_SAE_J1708`'s own doc comment) are
                // only a real default when an actual resource-table row was
                // matched -- same reasoning as the SW/FT/UART-Echo-Byte/
                // Honda-DIAG-H arms above: clause 17.3.2.1 identifies no
                // default pin at all, so a caller naming `J1708_PS`
                // directly, bypassing the resource table, has made no pin
                // choice.
                if resource_row.is_none() {
                    return Err(Status::invalid_argument(
                        "resource_data names the raw J1708_PS hardware protocol id directly, \
                         bypassing the resource table -- SAE J2534-2 clause 17.3.2.1 identifies no \
                         default pin, so dlc_pin_data must explicitly select pins when naming this id \
                         outside a resource-table row",
                    ));
                }
                // Packed as `0x0000PPSS` (primary pin 3, secondary pin 11
                // (0x0B)) -- the same `0x0000PPSS` convention the FT-CAN arm
                // above uses for its own second documented pin-pair option
                // (pins 3/11, `0x0000_030B`), computed directly here rather
                // than via `resources::default_pin_select_for_base`, for the
                // same "this table already owns the pin data, no base id to
                // derive it from" reason the SW/FT arms above avoid that
                // helper.
                0x0000_030B
            } else {
                Self::compute_pin_select(dlc_pin_data)?
            };
            return Ok(Some((raw_hw_protocol_id, raw_hw_protocol_id, pin_select)));
        }
        // ADR-179/Phase 5, restructured by ADR-206: SAE J2534-2 clause 16 SAE
        // J1939's one `_PS`-only hardware id, positioned right after the
        // J1708 arm above -- like UART Echo Byte/Honda DIAG-H/J1708, it has
        // no CAN-family relationship at all and no unqualified base id to
        // normalize onto (`base_id` here equals `raw_hw_protocol_id` itself),
        // even though it physically rides a CAN transceiver (ADR-179
        // Context: "Standalone protocol, not a CAN bus-type variant").
        // Structurally like J1708's own arm above (no closed-set pin
        // validation: clause 6.3.3.2 documents no J1962 pin table for J1939
        // either, so whatever `compute_pin_select`'s generic 1-or-2-pin
        // validation allows is accepted as-is) but UNLIKE every arm above
        // (SW/FT/UART-Echo-Byte/Honda-DIAG-H/J1708), this protocol's own
        // resource-table rows carry no default pins at all (`dlc_pins: &[]`,
        // ADR-179 Decision 2's own "connect always issues an explicit
        // SET_CONFIG(CONFIG_J1962_PINS)" text) -- so there is no per-row
        // hardcoded default to fall back to even when `resource_row.is_some()`,
        // unlike those five arms' own `resource_row.is_none()`-gated
        // rejection. `dlc_pin_data` is therefore required unconditionally
        // here, regardless of whether a resource-table row was matched.
        //
        // Until ADR-206, this arm's own outer gate was deliberately widened
        // from an exact-`_PS`-id match (every other standalone protocol's own
        // arm, e.g. GM UART's below) to the broader
        // `resources::is_j1939_protocol_id` predicate, specifically so it
        // could intercept a raw `_CHx` id and reject it with an accurate,
        // unambiguous "not yet supported" message (Codex review, PR #72
        // round 13) -- at the time, `resources::chx_base_protocol_id` did not
        // recognize J1939 at all, so letting a `_CHx` id fall through
        // untouched would have reached `resolve_channel_selection`'s generic
        // `_CHx` handling and been rejected for the wrong stated reason.
        // ADR-206 closes that gap in `resources.rs` (`chx_block_base`/
        // `chx_base_protocol_id` now recognize J1939), so this arm is now
        // structurally identical to GM UART's own arm below -- an exact
        // `_PS`-id match, with a `requested_index.is_some()` bypass for the
        // compound-name route -- and the same reasoning documented on GM
        // UART's arm (its own `requested_index.is_some()` bypass, the PR #98
        // round 3 pins/`_CHx` mutual-exclusion fix) applies here verbatim;
        // see that arm's doc comment rather than re-deriving it.
        if raw_hw_protocol_id == j2534_0404::PROTOCOL_J1939_PS {
            // ADR-206: mirrors GM UART's own `requested_index.is_some()`
            // bypass (below) exactly -- a compound `_CHx`-suffixed name (e.g.
            // "SAE_J1939_73_on_SAE_J1939_21_CH1") resolves its head to this
            // same row/protocol via `resolve_protocol_name_with_chx_suffix`,
            // reaching this arm with `raw_hw_protocol_id ==
            // PROTOCOL_J1939_PS` but `requested_index` set. Clause 7
            // Additional Channels have no J1962 pin concept at all, so a
            // caller combining a `_CHx` suffix with explicit dlc_pin_data
            // must be rejected here, not silently accepted with the pins
            // discarded (same rationale as GM UART's own PR #98 round 3 fix).
            // A directly-named `_CHx` id (e.g. `PROTOCOL_J1939_CH1`) never
            // reaches this arm at all -- it fails the outer `raw_hw_protocol_id
            // == PROTOCOL_J1939_PS` match and falls through to this
            // function's generic tail's `Ok(None)`, then on to
            // `resolve_channel_selection`'s own `already_chx`/
            // `chx_base_protocol_id` route, the same as every other in-scope
            // family's directly-named `_CHx` id already does.
            if requested_index.is_some() {
                if !dlc_pin_data.is_empty() {
                    return Err(Status::invalid_argument(
                        "resource_data requests both SAE J2534-2 clause 6 Pin Selection \
                         (non-empty dlc_pin_data) and a clause 7 Additional Channel (a compound \
                         `_CHx`-suffixed resource name) for a SAE J1939 Protocol (J1939_PS) \
                         resource -- these are mutually exclusive: clause 7 channels live on \
                         vendor connectors, never on J1962 pins",
                    ));
                }
                return Ok(None);
            }
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 16 SAE J1939 Protocol (J1939_PS) \
                     resource, which is a SAE J2534-2 feature -- this module has not opted into \
                     J2534-2 (its pname lacks the \"J2534-2:\" prefix, clause 5)",
                ));
            }
            if dlc_pin_data.is_empty() {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 16 SAE J1939 Protocol (J1939_PS) \
                     resource or hardware protocol id with no resource_data.dlc_pin_data -- \
                     clause 16.3.2.1 keeps the physical layer pin-unassigned until an explicit \
                     SET_CONFIG(CONFIG_J1962_PINS), and (unlike UART_ECHO_BYTE_PS/HONDA_DIAGH_PS/ \
                     J1708_PS) this protocol's own resource-table rows define no default pins \
                     either, so dlc_pin_data must explicitly select pins whether or not a \
                     resource-table row was matched",
                ));
            }
            let pin_select = Self::compute_pin_select(dlc_pin_data)?;
            return Ok(Some((raw_hw_protocol_id, raw_hw_protocol_id, pin_select)));
        }
        // ADR-188/Phase 7 Stage 7a: SAE J2534-2 clause 19 TP2.0's one
        // `_PS`-only hardware id, positioned right after the J1939 arm
        // above -- like UART Echo Byte/Honda DIAG-H/J1708/J1939, it has no
        // CAN-family relationship at all and no unqualified base id to
        // normalize onto (`base_id` here equals `raw_hw_protocol_id`
        // itself), even though it physically rides a CAN transceiver
        // (ADR-188 Decision item 1, the identical ADR-179 J1939 reasoning).
        // Unlike J1939's own no-default-pins arm just above but LIKE the
        // FT-CAN closed-two-pin-pair arm further up, clause 19.2.2
        // documents a closed pin set -- but exactly ONE pair (6/14), not
        // two: any other pin/pin-pair is rejected outright.
        if resources::is_tp2_0_protocol_id(raw_hw_protocol_id) {
            // ADR-210 Decision item 5: a directly-named `_CHx` id (e.g.
            // `PROTOCOL_TP2_0_CH1`) never reaches this arm at all -- it fails
            // the outer `is_tp2_0_protocol_id` exact-match and falls through
            // to this function's generic tail's `Ok(None)`, then on to
            // `resolve_channel_selection`'s own `already_chx`/
            // `chx_base_protocol_id` route, the same as every other in-scope
            // family's directly-named `_CHx` id already does. But a compound
            // `_CHx`-suffixed *name* (e.g. `"...CH1"`) resolves its
            // `raw_hw_protocol_id` to the bare `TP2_0_PS` id before reaching
            // here, so it WOULD wrongly fall through this arm's own
            // dlc_pin_data/default-pin logic below and get rejected by
            // `resolve_channel_selection`'s clause-6/7 mutual-exclusion check
            // without this bypass -- mirrors every other in-scope family's
            // arm above.
            if requested_index.is_some() {
                if !dlc_pin_data.is_empty() {
                    return Err(Status::invalid_argument(
                        "resource_data requests both SAE J2534-2 clause 6 Pin Selection \
                         (non-empty dlc_pin_data) and a clause 7 Additional Channel (a compound \
                         `_CHx`-suffixed resource name) for a SAE J2534-2 clause 19 TP2.0 \
                         Protocol (TP2_0_PS) resource -- these are mutually exclusive: clause 7 \
                         channels live on vendor connectors, never on J1962 pins",
                    ));
                }
                return Ok(None);
            }
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 19 TP2.0 Protocol (TP2_0_PS) \
                     resource, which is a SAE J2534-2 feature -- this module has not opted into \
                     J2534-2 (its pname lacks the \"J2534-2:\" prefix, clause 5)",
                ));
            }
            let pin_select = if dlc_pin_data.is_empty() {
                // Row 0x025F's own default pins (6/14, the only pair clause
                // 19.2.2 documents) are only a real default when an actual
                // resource-table row was matched -- same reasoning as the
                // SW/FT/UART-Echo-Byte/Honda-DIAG-H/J1708 arms above.
                if resource_row.is_none() {
                    return Err(Status::invalid_argument(
                        "resource_data names the raw TP2_0_PS hardware protocol id directly, \
                         bypassing the resource table -- SAE J2534-2 clause 19.2.2 identifies \
                         only one pin pair (6/14), so dlc_pin_data must explicitly select it \
                         when naming this id outside a resource-table row",
                    ));
                }
                // Packed as `0x0000PPSS` (primary pin 6, secondary pin 14
                // (0x0E)) -- the same `0x0000PPSS` convention the FT-CAN/
                // J1708 arms above use for their own pin-pair defaults.
                0x0000_060E
            } else {
                let computed = Self::compute_pin_select(dlc_pin_data)?;
                // Clause 19.2.2 documents exactly ONE valid pin pair, unlike
                // FT-CAN's own two documented pairs -- any other pin or pin
                // pair is rejected here, mirroring the FT-CAN/Honda DIAG-H
                // arms' own closed-set checks above.
                if computed != 0x0000_060E {
                    return Err(Status::invalid_argument(
                        "resource_data.dlc_pin_data does not select the one pin pair SAE \
                         J2534-2 clause 19.2.2 defines for TP2.0 Protocol (TP2_0_PS) \
                         connections -- pins 6/14 -- unlike the general clause 6 Pin Selection \
                         mechanism, TP2_0_PS accepts no other pin or pin pair",
                    ));
                }
                computed
            };
            return Ok(Some((raw_hw_protocol_id, raw_hw_protocol_id, pin_select)));
        }
        // ADR-189/Phase 8: SAE J2534-2 clause 11 GM UART Protocol's one
        // `_PS`-only hardware id, positioned right after the TP2.0 arm above
        // -- like every arm above, it has no CAN-family relationship at all
        // and no unqualified base id to normalize onto (`base_id` here
        // equals `raw_hw_protocol_id` itself). Deliberately narrower than
        // [`resources::is_gm_uart_protocol_id`] (which also recognizes the
        // `_CH1..128` range, needed elsewhere for e.g. the two new IOCTLs'
        // protocol gate): GM UART's `_CHx` Additional Channels ARE in scope
        // (ADR-189 Decision 2, via the arithmetic `_CHx` funnel
        // `resources::chx_base_protocol_id` now covers -- J1939's own arm
        // above was restructured to match this same shape by ADR-206), so a
        // directly-named `_CHx` id must fall through this whole function
        // untouched (exactly like a directly-named `CAN_CHx` id already
        // does) to reach `resolve_channel_selection`'s own `already_chx`
        // route below, not be intercepted and rejected here. Clause 11.2.2
        // documents TWO valid single pins (9 primary, 1 secondary, not a
        // pair) -- the structural shape is otherwise identical to the Honda
        // DIAG-H arm above (`is_empty()`/`len() > 1`/else), only the
        // accepted value set in the `else` branch's closed-set check and the
        // row's own default pin (9, not 14) differ.
        if raw_hw_protocol_id == j2534_0404::PROTOCOL_GM_UART_PS {
            // Codex review, PR #98 round 3: unlike every arm above, GM UART
            // is the first standalone-row protocol this function handles
            // that ALSO has in-scope `_CHx` Additional Channels (ADR-189
            // Decision 2/Decision 4's `_CHx` reuse) -- so a compound
            // `_CHx`-suffixed name (e.g. "GM_UART_CH1") resolves its head to
            // this same row/protocol via `resolve_protocol_name_with_chx_suffix`,
            // reaching this arm with `raw_hw_protocol_id ==
            // PROTOCOL_GM_UART_PS` (the row's own base id) but
            // `requested_index` set. Clause 7 Additional Channels have no
            // J1962 pin concept at all -- exactly like a directly-named
            // `_CHx` id, which never enters this arm in the first place
            // (its `raw_hw_protocol_id` is the `_CHx` id itself, falling
            // through to this function's generic tail's `Ok(None)`
            // instead). Bypassing here mirrors that: `resolve_channel_selection`
            // is the shared choke point that performs the actual `_CHx`
            // expansion (via `requested_index`) AND re-enforces the J2534-2
            // opt-in check unconditionally whenever `requested_index` is
            // `Some` (its own check, run before this bypass could ever
            // reach it) -- but `resolve_channel_selection`'s own clause-6/
            // clause-7 mutual-exclusion check does NOT catch a caller
            // supplying dlc_pin_data here too: that check's
            // `resources::is_ps_protocol_id` only recognizes the original
            // seven CAN-family `_PS` ids, never extended to the
            // standalone-row families (SW/FT/UART Echo Byte/Honda DIAG-H/
            // J1708/J1939/TP2.0/GM UART), and `pin_selection` itself is
            // `None` here precisely because of this bypass -- so a caller
            // combining a `_CHx` suffix with explicit dlc_pin_data must be
            // rejected HERE, not silently accepted with the pins discarded
            // (edge-case-hunter, PR #98 round 3 verification pass, live
            // repro confirmed the pre-fix code accepted pin 9 +
            // "GM_UART_CH1" and silently dropped the pin).
            if requested_index.is_some() {
                if !dlc_pin_data.is_empty() {
                    return Err(Status::invalid_argument(
                        "resource_data requests both SAE J2534-2 clause 6 Pin Selection \
                         (non-empty dlc_pin_data) and a clause 7 Additional Channel (a compound \
                         `_CHx`-suffixed resource name) for a GM UART Protocol (GM_UART_PS) \
                         resource -- these are mutually exclusive: clause 7 channels live on \
                         vendor connectors, never on J1962 pins",
                    ));
                }
                return Ok(None);
            }
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 11 GM UART Protocol (GM_UART_PS) \
                     resource, which is a SAE J2534-2 feature -- this module has not opted into \
                     J2534-2 (its pname lacks the \"J2534-2:\" prefix, clause 5)",
                ));
            }
            let pin_select = if dlc_pin_data.is_empty() {
                // Row 0x0260's own default pin (9, clause 11.2.2's primary
                // pin) is only a real default when an actual resource-table
                // row was matched -- same reasoning as the SW/FT/UART-Echo-
                // Byte/Honda-DIAG-H/J1708 arms above: clause 11.2.2 names no
                // default pin at all, so a caller naming `GM_UART_PS`
                // directly, bypassing the resource table, has made no pin
                // choice.
                if resource_row.is_none() {
                    return Err(Status::invalid_argument(
                        "resource_data names the raw GM_UART_PS hardware protocol id directly, \
                         bypassing the resource table -- SAE J2534-2 clause 11.2.2 identifies no \
                         default pin, so dlc_pin_data must explicitly select pin 1 or pin 9 \
                         (clause 11.2.2's two documented pins) when naming this id outside a \
                         resource-table row",
                    ));
                }
                0x0000_0900
            } else if dlc_pin_data.len() > 1 {
                // Same reasoning as the SW/UART-Echo-Byte/Honda-DIAG-H arms
                // above: this protocol is single-wire, with no secondary pin
                // role at all, so `compute_pin_select`'s general 2-pin
                // packing must not be reached here.
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data supplies more than one pin for a SAE J2534-2 \
                     clause 11 GM UART Protocol (GM_UART_PS) connection -- this protocol is \
                     single-wire and has no secondary pin role to pair a second entry with",
                ));
            } else {
                let computed = Self::compute_pin_select(dlc_pin_data)?;
                // The two-single-pin analog of the Honda DIAG-H arm's own
                // closed-set check above -- clause 11.2.2 documents exactly
                // two valid single pins (9 and 1), not an open choice.
                if computed != 0x0000_0100 && computed != 0x0000_0900 {
                    return Err(Status::invalid_argument(
                        "resource_data.dlc_pin_data does not select one of the two pins SAE \
                         J2534-2 clause 11.2.2 defines for GM UART Protocol (GM_UART_PS) \
                         connections -- pin 1 or pin 9 -- unlike the general clause 6 Pin \
                         Selection mechanism, GM_UART_PS accepts no other pin",
                    ));
                }
                computed
            };
            return Ok(Some((raw_hw_protocol_id, raw_hw_protocol_id, pin_select)));
        }
        // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): the 32
        // native `PROTOCOL_ANALOG_IN_x` ids, positioned right after the
        // J1708 arm above -- like every other J2534-2 protocol arm in this
        // function, gates the SAE J2534-2 opt-in requirement (clause 5) at
        // connect time (ADR-152 Decision 1's "connect-time enforcement"),
        // even though a plain Analog Input connect supplies no
        // `dlc_pin_data` and would otherwise reach the generic tail's
        // unconditional `Ok(None)` below with no opt-in check at all.
        // Unlike every arm above, though, clause 10 has NO pin concept
        // whatsoever -- clause 10's 32 channels are enumerated directly, not
        // selected via clause 6 Pin Selection -- so this always
        // canonicalizes to `Ok(None)` (never a real `pin_select`, since
        // there is nothing to pack) and rejects any `dlc_pin_data` outright
        // rather than trying to interpret it.
        if resources::is_analog_in_protocol_id(raw_hw_protocol_id) {
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 10 Analog Input resource, which \
                     is a SAE J2534-2 feature -- this module has not opted into J2534-2 (its \
                     pname lacks the \"J2534-2:\" prefix, clause 5)",
                ));
            }
            if !dlc_pin_data.is_empty() {
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data is set, but a SAE J2534-2 clause 10 Analog \
                     Input resource has no pin concept at all -- clause 10's 32 channels are \
                     enumerated directly, not selected via clause 6 Pin Selection",
                ));
            }
            return Ok(None);
        }
        // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16, Codex review
        // finding on PR #102): positioned right after the Analog Inputs arm
        // above, for the identical reason its own doc comment states -- a
        // plain Ethernet_NDIS connect supplies no `dlc_pin_data` and would
        // otherwise reach the generic tail's unconditional `Ok(None)` below
        // with no opt-in check at all, silently letting a non-opted-in
        // module's `PassThruConnect(PROTOCOL_ETHERNET_NDIS)` through (the
        // Discovery capability check is explicitly a no-op for a
        // non-opted-in module, so nothing else in the connect path catches
        // this). Unlike every `_PS` arm above but exactly like Analog
        // Inputs, clause 24 has no `_PS`/`J1962_PINS`-style pin-selection
        // concept at all -- pin usage is chosen by connect flag
        // (`CP_NdisPinOption`, ADR-194 Decision), not by `dlc_pin_data` --
        // so this always canonicalizes to `Ok(None)` and rejects any
        // `dlc_pin_data` outright rather than trying to interpret it. No
        // `resources::is_ethernet_ndis_protocol_id` helper exists in this
        // crate (unlike `is_analog_in_protocol_id`'s range check) since this
        // is a single native id, not a range -- matches the direct-id-
        // comparison convention every other Ethernet_NDIS call site in this
        // crate already uses (`rpc_primitive.rs`/`rpc_misc.rs`/
        // `resources.rs`'s own `connect_discovery_check`).
        if raw_hw_protocol_id == j2534_0404::PROTOCOL_ETHERNET_NDIS {
            if !j2534_2_opted_in {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 24 Ethernet_NDIS resource, which \
                     is a SAE J2534-2 feature -- this module has not opted into J2534-2 (its \
                     pname lacks the \"J2534-2:\" prefix, clause 5)",
                ));
            }
            if !dlc_pin_data.is_empty() {
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data is set, but a SAE J2534-2 clause 24 \
                     Ethernet_NDIS resource has no pin concept at all -- pin usage is chosen by \
                     the CP_NdisPinOption ComParam and PassThruConnect's own connect flags, not \
                     selected via clause 6 Pin Selection",
                ));
            }
            return Ok(None);
        }
        let hw_protocol_id = resources::base_protocol_id(raw_hw_protocol_id);
        // Direct, finite check (edge-case-hunter, PR #29, post-approval
        // pass) -- not an inference from whether `base_protocol_id` changed
        // the id, which already needed a compensating `_CHx` exclusion once
        // (ADR-156 Decision 3 addendum/Phase 2b, when the normalization
        // funnel's domain grew to also cover `_CHx`) and would silently
        // miscompute again the next time that funnel's domain grows.
        // `resolve_channel_selection` is the `_CHx` analog of this whole
        // function; a `_CHx` id is recognized and routed there instead
        // (`resources::is_chx_protocol_id`).
        let already_ps = resources::is_ps_protocol_id(raw_hw_protocol_id);

        if dlc_pin_data.is_empty() {
            if already_ps {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 6 Pin Selection (_PS) hardware \
                     protocol id directly, but supplies no resource_data.dlc_pin_data -- a _PS \
                     channel connects with its DLC pins unassigned until \
                     SET_CONFIG(CONFIG_J1962_PINS), which this service only issues from pins \
                     supplied here, so a _PS protocol id requires dlc_pin_data",
                ));
            }
            return Ok(None);
        }

        // A directly-named `_PS` id is using J2534-2 vocabulary (clause
        // 6.3.1 Table 1) regardless of what its pins turn out to be worth
        // -- checked here, before the defaults comparison below can
        // short-circuit to `Ok(None)`, so opt-in stays required even for a
        // `_PS` id whose pins canonicalize to an ordinary base connect
        // (design-advisor, PR #28; pinned by
        // `resolve_pin_selection_rejects_a_raw_ps_protocol_id_when_not_opted_in`,
        // which uses CAN's own default pins).
        if already_ps && !j2534_2_opted_in {
            return Err(Status::invalid_argument(
                "resource_data names a SAE J2534-2 clause 6 Pin Selection (_PS) hardware \
                 protocol id directly, which is a SAE J2534-2 feature -- this module has not \
                 opted into J2534-2 (its pname lacks the \"J2534-2:\" prefix, clause 5)",
            ));
        }

        let default_pins = resource_row
            .map(|row| row.dlc_pins)
            .or_else(|| resources::default_dlc_pins_for_hw_protocol(hw_protocol_id));

        // Correction (design-advisor, PR #28): matches regardless of
        // `already_ps` -- see this function's own doc comment for why a
        // directly-named `_PS` id whose pins equal the base defaults must
        // canonicalize to the base identity rather than always computing a
        // real `pin_select`.
        if let Some(default_pins) = default_pins
            && Self::dlc_pin_data_matches_defaults(dlc_pin_data, default_pins)?
        {
            return Ok(None);
        }

        if !j2534_2_opted_in {
            return Err(Status::invalid_argument(
                "resource_data.dlc_pin_data requests non-default DLC pins, which is a SAE \
                 J2534-2 clause 6 Pin Selection feature -- this module has not opted into \
                 J2534-2 (its pname lacks the \"J2534-2:\" prefix, clause 5)",
            ));
        }

        let Some(ps_protocol_id) = resources::ps_protocol_id(hw_protocol_id) else {
            return Err(Status::invalid_argument(
                "resource_data.dlc_pin_data requests non-default DLC pins, but this protocol \
                 has no SAE J2534-2 clause 6 Pin Selection (_PS) variant in this service's \
                 supported scope (ADR-156 Decision 2: J1850VPW, J1850PWM, ISO9141, ISO14230, \
                 CAN, ISO15765, SAE J2610 only)",
            ));
        };

        let pin_select = Self::compute_pin_select(dlc_pin_data)?;

        // ADR-201: per-bus completeness validation for the general clause 6
        // fallback path, closing the gap design-advisor left open in
        // ADR-168's Seventh correction (which fixed only the FT-CAN arm's
        // own closed two-pair check). `ps_protocol_id` having returned
        // `Some` above guarantees `secondary_pin_requirement` also covers
        // `hw_protocol_id` -- both cover exactly the same ten ids by
        // construction -- so a `None` here is a domain-invariant break (a
        // future protocol added to one match but not the other), not
        // something to silently skip validation for (this file's established
        // anti-silent-canonicalization stance, see the FD-id rejection above
        // at this function's top for the precedent/tone to match).
        let secondary_byte = pin_select & 0xFF;
        match resources::secondary_pin_requirement(hw_protocol_id) {
            Some(resources::SecondaryPinRequirement::Required) if secondary_byte == 0 => {
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data selects only a primary pin, but this protocol's \
                     physical layer requires both a primary and a secondary DLC pin (e.g. \
                     CAN/ISO15765's HI+LOW differential pair, J1850PWM's +/- differential pair, \
                     or SAE J2610 SCI's Tx+Rx pair) -- SAE J2534-2 clause 6.3.3.2 Table 3 allows \
                     a zero secondary byte only when the protocol requires no secondary pin, \
                     which is never the case for this bus",
                ));
            }
            Some(resources::SecondaryPinRequirement::NeverPresent) if secondary_byte != 0 => {
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data supplies a secondary pin for SAE J1850VPW, a \
                     single-wire bus with no secondary signal -- SAE J2534-2 clause 6.3.3.2 \
                     Table 3 requires the secondary byte be zero when no secondary pin exists \
                     for the protocol",
                ));
            }
            Some(_) => {}
            None => {
                return Err(Status::internal(
                    "resource_data.dlc_pin_data resolved a _PS protocol id via ps_protocol_id, \
                     but resources::secondary_pin_requirement has no matching entry for the same \
                     hw_protocol_id -- these two functions must cover exactly the same hardware \
                     ids (a domain-invariant break, not a caller error)",
                ));
            }
        }

        Ok(Some((hw_protocol_id, ps_protocol_id, pin_select)))
    }

    /// SAE J2534-2 clause 7 (Additional Channels) resolution (ADR-156
    /// Decision 3/Phase 2b; the `channel_index` proto field route removed by
    /// ADR-178 and later reinstated, in adapted form, as a compound-name
    /// grammar layered on the pre-existing string-typed name-resolution
    /// routes -- see [`resolve_protocol_name_with_chx_suffix`] for the
    /// grammar itself), layered on top of an already-resolved `(protocol,
    /// resource_row)` pair -- mirrors [`resolve_pin_selection`]'s structure
    /// and error style, and must run after it: `pin_selection` is that
    /// function's own outcome for the same request, needed here to enforce
    /// clause 6/7 mutual exclusion.
    ///
    /// `requested_index` carries the index parsed from a compound
    /// `"<name>_CH<n>"` suffix (see [`resolve_protocol_name_with_chx_suffix`]),
    /// when that grammar's fallback split engaged for this request; `None`
    /// otherwise (including every numeric `ResourceId`/`protocol_id` route,
    /// which never attempts the compound-name grammar at all). Unlike the
    /// pre-ADR-178 `channel_index` proto3 field, there is no default-value
    /// sentinel to lean on -- `None` unambiguously means "no compound
    /// suffix was present," not "index zero."
    ///
    /// - A `resource_row`'s `hw_protocol_override` (or, absent a row, a
    ///   directly-named `protocol`) that is a SAE J2534-2 clause 21 CAN FD
    ///   (`_PS`) id ([`resources::is_fd_protocol_id`]) is rejected
    ///   `invalid_argument` unconditionally, before every other check
    ///   below -- including the "neither a `_CHx` id nor a requested index"
    ///   no-op path, which would otherwise silently accept it (ADR-158
    ///   Corrections item 4). Mirrors [`resolve_pin_selection`]'s own
    ///   identical check, needed here too since this function is the shared
    ///   choke point every resolution route funnels through, including the
    ///   table-row-matched route that never calls [`resolve_pin_selection`]
    ///   at all.
    /// - A directly-named `_CHx` id combined with a nonzero `requested_index`
    ///   (a caller naming a raw `_CHx` id directly AND ALSO supplying a
    ///   compound `_CHx`-suffixed name) is rejected `invalid_argument` --
    ///   double qualification via two routes to the same qualifier is never
    ///   silently accepted, even when the decomposed index would agree. This
    ///   combination is reachable: a compound name's head is resolved
    ///   through the same numeric-string fallback every other route uses,
    ///   so a head that is itself the decimal string of a raw `_CHx` id
    ///   reaches here with both `already_chx` and `requested_index.is_some()`
    ///   true.
    /// - Either route (a `requested_index`, or a directly-named `_CHx` id)
    ///   combined with `pin_selection.is_some()` (a genuine, non-default
    ///   clause 6 Pin Selection request), OR with a directly-named `_PS` id
    ///   whose pins canonicalize to defaults ([`resources::is_ps_protocol_id`]
    ///   on the resolved raw hardware id -- only reachable via the
    ///   `requested_index` route, since `already_chx` and `is_ps_protocol_id`
    ///   are mutually exclusive by construction), is rejected
    ///   `invalid_argument` -- clause 7 channels live on vendor connectors,
    ///   never on J1962 pins, so the two qualifiers are mutually exclusive.
    /// - Either route requires the connecting module's clause-5 J2534-2
    ///   opt-in -- rejected `invalid_argument` before resolution otherwise,
    ///   mirroring [`resolve_pin_selection`]'s own gate.
    /// - A directly-named `_CHx` id decomposes via
    ///   [`resources::chx_base_protocol_id`] to `(base, index)` and resolves
    ///   exactly like the `requested_index` route -- accepted, since (unlike
    ///   a bare `_PS` id) the id is fully self-describing. An id inside the
    ///   full `_CHx` region but outside the seven in-scope families
    ///   (`chx_base_protocol_id` returns `None`) is rejected
    ///   `invalid_argument`, not silently passed through as an unrecognized
    ///   raw id.
    /// - `requested_index` alone (no direct `_CHx` naming) resolves via
    ///   [`resources::chx_protocol_id`]; `None` (an out-of-scope base
    ///   protocol, or an index outside `1..=128`) is rejected
    ///   `invalid_argument`.
    fn resolve_channel_selection(
        protocol: ChannelProtocol,
        resource_row: Option<&'static resources::ResourceDef>,
        requested_index: Option<u32>,
        pin_selection: Option<(u32, u32, u32)>,
        j2534_2_opted_in: bool,
    ) -> Result<Option<(u32, u32, u32)>, Status> {
        let raw_hw_protocol_id = resource_row
            .and_then(|row| row.hw_protocol_override)
            .unwrap_or_else(|| protocol.j2534_protocol_id());
        // ADR-158 Corrections item 4 (`edge-case-hunter` Finding 3): the
        // same rejection `resolve_pin_selection` already carries, added here
        // too -- this function is the shared choke point every resolution
        // route (`RscData::ProtocolId`, both `RscData::ProtocolName`
        // branches, and a bare `ResourceId`/`ResourceName` table-row match)
        // funnels through, including the table-row-matched route that never
        // calls `resolve_pin_selection` at all and so would otherwise reach
        // `resolve_channel_selection` -- and, via the "not a directly-named
        // _CHx id" no-op path just below, connect -- with no FD check
        // whatsoever. Runs unconditionally, before the no-op check,
        // mirroring `resolve_pin_selection`'s own placement rationale:
        // naming the id at all is the error, not something a module's
        // opt-in status could legitimize. Does NOT replace
        // `resolve_pin_selection`'s own check -- that one guards a
        // different hazard (its own `base_protocol_id` call would silently
        // collapse an FD id before this function even runs, on the
        // alias-fallback resolution route specifically), so both checks are
        // needed, each guarding its own function's own hazard.
        if resources::is_fd_protocol_id(raw_hw_protocol_id) {
            return Err(Status::invalid_argument(
                "resource_data names a SAE J2534-2 clause 21 CAN FD or clause 22 \
                 ISO15765-on-CAN-FD (_PS or _CHx) hardware protocol id directly -- FD mode is \
                 derived from staged Working ComParams (CP_CANFDTxMaxDataLength / \
                 CP_CANFDBaudrate) at ConnectComLogicalLink time, not from a directly-named \
                 protocol id; stage those ComParams via SetComParam and connect to the ordinary \
                 CAN or ISO15765 resource (optionally with a SAE J2534-2 clause 7 Additional \
                 Channel index) instead",
            ));
        }
        let already_chx = resources::is_chx_protocol_id(raw_hw_protocol_id);

        if requested_index.is_none() && !already_chx {
            return Ok(None);
        }

        // Checked here, immediately after the no-op check above and before
        // the double-qualification/mutual-exclusion rejections below, to
        // match `resolve_pin_selection`'s own precedent (see the comment
        // there: "checked here, before the defaults comparison below can
        // short-circuit... so opt-in stays required even for a `_PS` id
        // whose pins canonicalize to an ordinary base connect") -- opt-in
        // stays required regardless of which other business-logic rejection
        // a non-opted-in request would otherwise hit first.
        if !j2534_2_opted_in {
            return Err(Status::invalid_argument(
                "resource_data names a SAE J2534-2 clause 7 Additional Channels (_CHx) \
                 resource (via a directly-named _CHx hardware protocol id, or a compound \
                 `_CHx`-suffixed resource name), which is a SAE J2534-2 feature -- this module \
                 has not opted into J2534-2 (its pname lacks the \"J2534-2:\" prefix, clause 5)",
            ));
        }

        // A caller naming a `_CHx` id directly AND ALSO supplying a compound
        // `_CHx`-suffixed name is rejected -- two routes to the same
        // qualifier must not both be supplied, even when they would agree.
        // Reachable: a compound name's head is resolved through the exact
        // same numeric-string fallback every other name-resolution route
        // uses, so a head that happens to be the decimal string of a raw
        // `_CHx` id reaches here with both `already_chx` and
        // `requested_index.is_some()` true (e.g. resource_name
        // `"<decimal _CH5 id>_CH3"`).
        if requested_index.is_some() && already_chx {
            return Err(Status::invalid_argument(
                "resource_data names a directly-named SAE J2534-2 clause 7 Additional Channels \
                 (_CHx) hardware protocol id together with a compound `_CHx`-suffixed resource \
                 name -- these are two routes to the same qualifier and must not both be \
                 supplied, even when they would agree",
            ));
        }

        // Widened (edge-case-hunter, PR #29, post-approval pass) to also
        // trigger on a directly-named `_PS` id whose pins canonicalize to
        // the base protocol's own defaults, not just a genuine (non-`None`)
        // `pin_selection` outcome. `resolve_pin_selection`'s canonicalized
        // `Ok(None)` deliberately erases whether clause-6 vocabulary was
        // used at all (PR #28's `9db653f`, unifying `ChannelKey`/lock scope
        // with an ordinary base connect) -- so `pin_selection.is_some()`
        // alone missed a directly-named `_PS` id (e.g. `PROTOCOL_CAN_PS`
        // with CAN's own default pins) combined with a clause 7 Additional
        // Channel qualifier: the caller named two irreconcilable connect
        // identities (a native `PassThruConnect` takes exactly one
        // `ProtocolID`), and this function would otherwise silently pick the
        // `_CHx`/index reading. `is_ps_protocol_id(raw_hw_protocol_id)` is
        // unconditional here (not scoped to either route specifically): on
        // the `already_chx` path it is always `false` by construction
        // (`is_chx_protocol_id`/`is_ps_protocol_id` are mutually exclusive),
        // so it is a no-op there; on the `requested_index` path it is
        // exactly what recovers the bug ADR-178's revert reintroduced --
        // `requested_index` reaching here means the double-qualification
        // check above did not reject, so `raw_hw_protocol_id` can be an
        // arbitrary resolved base/qualified id, including a genuine `_PS`
        // one (e.g. a compound name whose head is the decimal string of a
        // raw `_PS` id). Deliberately NOT widened to "any `dlc_pin_data` was
        // supplied": default-matching pins on a *base* id or a
        // canonical-name row match remain a legitimate combination with a
        // clause 7 Additional Channel qualifier (ISO 22900-2's generic
        // typed-pin resource field, and — for a canonical name — pre-existing
        // row disambiguation, e.g. the SCI configuration rows).
        if pin_selection.is_some() || resources::is_ps_protocol_id(raw_hw_protocol_id) {
            return Err(Status::invalid_argument(
                "resource_data requests both SAE J2534-2 clause 6 Pin Selection (non-default \
                 dlc_pin_data, or a directly-named _PS hardware protocol id) and a clause 7 \
                 Additional Channel (a directly-named _CHx hardware protocol id, or a compound \
                 `_CHx`-suffixed resource name) -- these are mutually exclusive: clause 7 \
                 channels live on vendor connectors, never on J1962/J1939/J1708 pins",
            ));
        }

        if already_chx {
            let Some((base_hw_protocol_id, index)) =
                resources::chx_base_protocol_id(raw_hw_protocol_id)
            else {
                return Err(Status::invalid_argument(
                    "resource_data names a SAE J2534-2 clause 7 Additional Channels (_CHx) \
                     hardware protocol id from a protocol family this service does not \
                     implement Additional Channels for (ADR-156 Decision 3: J1850VPW, \
                     J1850PWM, ISO9141, ISO14230, CAN, ISO15765, SAE J2610 only)",
                ));
            };
            // ADR-211: `chx_base_protocol_id` returns the RAW extracted base
            // (e.g. `PROTOCOL_FT_CAN_PS` for a directly-named
            // `PROTOCOL_FT_CAN_CH1`), keyed on a family's own qualifying
            // `_PS` id the same way `chx_protocol_id`/`chx_block_base` are
            // (see the compound-`_CHx`-name route below). It is not yet the
            // fully-normalized CAN/ISO15765 base a CAN-collapse family like
            // Fault-Tolerant CAN normalizes onto -- composing through
            // `resources::base_protocol_id` here, mirroring both the
            // `resolve_pin_selection` (`_PS`-direct-naming) route and the
            // compound-`_CHx`-name route below, is required so a directly-
            // named `_CHx` id (a bare numeric `ResourceId`/`ResourceName`,
            // or `RscData::ProtocolId(PROTOCOL_FT_CAN_CH1)`) ends up with the
            // same `CAN`/`ISO15765` base as the equivalent compound-name
            // connect, instead of the intermediate `_PS` value. This is a
            // strict no-op for every one of the 13 already-shipped families
            // (their own `chx_base_protocol_id` output is already the true
            // base, so `base_protocol_id` of it is itself).
            return Ok(Some((
                resources::base_protocol_id(base_hw_protocol_id),
                raw_hw_protocol_id,
                index,
            )));
        }

        // The no-op check above guarantees `requested_index.is_some()` here
        // (it is the only other way past it once `already_chx` is `false`).
        let requested_index = requested_index
            .expect("no-op check above returns Ok(None) unless requested_index.is_some()");
        let base_hw_protocol_id = resources::base_protocol_id(raw_hw_protocol_id);
        // ADR-211 Decision item 3: compute the `_CHx` id from the RAW row id
        // (`raw_hw_protocol_id`), not the `base_protocol_id`-normalized
        // value (`base_hw_protocol_id`, still needed just above/below as the
        // returned triple's own first element). `chx_block_base`/
        // `chx_protocol_id` are keyed on a family's own qualifying `_PS` id
        // when it has one (matching every closed family's own convention),
        // not on the true CAN/ISO15765 base a CAN-collapse family like
        // Fault-Tolerant CAN normalizes onto -- composing through
        // `base_protocol_id` first, as this used to, would silently compute
        // a plain `CAN_CHx`/`ISO15765_CHx` id instead of an `FT_CAN_CHx`/
        // `FT_ISO15765_CHx` one for a compound `_CHx`-suffixed name on an FT
        // resource row. This is a no-op for every already-shipped family
        // (their raw row id and `base_protocol_id`-normalized value already
        // coincide).
        let Some(chx_id) = resources::chx_protocol_id(raw_hw_protocol_id, requested_index) else {
            return Err(Status::invalid_argument(
                "the compound `_CHx`-suffixed resource name's index is outside the valid \
                 1..=128 range, or this protocol has no SAE J2534-2 clause 7 Additional \
                 Channels (_CHx) support in this service's supported scope (ADR-156 Decision 3: \
                 J1850VPW, J1850PWM, ISO9141, ISO14230, CAN, ISO15765, SAE J2610 only)",
            ));
        };
        Ok(Some((base_hw_protocol_id, chx_id, requested_index)))
    }

    /// Exact structural match between a caller's `dlc_pin_data` and a
    /// protocol's default typed `dlc_pins`, per ADR-156 Decision 2's
    /// "differs from the matched row's default `dlc_pins`" test -- checking
    /// the full typed assignment, not just which pin *numbers* appear (a
    /// number-only-set comparison would silently accept a swapped-polarity
    /// request, e.g. CAN's default pins 6/14 supplied with 6 typed `LOW` and
    /// 14 typed `HI`, as "default", and would also silently accept a
    /// malformed `dlc_pin_data` with duplicate/extra entries whose
    /// deduplicated numbers happen to equal the default set).
    ///
    /// Matches iff `dlc_pin_data` has exactly the same pin *count* as
    /// `default_pins` (so no duplicate or extra entries slip through), every
    /// requested pin number is one of the default pins' numbers, and --
    /// where the caller specified a type at all -- that type equals the
    /// specific default pin's own type for that number. A requested pin with
    /// no `dlc_pin_type` (a bare `dlc_pin_number`) matches any default type
    /// for that number, preserving clause 6.3.3.2's auto-connect-to-
    /// default-pins behavior for a caller that doesn't bother typing its
    /// pins. Any other case (wrong count, a pin number absent from the
    /// default set, or an explicitly-typed pin whose type doesn't match) is
    /// "does NOT match default", so resolution falls through to the
    /// non-default path -- which already correctly rejects duplicate/
    /// malformed input via `compute_pin_select`'s own validation.
    ///
    /// Returns `Err` only for an unrecognized pin-type name (via
    /// `resolve_pin_type_id`).
    fn dlc_pin_data_matches_defaults(
        dlc_pin_data: &[vci_service_interface::PinData],
        default_pins: &[(u32, u32)],
    ) -> Result<bool, Status> {
        if dlc_pin_data.len() != default_pins.len() {
            return Ok(false);
        }

        let mut seen_numbers = std::collections::BTreeSet::new();
        for pin in dlc_pin_data {
            if !seen_numbers.insert(pin.dlc_pin_number) {
                // A duplicate pin number can't be part of a 1:1 match against
                // a default set whose pin numbers are themselves unique --
                // this also means a caller can never disguise a malformed,
                // duplicate-laden dlc_pin_data as "default" just because the
                // deduplicated numbers happen to coincide.
                return Ok(false);
            }

            let Some(&(_, default_type)) = default_pins
                .iter()
                .find(|&&(num, _)| num == pin.dlc_pin_number)
            else {
                return Ok(false);
            };

            let requested_type = Self::resolve_pin_type_id(pin)?;
            if requested_type.is_some_and(|ty| ty != default_type) {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// SAE J2534-2 clause 6.3.3.2 Table 3's `J1962_PINS` pin numbers excluded
    /// from `CONFIG_J1962_PINS` regardless of protocol or PP/SS position.
    /// Note pin 16 sits at the top of `J1962_PIN_NUMBER_MAX` yet is still
    /// excluded -- being inside the numeric range does not by itself make a
    /// pin number legal.
    const J1962_EXCLUDED_PINS: [u32; 3] = [4, 5, 16];
    /// SAE J2534-2 clause 6.3.3.2 Table 3's valid numeric range for a single
    /// `J1962_PINS` PP/SS byte (`0x00`-`0x10`).
    const J1962_PIN_NUMBER_MAX: u32 = 0x10;

    /// Validates a single caller-supplied pin number against SAE J2534-2
    /// clause 6.3.3.2 Table 3's universal `CONFIG_J1962_PINS` syntactic
    /// constraints (numeric range, excluded pins) -- constraints that apply
    /// regardless of protocol. This is deliberately NOT where per-protocol
    /// pin *legality* is checked (e.g. whether pin 7 makes physical sense
    /// for a CAN channel): J2534-2 leaves that to native J2534-1 hardware
    /// validation, surfaced as `ERR_PIN_INVALID` at the real `SET_CONFIG`
    /// call, not a static table here -- its omission here is intentional,
    /// not a missed requirement.
    fn validate_j1962_pin_number(pin_number: u32) -> Result<(), Status> {
        if pin_number > Self::J1962_PIN_NUMBER_MAX {
            return Err(Status::invalid_argument(format!(
                "resource_data.dlc_pin_data pin number {pin_number} is outside SAE J2534-2 \
                 clause 6.3.3.2 Table 3's valid CONFIG_J1962_PINS range (0x00-0x10)",
            )));
        }
        if Self::J1962_EXCLUDED_PINS.contains(&pin_number) {
            return Err(Status::invalid_argument(format!(
                "resource_data.dlc_pin_data pin number {pin_number} is one of the J1962 pins \
                 SAE J2534-2 clause 6.3.3.2 Table 3 excludes from CONFIG_J1962_PINS regardless \
                 of protocol (pins 4, 5, and 16)",
            )));
        }
        Ok(())
    }

    /// Packs up to 2 typed `PinData` entries into ADR-156 Decision 2's
    /// `0x0000PPSS` `pin_select` bitmask -- `PP` the primary (HI/K/TX/PLUS)
    /// pin number, `SS` the secondary (LOW/L/RX/MINUS) pin number, `SS = 0`
    /// for a single supplied pin (a single-wire protocol, e.g. J1850VPW). A
    /// single supplied pin still has its type resolved and checked
    /// (`resolve_pin_type_id`): an unrecognized type name is rejected rather
    /// than silently ignored, and a pin explicitly typed anything other than
    /// primary (HI/K/TX/PLUS) -- secondary (LOW/L/RX/MINUS), a recognized
    /// non-communication role (IGN/SINGLE/PROGV), or an arbitrary numeric
    /// type id -- is rejected outright rather than packed as if it were
    /// primary (an allow-list, not a block-list, per a Codex-found follow-up
    /// gap: there is no secondary byte for a single-wire selection to pair
    /// any of these with, so packing one into `PP` anyway would misrepresent
    /// the caller's explicit request). An untyped pin, or one explicitly
    /// typed primary, packs unchanged.
    ///
    /// Each entry needs a concrete `dlc_pin_number > 0` (a type-only
    /// wildcard, `dlc_pin_number == 0`, cannot be packed into a concrete
    /// bitmask) within SAE J2534-2 clause 6.3.3.2 Table 3's valid
    /// range/exclusion set (`validate_j1962_pin_number`), and, when two pins
    /// are supplied, a type that resolves to exactly one of
    /// `PIN_TYPE_PRIMARY`/`PIN_TYPE_SECONDARY` -- otherwise there is no way
    /// to know which byte a given pin belongs in -- plus Table 3's `PP !=
    /// SS` constraint (the `0x0000` exception it names is the "no selection
    /// made yet" sentinel this function never produces, since a wildcard
    /// pin number is already rejected above). Rejects with
    /// `invalid_argument` for any of: more than 2 pins, a wildcard pin
    /// number, a pin number outside Table 3's range/exclusion set, an
    /// unrecognized pin-type name (single or two-pin case alike), a single
    /// pin explicitly typed anything other than primary, an untyped/
    /// unrecognized-type pin when 2 are supplied, two pins landing in the
    /// same primary/secondary slot, or identical primary/secondary pin
    /// numbers.
    fn compute_pin_select(dlc_pin_data: &[vci_service_interface::PinData]) -> Result<u32, Status> {
        if dlc_pin_data.len() > 2 {
            return Err(Status::invalid_argument(
                "resource_data.dlc_pin_data supplies more than 2 pins; SAE J2534-2 clause 6 Pin \
                 Selection supports at most one primary and one secondary DLC pin",
            ));
        }
        for pin in dlc_pin_data {
            if pin.dlc_pin_number == 0 {
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data's pin selection requires a concrete \
                     dlc_pin_number on every entry, not a type-only wildcard (dlc_pin_number == 0)",
                ));
            }
            Self::validate_j1962_pin_number(pin.dlc_pin_number)?;
        }

        if let [pin] = dlc_pin_data {
            if let Some(ty) = Self::resolve_pin_type_id(pin)?
                && !Self::PIN_TYPE_PRIMARY.contains(&ty)
            {
                return Err(Status::invalid_argument(
                    "resource_data.dlc_pin_data supplies a single pin explicitly typed as \
                     something other than a primary (HI/K/TX/PLUS) role; a single-wire SAE \
                     J2534-2 clause 6 pin selection has no secondary byte to pair a \
                     secondary/non-communication role (LOW/L/RX/MINUS, IGN, SINGLE, PROGV, or \
                     an unrecognized numeric type) with, so it cannot be packed as if it were \
                     the primary pin",
                ));
            }
            return Ok((pin.dlc_pin_number & 0xFF) << 8);
        }

        let mut primary = None;
        let mut secondary = None;
        for pin in dlc_pin_data {
            let pin_type_id = Self::resolve_pin_type_id(pin)?;
            match pin_type_id {
                Some(ty) if Self::PIN_TYPE_PRIMARY.contains(&ty) => {
                    if primary.replace(pin.dlc_pin_number).is_some() {
                        return Err(Status::invalid_argument(
                            "resource_data.dlc_pin_data supplies two primary (HI/K/TX/PLUS) pins",
                        ));
                    }
                }
                Some(ty) if Self::PIN_TYPE_SECONDARY.contains(&ty) => {
                    if secondary.replace(pin.dlc_pin_number).is_some() {
                        return Err(Status::invalid_argument(
                            "resource_data.dlc_pin_data supplies two secondary (LOW/L/RX/MINUS) \
                             pins",
                        ));
                    }
                }
                _ => {
                    return Err(Status::invalid_argument(
                        "resource_data.dlc_pin_data's pin selection needs each pin typed as \
                         primary (hi/k/tx/plus) or secondary (low/l/rx/minus) to disambiguate \
                         which is which -- an untyped or unrecognized pin type cannot be packed \
                         into the SAE J2534-2 clause 6 pin_select bitmask",
                    ));
                }
            }
        }
        let (Some(pp), Some(ss)) = (primary, secondary) else {
            return Err(Status::invalid_argument(
                "resource_data.dlc_pin_data's two pins must include exactly one primary \
                 (hi/k/tx/plus) and one secondary (low/l/rx/minus) pin",
            ));
        };
        if pp == ss {
            return Err(Status::invalid_argument(format!(
                "resource_data.dlc_pin_data's primary and secondary pins must differ per SAE \
                 J2534-2 clause 6.3.3.2 Table 3 (both resolved to pin {pp})",
            )));
        }
        Ok(((pp & 0xFF) << 8) | (ss & 0xFF))
    }

    /// Resolves the `Resource`/`RscData` protocol field of a
    /// `CreateComLogicalLinkRequest` to a `ChannelProtocol`, returning the
    /// matched `resources` table row alongside it (when one matched) so the
    /// caller can derive ComParam defaults from the row's canonical
    /// `bus_type_name`/`protocol_name` (see `rpc_link::rpc_create_com_logical_link`).
    /// `None` for the row means the legacy path was used (a raw/extended
    /// `ChannelProtocol` value or name not present in the table) -- callers
    /// must fall back to their pre-existing behavior in that case.
    ///
    /// The third element is ADR-156 Decision 2's Pin Selection outcome --
    /// `Some((base_hw_protocol_id, ps_protocol_id, pin_select))` when
    /// `dlc_pin_data` requested non-default pins and resolved to a `_PS`
    /// hardware variant, `None` otherwise (see `resolve_pin_selection`).
    /// `j2534_2_opted_in` (Decision
    /// 4/clause 5, from the connecting module's `pname`) gates whether a
    /// non-default pin request is honored at all -- passed in rather than
    /// looked up here since this function has no `&self` to read
    /// `self.modules` from.
    ///
    /// Pin Selection is layered on top of the `ProtocolId` route (where
    /// `dlc_pin_data` was previously never even inspected) and the
    /// `ProtocolName` route ONLY when `name` does not match any `resources`
    /// table row by name at all (the `map_protocol_name` legacy-alias
    /// fallback, e.g. `"can"`/`"iso15765"`) -- a name that DOES match a
    /// table row (e.g. the canonical `"ISO_15765_2"`) keeps
    /// `find_table_row_by_name`'s pre-existing all-or-nothing pin-narrowing
    /// contract completely unchanged (ADR-106/ADR-069's disambiguation
    /// mechanism, pinned by
    /// `create_com_logical_link_rejects_pin_narrowing_a_unique_name_to_zero_rows`).
    /// Requesting a canonical ISO 22900-2 resource name IS requesting that
    /// name's own fixed pin wiring; a caller wanting Pin Selection's
    /// caller-chosen pins uses `protocol_id`/a generic alias name instead.
    ///
    /// The bare `ResourceId`/`ResourceName` variants (no `RscData`, so no
    /// `dlc_pin_data` field exists at all on either) also call
    /// `resolve_pin_selection` with an empty pin slice (found by Codex
    /// review, PR #28, a later round) -- not to attempt genuine Pin
    /// Selection, which is structurally impossible without a pins field, but
    /// to reuse that function's raw-`_PS`-id rejection: `id`/a numeric
    /// `name` can still resolve to a raw `_PS` hardware protocol id via the
    /// same `ChannelProtocol::from_raw` fallback the `ProtocolId` route
    /// hits, and a route with no way to ever supply pins must reject that
    /// case rather than silently connect an unpinned `_PS` channel. Every
    /// non-`_PS` id -- the overwhelming common case, and every id these two
    /// variants supported before this fix -- still resolves to `Ok(None)`
    /// unchanged.
    pub(super) fn parse_protocol_id_from_resource(
        resource: vci_service_interface::create_com_logical_link_request::Resource,
        j2534_2_opted_in: bool,
    ) -> ParsedProtocolResourceResult {
        let (protocol, row, pin_selection, channel_selection) = match resource {
            vci_service_interface::create_com_logical_link_request::Resource::ResourceId(id) => {
                let (protocol, row) = Self::resolve_protocol_id(id);
                // Neither this variant nor `ResourceName` below carries
                // `dlc_pin_data` at all, so genuine Pin Selection resolution
                // is never possible here -- but `id` can still numerically
                // equal a raw `_PS`/`_CHx` hardware protocol id (found by
                // Codex review, PR #28, a later round, for `_PS`; the same
                // reasoning extends to `_CHx`, ADR-156 Decision 3/Phase 2b:
                // the same `ChannelProtocol::from_raw` pass-through
                // `resolve_pin_selection`/`resolve_channel_selection`'s
                // ProtocolId-route handling addresses does not stop at that
                // route -- `ResourceId` hits the identical
                // `resolve_protocol_id` fallback). Calling both resolvers
                // with an empty pin slice reuses their raw-id rejections (a
                // `_PS` id this route has no way to supply pins for; a
                // `_CHx` id decomposes and resolves normally, since it needs
                // no pins at all) rather than silently connecting an unpinned
                // `_PS` channel or falling through as an unrecognized raw
                // id; every unqualified id (the overwhelming common case)
                // still resolves to `Ok(None)`/`Ok(None)` unchanged.
                let pin_selection =
                    Self::resolve_pin_selection(protocol, row, &[], j2534_2_opted_in, None)?;
                let channel_selection = Self::resolve_channel_selection(
                    protocol,
                    row,
                    None,
                    pin_selection,
                    j2534_2_opted_in,
                )?;
                (protocol, row, pin_selection, channel_selection)
            }
            vci_service_interface::create_com_logical_link_request::Resource::ResourceName(
                name,
            ) => {
                // ResourceName is a bare string -- it cannot carry
                // dlc_pin_data, so no pin narrowing/Pin Selection resolution
                // via that field is possible here (only RscData below
                // carries it at all). Same reasoning and calls as
                // `ResourceId` above: a numeric `name` can still resolve
                // (via `resolve_protocol_name`'s numeric-string fallback) to
                // a raw `_PS`/`_CHx` id. `resolve_protocol_name_with_chx_suffix`
                // additionally attempts the SAE J2534-2 clause 7 compound-name
                // grammar (`"<name>_CH<n>"`) as a fallback when the whole
                // string doesn't resolve directly -- see that function's own
                // doc comment.
                let (protocol, row, requested_index, _resolved_name) =
                    Self::resolve_protocol_name_with_chx_suffix(&name, &[])?.ok_or_else(|| {
                        Status::invalid_argument(
                            "resource_name must be a valid protocol name or numeric id",
                        )
                    })?;
                let pin_selection = Self::resolve_pin_selection(
                    protocol,
                    row,
                    &[],
                    j2534_2_opted_in,
                    requested_index,
                )?;
                let channel_selection = Self::resolve_channel_selection(
                    protocol,
                    row,
                    requested_index,
                    pin_selection,
                    j2534_2_opted_in,
                )?;
                (protocol, row, pin_selection, channel_selection)
            }
            vci_service_interface::create_com_logical_link_request::Resource::RscData(data) => {
                match data.protocol {
                    Some(vci_service_interface::resource_data::Protocol::ProtocolId(id)) => {
                        let (protocol, row) = Self::resolve_protocol_id(id);
                        let pin_selection = Self::resolve_pin_selection(
                            protocol,
                            row,
                            &data.dlc_pin_data,
                            j2534_2_opted_in,
                            None,
                        )?;
                        // A numeric protocol_id route never attempts the
                        // compound-name grammar (there is no name to split
                        // here at all) -- requested_index is always `None`;
                        // this route can still only reach the directly-named
                        // `_CHx` id path inside `resolve_channel_selection`.
                        let channel_selection = Self::resolve_channel_selection(
                            protocol,
                            row,
                            None,
                            pin_selection,
                            j2534_2_opted_in,
                        )?;
                        (protocol, row, pin_selection, channel_selection)
                    }
                    Some(vci_service_interface::resource_data::Protocol::ProtocolName(name)) => {
                        // `resolve_protocol_name_with_chx_suffix` resolves
                        // `name` (attempting the SAE J2534-2 clause 7
                        // compound-name grammar as a fallback -- see its own
                        // doc comment) and reports which name was actually
                        // matched (`resolved_name`: `name` itself, or the
                        // compound split's head when that fallback engaged).
                        // `table_row_matched` -- and the SW/FT/UART Echo
                        // Byte/Honda DIAG-H/J1708 pin-selection-scoping split
                        // below -- must be computed from `resolved_name`, not
                        // the original possibly-compound `name`: a compound
                        // name never matches any table row itself, only its
                        // head can.
                        let (protocol, row, requested_index, resolved_name) =
                            Self::resolve_protocol_name_with_chx_suffix(&name, &data.dlc_pin_data)?
                                .ok_or_else(|| {
                                    Status::invalid_argument(
                                        "resource protocol_name must be a valid protocol name or numeric id",
                                    )
                                })?;
                        let table_row_matched =
                            !Self::find_table_rows_by_name(resolved_name).is_empty();
                        // ADR-156 Corrections (Decision 2 scoping): a
                        // `protocol_name` that matches a `resources` table
                        // row by canonical ISO 22900-2 name keeps
                        // `find_table_row_by_name`'s pre-existing
                        // all-or-nothing pin-narrowing contract completely
                        // unchanged -- `dlc_pin_data` there is identifying a
                        // specific, fixed resource configuration (it is an
                        // input to `find_table_rows_by_name`'s own row
                        // match), not requesting a dynamic clause 6 pin
                        // choice, so Pin Selection resolution does not apply
                        // on this route.
                        //
                        // `resolve_channel_selection` is still called on this
                        // route (below) even though a canonical-name match
                        // itself never resolves to a directly-named `_CHx`
                        // id in this table: it is the same shared choke
                        // point every resolution route funnels through, and
                        // `requested_index` (from the compound-name grammar
                        // above) needs it regardless of whether `name` also
                        // happened to match a table row -- e.g.
                        // `"ISO_15765_2_CH5"` must still resolve Additional
                        // Channels normally, including the
                        // opt-in/mutual-exclusion/index-range checks
                        // `resolve_channel_selection` performs.
                        //
                        // ADR-164/Phase 4 correction: an SW resource row is
                        // the FIRST case where a matched row's own
                        // `hw_protocol_override` is itself a genuine `_PS`
                        // hardware id needing an explicit connect-time
                        // SET_CONFIG(CONFIG_J1962_PINS) (clause 9.2.1) --
                        // every other `hw_protocol_override` in the table
                        // (the four `SAE_J2610_on_SAE_J2610_SCI` rows) is a
                        // native, already-self-pinned hardware id that needs
                        // no such call. The `table_row_matched` bypass above
                        // is safe for THAT case (a canonical name is
                        // requesting a specific, already-fully-pinned
                        // configuration, no Pin Selection resolution
                        // needed) but would silently skip
                        // `resolve_pin_selection` -- and therefore
                        // `link.pin_select`, and therefore the connect-time
                        // SET_CONFIG -- for an SW row selected by its
                        // (possibly shared-with-its-dual-wire-sibling)
                        // canonical name. `matched_row_needs_pin_selection`
                        // routes an SW, FT, or (ADR-170/Phase 9,
                        // edge-case-hunter finding) UART Echo Byte match
                        // through the same `resolve_pin_selection` path a
                        // `protocol_id`/`resource_id` selection already uses,
                        // so every selection route reaches the new SW/FT/UART
                        // Echo Byte arm there (ADR-164 Decision 1's "connect
                        // always emits an explicit SET_CONFIG" is enforced
                        // exactly once, in `resolve_pin_selection`, not
                        // duplicated here -- ADR-168/Phase 6 extends this same
                        // reasoning to FT, and ADR-170/Phase 9 to UART Echo
                        // Byte: row 0x023A's `hw_protocol_override` is `None`,
                        // so the `unwrap_or_else` fallback below yields
                        // `protocol.j2534_protocol_id()` ==
                        // `PROTOCOL_UART_ECHO_BYTE_PS` directly, matching
                        // neither the SW nor FT predicate despite needing the
                        // identical mandatory-pin-selection treatment --
                        // renamed from `matched_row_is_sw_or_ft` since a
                        // "sw_or_ft"-scoped name is no longer accurate once a
                        // third, non-CAN-family protocol is included).
                        // ADR-174/Phase 10 extends this same reasoning to
                        // Honda DIAG-H: row 0x023B's `hw_protocol_override`
                        // is likewise `None`, so it needs its own predicate
                        // in this check too.
                        let matched_row_needs_pin_selection =
                            row.is_some_and(Self::row_needs_dynamic_pin_selection);
                        // Ethernet_NDIS (ADR-194/Phase 16, edge-case-hunter
                        // finding on PR #102 round 2): excluded from
                        // `row_needs_dynamic_pin_selection` above -- clause 24
                        // genuinely has no pin concept, so there is no
                        // SET_CONFIG binding to route dynamically -- but
                        // `resolve_pin_selection` is ALSO this crate's only
                        // opt-in enforcement + "reject any dlc_pin_data"
                        // checkpoint for this protocol (its own dedicated
                        // arm), and `resolve_channel_selection`'s own opt-in
                        // check is deliberately scoped to `_CHx` only (its own
                        // doc comment). Without this exception, a table-row
                        // match reached via `RscData::ProtocolName`/a bare
                        // `ResourceName` (as opposed to a direct
                        // `ProtocolId`/`ResourceId` selector, which instead
                        // reaches `resolve_pin_selection` through a different
                        // route entirely) would skip `resolve_pin_selection`
                        // via the branch below, reaching
                        // `resolve_channel_selection`'s no-op early return
                        // with no opt-in check at all -- the exact bypass
                        // this phase's opt-in fix (`4446a15`) was meant to
                        // close, left open on this one route.
                        let matched_row_is_ethernet_ndis =
                            row.is_some_and(|r| r.protocol == ChannelProtocol::ETHERNET_NDIS);
                        // Analog Inputs (ADR-177/Phase 15, sibling gap to the
                        // Ethernet_NDIS exception above -- edge-case-hunter
                        // finding, PR #102 round-2 close-out, `IMPLEMENTATION_
                        // NOTE.md`'s Prioritized Backlog): also excluded from
                        // `row_needs_dynamic_pin_selection` above (clause 10
                        // has no pin concept either), so a table-row match
                        // reached via `RscData::ProtocolName`/a bare
                        // `ResourceName` skipped `resolve_pin_selection`
                        // entirely via the branch below, reaching
                        // `resolve_channel_selection`'s no-op early return
                        // with no opt-in check at all -- the identical bypass
                        // the Ethernet_NDIS exception above closes, left open
                        // for this sibling protocol. `resolve_pin_selection`'s
                        // own dedicated Analog Inputs arm (`is_analog_in_
                        // protocol_id`) already correctly gates the direct
                        // `ProtocolId` route; only this table-row-matched
                        // route needed the same treatment.
                        let matched_row_is_analog_in =
                            row.is_some_and(|r| r.protocol == ChannelProtocol::ANALOG_IN);
                        let (pin_selection, channel_selection) = if table_row_matched
                            && !matched_row_needs_pin_selection
                            && !matched_row_is_ethernet_ndis
                            && !matched_row_is_analog_in
                        {
                            let channel_selection = Self::resolve_channel_selection(
                                protocol,
                                row,
                                requested_index,
                                None,
                                j2534_2_opted_in,
                            )?;
                            (None, channel_selection)
                        } else {
                            let pin_selection = Self::resolve_pin_selection(
                                protocol,
                                row,
                                &data.dlc_pin_data,
                                j2534_2_opted_in,
                                requested_index,
                            )?;
                            let channel_selection = Self::resolve_channel_selection(
                                protocol,
                                row,
                                requested_index,
                                pin_selection,
                                j2534_2_opted_in,
                            )?;
                            (pin_selection, channel_selection)
                        };
                        (protocol, row, pin_selection, channel_selection)
                    }
                    None => {
                        return Err(Status::invalid_argument(
                            "resource data must include protocol_id or protocol_name",
                        ));
                    }
                }
            }
        };

        // ADR-157 Correction (design-advisor, PR #28, after a 3rd/4th
        // independent instance of the same normalization gap surfaced):
        // every route above that resolves via `resolve_protocol_id`'s
        // `ChannelProtocol::from_raw` fallback can produce a `protocol` that
        // itself wraps a raw `_PS`/`_CHx` hardware id when the caller named
        // one directly (no `resources` table row exists for either). This
        // violates the invariant every pre-existing Plane B site (and this
        // ADR's own original ~30-site sweep) relies on: `LogicalLinkState`'s
        // `protocol` field is always a base/extended *service-level*
        // identity, never a raw hardware `_PS`/`_CHx` value -- only
        // `hw_protocol_id`/`pin_select`/`channel_index`/
        // `base_hw_protocol_override` carry qualifier-specific information,
        // exactly as the table-resolved Pin Selection/Additional Channels
        // paths already behave. Restoring that invariant here, once, closes
        // every current and future consumer of `protocol` in one place,
        // rather than requiring an open-ended sweep of every
        // `link.protocol`/`ChannelProtocol::is_*_family()` call site (three
        // such sites were already found broken this same round:
        // `rpc_connect_com_logical_link`'s `base_proto_id`,
        // `comparam_support::unique_id_params`, and more `is_*_family()`/
        // `tx_message_size_range` consumers design-advisor traced). This is
        // a no-op (`base_protocol_id` is identity) for every unqualified
        // value, including every extended 0x0100+ id -- i.e. every path
        // this function supported before the raw-`_PS`/`_CHx`-direct-naming
        // features existed. `resolve_pin_selection`'s own `already_ps`
        // detection (which this normalization must NOT run before) already
        // saw the raw, un-normalized value above -- normalizing only here,
        // after that decision is made, preserves it. `resources::base_protocol_id`
        // covers `_CHx` too (ADR-156 Decision 3 addendum/Phase 2b), so a
        // directly-named `_CHx` id's `protocol` normalizes here for free,
        // the same way a directly-named `_PS` id's already did.
        let normalized_id = resources::base_protocol_id(protocol.value());
        let protocol = if normalized_id == protocol.value() {
            protocol
        } else {
            ChannelProtocol::from_raw(normalized_id)
        };

        Ok((protocol, row, pin_selection, channel_selection))
    }

    pub(super) fn map_protocol_name(name: &str) -> Option<ChannelProtocol> {
        match name.to_ascii_lowercase().as_str() {
            // CAN & ISO11898 — native CAN channel (0x05)
            "can" | "iso11898" | "iso-11898" | "iso_11898" | "iso11898-1" | "iso-11898-1"
            | "iso_11898_1" | "can_iso" | "can-iso" | "iso_11898_raw" => Some(ChannelProtocol::CAN),
            // ISO 11783-12 on ISO 11783-5 — CAN channel, but distinct service-level protocol
            "iso_11783_12_on_iso_11783_5" => Some(ChannelProtocol::ISO_11783_12_ON_ISO_11783_5),
            // ISO15765 — native ISO-TP channel (0x06)
            "iso15765" | "iso-15765" | "iso_15765" | "iso15765-2" | "iso-15765-2"
            | "iso_15765_2" | "iso15765-4" | "iso-15765-4" | "iso_15765_4" | "iso-tp" | "isotp"
            | "iso_tp" => Some(ChannelProtocol::ISO15765),
            // ISO 15765-2 channel variants — distinct service-level protocols
            "iso_15765_3_on_iso_15765_2" => Some(ChannelProtocol::ISO_15765_3_ON_ISO_15765_2),
            "iso_14229_3_on_iso_15765_2" => Some(ChannelProtocol::ISO_14229_3_ON_ISO_15765_2),
            "iso_14230_3_on_iso_15765_2" => Some(ChannelProtocol::ISO_14230_3_ON_ISO_15765_2),
            "sae_j2190_on_iso_15765_2" => Some(ChannelProtocol::SAE_J2190_ON_ISO_15765_2),
            "iso_15031_5_on_iso_15765_4" => Some(ChannelProtocol::ISO_15031_5_ON_ISO_15765_4),
            "iso_14229_3_on_iso_15765_2_with_iso_11783_5" => {
                Some(ChannelProtocol::ISO_14229_3_ON_ISO_15765_2_WITH_ISO_11783_5)
            }
            // ISO_14229_3/ISO_15765_3 used standalone (not qualified
            // "_on_ISO_15765_2") — same underlying ISO15765 channel.
            "iso_14229_3" => Some(ChannelProtocol::ISO_14229_3),
            "iso_15765_3" => Some(ChannelProtocol::ISO_15765_3),
            // ISO9141 — native KWP1 channel (0x03)
            "iso9141" | "iso-9141" | "iso_9141" | "iso9141-2" | "iso-9141-2" | "iso_9141_2"
            | "kwp" | "kwp1" => Some(ChannelProtocol::ISO9141),
            // ISO 9141-2 channel variants
            "sae_j2190_on_iso_9141_2" => Some(ChannelProtocol::SAE_J2190_ON_ISO_9141_2),
            "iso_15031_5_on_iso_9141_2" => Some(ChannelProtocol::ISO_15031_5_ON_ISO_9141_2),
            // Combined ISO9141/ISO14230 K-line bus (ISO_9141_2_UART_and_ISO_14230_1_UART)
            "iso_15031_5_on_iso_9141_2_and_iso_14230_4" => {
                Some(ChannelProtocol::ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4)
            }
            "sae_j2190_on_iso_9141_2_and_iso_14230_2" => {
                Some(ChannelProtocol::SAE_J2190_ON_ISO_9141_2_AND_ISO_14230_2)
            }
            // ISO14230 — native KWP2000 channel (0x04)
            "iso14230" | "iso-14230" | "iso_14230" | "iso14230-1" | "iso-14230-1"
            | "iso_14230_1" | "kwp2000" | "kwp_2000" | "kwp-2000" | "kwp2" => {
                Some(ChannelProtocol::ISO14230)
            }
            // ISO 14230-2/4 channel variants
            "iso_14230_3_on_iso_14230_2" => Some(ChannelProtocol::ISO_14230_3_ON_ISO_14230_2),
            "sae_j2190_on_iso_14230_2" => Some(ChannelProtocol::SAE_J2190_ON_ISO_14230_2),
            "iso_15031_5_on_iso_14230_4" => Some(ChannelProtocol::ISO_15031_5_ON_ISO_14230_4),
            // J1850 PWM — native channel (0x02)
            "j1850pwm" | "j1850_pwm" | "j1850-pwm" | "pwm" => Some(ChannelProtocol::J1850PWM),
            // SAE J1850 PWM channel variants
            "sae_j2190_on_sae_j1850_pwm" => Some(ChannelProtocol::SAE_J2190_ON_SAE_J1850_PWM),
            "iso_15031_5_on_sae_j1850_pwm" => Some(ChannelProtocol::ISO_15031_5_ON_SAE_J1850_PWM),
            // J1850 VPW — native channel (0x01)
            "j1850vpw" | "j1850_vpw" | "j1850-vpw" | "vpw" => Some(ChannelProtocol::J1850VPW),
            // SAE J1850 VPW channel variants
            "sae_j2190_on_sae_j1850_vpw" => Some(ChannelProtocol::SAE_J2190_ON_SAE_J1850_VPW),
            "iso_15031_5_on_sae_j1850_vpw" => Some(ChannelProtocol::ISO_15031_5_ON_SAE_J1850_VPW),
            // SAE J1850 bus-agnostic variants (combined VPW/PWM bus type)
            "sae_j2190_on_sae_j1850" => Some(ChannelProtocol::SAE_J2190_ON_SAE_J1850),
            "iso_15031_5_on_sae_j1850" => Some(ChannelProtocol::ISO_15031_5_ON_SAE_J1850),
            // SCI-A Engine (0x07)
            "sci_a_engine" | "sci_a" | "sciaengine" | "scia_engine" => {
                Some(ChannelProtocol::SCI_A_ENGINE)
            }
            // SCI-A Transmission (0x08)
            "sci_a_trans" | "sci_a_transmission" | "sciatrans" | "scia_trans"
            | "scia_transmission" => Some(ChannelProtocol::SCI_A_TRANS),
            // SCI-B Engine (0x09)
            "sci_b_engine" | "sci_b" | "scibengine" | "scib_engine" => {
                Some(ChannelProtocol::SCI_B_ENGINE)
            }
            // SCI-B Transmission (0x0A)
            "sci_b_trans" | "sci_b_transmission" | "scibtrans" | "scib_trans"
            | "scib_transmission" => Some(ChannelProtocol::SCI_B_TRANS),
            // SCI Mode / SAE J2610 — uses TX_FLAG_SCI_MODE quirk at the hardware level
            "sci_mode" | "sci" | "sci_mode_select" | "sae_j2610_on_sae_j2610_sci" => {
                Some(ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI)
            }
            // SAE J2534-2 clause 12 UART Echo Byte Protocol (ADR-170/Phase
            // 9) — a legacy/direct-construction alias, distinct from the
            // resource-table row (0x023A, protocol_name "UART_ECHO_BYTE",
            // matched case-insensitively by `find_table_row_by_name` before
            // this fallback is ever consulted). No ISO 22900-2 short name
            // exists to alias (ADR-170 Context).
            "uart_echo_byte" | "uart-echo-byte" | "uart_echo_byte_ps" => {
                Some(ChannelProtocol::UART_ECHO_BYTE_PS)
            }
            // SAE J2534-2 clause 13 Honda DIAG-H Protocol (ADR-174/Phase
            // 10) — a legacy/direct-construction alias, distinct from the
            // resource-table row (0x023B, protocol_name "HONDA_DIAGH",
            // matched case-insensitively by `find_table_row_by_name` before
            // this fallback is ever consulted). No ISO 22900-2 short name
            // exists to alias (ADR-174 Context).
            "honda_diagh" | "honda-diagh" | "honda_diagh_ps" => {
                Some(ChannelProtocol::HONDA_DIAGH_PS)
            }
            // SAE J2534-2 clause 17 SAE J1708 Protocol (ADR-175/Phase 11) —
            // a legacy/direct-construction alias, distinct from the
            // resource-table row (0x023C, protocol_name "SAE_J1708",
            // matched case-insensitively by `find_table_row_by_name` before
            // this fallback is ever consulted). No ISO 22900-2 short name
            // exists to alias (ADR-175 Context). Codex review finding, PR
            // #64: mirrors the identical UART Echo Byte/Honda DIAG-H arms
            // just above -- without this, `CreateComLogicalLink`/
            // `GetObjectId(OBJT_PROTOCOL, "J1708_PS")` naming the symbolic
            // protocol id directly (rather than the project-chosen table
            // row name) fell through to a rejection.
            "j1708" | "j1708_ps" => Some(ChannelProtocol::J1708_PS),
            // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16) -- a
            // legacy/direct-construction alias, distinct from the
            // resource-table row (0x0261, protocol_name "ETHERNET_NDIS",
            // matched case-insensitively by `find_table_row_by_name` before
            // this fallback is ever consulted). No ISO 22900-2 short name
            // exists to alias for the PROTOCOL identity itself (clause 24
            // defines no D-PDU resource shortname).
            "ethernet_ndis" | "ethernet-ndis" => Some(ChannelProtocol::ETHERNET_NDIS),
            _ => None,
        }
    }

    pub(super) fn map_bustype_name(name: &str) -> Option<u32> {
        match name.to_ascii_lowercase().as_str() {
            // DW-CAN (Differential Wiring CAN) - ISO 11898-2 / ISO 11898-3
            "iso_11898_2_dwcan"
            | "iso-11898-2-dwcan"
            | "iso11898_2_dwcan"
            | "iso_11898_3_dwftcan"
            | "iso-11898-3-dwftcan"
            | "iso11898_3_dwftcan"
            | "iso_11992_1_dwcan"
            | "iso-11992-1-dwcan"
            | "iso11992_1_dwcan" => Some(j2534_0404::CAN),
            // SAE J1939 (11-bit DW-CAN, 250 kbit/s) — uses standard CAN hardware channel
            "sae_j1939_11_dwcan" | "sae-j1939-11-dwcan" | "j1939_11_dwcan" => Some(j2534_0404::CAN),
            // SAE J2411 SW-CAN (Single Wire CAN) — not supported by J2534-0404
            "sae_j2411_swcan" | "sae-j2411-swcan" | "j2411_swcan" => None,
            // K-Line protocols (UART) - ISO 9141-2 / ISO 14230-1
            "iso_9141_2_uart" | "iso-9141-2-uart" | "iso9141_2_uart" | "iso_14230_1_uart"
            | "iso-14230-1-uart" | "iso14230_1_uart" => {
                // Physical layer only; caller must determine transport protocol from context.
                None
            }
            // SAE J1708 UART (heavy-duty truck): before ADR-175/Phase 11,
            // this alias mapped to the ISO9141 hardware channel as a
            // physical-layer placeholder (J1708 had no real implementation
            // yet). Now that row 0x023C gives it a genuine native protocol
            // id (`PROTOCOL_J1708_PS`), this legacy hw-id alias must resolve
            // to that id instead -- otherwise `candidates_matching_bustype`'s
            // fallback (and `GetObjectId(OBJT_BUSTYPE, ...)`'s own fallback)
            // would route a non-canonical alias like "sae-j1708-uart"/
            // "j1708_uart" to ISO9141 rows instead of the new J1708 row, even
            // though the canonical exact name "SAE_J1708_UART" already
            // resolves correctly through the table match that runs before
            // this fallback is ever consulted (Codex review finding, PR #64).
            "sae_j1708_uart" | "sae-j1708-uart" | "j1708_uart" => {
                Some(j2534_0404::PROTOCOL_J1708_PS)
            }
            // SAE J1850 VPW (Variable Pulse Width)
            "sae_j1850_vpw" | "sae-j1850-vpw" | "j1850_vpw" | "j1850-vpw" => {
                Some(j2534_0404::J1850VPW)
            }
            // SAE J1850 PWM (Pulse Width Modulation)
            "sae_j1850_pwm" | "sae-j1850-pwm" | "j1850_pwm" | "j1850-pwm" => {
                Some(j2534_0404::J1850PWM)
            }
            // Chrysler SCI (Serial Communications Interface)
            "sae_j2610_uart" | "sae-j2610-uart" | "sae_j2610_sci" | "sae-j2610-sci" => {
                Some(j2534_0404::SCI_MODE)
            }
            // SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage
            // 7a): resolves to its own native id directly, the same J1708
            // fix (Codex review, PR #64) applies -- TP2.0 has its own
            // standalone native protocol id, not a generic CAN-channel
            // placeholder.
            "tp2_0_dwcan" | "tp2-0-dwcan" => Some(j2534_0404::PROTOCOL_TP2_0_PS),
            _ => None,
        }
    }

    pub(super) fn map_pintype_name(name: &str) -> Option<u32> {
        match name.to_ascii_lowercase().as_str() {
            "hi" => Some(2000),
            "low" => Some(2001),
            "k" => Some(2002),
            "l" => Some(2003),
            "tx" => Some(2004),
            "rx" => Some(2005),
            "plus" => Some(2006),
            "minus" => Some(2007),
            "single" => Some(2008),
            "ign" | "ignition_clamp" => Some(2009),
            "progv" => Some(2010),
            _ => None,
        }
    }

    /// Resolves an IOCTL shortname to its numeric `io_ctrl_command_id`, per
    /// the private `PDU_IOCTL_BASE` namespace this adapter defines for the 25
    /// D-PDU IOCTL commands it implements (17 from ADR-079, plus
    /// SW_CAN_HS/SW_CAN_NS from ADR-164 Decision 3/Phase 4, plus
    /// START/QUERY/STOP_REPEAT_MESSAGE from ADR-165/Phase 12, plus
    /// READ_J1962PIN_VOLTAGE from Phase 13, plus GET/SET_DEVICE_CONFIG from
    /// ADR-176/Phase 14) -- distinct from the 4
    /// legacy raw J2534 IOCTL IDs (`CLEAR_RX_BUFFER` et al.) `rpc_io_ctl`
    /// separately recognizes by their own numeric value, which have no
    /// shortname of their own here.
    pub(super) fn map_ioctl_name(name: &str) -> Option<u32> {
        match name.to_ascii_lowercase().as_str() {
            "pdu_ioctl_reset" => Some(PDU_IOCTL_RESET),
            "pdu_ioctl_clear_tx_queue" => Some(PDU_IOCTL_CLEAR_TX_QUEUE),
            "pdu_ioctl_suspend_tx_queue" => Some(PDU_IOCTL_SUSPEND_TX_QUEUE),
            "pdu_ioctl_resume_tx_queue" => Some(PDU_IOCTL_RESUME_TX_QUEUE),
            "pdu_ioctl_clear_rx_queue" => Some(PDU_IOCTL_CLEAR_RX_QUEUE),
            "pdu_ioctl_read_vbatt" => Some(PDU_IOCTL_READ_VBATT),
            "pdu_ioctl_set_prog_voltage" => Some(PDU_IOCTL_SET_PROG_VOLTAGE),
            "pdu_ioctl_read_prog_voltage" => Some(PDU_IOCTL_READ_PROG_VOLTAGE),
            "pdu_ioctl_generic" => Some(PDU_IOCTL_GENERIC),
            "pdu_ioctl_set_buffer_size" => Some(PDU_IOCTL_SET_BUFFER_SIZE),
            "pdu_ioctl_start_msg_filter" => Some(PDU_IOCTL_START_MSG_FILTER),
            "pdu_ioctl_stop_msg_filter" => Some(PDU_IOCTL_STOP_MSG_FILTER),
            "pdu_ioctl_clear_msg_filter" => Some(PDU_IOCTL_CLEAR_MSG_FILTER),
            "pdu_ioctl_set_event_queue_properties" => Some(PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES),
            "pdu_ioctl_get_cable_id" => Some(PDU_IOCTL_GET_CABLE_ID),
            "pdu_ioctl_send_break" => Some(PDU_IOCTL_SEND_BREAK),
            "pdu_ioctl_read_ignition_sense_state" => Some(PDU_IOCTL_READ_IGNITION_SENSE_STATE),
            // ADR-164 Decision 3/Phase 4: SAE J2534-2 clause 9 Single Wire
            // CAN's two ChannelID-scoped (L) commands.
            "pdu_ioctl_sw_can_hs" => Some(PDU_IOCTL_SW_CAN_HS),
            "pdu_ioctl_sw_can_ns" => Some(PDU_IOCTL_SW_CAN_NS),
            // ADR-165/Phase 12: SAE J2534-2 clause 14 Repeat Messaging's
            // three ChannelID-scoped (L) commands.
            "pdu_ioctl_start_repeat_message" => Some(PDU_IOCTL_START_REPEAT_MESSAGE),
            "pdu_ioctl_query_repeat_message" => Some(PDU_IOCTL_QUERY_REPEAT_MESSAGE),
            "pdu_ioctl_stop_repeat_message" => Some(PDU_IOCTL_STOP_REPEAT_MESSAGE),
            // Phase 13: SAE J2534-2 clause 23 J1962 Pin Voltage Read's
            // module-scoped (M) command.
            "pdu_ioctl_read_j1962pin_voltage" => Some(PDU_IOCTL_READ_J1962PIN_VOLTAGE),
            // ADR-176/Phase 14: SAE J2534-2 clause 18 Device Configuration's
            // module-scoped (M) commands.
            "pdu_ioctl_get_device_config" => Some(PDU_IOCTL_GET_DEVICE_CONFIG),
            "pdu_ioctl_set_device_config" => Some(PDU_IOCTL_SET_DEVICE_CONFIG),
            // ADR-189/Phase 8: SAE J2534-2 clause 11 GM UART Protocol's two
            // ChannelID-scoped (L) commands.
            "pdu_ioctl_set_poll_response" => Some(PDU_IOCTL_SET_POLL_RESPONSE),
            "pdu_ioctl_become_master" => Some(PDU_IOCTL_BECOME_MASTER),
            // ADR-194/Phase 16: SAE J2534-2 clause 24 Ethernet_NDIS's one
            // ChannelID-scoped (L) command.
            "pdu_ioctl_get_ndis_adapter_info" => Some(PDU_IOCTL_GET_NDIS_ADAPTER_INFO),
            _ => None,
        }
    }

    /// Resolves a ComParam shortname to its `ComParamId`, or a
    /// `PDU_ERR_INVALID_PARAMETERS`-equivalent error for an unrecognized
    /// name. Used by both `GetComParam` and `SetComParam` (and
    /// `SetUniqueRespIdTable`'s per-entry `ParamItem`s) so that the same
    /// name always resolves to the same id everywhere -- name-to-id mapping
    /// is CLL-independent; whether the resolved id is actually usable on a
    /// given CLL is checked separately (`comparam_support::check_param_allowed`).
    pub(super) fn resolve_comparam_name(name: &str) -> Result<ComParamId, Status> {
        Self::map_comparam_name(name)
            .ok_or_else(|| Status::invalid_argument(format!("unknown ComParam name {name:?}")))
    }

    /// Resolves a ComParam shortname to its `ComParamId`, trying each category
    /// group in turn.  Split into `map_comparam_name_{native,service_timing,
    /// transport,physical}` — one function per section of the D-PDU API mapping
    /// table — since a single flat match here was hard to navigate at over 200
    /// lines; the split has no effect on matching behaviour since shortnames are
    /// unique across all categories.
    pub(super) fn map_comparam_name(name: &str) -> Option<ComParamId> {
        let name = name.to_ascii_lowercase();
        let name = name.as_str();
        Self::map_comparam_name_native(name)
            .or_else(|| Self::map_comparam_name_service_timing(name))
            .or_else(|| Self::map_comparam_name_transport(name))
            .or_else(|| Self::map_comparam_name_physical(name))
    }

    /// Native J2534 `SET_CONFIG` params: baud rate, physical/link layer, KWP/ISO14230
    /// P-timers, ISO9141 W-/bus-state timers, SCI T-timers, ISO15765 flow-control,
    /// 5-baud init mode, UART data bits.
    fn map_comparam_name_native(name: &str) -> Option<ComParamId> {
        match name {
            // Baud rate / data rate
            "cp_baudrate" | "data_rate" => Some(ComParamId(j2534_0404::DATA_RATE)),
            // Physical / link layer
            "cp_loopback" | "loopback" => Some(ComParamId(j2534_0404::LOOPBACK)),
            "cp_testersourceaddress" | "cp_node_address" | "cp_nodeaddress" | "node_address" => {
                Some(ComParamId(j2534_0404::NODE_ADDRESS))
            }
            "cp_networkline" | "cp_network_line" | "network_line" => {
                Some(ComParamId(j2534_0404::NETWORK_LINE))
            }
            // KWP / ISO14230 P-timers (ComParam-space values are 1 us
            // resolution; P1_MAX/P3_MIN/P4_MIN convert to native J2534
            // 0.5 ms resolution at forwarding time via
            // `to_j2534_config_value`, ADR-072 -- the rest of this group
            // have no J2534 SET_CONFIG equivalent and are never forwarded)
            "cp_p1min" | "p1_min" => Some(ComParamId(j2534_0404::P1_MIN)),
            "cp_p1max" | "p1_max" => Some(ComParamId(j2534_0404::P1_MAX)),
            "cp_p2min" | "p2_min" => Some(ComParamId(j2534_0404::P2_MIN)),
            "cp_p2max" | "p2_max" => Some(ComParamId(j2534_0404::P2_MAX)),
            "cp_p3min" | "p3_min" => Some(ComParamId(j2534_0404::P3_MIN)),
            "cp_p3max_ecu" | "p3_max" => Some(ComParamId(j2534_0404::P3_MAX)),
            "cp_p4min" | "p4_min" => Some(ComParamId(j2534_0404::P4_MIN)),
            "cp_p4max" | "p4_max" => Some(ComParamId(j2534_0404::P4_MAX)),
            // ISO9141 W-timers (ComParam-space value is 1 us resolution;
            // converts to native J2534 1 ms resolution at forwarding time
            // via `to_j2534_config_value`, ADR-072) -- SAE J2534-1 Figure 30
            // defines only a single native register per pair (W1-W3: MAX
            // side only; W4: MIN side only), so only the side with an actual
            // native target resolves to the native `ComParamId` here; the
            // side with no native counterpart resolves to its own
            // project-minted, store-only id below (ADR-181, fixes the
            // storage-key collision that previously made both sides of each
            // pair silently overwrite the same Working/Active entry).
            "cp_w1max" | "w1" => Some(ComParamId(j2534_0404::W1)),
            "cp_w2max" | "w2" => Some(ComParamId(j2534_0404::W2)),
            "cp_w3max" | "w3" => Some(ComParamId(j2534_0404::W3)),
            "cp_w4min" | "w4" => Some(ComParamId(j2534_0404::W4)),
            "cp_w5max" | "w5" => Some(ComParamId(j2534_0404::W5)),
            // ISO9141 W-timer sides with no native J2534 register (ADR-181):
            // stored service-level only, never forwarded via
            // `ComParamId::to_j2534_config_id`'s catch-all.
            "cp_w1min" => Some(PARAM_W1_MIN),
            "cp_w2min" => Some(PARAM_W2_MIN),
            "cp_w3min" => Some(PARAM_W3_MIN),
            "cp_w4max" => Some(PARAM_W4_MAX),
            // ISO9141 bus-state timers (ComParam-space values are 1 us
            // resolution; convert to native J2534 1 ms resolution at
            // forwarding time via `to_j2534_config_value`, ADR-072 --
            // CP_TIdle additionally fans out to W0 (ISO9141) / W5
            // (ISO14230) at that same forwarding step, see `expand_tidle`)
            "cp_tidle" | "tidle" => Some(ComParamId(j2534_0404::TIDLE)),
            "cp_tinil" | "tinil" => Some(ComParamId(j2534_0404::TINIL)),
            "cp_twup" | "twup" => Some(ComParamId(j2534_0404::TWUP)),
            // Physical layer
            "cp_parity" | "parity" => Some(ComParamId(j2534_0404::PARITY)),
            "cp_bit_sample_point" | "cp_bitsamplepoint" | "bit_sample_point" => {
                Some(ComParamId(j2534_0404::BIT_SAMPLE_POINT))
            }
            "cp_sync_jump_width" | "cp_syncjumpwidth" | "sync_jump_width" => {
                Some(ComParamId(j2534_0404::SYNC_JUMP_WIDTH))
            }
            // ISO9141 W0 timer (ComParam-space value is 1 us resolution,
            // ADR-072; converts to native J2534 1 ms resolution at
            // forwarding time via `to_j2534_config_value`). `CP_W0Max` (here)
            // and `CP_W5Max` (below, with the other W-timers) are
            // project-invented direct-native-W0/W5-alias names with no ISO
            // 22900-2 counterpart -- unrelated to the CP_W1-W4 Min/Max
            // storage-key fix (ADR-181), an accepted residual left as-is.
            "cp_w0max" | "w0" => Some(ComParamId(j2534_0404::W0)),
            // SCI T-timers (ComParam-space value is 1 us resolution;
            // converts to native J2534 1 ms resolution at forwarding time
            // via `to_j2534_config_value`, ADR-072)
            "cp_t1max" | "t1_max" => Some(ComParamId(j2534_0404::T1_MAX)),
            "cp_t2max" | "t2_max" => Some(ComParamId(j2534_0404::T2_MAX)),
            "cp_t4max" | "t4_max" => Some(ComParamId(j2534_0404::T4_MAX)),
            "cp_t5max" | "t5_max" => Some(ComParamId(j2534_0404::T5_MAX)),
            "cp_t3max" | "t3_max" => Some(ComParamId(j2534_0404::T3_MAX)),
            // SAE J2534-2 clause 12.3.4.1 UART Echo Byte timing ComParams
            // (ADR-216, project-invented, no ISO 22900-2 source) -- native
            // whole-millisecond values, NOT the 1 us ISO 22900-2 `CP_*`
            // timing convention every group above converts at forwarding
            // time.
            "cp_uebt0min" => Some(PARAM_UEB_T0_MIN),
            "cp_uebt1max" => Some(PARAM_UEB_T1_MAX),
            "cp_uebt2max" => Some(PARAM_UEB_T2_MAX),
            "cp_uebt3max" => Some(PARAM_UEB_T3_MAX),
            "cp_uebt4min" => Some(PARAM_UEB_T4_MIN),
            "cp_uebt5max" => Some(PARAM_UEB_T5_MAX),
            "cp_uebt6max" => Some(PARAM_UEB_T6_MAX),
            "cp_uebt7min" => Some(PARAM_UEB_T7_MIN),
            "cp_uebt7max" => Some(PARAM_UEB_T7_MAX),
            "cp_uebt9min" => Some(PARAM_UEB_T9_MIN),
            // ISO15765 flow-control
            "cp_stmin" | "iso15765_stmin" => Some(ComParamId(j2534_0404::ISO15765_STMIN)),
            "cp_blocksize" | "iso15765_bs" => Some(ComParamId(j2534_0404::ISO15765_BS)),
            "cp_canmaxnumwaitframes" | "iso15765_wft_max" => {
                Some(ComParamId(j2534_0404::ISO15765_WFT_MAX))
            }
            // ISO15765 Tx-side flow-control overrides
            "cp_blocksizeoverride" | "cp_bstx" | "cp_bs_tx" | "bs_tx" => {
                Some(ComParamId(j2534_0404::BS_TX))
            }
            "cp_stminoverride" | "cp_stmintx" | "cp_stmin_tx" | "stmin_tx" => {
                Some(ComParamId(j2534_0404::STMIN_TX))
            }
            // 5-baud init mode
            "cp_5baudmode" | "five_baud_mod" => Some(ComParamId(j2534_0404::FIVE_BAUD_MOD)),
            // UART data bits (DATA_BITS)
            "cp_uartconfig" | "data_bits" => Some(ComParamId(j2534_0404::DATA_BITS)),
            _ => None,
        }
    }

    /// Service-level params (no J2534 `SET_CONFIG` equivalent): tester-present,
    /// timing, error-handling (RC21/RC23/RC78), and COM.
    fn map_comparam_name_service_timing(name: &str) -> Option<ComParamId> {
        match name {
            // ── Tester Present (service-level, not forwarded to J2534 hardware) ──────────
            "cp_testerpresentmessage" => Some(PARAM_TESTER_PRESENT_MSG),
            "cp_testerpresenttime" => Some(PARAM_TESTER_PRESENT_INTERVAL_US),
            "cp_testerpresentaddrmode" => Some(PARAM_TESTER_PRESENT_ADDR_MODE),
            "cp_testerpresentexpposresp" => Some(PARAM_TESTER_PRESENT_EXP_POS_RESP),
            "cp_testerpresentexpnegresp" => Some(PARAM_TESTER_PRESENT_EXP_NEG_RESP),
            "cp_testerpresenthandling" => Some(PARAM_TESTER_PRESENT_HANDLING),
            "cp_testerpresentreqrsp" => Some(PARAM_TESTER_PRESENT_REQ_RSP),
            "cp_testerpresentsendtype" => Some(PARAM_TESTER_PRESENT_SEND_TYPE),
            "cp_testerpresenttime_ecu" => Some(PARAM_TESTER_PRESENT_TIME_ECU),
            "cp_testerpresentimmed" => Some(PARAM_TESTER_PRESENT_IMMED),

            // ── Timing (service-level) ────────────────────────────────────────────────────
            "cp_cyclicresptimeout" => Some(PARAM_CYCLIC_RESP_TIMEOUT),
            // CP_P3Phys/CP_P3Func (CAN context): always resolve to the
            // service-level PARAM_P3_PHYS/PARAM_P3_FUNC, regardless of the
            // connected CLL's protocol -- name-to-id mapping is
            // CLL-independent by design (unlike the old
            // `map_comparam_name_for_protocol`, which redirected these two
            // names to native P3_MIN on non-CAN protocols on the mistaken
            // premise that KWP's P3_MIN is an interchangeable substitute for
            // CAN's physical/functional P3 gap; it is not -- KWP has its own
            // distinct name, `CP_P3Min`). Whether PARAM_P3_PHYS/PARAM_P3_FUNC
            // is actually usable on a given CLL is decided separately by
            // `comparam_support::check_param_allowed`, which already
            // correctly restricts them to CAN-family protocols.
            "cp_p3phys" => Some(PARAM_P3_PHYS),
            "cp_p3func" => Some(PARAM_P3_FUNC),
            "cp_p2star" => Some(PARAM_P2_STAR),
            "cp_p2star_ecu" => Some(PARAM_P2_STAR_ECU),
            "cp_p2max_ecu" => Some(PARAM_P2_MAX_ECU),
            "cp_modifytiming" => Some(PARAM_MODIFY_TIMING),
            "cp_sessiontiming_ecu" => Some(PARAM_SESSION_TIMING_ECU),
            "cp_sessiontimingoverride" => Some(PARAM_SESSION_TIMING_OVERRIDE),
            "cp_cantransmissiontime" => Some(PARAM_CAN_TRANSMISSION_TIME),
            "cp_messageindicationrate" => Some(PARAM_MESSAGE_INDICATION_RATE),
            "cp_changespeedtxdelay" => Some(PARAM_CHANGE_SPEED_TX_DELAY),

            // ── Error Handling (service-level) ────────────────────────────────────────────
            "cp_rc21completiontimeout" => Some(PARAM_RC21_COMPLETION_TIMEOUT),
            "cp_rc21handling" => Some(PARAM_RC21_HANDLING),
            "cp_rc21requesttime" => Some(PARAM_RC21_REQUEST_TIME),
            "cp_rc23completiontimeout" => Some(PARAM_RC23_COMPLETION_TIMEOUT),
            "cp_rc23handling" => Some(PARAM_RC23_HANDLING),
            "cp_rc23requesttime" => Some(PARAM_RC23_REQUEST_TIME),
            "cp_rc78completiontimeout" => Some(PARAM_RC78_COMPLETION_TIMEOUT),
            "cp_rc78handling" => Some(PARAM_RC78_HANDLING),
            "cp_rcbyteoffset" => Some(PARAM_RC_BYTE_OFFSET),
            "cp_repeatreqcountapp" => Some(PARAM_REPEAT_REQ_COUNT_APP),
            "cp_suspendqueueonerror" => Some(PARAM_SUSPEND_QUEUE_ON_ERROR),

            // ── COM (service-level) ───────────────────────────────────────────────────────
            "cp_changespeedctrl" => Some(PARAM_CHANGE_SPEED_CTRL),
            "cp_changespeedmessage" => Some(PARAM_CHANGE_SPEED_MSG),
            "cp_changespeedrate" => Some(PARAM_CHANGE_SPEED_RATE),
            "cp_changespeedresctrl" => Some(PARAM_CHANGE_SPEED_RES_CTRL),
            "cp_enableperformancetest" => Some(PARAM_ENABLE_PERFORMANCE_TEST),
            "cp_startmsgindenable" => Some(PARAM_START_MSG_IND_ENABLE),
            "cp_transmitindenable" => Some(PARAM_TRANSMIT_IND_ENABLE),
            "cp_swcan_highvoltage" | "cp_swcanhighvoltage" => Some(PARAM_SW_CAN_HIGH_VOLTAGE),

            // ── Protocol behavior (service-level) ─────────────────────────────────────────
            "cp_testmode" => Some(PARAM_TEST_MODE),
            _ => None,
        }
    }

    /// Transport-layer params (service-level): ISO 15765-2 frame timing, CAN
    /// addressing, ECU addressing/COM, INIT, and J1939 NAME (Bytefield).
    fn map_comparam_name_transport(name: &str) -> Option<ComParamId> {
        match name {
            // ── Transport layer: ISO 15765-2 frame timing (service-level) ─────────────────
            "cp_ar" => Some(PARAM_N_AR),
            "cp_ar_ecu" => Some(PARAM_N_AR_ECU),
            "cp_as" => Some(PARAM_N_AS),
            "cp_as_ecu" => Some(PARAM_N_AS_ECU),
            "cp_br" => Some(PARAM_N_BR),
            "cp_br_ecu" => Some(PARAM_N_BR_ECU),
            "cp_bs" => Some(PARAM_N_BS),
            "cp_bs_ecu" => Some(PARAM_N_BS_ECU),
            "cp_cr" => Some(PARAM_N_CR),
            "cp_cr_ecu" => Some(PARAM_N_CR_ECU),
            "cp_cs" => Some(PARAM_N_CS),
            "cp_cs_ecu" => Some(PARAM_N_CS_ECU),
            "cp_stmin_ecu" => Some(PARAM_ST_MIN_ECU),
            "cp_blocksize_ecu" | "cp_blocksizeecu" => Some(PARAM_BLOCK_SIZE_ECU),
            "cp_accesstiming_ecu" => Some(PARAM_ACCESS_TIMING_ECU),
            "cp_accesstimingoverride" => Some(PARAM_ACCESS_TIMING_OVERRIDE),
            "cp_extendedtiming" => Some(PARAM_EXTENDED_TIMING),
            "cp_j1939addrclaimtimeout" => Some(PARAM_J1939_ADDR_CLAIM_TIMEOUT),
            "cp_escapesequencehandling" => Some(PARAM_ESCAPE_SEQUENCE_HANDLING),
            "cp_maxdatalength_ecu" | "cp_maxdatalengthecu" => Some(PARAM_MAX_DATA_LENGTH_ECU),
            "cp_repeatreqcounttrans" => Some(PARAM_REPEAT_REQ_COUNT_TRANS),

            // ── Transport layer: checksum handling (service-level) ────────────────────────
            "cp_disabletransportchecksumcheck" => Some(PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK),
            "cp_ignorechecksum" => Some(PARAM_IGNORE_CHECKSUM),

            // ── Transport layer: CAN addressing (service-level) ───────────────────────────
            "cp_canphysreqextaddr" => Some(PARAM_CAN_PHYS_REQ_EXT_ADDR),
            "cp_canphysreqformat" => Some(PARAM_CAN_PHYS_REQ_FORMAT),
            "cp_canphysreqid" => Some(PARAM_CAN_PHYS_REQ_ID),
            "cp_canrespusdtextaddr" => Some(PARAM_CAN_RESP_USDT_EXT_ADDR),
            "cp_canrespusdtformat" => Some(PARAM_CAN_RESP_USDT_FORMAT),
            "cp_canrespusdtid" => Some(PARAM_CAN_RESP_USDT_ID),
            "cp_canrespuudtextaddr" => Some(PARAM_CAN_RESP_UUDT_EXT_ADDR),
            "cp_canrespuudtformat" => Some(PARAM_CAN_RESP_UUDT_FORMAT),
            "cp_canrespuudtid" => Some(PARAM_CAN_RESP_UUDT_ID),
            "cp_canfuncreqextaddr" => Some(PARAM_CAN_FUNC_REQ_EXT_ADDR),
            "cp_canfuncreqformat" => Some(PARAM_CAN_FUNC_REQ_FORMAT),
            "cp_canfuncreqid" => Some(PARAM_CAN_FUNC_REQ_ID),
            "cp_candatasizeoffset" => Some(PARAM_CAN_DATA_SIZE_OFFSET),
            "cp_canfillerbyte" => Some(PARAM_CAN_FILLER_BYTE),
            "cp_canfillerbytehandling" => Some(PARAM_CAN_FILLER_BYTE_HANDLING),
            "cp_canfirstconsecutiveframevalue" => Some(PARAM_CAN_FIRST_CF_VALUE),
            "cp_canmixedformat" => Some(PARAM_CAN_MIXED_FORMAT),

            // ── Transport layer: ECU addressing / COM (service-level) ─────────────────────
            // "Soure" variants are misspelled aliases kept for backward
            // compatibility with clients and saved configs built against
            // builds that only accepted the misspelling.
            "cp_ecurespsourceaddress"
            | "cp_ecurespsourceaddr"
            | "cp_ecurespsoureaddress"
            | "cp_ecurespsoureaddr" => Some(PARAM_ECU_RESP_SOURCE_ADDR),
            "cp_funcreqformatprioritytype" => Some(PARAM_FUNC_REQ_FORMAT_PRIORITY),
            "cp_funcreqtargetaddr" => Some(PARAM_FUNC_REQ_TARGET_ADDR),
            "cp_funcrespformatprioritytype" => Some(PARAM_FUNC_RESP_FORMAT_PRIORITY),
            "cp_funcresptargetaddr" => Some(PARAM_FUNC_RESP_TARGET_ADDR),
            "cp_physreqformatprioritytype" => Some(PARAM_PHYS_REQ_FORMAT_PRIORITY),
            "cp_physreqtargetaddr" => Some(PARAM_PHYS_REQ_TARGET_ADDR),
            "cp_physrespformatprioritytype" => Some(PARAM_PHYS_RESP_FORMAT_PRIORITY),
            "cp_requestaddrmode" => Some(PARAM_REQUEST_ADDR_MODE),
            "cp_headerformatj1850" => Some(PARAM_HEADER_FORMAT_J1850),
            "cp_headerformatkw" => Some(PARAM_HEADER_FORMAT_KW),
            "cp_enableconcatenation" => Some(PARAM_ENABLE_CONCATENATION),
            "cp_fillerbyte" => Some(PARAM_FILLER_BYTE),
            "cp_fillerbytehandling" => Some(PARAM_FILLER_BYTE_HANDLING),
            "cp_fillerbytelength" => Some(PARAM_FILLER_BYTE_LENGTH),
            "cp_5baudaddressfunc" => Some(PARAM_5BAUD_ADDR_FUNC),
            "cp_5baudaddressphys" => Some(PARAM_5BAUD_ADDR_PHYS),
            "cp_5baudcommbaudrateoverride" => Some(PARAM_5BAUD_COMM_BAUDRATE_OVERRIDE),
            "cp_5baudinitbaudrate" => Some(PARAM_5BAUD_INIT_BAUDRATE),
            "cp_sendremoteframe" => Some(PARAM_SEND_REMOTE_FRAME),
            "cp_tpconnectionmanagement" => Some(PARAM_TP_CONNECTION_MGMT),
            "cp_messagepriority" => Some(PARAM_MESSAGE_PRIORITY),
            "cp_midreqid" => Some(PARAM_MID_REQ_ID),
            "cp_midrespid" => Some(PARAM_MID_RESP_ID),
            "cp_j1939addressnegotiationrule" => Some(PARAM_J1939_ADDR_NEG_RULE),
            "cp_j1939datapage" => Some(PARAM_J1939_DATA_PAGE),
            "cp_j1939maxpackettx" => Some(PARAM_J1939_MAX_PACKET_TX),
            "cp_j1939pduformat" => Some(PARAM_J1939_PDU_FORMAT),
            "cp_j1939pduspecific" => Some(PARAM_J1939_PDU_SPECIFIC),
            "cp_j1939sourceaddress" => Some(PARAM_J1939_SOURCE_ADDRESS),
            "cp_j1939targetaddress" => Some(PARAM_J1939_TARGET_ADDRESS),

            // ── Transport layer: INIT (service-level) ─────────────────────────────────────
            "cp_initializationsettings" => Some(PARAM_INIT_SETTINGS),
            "cp_enableinitseqrepetition" => Some(PARAM_ENABLE_INIT_SEQ_REPETITION),
            "cp_numheaderbytesstartcommkw" => Some(PARAM_NUM_HEADER_BYTES_START_COMM_KW),
            "cp_isokeybytecount" => Some(PARAM_ISO_KEYBYTE_COUNT),
            "cp_scitransmitmode" => Some(PARAM_SCI_TRANSMIT_MODE),
            "cp_scisetprogvoltage" => Some(PARAM_SCI_SET_PROG_VOLTAGE),
            "cp_sciecusimulator" => Some(PARAM_SCI_ECU_SIMULATOR),
            "cp_j1939preferredaddress" => Some(PARAM_J1939_PREFERRED_ADDRESS),
            "cp_j1939preferredaddress_ecu" => Some(PARAM_J1939_PREFERRED_ADDRESS_ECU),

            // ── Transport layer: J1939/J1587 TP session control (service-level) ───────────
            // "cp_t3max"/"cp_t4max"/"cp_t5max" are NOT registered here:
            // ADR-179 (Phase 5) resolved the apparent SCI-vs-J1939/J1587
            // collision this comment previously flagged as an open gap --
            // `CP_T3Max`/`CP_T4Max`/`CP_T5Max` are each a single ISO
            // 22900-2 ComParam with a per-protocol default value, not two
            // distinct D-PDU constants, so `map_comparam_name_native`'s
            // existing SCI T-timer mapping (`j2534_0404::T3_MAX`/`T4_MAX`/
            // `T5_MAX`, tried first in `map_comparam_name`'s `or_else`
            // chain) is already the single, correct registration for both
            // the SCI and J1939 contexts at once -- adding the same
            // shortname string here would still be unreachable dead code,
            // but for the opposite reason: it is unnecessary, not blocked on
            // an unresolved design question.
            "cp_maxctsreq" => Some(PARAM_MAX_CTS_REQ),
            "cp_collisiontestmode" => Some(PARAM_COLLISION_TEST_MODE),

            // ── Transport layer: J1939 NAME (Bytefield, service-level) ────────────────────
            "cp_j1939name" => Some(PARAM_J1939_NAME),
            "cp_j1939name_ecu" => Some(PARAM_J1939_NAME_ECU),
            "cp_j1939sourcename" => Some(PARAM_J1939_SOURCE_NAME),
            "cp_j1939targetname" => Some(PARAM_J1939_TARGET_NAME),
            _ => None,
        }
    }

    /// Physical-layer params (service-level; no J2534 `SET_CONFIG` equivalent),
    /// CAN FD physical layer, and J1850 IFR control.
    ///
    /// `CP_Baudrate` → `DATA_RATE`, `CP_BitSamplePoint` → `BIT_SAMPLE_POINT`,
    /// `CP_SyncJumpWidth` → `SYNC_JUMP_WIDTH`, `CP_NetworkLine` → `NETWORK_LINE`,
    /// `CP_UartConfig` → `DATA_BITS` are forwarded directly (see
    /// `map_comparam_name_native`) and not listed here.
    fn map_comparam_name_physical(name: &str) -> Option<ComParamId> {
        match name {
            "cp_bitsamplepoint_ecu" | "cp_bitsamplepointecu" => Some(PARAM_BIT_SAMPLE_POINT_ECU),
            "cp_samplesperbit" => Some(PARAM_SAMPLES_PER_BIT),
            "cp_samplesperbit_ecu" | "cp_samplesperbitecu" => Some(PARAM_SAMPLES_PER_BIT_ECU),
            "cp_syncjumpwidth_ecu" | "cp_syncjumpwidthecu" => Some(PARAM_SYNC_JUMP_WIDTH_ECU),
            "cp_listenonly" => Some(PARAM_LISTEN_ONLY),
            "cp_canbaudraterecord" => Some(PARAM_CAN_BAUDRATE_RECORD),
            "cp_k_l_lineinit" | "cp_kllineinit" => Some(PARAM_K_L_LINE_INIT),
            "cp_k_linepullup" | "cp_klinepullup" => Some(PARAM_K_LINE_PULLUP),
            "cp_terminationtype" => Some(PARAM_TERMINATION_TYPE),
            "cp_terminationtype_ecu" | "cp_terminationtypeecu" => Some(PARAM_TERMINATION_TYPE_ECU),

            // ── CAN FD physical layer (service-level) ─────────────────────────────────────
            "cp_canfdbaudrate" => Some(PARAM_CANFD_BAUDRATE),
            "cp_canfdbitsamplepoint" => Some(PARAM_CANFD_BIT_SAMPLE_POINT),
            "cp_canfdsyncjumpwidth" => Some(PARAM_CANFD_SYNC_JUMP_WIDTH),
            "cp_canfdtxmaxdatalength" => Some(PARAM_CANFD_TX_MAX_DATA_LENGTH),

            // ── J1850 IFR control (service-level) ────────────────────────────────────────
            "cp_j1850ifrctrl" => Some(PARAM_J1850_IFR_CTRL),

            // ── SAE J2534-2 clause 10 Analog Inputs acquisition rate (service-level, ADR-178) ──
            "cp_analogsamplerate" => Some(PARAM_ANALOG_SAMPLE_RATE),
            // ── SAE J2534-2 clause 10 Analog Inputs remaining parameters (service-level, ADR-216) ──
            "cp_analogactivechannels" => Some(PARAM_ANALOG_ACTIVE_CHANNELS),
            "cp_analogsamplesperreading" => Some(PARAM_ANALOG_SAMPLES_PER_READING),
            "cp_analogreadingspermsg" => Some(PARAM_ANALOG_READINGS_PER_MSG),
            "cp_analogaveragingmethod" => Some(PARAM_ANALOG_AVERAGING_METHOD),
            "cp_analogsampleresolution" => Some(PARAM_ANALOG_SAMPLE_RESOLUTION),
            "cp_analoginputrangelow" => Some(PARAM_ANALOG_INPUT_RANGE_LOW),
            "cp_analoginputrangehigh" => Some(PARAM_ANALOG_INPUT_RANGE_HIGH),

            // ── SAE J2534-2 clause 24 Ethernet_NDIS pin option (service-level, ADR-194) ──
            "cp_ndispinoption" => Some(PARAM_NDIS_PIN_OPTION),

            _ => None,
        }
    }

    /// Resolves the table rows matching `resource_data.protocol`, one of:
    /// - `protocol_id`: matched against `row.protocol.value()` first (the
    ///   opaque `ChannelProtocol` value, which for a table row is usually a
    ///   raw/extended service-level ID); if no row matches by value at all,
    ///   falls back to matching a row's effective hardware protocol ID --
    ///   `row.hw_protocol_override.unwrap_or_else(|| row.protocol.
    ///   j2534_protocol_id())` -- so both a legacy raw J2534 protocol ID
    ///   (e.g. `6` for ISO15765) and a raw SW/FT `_PS` id (e.g.
    ///   `PROTOCOL_FT_CAN_PS`, which lives only in `hw_protocol_override` for
    ///   every SW/FT resource row, ADR-164/ADR-168) still resolve. Mirrors
    ///   `legacy_bustype_hw_id`'s identical fallback shape below.
    /// - `protocol_name`: matched case-insensitively against `row.protocol_name`
    ///   or `row.config_name` first; if no row matches by name at all, the name
    ///   is resolved to a `ChannelProtocol` via `map_protocol_name` (or a numeric
    ///   parse) and rows are matched the same value-first/hardware-ID-fallback
    ///   way. An unrecognized name is an error, matching prior behavior.
    fn candidates_matching_protocol(
        resource_data: &vci_service_interface::ResourceData,
        candidates: &mut Vec<&resources::ResourceDef>,
    ) -> Result<(), Status> {
        let effective_hw_id = |row: &resources::ResourceDef| {
            row.hw_protocol_override
                .unwrap_or_else(|| row.protocol.j2534_protocol_id())
        };
        match &resource_data.protocol {
            Some(vci_service_interface::resource_data::Protocol::ProtocolId(id)) => {
                let id = *id;
                if resources::resource_table()
                    .iter()
                    .any(|row| row.protocol.value() == id)
                {
                    candidates.retain(|row| row.protocol.value() == id);
                } else {
                    candidates.retain(|row| effective_hw_id(row) == id);
                }
            }
            Some(vci_service_interface::resource_data::Protocol::ProtocolName(name)) => {
                let matches_table_name = |row: &&resources::ResourceDef| {
                    row.protocol_name.eq_ignore_ascii_case(name)
                        || row
                            .config_name
                            .is_some_and(|c| c.eq_ignore_ascii_case(name))
                };
                if resources::resource_table()
                    .iter()
                    .any(|row| matches_table_name(&row))
                {
                    candidates.retain(|row| matches_table_name(row));
                } else {
                    let proto = if let Some(proto) = Self::map_protocol_name(name) {
                        proto
                    } else if let Ok(id) = name.parse::<u32>() {
                        ChannelProtocol::from_raw(id)
                    } else {
                        return Err(Status::invalid_argument(
                            "resource_data.protocol_name is not recognized",
                        ));
                    };
                    if resources::resource_table()
                        .iter()
                        .any(|row| row.protocol == proto)
                    {
                        candidates.retain(|row| row.protocol == proto);
                    } else {
                        let hw_id = proto.j2534_protocol_id();
                        candidates.retain(|row| effective_hw_id(row) == hw_id);
                    }
                }
            }
            None => {}
        }
        Ok(())
    }

    /// A row's effective *fixed* J2534 connect-protocol ID, for the legacy
    /// hw-id-based bus-type fallback below only -- `None` when the row has
    /// no single fixed connect protocol to compare a legacy hw id against at
    /// all (verification-pass fix, P2).
    ///
    /// `None` for every row on the `SAE_J1850` auto-detect bus
    /// (`resources::BUSTYPE_SAE_J1850`, `0x0307`): `row.protocol.
    /// j2534_protocol_id()` for these rows (`SAE_J2190_ON_SAE_J1850`/
    /// `ISO_15031_5_ON_SAE_J1850`) is only the VPW *initial probe candidate*
    /// (ADR-070) -- the row has no fixed connect protocol at all, so a
    /// legacy hw-id bus alias/numeric id (e.g. `"j1850_vpw"` ->
    /// `map_bustype_name` -> `J1850VPW`) must never match it. These rows are
    /// only selectable via their own bus name (`"SAE_J1850"`), its
    /// `bus_type_id` (`0x0307`), a `protocol` selector, or a resource ID --
    /// never a legacy hw-id bus alias, which by construction cannot express
    /// "the auto-detecting bus."
    ///
    /// `Some(hw_protocol_override)` for a row that has one (the four
    /// `SAE_J2610_on_SAE_J2610_SCI` rows, `0x021E`-`0x0221`): their shared
    /// `ChannelProtocol`'s own `j2534_protocol_id()` is `SCI_MODE`, which is
    /// no longer used as a connect protocol by any table row (ADR-069's
    /// pin-typing amendment) -- `hw_protocol_override` is each row's actual,
    /// fixed `PassThruConnect` protocol, so that is what a legacy hw id must
    /// compare against for these rows to be found by it at all (e.g. a
    /// legacy numeric bus_type_id of `SCI_A_ENGINE`'s hw id now matches both
    /// `0x0222` *and* `0x021E`, since both actually connect with
    /// `SCI_A_ENGINE`). `Some(protocol.j2534_protocol_id())` for every other
    /// row (unaffected by this fix).
    fn legacy_bustype_hw_id(row: &resources::ResourceDef) -> Option<u32> {
        if row.bus_type_id == resources::BUSTYPE_SAE_J1850 {
            None
        } else {
            Some(
                row.hw_protocol_override
                    .unwrap_or_else(|| row.protocol.j2534_protocol_id()),
            )
        }
    }

    /// Canonicalizes a legacy DW-CAN-family alias spelling to the specific
    /// table row's own `bus_type_name`, for the aliases `map_bustype_name`
    /// maps onto its generic `CAN` hardware ID even though the physical bus
    /// they name has its own dedicated table row and `bus_type_id` distinct
    /// from that of the other rows sharing the same hw id (Codex review
    /// finding, PR #64: `GetObjectId(OBJT_BUSTYPE, "iso-11898-3-dwftcan")`
    /// was resolving to the first CAN-hw-id row in table order --
    /// `ISO_11898_2_DWCAN`'s `0x0301` -- instead of `ISO_11898_3_DWFTCAN`'s
    /// own `0x030A`, because the generic hw id can't distinguish the two).
    /// `None` for every other bustype alias, including CAN-family spellings
    /// with no dedicated table row of their own (`ISO_11992_1_DWCAN`,
    /// `SAE_J1939_11_DWCAN`), which correctly keep resolving through the
    /// lossy hw-id path below since there is no more specific row to prefer.
    fn canonical_bustype_name_for_alias(name: &str) -> Option<&'static str> {
        match name.to_ascii_lowercase().as_str() {
            "iso_11898_2_dwcan" | "iso-11898-2-dwcan" | "iso11898_2_dwcan" => {
                Some("ISO_11898_2_DWCAN")
            }
            "iso_11898_3_dwftcan" | "iso-11898-3-dwftcan" | "iso11898_3_dwftcan" => {
                Some("ISO_11898_3_DWFTCAN")
            }
            _ => None,
        }
    }

    /// Resolves the table rows matching `resource_data.bus_type`, one of:
    /// - `bus_type_id`: matched against `row.bus_type_id` first; if no row
    ///   matches at all, falls back to the legacy J2534-hardware-protocol-ID
    ///   interpretation (`legacy_bustype_hw_id(row) == Some(id)`).
    /// - `bus_type_name`: matched case-insensitively against `row.bus_type_name`
    ///   first; if no row matches by name, falls back to `map_bustype_name`
    ///   (or a numeric parse) resolving to a J2534 hardware protocol ID, then
    ///   matches `legacy_bustype_hw_id(row) == Some(hw_id)`. An unrecognized
    ///   name is an error, matching prior behavior.
    ///
    /// `legacy_bustype_hw_id` excludes the `SAE_J1850` auto-detect bus's rows
    /// from ever matching through either fallback (verification-pass fix,
    /// P2) -- see its doc comment for why, and for the SCI
    /// `hw_protocol_override` rows' analogous adjustment.
    fn candidates_matching_bustype(
        resource_data: &vci_service_interface::ResourceData,
        candidates: &mut Vec<&resources::ResourceDef>,
    ) -> Result<(), Status> {
        match &resource_data.bus_type {
            Some(vci_service_interface::resource_data::BusType::BusTypeId(id)) => {
                let id = *id;
                if resources::resource_table()
                    .iter()
                    .any(|row| row.bus_type_id == id)
                {
                    candidates.retain(|row| row.bus_type_id == id);
                } else {
                    candidates.retain(|row| Self::legacy_bustype_hw_id(row) == Some(id));
                }
            }
            Some(vci_service_interface::resource_data::BusType::BusTypeName(name)) => {
                if resources::resource_table()
                    .iter()
                    .any(|row| row.bus_type_name.eq_ignore_ascii_case(name))
                {
                    candidates.retain(|row| row.bus_type_name.eq_ignore_ascii_case(name));
                } else if let Some(canonical) = Self::canonical_bustype_name_for_alias(name) {
                    // Same fix as `resolve_object_id`'s `ObjtBustype` arm
                    // below, applied here too: without it, an alias
                    // spelling that doesn't exactly match its row's
                    // `bus_type_name` (e.g. "iso-11898-3-dwftcan") would
                    // fall to the generic hw-id fallback and return every
                    // row sharing that raw hw id -- both the DWCAN and
                    // DWFTCAN families -- instead of just the one the
                    // caller actually asked for.
                    candidates.retain(|row| row.bus_type_name.eq_ignore_ascii_case(canonical));
                } else {
                    let hw_id = if let Some(id) = Self::map_bustype_name(name) {
                        id
                    } else if let Ok(id) = name.parse::<u32>() {
                        id
                    } else {
                        return Err(Status::invalid_argument(
                            "resource_data.bus_type_name is not recognized",
                        ));
                    };
                    candidates.retain(|row| Self::legacy_bustype_hw_id(row) == Some(hw_id));
                }
            }
            None => {}
        }
        Ok(())
    }

    /// Resolves `GetResourceIds`' selector fields to the matching table
    /// resource IDs, in table order: `protocol`/`bus_type` narrow first
    /// (`candidates_matching_protocol`/`candidates_matching_bustype`), then
    /// each `dlc_pin_data` entry narrows further by typed pin
    /// (`retain_rows_matching_pin` -- row-based: a row must contain the
    /// requested pin number, matching its declared type when one is also
    /// supplied; a pin type with no usable number matches any row having a
    /// pin of that type at all). An unrecognized pin-type name is
    /// `invalid_argument`; a recognized selector that narrows to zero rows
    /// is simply an empty result, not an error.
    pub(super) fn resolve_resource_ids_from_data(
        resource_data: &vci_service_interface::ResourceData,
    ) -> Result<Vec<u32>, Status> {
        let mut candidates: Vec<&resources::ResourceDef> =
            resources::resource_table().iter().collect();

        Self::candidates_matching_protocol(resource_data, &mut candidates)?;
        Self::candidates_matching_bustype(resource_data, &mut candidates)?;

        Self::retain_rows_matching_all_pins(&mut candidates, &resource_data.dlc_pin_data)?;

        Ok(candidates.into_iter().map(|row| row.resource_id).collect())
    }

    /// Unrecognized shortnames are rejected with `PDU_ERR_INVALID_PARAMETERS`
    /// (ADR-078) rather than silently resolving to object ID `0`; the prior
    /// `shortname.parse::<u32>().unwrap_or(0)`-style fallback is removed for
    /// every object type except `ObjtProtocol`, whose own numeric fallback is
    /// a legitimate raw `ChannelProtocol` value, not an arbitrary passthrough.
    pub(super) fn resolve_object_id(
        object_type: vci_service_interface::ObjectType,
        shortname: &str,
    ) -> Result<u32, Status> {
        let object_id = match object_type {
            // ADR-069: resolves through the resources table first, exactly
            // like the ObjtResource arm below -- a table-only protocol_name
            // (e.g. an ISO 22900-2 canonical name with no legacy
            // map_protocol_name alias) returns the table row's own protocol
            // value instead of failing or falling through to the numeric
            // parse. Unlike ObjtResource, this uses find_protocol_for_name,
            // not find_table_row_by_name: a protocol-identity query only
            // cares about ChannelProtocol, so rows sharing one protocol but
            // differing in hw_protocol_override (e.g. the four
            // "SAE_J2610_on_SAE_J2610_SCI" configurations) resolve
            // unambiguously here, unlike ObjtResource's stricter check
            // (Codex-review fix, PR #115 -- reusing find_table_row_by_name
            // directly regressed this previously-working name).
            vci_service_interface::ObjectType::ObjtProtocol => {
                if let Some(proto) = Self::find_protocol_for_name(shortname)? {
                    proto.value()
                } else if let Some(proto) = Self::map_protocol_name(shortname) {
                    proto.value()
                } else {
                    shortname
                        .parse::<u32>()
                        .map_err(|_| Status::not_found("unknown protocol shortname"))?
                }
            }
            // ADR-069: resolves through the resources table's bus_type_name
            // first, same convention as ObjtResource/ObjtProtocol. Unlike
            // protocol_name (which can map to several distinct
            // ChannelProtocols, hence find_table_row_by_name's ambiguity
            // handling), bus_type_name is 1:1 with bus_type_id for every row
            // by construction, so find_bustype_id_by_name's first match is
            // authoritative -- no ambiguity handling needed here.
            vci_service_interface::ObjectType::ObjtBustype => {
                if let Some(id) = Self::find_bustype_id_by_name(shortname) {
                    id
                } else if let Some(canonical) = Self::canonical_bustype_name_for_alias(shortname) {
                    // Codex review finding, PR #64 (round 6): a hw-id-based
                    // fallback can't distinguish DWCAN from DWFTCAN -- both
                    // share `map_bustype_name`'s generic `CAN` id, but have
                    // distinct table rows and `bus_type_id`s. Resolve these
                    // aliases to their own specific row's exact name instead
                    // of the raw hw id, before ever falling into the lossy
                    // hw-id path below. `canonical_bustype_name_for_alias`'s
                    // own doc comment explains which aliases need this and
                    // why; `find_bustype_id_by_name` is guaranteed to find
                    // the row since these canonical names are table
                    // constants copied verbatim from `resources.rs`.
                    Self::find_bustype_id_by_name(canonical).unwrap_or_else(|| {
                        unreachable!(
                            "canonical_bustype_name_for_alias returned a name with no matching table row: {canonical}"
                        )
                    })
                } else if let Some(id) = Self::map_bustype_name(shortname) {
                    // `map_bustype_name` resolves a legacy hw-id alias (e.g.
                    // "j1708_uart" -> `PROTOCOL_J1708_PS`), but that raw hw id
                    // lives in a different numeric space than a table row's
                    // own opaque `bus_type_id` (e.g. `BUSTYPE_SAE_J1708`) --
                    // returning it directly here would make a non-canonical
                    // alias ("sae-j1708-uart") resolve to a different object
                    // id than the canonical exact name ("SAE_J1708_UART")
                    // does just above, even though both name the same
                    // physical bus. Preferring the first table row whose
                    // `legacy_bustype_hw_id` matches this hw id -- and
                    // returning THAT row's own `bus_type_id` -- normalizes
                    // every alias onto the same canonical id its exact name
                    // would produce. This is safe only for hw ids where every
                    // row that shares it also shares one `bus_type_name`/
                    // `bus_type_id` (e.g. `BUSTYPE_SAE_J1708`, `SCI_MODE`) --
                    // the `canonical_bustype_name_for_alias` branch above
                    // handles the DW-CAN-family hw id specifically because
                    // that invariant does NOT hold for it (Codex review
                    // finding, PR #64, round 6). Falls back to the raw
                    // legacy hw id only when no table row corresponds to it
                    // at all (a hw id with no modern table representation).
                    resources::resource_table()
                        .iter()
                        .find(|row| Self::legacy_bustype_hw_id(row) == Some(id))
                        .map_or(id, |row| row.bus_type_id)
                } else if let Some(proto) = Self::map_protocol_name(shortname) {
                    // Bus type queries use the J2534 hardware ID (physical level).
                    proto.j2534_protocol_id()
                } else {
                    return Err(Status::invalid_argument(format!(
                        "PDU_ERR_INVALID_PARAMETERS: unrecognized bus type shortname {shortname:?}"
                    )));
                }
            }
            // ADR-079: resolves through `map_ioctl_name`'s private
            // `PDU_IOCTL_BASE` namespace first (the 25 D-PDU IOCTL commands
            // this adapter implements, ADR-164 Decision 3/Phase 4 having
            // added 2, ADR-165/Phase 12 having added 3 more, Phase 13
            // having added 1 more, and ADR-176/Phase 14 having added 2
            // more); a name matching none of them is
            // rejected with `PDU_ERR_INVALID_PARAMETERS` (ADR-078), same as
            // before `map_ioctl_name` existed -- this only narrows what
            // ADR-078 called "no name table at all" to "a name table that
            // does not cover this particular shortname."
            vci_service_interface::ObjectType::ObjtIoCtrl => {
                Self::map_ioctl_name(shortname).ok_or_else(|| {
                    Status::invalid_argument(format!(
                        "PDU_ERR_INVALID_PARAMETERS: unrecognized IO control shortname {shortname:?}"
                    ))
                })?
            }
            vci_service_interface::ObjectType::ObjtComparam => {
                Self::map_comparam_name(shortname)
                    .map(|id| id.0)
                    .ok_or_else(|| {
                        Status::invalid_argument(format!(
                            "PDU_ERR_INVALID_PARAMETERS: unrecognized ComParam shortname {shortname:?}"
                        ))
                    })?
            }
            vci_service_interface::ObjectType::ObjtPintype => {
                Self::map_pintype_name(shortname).ok_or_else(|| {
                    Status::invalid_argument(format!(
                        "PDU_ERR_INVALID_PARAMETERS: unrecognized pin type shortname {shortname:?}"
                    ))
                })?
            }
            // ADR-069: resolves through the resources table first, exactly
            // like `CreateComLogicalLink`'s resource-name resolution and
            // `GetResourceStatus`'s response echo (`find_table_row_by_name`,
            // `find_resource_id_for_protocol`) -- so a table-only name (e.g.
            // `"ISO_OBD_on_K_Line"`) returns its resource ID (`0x0213`)
            // instead of `0`, and a name resolvable only via the legacy
            // `map_protocol_name` (e.g. `"ISO15765"`) returns the table
            // resource ID that carries that protocol (`0x0206`) instead of
            // the raw protocol value. An ambiguous name (`"SAE_J2610_SCI"`,
            // matching multiple rows with different `ChannelProtocol`s) is
            // rejected with `invalid_argument` naming the available
            // configurations/resource IDs, via the same
            // `find_table_row_by_name` `CreateComLogicalLink` uses --
            // `GetObjectId` has no other way to signal "which one did you
            // mean" than an error, and this keeps the two RPCs consistent.
            // An unrecognized name is rejected with
            // `PDU_ERR_INVALID_PARAMETERS` (ADR-078), superseding the
            // numeric-fallback/`0` behavior this ADR originally documented.
            vci_service_interface::ObjectType::ObjtResource => {
                if let Some(row) = Self::find_table_row_by_name(shortname, &[])? {
                    row.resource_id
                } else if let Some(proto) = Self::map_protocol_name(shortname) {
                    resources::find_resource_id_for_protocol(proto).unwrap_or(proto.value())
                } else {
                    return Err(Status::invalid_argument(format!(
                        "PDU_ERR_INVALID_PARAMETERS: unrecognized resource shortname {shortname:?}"
                    )));
                }
            }
            vci_service_interface::ObjectType::ObjtUnspecified => {
                return Err(Status::invalid_argument(
                    "object_type must be a concrete object type",
                ));
            }
        };

        Ok(object_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{ComParamId, J2534Service, PARAM_P3_FUNC, PARAM_P3_PHYS, resources};

    #[test]
    fn map_comparam_name_supports_expected_shortnames() {
        let cases: &[(&str, ComParamId)] = &[
            // Baud rate
            ("CP_Baudrate", ComParamId(j2534_0404::DATA_RATE)),
            // Physical / link
            ("CP_Loopback", ComParamId(j2534_0404::LOOPBACK)),
            ("CP_Node_Address", ComParamId(j2534_0404::NODE_ADDRESS)),
            ("CP_NetworkLine", ComParamId(j2534_0404::NETWORK_LINE)),
            // Tester source address (alias for NODE_ADDRESS)
            (
                "CP_TesterSourceAddress",
                ComParamId(j2534_0404::NODE_ADDRESS),
            ),
            // KWP P-timers
            ("CP_P1Min", ComParamId(j2534_0404::P1_MIN)),
            ("CP_P1Max", ComParamId(j2534_0404::P1_MAX)),
            ("CP_P2Min", ComParamId(j2534_0404::P2_MIN)),
            ("CP_P2Max", ComParamId(j2534_0404::P2_MAX)),
            ("CP_P3Min", ComParamId(j2534_0404::P3_MIN)),
            ("CP_P3Phys", PARAM_P3_PHYS),
            ("CP_P3Func", PARAM_P3_FUNC),
            ("CP_P3Max_Ecu", ComParamId(j2534_0404::P3_MAX)),
            ("CP_P4Min", ComParamId(j2534_0404::P4_MIN)),
            ("CP_P4Max", ComParamId(j2534_0404::P4_MAX)),
            // ISO9141 W-timers -- ADR-181: only the side with an actual
            // native register resolves to the native `ComParamId`; the side
            // with none resolves to its own project-minted, store-only id
            // (`super::PARAM_W1_MIN`/etc., see the "W min/max" block below).
            ("CP_W1Max", ComParamId(j2534_0404::W1)),
            ("CP_W2Max", ComParamId(j2534_0404::W2)),
            ("CP_W3Max", ComParamId(j2534_0404::W3)),
            ("CP_W4Min", ComParamId(j2534_0404::W4)),
            ("CP_W5Max", ComParamId(j2534_0404::W5)),
            // ISO9141 bus-state timers
            ("CP_TIdle", ComParamId(j2534_0404::TIDLE)),
            ("CP_TInil", ComParamId(j2534_0404::TINIL)),
            ("CP_TWup", ComParamId(j2534_0404::TWUP)),
            // Physical layer
            ("CP_Parity", ComParamId(j2534_0404::PARITY)),
            (
                "CP_BitSamplePoint",
                ComParamId(j2534_0404::BIT_SAMPLE_POINT),
            ),
            ("CP_SyncJumpWidth", ComParamId(j2534_0404::SYNC_JUMP_WIDTH)),
            // ISO9141 W0
            ("CP_W0Max", ComParamId(j2534_0404::W0)),
            // SCI T-timers
            ("CP_T1Max", ComParamId(j2534_0404::T1_MAX)),
            ("CP_T2Max", ComParamId(j2534_0404::T2_MAX)),
            ("CP_T3Max", ComParamId(j2534_0404::T3_MAX)),
            ("CP_T4Max", ComParamId(j2534_0404::T4_MAX)),
            ("CP_T5Max", ComParamId(j2534_0404::T5_MAX)),
            // ISO15765 flow-control
            ("CP_StMin", ComParamId(j2534_0404::ISO15765_STMIN)),
            ("CP_BlockSize", ComParamId(j2534_0404::ISO15765_BS)),
            (
                "CP_CanMaxNumWaitFrames",
                ComParamId(j2534_0404::ISO15765_WFT_MAX),
            ),
            // ISO15765 Tx-side overrides (standard names from D-PDU API mapping table)
            ("CP_BlockSizeOverride", ComParamId(j2534_0404::BS_TX)),
            ("CP_StMinOverride", ComParamId(j2534_0404::STMIN_TX)),
            // Raw aliases
            ("CP_BsTx", ComParamId(j2534_0404::BS_TX)),
            ("CP_StMinTx", ComParamId(j2534_0404::STMIN_TX)),
            // 5-baud init
            ("CP_5BaudMode", ComParamId(j2534_0404::FIVE_BAUD_MOD)),
            // UART data bits
            ("CP_UartConfig", ComParamId(j2534_0404::DATA_BITS)),
            // ── W min/max sides with no native J2534 register (ADR-181) ──────────────────
            ("CP_W1Min", super::PARAM_W1_MIN),
            ("CP_W2Min", super::PARAM_W2_MIN),
            ("CP_W3Min", super::PARAM_W3_MIN),
            ("CP_W4Max", super::PARAM_W4_MAX),
            // ── Transport layer: ISO 15765-2 frame timing ────────────────────────────────
            ("CP_Ar", super::PARAM_N_AR),
            ("CP_Ar_Ecu", super::PARAM_N_AR_ECU),
            ("CP_As", super::PARAM_N_AS),
            ("CP_As_Ecu", super::PARAM_N_AS_ECU),
            ("CP_Br", super::PARAM_N_BR),
            ("CP_Br_Ecu", super::PARAM_N_BR_ECU),
            ("CP_Bs", super::PARAM_N_BS),
            ("CP_Bs_Ecu", super::PARAM_N_BS_ECU),
            ("CP_Cr", super::PARAM_N_CR),
            ("CP_Cr_Ecu", super::PARAM_N_CR_ECU),
            ("CP_Cs", super::PARAM_N_CS),
            ("CP_Cs_Ecu", super::PARAM_N_CS_ECU),
            ("CP_StMin_Ecu", super::PARAM_ST_MIN_ECU),
            ("CP_BlockSize_Ecu", super::PARAM_BLOCK_SIZE_ECU),
            ("CP_AccessTiming_Ecu", super::PARAM_ACCESS_TIMING_ECU),
            (
                "CP_AccessTimingOverride",
                super::PARAM_ACCESS_TIMING_OVERRIDE,
            ),
            ("CP_ExtendedTiming", super::PARAM_EXTENDED_TIMING),
            (
                "CP_J1939AddrClaimTimeout",
                super::PARAM_J1939_ADDR_CLAIM_TIMEOUT,
            ),
            (
                "CP_EscapeSequenceHandling",
                super::PARAM_ESCAPE_SEQUENCE_HANDLING,
            ),
            ("CP_MaxDataLength_Ecu", super::PARAM_MAX_DATA_LENGTH_ECU),
            (
                "CP_RepeatReqCountTrans",
                super::PARAM_REPEAT_REQ_COUNT_TRANS,
            ),
            // ── Transport layer: checksum handling ───────────────────────────────────────
            (
                "CP_DisableTransportChecksumCheck",
                super::PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK,
            ),
            ("CP_IgnoreChecksum", super::PARAM_IGNORE_CHECKSUM),
            // ── Transport layer: CAN addressing ──────────────────────────────────────────
            ("CP_CanPhysReqExtAddr", super::PARAM_CAN_PHYS_REQ_EXT_ADDR),
            ("CP_CanPhysReqFormat", super::PARAM_CAN_PHYS_REQ_FORMAT),
            ("CP_CanPhysReqId", super::PARAM_CAN_PHYS_REQ_ID),
            ("CP_CanRespUSDTExtAddr", super::PARAM_CAN_RESP_USDT_EXT_ADDR),
            ("CP_CanRespUSDTFormat", super::PARAM_CAN_RESP_USDT_FORMAT),
            ("CP_CanRespUSDTId", super::PARAM_CAN_RESP_USDT_ID),
            ("CP_CanRespUUDTExtAddr", super::PARAM_CAN_RESP_UUDT_EXT_ADDR),
            ("CP_CanRespUUDTFormat", super::PARAM_CAN_RESP_UUDT_FORMAT),
            ("CP_CanRespUUDTId", super::PARAM_CAN_RESP_UUDT_ID),
            ("CP_CanFuncReqExtAddr", super::PARAM_CAN_FUNC_REQ_EXT_ADDR),
            ("CP_CanFuncReqFormat", super::PARAM_CAN_FUNC_REQ_FORMAT),
            ("CP_CanFuncReqId", super::PARAM_CAN_FUNC_REQ_ID),
            ("CP_CanDataSizeOffset", super::PARAM_CAN_DATA_SIZE_OFFSET),
            ("CP_CanFillerByte", super::PARAM_CAN_FILLER_BYTE),
            (
                "CP_CanFillerByteHandling",
                super::PARAM_CAN_FILLER_BYTE_HANDLING,
            ),
            (
                "CP_CanFirstConsecutiveFrameValue",
                super::PARAM_CAN_FIRST_CF_VALUE,
            ),
            ("CP_CanMixedFormat", super::PARAM_CAN_MIXED_FORMAT),
            // ── Transport layer: ECU addressing / COM ────────────────────────────────────
            ("CP_EcuRespSourceAddress", super::PARAM_ECU_RESP_SOURCE_ADDR),
            // Deprecated misspelled alias kept for backward compatibility
            ("CP_EcuRespSoureAddress", super::PARAM_ECU_RESP_SOURCE_ADDR),
            (
                "CP_FuncReqFormatPriorityType",
                super::PARAM_FUNC_REQ_FORMAT_PRIORITY,
            ),
            ("CP_FuncReqTargetAddr", super::PARAM_FUNC_REQ_TARGET_ADDR),
            (
                "CP_FuncRespFormatPriorityType",
                super::PARAM_FUNC_RESP_FORMAT_PRIORITY,
            ),
            ("CP_FuncRespTargetAddr", super::PARAM_FUNC_RESP_TARGET_ADDR),
            (
                "CP_PhysReqFormatPriorityType",
                super::PARAM_PHYS_REQ_FORMAT_PRIORITY,
            ),
            ("CP_PhysReqTargetAddr", super::PARAM_PHYS_REQ_TARGET_ADDR),
            (
                "CP_PhysRespFormatPriorityType",
                super::PARAM_PHYS_RESP_FORMAT_PRIORITY,
            ),
            ("CP_RequestAddrMode", super::PARAM_REQUEST_ADDR_MODE),
            ("CP_HeaderFormatJ1850", super::PARAM_HEADER_FORMAT_J1850),
            ("CP_HeaderFormatKW", super::PARAM_HEADER_FORMAT_KW),
            ("CP_EnableConcatenation", super::PARAM_ENABLE_CONCATENATION),
            ("CP_FillerByte", super::PARAM_FILLER_BYTE),
            ("CP_FillerByteHandling", super::PARAM_FILLER_BYTE_HANDLING),
            ("CP_FillerByteLength", super::PARAM_FILLER_BYTE_LENGTH),
            ("CP_5BaudAddressFunc", super::PARAM_5BAUD_ADDR_FUNC),
            ("CP_5BaudAddressPhys", super::PARAM_5BAUD_ADDR_PHYS),
            (
                "CP_5BaudCommBaudrateOverride",
                super::PARAM_5BAUD_COMM_BAUDRATE_OVERRIDE,
            ),
            ("CP_5BaudInitBaudrate", super::PARAM_5BAUD_INIT_BAUDRATE),
            ("CP_SendRemoteFrame", super::PARAM_SEND_REMOTE_FRAME),
            ("CP_TPConnectionManagement", super::PARAM_TP_CONNECTION_MGMT),
            ("CP_MessagePriority", super::PARAM_MESSAGE_PRIORITY),
            ("CP_MidReqId", super::PARAM_MID_REQ_ID),
            ("CP_MidRespId", super::PARAM_MID_RESP_ID),
            (
                "CP_J1939AddressNegotiationRule",
                super::PARAM_J1939_ADDR_NEG_RULE,
            ),
            ("CP_J1939DataPage", super::PARAM_J1939_DATA_PAGE),
            ("CP_J1939MaxPacketTx", super::PARAM_J1939_MAX_PACKET_TX),
            ("CP_J1939PDUFormat", super::PARAM_J1939_PDU_FORMAT),
            ("CP_J1939PDUSpecific", super::PARAM_J1939_PDU_SPECIFIC),
            ("CP_J1939SourceAddress", super::PARAM_J1939_SOURCE_ADDRESS),
            ("CP_J1939TargetAddress", super::PARAM_J1939_TARGET_ADDRESS),
            // ── Transport layer: INIT ─────────────────────────────────────────────────────
            ("CP_InitializationSettings", super::PARAM_INIT_SETTINGS),
            (
                "CP_EnableInitSeqRepetition",
                super::PARAM_ENABLE_INIT_SEQ_REPETITION,
            ),
            (
                "CP_NumHeaderBytesStartCommKW",
                super::PARAM_NUM_HEADER_BYTES_START_COMM_KW,
            ),
            ("CP_ISOKeybyteCount", super::PARAM_ISO_KEYBYTE_COUNT),
            ("CP_SCITransmitMode", super::PARAM_SCI_TRANSMIT_MODE),
            ("CP_SCISetProgVoltage", super::PARAM_SCI_SET_PROG_VOLTAGE),
            ("CP_SCIEcuSimulator", super::PARAM_SCI_ECU_SIMULATOR),
            (
                "CP_J1939PreferredAddress",
                super::PARAM_J1939_PREFERRED_ADDRESS,
            ),
            (
                "CP_J1939PreferredAddress_Ecu",
                super::PARAM_J1939_PREFERRED_ADDRESS_ECU,
            ),
            // ── Transport layer: J1939/J1587 TP session control ──────────────────────────
            ("CP_MaxCTSReq", super::PARAM_MAX_CTS_REQ),
            ("CP_CollisionTestMode", super::PARAM_COLLISION_TEST_MODE),
            // ── Transport layer: J1939 NAME (Bytefield) ───────────────────────────────────
            ("CP_J1939Name", super::PARAM_J1939_NAME),
            ("CP_J1939Name_Ecu", super::PARAM_J1939_NAME_ECU),
            ("CP_J1939SourceName", super::PARAM_J1939_SOURCE_NAME),
            ("CP_J1939TargetName", super::PARAM_J1939_TARGET_NAME),
            // ── Physical layer (service-level) ────────────────────────────────────────────
            ("CP_BitSamplePoint_Ecu", super::PARAM_BIT_SAMPLE_POINT_ECU),
            ("CP_SamplesPerBit", super::PARAM_SAMPLES_PER_BIT),
            ("CP_SamplesPerBit_Ecu", super::PARAM_SAMPLES_PER_BIT_ECU),
            ("CP_SyncJumpWidth_Ecu", super::PARAM_SYNC_JUMP_WIDTH_ECU),
            ("CP_ListenOnly", super::PARAM_LISTEN_ONLY),
            ("CP_CanBaudrateRecord", super::PARAM_CAN_BAUDRATE_RECORD),
            ("CP_K_L_LineInit", super::PARAM_K_L_LINE_INIT),
            ("CP_K_LinePullup", super::PARAM_K_LINE_PULLUP),
            ("CP_TerminationType", super::PARAM_TERMINATION_TYPE),
            ("CP_TerminationType_Ecu", super::PARAM_TERMINATION_TYPE_ECU),
            // ── CAN FD physical layer (service-level) ─────────────────────────────────────
            ("CP_CANFDBaudrate", super::PARAM_CANFD_BAUDRATE),
            (
                "CP_CANFDBitSamplePoint",
                super::PARAM_CANFD_BIT_SAMPLE_POINT,
            ),
            ("CP_CANFDSyncJumpWidth", super::PARAM_CANFD_SYNC_JUMP_WIDTH),
            (
                "CP_CANFDTxMaxDataLength",
                super::PARAM_CANFD_TX_MAX_DATA_LENGTH,
            ),
            // ── J1850 IFR control (service-level) ────────────────────────────────────────
            ("CP_J1850IFRCtrl", super::PARAM_J1850_IFR_CTRL),
            ("CP_AnalogSampleRate", super::PARAM_ANALOG_SAMPLE_RATE),
            (
                "CP_AnalogActiveChannels",
                super::PARAM_ANALOG_ACTIVE_CHANNELS,
            ),
            (
                "CP_AnalogSamplesPerReading",
                super::PARAM_ANALOG_SAMPLES_PER_READING,
            ),
            (
                "CP_AnalogReadingsPerMsg",
                super::PARAM_ANALOG_READINGS_PER_MSG,
            ),
            (
                "CP_AnalogAveragingMethod",
                super::PARAM_ANALOG_AVERAGING_METHOD,
            ),
            (
                "CP_AnalogSampleResolution",
                super::PARAM_ANALOG_SAMPLE_RESOLUTION,
            ),
            (
                "CP_AnalogInputRangeLow",
                super::PARAM_ANALOG_INPUT_RANGE_LOW,
            ),
            (
                "CP_AnalogInputRangeHigh",
                super::PARAM_ANALOG_INPUT_RANGE_HIGH,
            ),
            ("CP_UebT0Min", super::PARAM_UEB_T0_MIN),
            ("CP_UebT1Max", super::PARAM_UEB_T1_MAX),
            ("CP_UebT2Max", super::PARAM_UEB_T2_MAX),
            ("CP_UebT3Max", super::PARAM_UEB_T3_MAX),
            ("CP_UebT4Min", super::PARAM_UEB_T4_MIN),
            ("CP_UebT5Max", super::PARAM_UEB_T5_MAX),
            ("CP_UebT6Max", super::PARAM_UEB_T6_MAX),
            ("CP_UebT7Min", super::PARAM_UEB_T7_MIN),
            ("CP_UebT7Max", super::PARAM_UEB_T7_MAX),
            ("CP_UebT9Min", super::PARAM_UEB_T9_MIN),
            // ── Application layer / service params ────────────────────────────────────────
            // Tester Present
            ("CP_TesterPresentMessage", super::PARAM_TESTER_PRESENT_MSG),
            (
                "CP_TesterPresentTime",
                super::PARAM_TESTER_PRESENT_INTERVAL_US,
            ),
            (
                "CP_TesterPresentAddrMode",
                super::PARAM_TESTER_PRESENT_ADDR_MODE,
            ),
            (
                "CP_TesterPresentExpPosResp",
                super::PARAM_TESTER_PRESENT_EXP_POS_RESP,
            ),
            (
                "CP_TesterPresentExpNegResp",
                super::PARAM_TESTER_PRESENT_EXP_NEG_RESP,
            ),
            (
                "CP_TesterPresentHandling",
                super::PARAM_TESTER_PRESENT_HANDLING,
            ),
            (
                "CP_TesterPresentReqRsp",
                super::PARAM_TESTER_PRESENT_REQ_RSP,
            ),
            (
                "CP_TesterPresentSendType",
                super::PARAM_TESTER_PRESENT_SEND_TYPE,
            ),
            (
                "CP_TesterPresentTime_Ecu",
                super::PARAM_TESTER_PRESENT_TIME_ECU,
            ),
            ("CP_TesterPresentImmed", super::PARAM_TESTER_PRESENT_IMMED),
            // Timing
            ("CP_CyclicRespTimeout", super::PARAM_CYCLIC_RESP_TIMEOUT),
            ("CP_P2Star", super::PARAM_P2_STAR),
            ("CP_P2Star_Ecu", super::PARAM_P2_STAR_ECU),
            ("CP_P2Max_Ecu", super::PARAM_P2_MAX_ECU),
            ("CP_ModifyTiming", super::PARAM_MODIFY_TIMING),
            ("CP_SessionTiming_Ecu", super::PARAM_SESSION_TIMING_ECU),
            (
                "CP_SessionTimingOverride",
                super::PARAM_SESSION_TIMING_OVERRIDE,
            ),
            ("CP_CanTransmissionTime", super::PARAM_CAN_TRANSMISSION_TIME),
            (
                "CP_MessageIndicationRate",
                super::PARAM_MESSAGE_INDICATION_RATE,
            ),
            ("CP_ChangeSpeedTxDelay", super::PARAM_CHANGE_SPEED_TX_DELAY),
            // Error handling
            (
                "CP_RC21CompletionTimeout",
                super::PARAM_RC21_COMPLETION_TIMEOUT,
            ),
            ("CP_RC21Handling", super::PARAM_RC21_HANDLING),
            ("CP_RC21RequestTime", super::PARAM_RC21_REQUEST_TIME),
            (
                "CP_RC23CompletionTimeout",
                super::PARAM_RC23_COMPLETION_TIMEOUT,
            ),
            ("CP_RC23Handling", super::PARAM_RC23_HANDLING),
            ("CP_RC23RequestTime", super::PARAM_RC23_REQUEST_TIME),
            (
                "CP_RC78CompletionTimeout",
                super::PARAM_RC78_COMPLETION_TIMEOUT,
            ),
            ("CP_RC78Handling", super::PARAM_RC78_HANDLING),
            ("CP_RCByteOffset", super::PARAM_RC_BYTE_OFFSET),
            ("CP_RepeatReqCountApp", super::PARAM_REPEAT_REQ_COUNT_APP),
            (
                "CP_SuspendQueueOnError",
                super::PARAM_SUSPEND_QUEUE_ON_ERROR,
            ),
            // COM
            ("CP_ChangeSpeedCtrl", super::PARAM_CHANGE_SPEED_CTRL),
            ("CP_ChangeSpeedMessage", super::PARAM_CHANGE_SPEED_MSG),
            ("CP_ChangeSpeedRate", super::PARAM_CHANGE_SPEED_RATE),
            ("CP_ChangeSpeedResCtrl", super::PARAM_CHANGE_SPEED_RES_CTRL),
            (
                "CP_EnablePerformanceTest",
                super::PARAM_ENABLE_PERFORMANCE_TEST,
            ),
            ("CP_StartMsgIndEnable", super::PARAM_START_MSG_IND_ENABLE),
            ("CP_TransmitIndEnable", super::PARAM_TRANSMIT_IND_ENABLE),
            ("CP_SwCan_HighVoltage", super::PARAM_SW_CAN_HIGH_VOLTAGE),
            ("CP_SwCanHighVoltage", super::PARAM_SW_CAN_HIGH_VOLTAGE),
            // Protocol behavior
            ("CP_TestMode", super::PARAM_TEST_MODE),
            // Raw CONFIG_* name aliases
            ("DATA_RATE", ComParamId(j2534_0404::DATA_RATE)),
            ("LOOPBACK", ComParamId(j2534_0404::LOOPBACK)),
            ("NODE_ADDRESS", ComParamId(j2534_0404::NODE_ADDRESS)),
            ("NETWORK_LINE", ComParamId(j2534_0404::NETWORK_LINE)),
            ("P1_MIN", ComParamId(j2534_0404::P1_MIN)),
            ("P1_MAX", ComParamId(j2534_0404::P1_MAX)),
            ("P2_MIN", ComParamId(j2534_0404::P2_MIN)),
            ("P2_MAX", ComParamId(j2534_0404::P2_MAX)),
            ("P3_MIN", ComParamId(j2534_0404::P3_MIN)),
            ("P3_MAX", ComParamId(j2534_0404::P3_MAX)),
            ("P4_MIN", ComParamId(j2534_0404::P4_MIN)),
            ("P4_MAX", ComParamId(j2534_0404::P4_MAX)),
            ("W0", ComParamId(j2534_0404::W0)),
            ("W1", ComParamId(j2534_0404::W1)),
            ("W2", ComParamId(j2534_0404::W2)),
            ("W3", ComParamId(j2534_0404::W3)),
            ("W4", ComParamId(j2534_0404::W4)),
            ("W5", ComParamId(j2534_0404::W5)),
            ("TIDLE", ComParamId(j2534_0404::TIDLE)),
            ("TINIL", ComParamId(j2534_0404::TINIL)),
            ("TWUP", ComParamId(j2534_0404::TWUP)),
            ("PARITY", ComParamId(j2534_0404::PARITY)),
            ("BIT_SAMPLE_POINT", ComParamId(j2534_0404::BIT_SAMPLE_POINT)),
            ("SYNC_JUMP_WIDTH", ComParamId(j2534_0404::SYNC_JUMP_WIDTH)),
            ("T1_MAX", ComParamId(j2534_0404::T1_MAX)),
            ("T2_MAX", ComParamId(j2534_0404::T2_MAX)),
            ("T3_MAX", ComParamId(j2534_0404::T3_MAX)),
            ("T4_MAX", ComParamId(j2534_0404::T4_MAX)),
            ("T5_MAX", ComParamId(j2534_0404::T5_MAX)),
            ("ISO15765_STMIN", ComParamId(j2534_0404::ISO15765_STMIN)),
            ("ISO15765_BS", ComParamId(j2534_0404::ISO15765_BS)),
            ("ISO15765_WFT_MAX", ComParamId(j2534_0404::ISO15765_WFT_MAX)),
            ("BS_TX", ComParamId(j2534_0404::BS_TX)),
            ("STMIN_TX", ComParamId(j2534_0404::STMIN_TX)),
            ("FIVE_BAUD_MOD", ComParamId(j2534_0404::FIVE_BAUD_MOD)),
            ("DATA_BITS", ComParamId(j2534_0404::DATA_BITS)),
        ];
        for (name, expected) in cases {
            assert_eq!(
                J2534Service::map_comparam_name(name),
                Some(*expected),
                "mapping mismatch for {name}"
            );
        }
    }

    #[test]
    fn map_comparam_name_is_case_insensitive() {
        assert_eq!(
            J2534Service::map_comparam_name("cp_stmin"),
            Some(ComParamId(j2534_0404::ISO15765_STMIN))
        );
        assert_eq!(
            J2534Service::map_comparam_name("Cp_P3FuNc"),
            Some(PARAM_P3_FUNC)
        );
    }

    #[test]
    fn map_comparam_name_returns_none_for_unknown_name() {
        assert_eq!(J2534Service::map_comparam_name("CP_DoesNotExist"), None);
    }

    /// `CP_P3Phys`/`CP_P3Func` always resolve to the CAN-context
    /// service-level `PARAM_P3_PHYS`/`PARAM_P3_FUNC`, regardless of the
    /// protocol they end up being used on -- name-to-id mapping is
    /// CLL-independent. KWP's own P3 timer has a distinct name, `CP_P3Min`,
    /// and is not an alias of `CP_P3Phys`/`CP_P3Func`. Whether
    /// `PARAM_P3_PHYS`/`PARAM_P3_FUNC` is actually usable on a given CLL is
    /// a separate, protocol-dependent question answered by
    /// `comparam_support::check_param_allowed`.
    #[test]
    fn map_comparam_name_resolves_p3_phys_func_independent_of_protocol() {
        assert_eq!(
            J2534Service::map_comparam_name("cp_p3phys"),
            Some(PARAM_P3_PHYS)
        );
        assert_eq!(
            J2534Service::map_comparam_name("CP_P3Func"),
            Some(PARAM_P3_FUNC)
        );
        assert_eq!(
            J2534Service::map_comparam_name("cp_p3min"),
            Some(ComParamId(j2534_0404::P3_MIN))
        );
    }

    #[test]
    fn resolve_object_id_maps_objt_comparam_shortnames_and_aliases() {
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtComparam,
                "CP_StMin",
            )
            .unwrap(),
            j2534_0404::ISO15765_STMIN
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtComparam,
                "ISO15765_BS",
            )
            .unwrap(),
            j2534_0404::ISO15765_BS
        );
    }

    #[test]
    fn resolve_object_id_objt_comparam_rejects_unrecognized_shortname() {
        // ADR-078: the numeric-string fallback is removed, so a shortname
        // that parses as a plain number but isn't a known ComParam name no
        // longer round-trips to that number.
        let status =
            J2534Service::resolve_object_id(vci_service_interface::ObjectType::ObjtComparam, "31")
                .expect_err("a numeric shortname absent from the name table should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("PDU_ERR_INVALID_PARAMETERS"));

        let status = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtComparam,
            "CP_DoesNotExist",
        )
        .expect_err("an unrecognized ComParam shortname should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("PDU_ERR_INVALID_PARAMETERS"));
    }

    #[test]
    fn map_protocol_name_supports_canonical_and_alias_names() {
        use super::ChannelProtocol;
        let cases: &[(&str, ChannelProtocol)] = &[
            ("can", ChannelProtocol::CAN),
            ("iso11898", ChannelProtocol::CAN),
            ("iso-11898", ChannelProtocol::CAN),
            ("iso_11898", ChannelProtocol::CAN),
            ("iso11898-1", ChannelProtocol::CAN),
            ("iso-11898-1", ChannelProtocol::CAN),
            ("iso_11898_1", ChannelProtocol::CAN),
            ("can_iso", ChannelProtocol::CAN),
            ("can-iso", ChannelProtocol::CAN),
            ("iso_11898_raw", ChannelProtocol::CAN),
            (
                "iso_11783_12_on_iso_11783_5",
                ChannelProtocol::ISO_11783_12_ON_ISO_11783_5,
            ),
            ("iso15765", ChannelProtocol::ISO15765),
            ("iso-15765", ChannelProtocol::ISO15765),
            ("iso_15765", ChannelProtocol::ISO15765),
            ("iso15765-2", ChannelProtocol::ISO15765),
            ("iso-15765-2", ChannelProtocol::ISO15765),
            ("iso_15765_2", ChannelProtocol::ISO15765),
            ("iso15765-4", ChannelProtocol::ISO15765),
            ("iso-15765-4", ChannelProtocol::ISO15765),
            ("iso_15765_4", ChannelProtocol::ISO15765),
            ("iso-tp", ChannelProtocol::ISO15765),
            ("isotp", ChannelProtocol::ISO15765),
            ("iso_tp", ChannelProtocol::ISO15765),
            (
                "iso_14230_3_on_iso_15765_2",
                ChannelProtocol::ISO_14230_3_ON_ISO_15765_2,
            ),
            (
                "iso_15765_3_on_iso_15765_2",
                ChannelProtocol::ISO_15765_3_ON_ISO_15765_2,
            ),
            (
                "iso_14229_3_on_iso_15765_2",
                ChannelProtocol::ISO_14229_3_ON_ISO_15765_2,
            ),
            (
                "sae_j2190_on_iso_15765_2",
                ChannelProtocol::SAE_J2190_ON_ISO_15765_2,
            ),
            (
                "iso_15031_5_on_iso_15765_4",
                ChannelProtocol::ISO_15031_5_ON_ISO_15765_4,
            ),
            (
                "iso_14229_3_on_iso_15765_2_with_iso_11783_5",
                ChannelProtocol::ISO_14229_3_ON_ISO_15765_2_WITH_ISO_11783_5,
            ),
            ("iso9141", ChannelProtocol::ISO9141),
            ("iso-9141", ChannelProtocol::ISO9141),
            ("iso_9141", ChannelProtocol::ISO9141),
            ("iso9141-2", ChannelProtocol::ISO9141),
            ("iso-9141-2", ChannelProtocol::ISO9141),
            ("iso_9141_2", ChannelProtocol::ISO9141),
            ("kwp", ChannelProtocol::ISO9141),
            ("kwp1", ChannelProtocol::ISO9141),
            (
                "sae_j2190_on_iso_9141_2",
                ChannelProtocol::SAE_J2190_ON_ISO_9141_2,
            ),
            (
                "iso_15031_5_on_iso_9141_2",
                ChannelProtocol::ISO_15031_5_ON_ISO_9141_2,
            ),
            ("iso14230", ChannelProtocol::ISO14230),
            ("iso-14230", ChannelProtocol::ISO14230),
            ("iso_14230", ChannelProtocol::ISO14230),
            ("iso14230-1", ChannelProtocol::ISO14230),
            ("iso-14230-1", ChannelProtocol::ISO14230),
            ("iso_14230_1", ChannelProtocol::ISO14230),
            ("kwp2000", ChannelProtocol::ISO14230),
            ("kwp_2000", ChannelProtocol::ISO14230),
            ("kwp-2000", ChannelProtocol::ISO14230),
            ("kwp2", ChannelProtocol::ISO14230),
            (
                "iso_14230_3_on_iso_14230_2",
                ChannelProtocol::ISO_14230_3_ON_ISO_14230_2,
            ),
            (
                "sae_j2190_on_iso_14230_2",
                ChannelProtocol::SAE_J2190_ON_ISO_14230_2,
            ),
            (
                "iso_15031_5_on_iso_14230_4",
                ChannelProtocol::ISO_15031_5_ON_ISO_14230_4,
            ),
            ("j1850pwm", ChannelProtocol::J1850PWM),
            ("j1850_pwm", ChannelProtocol::J1850PWM),
            ("j1850-pwm", ChannelProtocol::J1850PWM),
            ("pwm", ChannelProtocol::J1850PWM),
            (
                "sae_j2190_on_sae_j1850_pwm",
                ChannelProtocol::SAE_J2190_ON_SAE_J1850_PWM,
            ),
            (
                "iso_15031_5_on_sae_j1850_pwm",
                ChannelProtocol::ISO_15031_5_ON_SAE_J1850_PWM,
            ),
            ("j1850vpw", ChannelProtocol::J1850VPW),
            ("j1850_vpw", ChannelProtocol::J1850VPW),
            ("j1850-vpw", ChannelProtocol::J1850VPW),
            ("vpw", ChannelProtocol::J1850VPW),
            (
                "sae_j2190_on_sae_j1850_vpw",
                ChannelProtocol::SAE_J2190_ON_SAE_J1850_VPW,
            ),
            (
                "iso_15031_5_on_sae_j1850_vpw",
                ChannelProtocol::ISO_15031_5_ON_SAE_J1850_VPW,
            ),
            ("sci_a_engine", ChannelProtocol::SCI_A_ENGINE),
            ("sci_a", ChannelProtocol::SCI_A_ENGINE),
            ("sciaengine", ChannelProtocol::SCI_A_ENGINE),
            ("scia_engine", ChannelProtocol::SCI_A_ENGINE),
            ("sci_a_trans", ChannelProtocol::SCI_A_TRANS),
            ("sci_a_transmission", ChannelProtocol::SCI_A_TRANS),
            ("sciatrans", ChannelProtocol::SCI_A_TRANS),
            ("scia_trans", ChannelProtocol::SCI_A_TRANS),
            ("scia_transmission", ChannelProtocol::SCI_A_TRANS),
            ("sci_b_engine", ChannelProtocol::SCI_B_ENGINE),
            ("sci_b", ChannelProtocol::SCI_B_ENGINE),
            ("scibengine", ChannelProtocol::SCI_B_ENGINE),
            ("scib_engine", ChannelProtocol::SCI_B_ENGINE),
            ("sci_b_trans", ChannelProtocol::SCI_B_TRANS),
            ("sci_b_transmission", ChannelProtocol::SCI_B_TRANS),
            ("scibtrans", ChannelProtocol::SCI_B_TRANS),
            ("scib_trans", ChannelProtocol::SCI_B_TRANS),
            ("scib_transmission", ChannelProtocol::SCI_B_TRANS),
            ("sci_mode", ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI),
            ("sci", ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI),
            (
                "sci_mode_select",
                ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI,
            ),
            (
                "sae_j2610_on_sae_j2610_sci",
                ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI,
            ),
            ("uart_echo_byte", ChannelProtocol::UART_ECHO_BYTE_PS),
            ("uart-echo-byte", ChannelProtocol::UART_ECHO_BYTE_PS),
            ("uart_echo_byte_ps", ChannelProtocol::UART_ECHO_BYTE_PS),
            ("honda_diagh", ChannelProtocol::HONDA_DIAGH_PS),
            ("honda-diagh", ChannelProtocol::HONDA_DIAGH_PS),
            ("honda_diagh_ps", ChannelProtocol::HONDA_DIAGH_PS),
            // ADR-175/Phase 11 (Codex review, PR #64): mirrors the identical
            // UART Echo Byte/Honda DIAG-H cases above.
            ("j1708", ChannelProtocol::J1708_PS),
            ("j1708_ps", ChannelProtocol::J1708_PS),
            // ADR-194/Phase 16: mirrors the identical UART Echo Byte/Honda
            // DIAG-H/J1708 cases above.
            ("ethernet_ndis", ChannelProtocol::ETHERNET_NDIS),
            ("ethernet-ndis", ChannelProtocol::ETHERNET_NDIS),
        ];
        for (name, expected) in cases {
            assert_eq!(
                J2534Service::map_protocol_name(name),
                Some(*expected),
                "protocol mapping mismatch for {}",
                name
            );
        }
    }

    #[test]
    fn map_protocol_name_is_case_insensitive() {
        use super::ChannelProtocol;
        assert_eq!(
            J2534Service::map_protocol_name("CAN"),
            Some(ChannelProtocol::CAN)
        );
        assert_eq!(
            J2534Service::map_protocol_name("Iso15765"),
            Some(ChannelProtocol::ISO15765)
        );
        assert_eq!(
            J2534Service::map_protocol_name("ISO-TP"),
            Some(ChannelProtocol::ISO15765)
        );
        assert_eq!(
            J2534Service::map_protocol_name("KWP2000"),
            Some(ChannelProtocol::ISO14230)
        );
        assert_eq!(
            J2534Service::map_protocol_name("J1850VPW"),
            Some(ChannelProtocol::J1850VPW)
        );
    }

    #[test]
    fn map_protocol_name_supports_iso22900_standard_names() {
        use super::ChannelProtocol;
        assert_eq!(
            J2534Service::map_protocol_name("ISO11898-1"),
            Some(ChannelProtocol::CAN)
        );
        assert_eq!(
            J2534Service::map_protocol_name("ISO-11898-1"),
            Some(ChannelProtocol::CAN)
        );
        assert_eq!(
            J2534Service::map_protocol_name("ISO15765-2"),
            Some(ChannelProtocol::ISO15765)
        );
        assert_eq!(
            J2534Service::map_protocol_name("ISO-15765-2"),
            Some(ChannelProtocol::ISO15765)
        );
        assert_eq!(
            J2534Service::map_protocol_name("ISO9141-2"),
            Some(ChannelProtocol::ISO9141)
        );
        assert_eq!(
            J2534Service::map_protocol_name("ISO-9141-2"),
            Some(ChannelProtocol::ISO9141)
        );
        assert_eq!(
            J2534Service::map_protocol_name("ISO14230-1"),
            Some(ChannelProtocol::ISO14230)
        );
        assert_eq!(
            J2534Service::map_protocol_name("ISO-14230-1"),
            Some(ChannelProtocol::ISO14230)
        );
    }

    #[test]
    fn map_protocol_name_returns_none_for_unknown_name() {
        assert_eq!(J2534Service::map_protocol_name("PROTOCOL_UNKNOWN"), None);
        assert_eq!(J2534Service::map_protocol_name(""), None);
        assert_eq!(J2534Service::map_protocol_name("FOOBAR"), None);
    }

    #[test]
    fn resolve_object_id_maps_objt_protocol_shortnames_and_aliases() {
        assert_eq!(
            J2534Service::resolve_object_id(vci_service_interface::ObjectType::ObjtProtocol, "CAN")
                .unwrap(),
            j2534_0404::CAN
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtProtocol,
                "ISO15765"
            )
            .unwrap(),
            j2534_0404::ISO15765
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtProtocol,
                "ISO-TP"
            )
            .unwrap(),
            j2534_0404::ISO15765
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtProtocol,
                "KWP2000"
            )
            .unwrap(),
            j2534_0404::ISO14230
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtProtocol,
                "iso9141"
            )
            .unwrap(),
            j2534_0404::ISO9141
        );
    }

    #[test]
    fn resolve_object_id_objt_protocol_supports_numeric_fallback() {
        assert_eq!(
            J2534Service::resolve_object_id(vci_service_interface::ObjectType::ObjtProtocol, "5")
                .unwrap(),
            j2534_0404::CAN
        );
        assert_eq!(
            J2534Service::resolve_object_id(vci_service_interface::ObjectType::ObjtProtocol, "6")
                .unwrap(),
            j2534_0404::ISO15765
        );
        assert!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtProtocol,
                "unknown_protocol"
            )
            .is_err()
        );
    }

    #[test]
    fn map_bustype_name_supports_iso22900_physical_layer_names() {
        assert_eq!(
            J2534Service::map_bustype_name("ISO_11898_2_DWCAN"),
            Some(j2534_0404::CAN)
        );
        assert_eq!(
            J2534Service::map_bustype_name("iso-11898-2-dwcan"),
            Some(j2534_0404::CAN)
        );
        assert_eq!(
            J2534Service::map_bustype_name("ISO_11898_3_DWFTCAN"),
            Some(j2534_0404::CAN)
        );
        assert_eq!(
            J2534Service::map_bustype_name("iso_11992_1_dwcan"),
            Some(j2534_0404::CAN)
        );
        assert_eq!(J2534Service::map_bustype_name("ISO_9141_2_UART"), None);
        assert_eq!(J2534Service::map_bustype_name("iso-9141-2-uart"), None);
        assert_eq!(J2534Service::map_bustype_name("ISO_14230_1_UART"), None);
        assert_eq!(
            J2534Service::map_bustype_name("SAE_J1850_VPW"),
            Some(j2534_0404::J1850VPW)
        );
        assert_eq!(
            J2534Service::map_bustype_name("sae-j1850-vpw"),
            Some(j2534_0404::J1850VPW)
        );
        assert_eq!(
            J2534Service::map_bustype_name("SAE_J1850_PWM"),
            Some(j2534_0404::J1850PWM)
        );
        assert_eq!(
            J2534Service::map_bustype_name("j1850_pwm"),
            Some(j2534_0404::J1850PWM)
        );
        assert_eq!(
            J2534Service::map_bustype_name("SAE_J2610_UART"),
            Some(j2534_0404::SCI_MODE)
        );
        assert_eq!(
            J2534Service::map_bustype_name("sae_j2610_sci"),
            Some(j2534_0404::SCI_MODE)
        );
        // SAE J1939 (11-bit DW-CAN) uses the standard CAN hardware channel
        assert_eq!(
            J2534Service::map_bustype_name("SAE_J1939_11_DWCAN"),
            Some(j2534_0404::CAN)
        );
        assert_eq!(
            J2534Service::map_bustype_name("sae-j1939-11-dwcan"),
            Some(j2534_0404::CAN)
        );
        // SAE J1708 UART (heavy-duty truck) -- ADR-175/Phase 11: resolves to
        // its own native PROTOCOL_J1708_PS id, not ISO9141 (the pre-Phase-11
        // placeholder this alias used to map to, back when J1708 had no real
        // implementation).
        assert_eq!(
            J2534Service::map_bustype_name("SAE_J1708_UART"),
            Some(j2534_0404::PROTOCOL_J1708_PS)
        );
        assert_eq!(
            J2534Service::map_bustype_name("sae_j1708_uart"),
            Some(j2534_0404::PROTOCOL_J1708_PS)
        );
    }

    #[test]
    fn resolve_object_id_maps_objt_bustype_iso22900_names() {
        // "ISO_11898_2_DWCAN" and "SAE_J1850_VPW" are canonical resource-table
        // bus_type_names (A1-2 fix): they now resolve through the table
        // first, to its opaque bus_type_id, not the legacy J2534 hardware ID
        // map_bustype_name would have returned.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "ISO_11898_2_DWCAN"
            )
            .unwrap(),
            0x0301
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "SAE_J1850_VPW"
            )
            .unwrap(),
            0x0306
        );
        // "ISO9141" is not a table bus_type_name (the table only has
        // "ISO_9141_2_UART"), so it still falls through to the legacy
        // map_protocol_name/j2534_protocol_id() path, unchanged.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "ISO9141"
            )
            .unwrap(),
            j2534_0404::ISO9141
        );
        // ADR-078: the numeric-string fallback is removed, so a bare number
        // that isn't a recognized bus type/protocol name no longer
        // round-trips to that number.
        let status =
            J2534Service::resolve_object_id(vci_service_interface::ObjectType::ObjtBustype, "5")
                .expect_err("a numeric shortname absent from the name tables should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("PDU_ERR_INVALID_PARAMETERS"));
    }

    /// Regression test (Codex review, PR #64, follow-up on the
    /// `map_bustype_name` J1708 alias fix): a non-canonical bus-type alias
    /// must resolve to the SAME object id as the canonical exact name for
    /// the same physical bus -- `map_bustype_name` itself returns a legacy
    /// hw-id (a different numeric space than a table row's own opaque
    /// `bus_type_id`), so `resolve_object_id`'s `ObjtBustype` fallback must
    /// normalize it onto the matching row's `bus_type_id`, not return the
    /// raw hw id verbatim. Covers both the newly-fixed J1708 case and a
    /// pre-existing CAN-family alias, proving the fix is general (not a
    /// J1708-only special case) and doesn't regress an already-working name.
    #[test]
    fn resolve_object_id_normalizes_bustype_aliases_to_the_canonical_row_id() {
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "sae-j1708-uart"
            )
            .unwrap(),
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "SAE_J1708_UART"
            )
            .unwrap(),
            "the alias and the canonical exact name must resolve to the same bus-type object id"
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "j1708_uart"
            )
            .unwrap(),
            0x030D,
            "should resolve to BUSTYPE_SAE_J1708, not PROTOCOL_J1708_PS (0x800D) or ISO9141's hw id"
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "sae-j1939-11-dwcan"
            )
            .unwrap(),
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "ISO_11898_2_DWCAN"
            )
            .unwrap(),
            "a pre-existing CAN-family alias should likewise normalize to the shared dual-wire-CAN \
             bus_type_id, not CAN's own raw hw id -- proving this fix isn't J1708-specific"
        );
    }

    /// Regression test (Codex review, PR #64, round 6): DWCAN and DWFTCAN
    /// both map to `map_bustype_name`'s generic `CAN` hw id, but they are
    /// different physical buses with distinct table rows and `bus_type_id`s
    /// (`0x0301` vs `0x030A`). Before this fix, every DWFTCAN alias that
    /// didn't exactly match the row's own underscore-separated
    /// `bus_type_name` fell through to the hw-id fallback and silently
    /// resolved to DWCAN's id instead (the first CAN-hw-id row in table
    /// order) -- a real "these two named buses are actually the same
    /// object" bug, not just a missed alias.
    #[test]
    fn resolve_object_id_disambiguates_dwcan_from_dwftcan_aliases() {
        let dwcan_id = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtBustype,
            "ISO_11898_2_DWCAN",
        )
        .unwrap();
        let dwftcan_id = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtBustype,
            "ISO_11898_3_DWFTCAN",
        )
        .unwrap();
        assert_ne!(
            dwcan_id, dwftcan_id,
            "DWCAN and DWFTCAN are distinct physical buses with distinct bus_type_ids"
        );
        assert_eq!(dwftcan_id, 0x030A);

        for alias in [
            "iso-11898-3-dwftcan",
            "iso11898_3_dwftcan",
            "iso_11898_3_dwftcan",
        ] {
            assert_eq!(
                J2534Service::resolve_object_id(
                    vci_service_interface::ObjectType::ObjtBustype,
                    alias
                )
                .unwrap(),
                dwftcan_id,
                "alias {alias:?} must resolve to DWFTCAN's own bus_type_id, not DWCAN's"
            );
        }
        for alias in ["iso-11898-2-dwcan", "iso11898_2_dwcan"] {
            assert_eq!(
                J2534Service::resolve_object_id(
                    vci_service_interface::ObjectType::ObjtBustype,
                    alias
                )
                .unwrap(),
                dwcan_id,
                "alias {alias:?} must still resolve to DWCAN's own bus_type_id"
            );
        }
    }

    #[test]
    fn map_bustype_name_is_case_insensitive() {
        assert_eq!(
            J2534Service::map_bustype_name("iso_11898_2_dwcan"),
            Some(j2534_0404::CAN)
        );
        assert_eq!(
            J2534Service::map_bustype_name("ISO_11898_2_DWCAN"),
            Some(j2534_0404::CAN)
        );
        assert_eq!(
            J2534Service::map_bustype_name("ISO-11898-2-DWCAN"),
            Some(j2534_0404::CAN)
        );
        assert_eq!(
            J2534Service::map_bustype_name("SAE_J1850_VPW"),
            Some(j2534_0404::J1850VPW)
        );
        assert_eq!(
            J2534Service::map_bustype_name("sae_j1850_vpw"),
            Some(j2534_0404::J1850VPW)
        );
    }

    #[test]
    fn map_bustype_name_returns_none_for_unsupported_physical_layers() {
        // SAE J2411 SW-CAN is not supported by J2534-0404 (see ADR-017)
        assert_eq!(J2534Service::map_bustype_name("SAE_J2411_SWCAN"), None);
        assert_eq!(
            J2534Service::map_bustype_name("ISO_11783_12_on_ISO_11783_5"),
            None
        );
        assert_eq!(J2534Service::map_bustype_name("UNKNOWN_BUSTYPE"), None);
    }

    #[test]
    fn map_pintype_name_supports_iso22900_shortnames() {
        assert_eq!(J2534Service::map_pintype_name("HI"), Some(2000));
        assert_eq!(J2534Service::map_pintype_name("LOW"), Some(2001));
        assert_eq!(J2534Service::map_pintype_name("K"), Some(2002));
        assert_eq!(J2534Service::map_pintype_name("L"), Some(2003));
        assert_eq!(J2534Service::map_pintype_name("TX"), Some(2004));
        assert_eq!(J2534Service::map_pintype_name("RX"), Some(2005));
        assert_eq!(J2534Service::map_pintype_name("PLUS"), Some(2006));
        assert_eq!(J2534Service::map_pintype_name("MINUS"), Some(2007));
        assert_eq!(J2534Service::map_pintype_name("SINGLE"), Some(2008));
        assert_eq!(J2534Service::map_pintype_name("IGN"), Some(2009));
        assert_eq!(J2534Service::map_pintype_name("IGNITION_CLAMP"), Some(2009));
        assert_eq!(J2534Service::map_pintype_name("PROGV"), Some(2010));
    }

    /// ADR-079 (extended by ADR-164 Decision 3/Phase 4, ADR-165/Phase 12,
    /// Phase 13, ADR-176/Phase 14, ADR-189/Phase 8, and ADR-194/Phase 16):
    /// every one of the 28 `PDU_IOCTL_*` shortnames round-trips to
    /// `PDU_IOCTL_BASE + <its table offset>`, case-insensitively (matching
    /// this file's other `map_*_name` helpers), and an unrecognized name
    /// resolves to `None`.
    #[test]
    fn map_ioctl_name_resolves_all_twenty_eight_commands() {
        use super::super::PDU_IOCTL_BASE;

        let cases: &[(&str, u32)] = &[
            ("PDU_IOCTL_RESET", PDU_IOCTL_BASE + 0x01),
            ("PDU_IOCTL_CLEAR_TX_QUEUE", PDU_IOCTL_BASE + 0x02),
            ("PDU_IOCTL_SUSPEND_TX_QUEUE", PDU_IOCTL_BASE + 0x03),
            ("PDU_IOCTL_RESUME_TX_QUEUE", PDU_IOCTL_BASE + 0x04),
            ("PDU_IOCTL_CLEAR_RX_QUEUE", PDU_IOCTL_BASE + 0x05),
            ("PDU_IOCTL_READ_VBATT", PDU_IOCTL_BASE + 0x06),
            ("PDU_IOCTL_SET_PROG_VOLTAGE", PDU_IOCTL_BASE + 0x07),
            ("PDU_IOCTL_READ_PROG_VOLTAGE", PDU_IOCTL_BASE + 0x08),
            ("PDU_IOCTL_GENERIC", PDU_IOCTL_BASE + 0x09),
            ("PDU_IOCTL_SET_BUFFER_SIZE", PDU_IOCTL_BASE + 0x0A),
            ("PDU_IOCTL_START_MSG_FILTER", PDU_IOCTL_BASE + 0x0B),
            ("PDU_IOCTL_STOP_MSG_FILTER", PDU_IOCTL_BASE + 0x0C),
            ("PDU_IOCTL_CLEAR_MSG_FILTER", PDU_IOCTL_BASE + 0x0D),
            (
                "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES",
                PDU_IOCTL_BASE + 0x0E,
            ),
            ("PDU_IOCTL_GET_CABLE_ID", PDU_IOCTL_BASE + 0x0F),
            ("PDU_IOCTL_SEND_BREAK", PDU_IOCTL_BASE + 0x10),
            ("PDU_IOCTL_READ_IGNITION_SENSE_STATE", PDU_IOCTL_BASE + 0x11),
            ("PDU_IOCTL_SW_CAN_HS", PDU_IOCTL_BASE + 0x12),
            ("PDU_IOCTL_SW_CAN_NS", PDU_IOCTL_BASE + 0x13),
            ("PDU_IOCTL_START_REPEAT_MESSAGE", PDU_IOCTL_BASE + 0x14),
            ("PDU_IOCTL_QUERY_REPEAT_MESSAGE", PDU_IOCTL_BASE + 0x15),
            ("PDU_IOCTL_STOP_REPEAT_MESSAGE", PDU_IOCTL_BASE + 0x16),
            ("PDU_IOCTL_READ_J1962PIN_VOLTAGE", PDU_IOCTL_BASE + 0x17),
            ("PDU_IOCTL_GET_DEVICE_CONFIG", PDU_IOCTL_BASE + 0x18),
            ("PDU_IOCTL_SET_DEVICE_CONFIG", PDU_IOCTL_BASE + 0x19),
            ("PDU_IOCTL_SET_POLL_RESPONSE", PDU_IOCTL_BASE + 0x1A),
            ("PDU_IOCTL_BECOME_MASTER", PDU_IOCTL_BASE + 0x1B),
            ("PDU_IOCTL_GET_NDIS_ADAPTER_INFO", PDU_IOCTL_BASE + 0x1C),
        ];
        for &(name, expected) in cases {
            assert_eq!(J2534Service::map_ioctl_name(name), Some(expected), "{name}");
            // Case-insensitive, like map_pintype_name/map_comparam_name.
            assert_eq!(
                J2534Service::map_ioctl_name(&name.to_ascii_lowercase()),
                Some(expected),
                "{name} (lowercase)"
            );
        }
        assert_eq!(J2534Service::map_ioctl_name("PDU_IOCTL_UNKNOWN"), None);
    }

    #[test]
    fn resolve_object_id_maps_objt_pintype_shortnames() {
        assert_eq!(
            J2534Service::resolve_object_id(vci_service_interface::ObjectType::ObjtPintype, "HI")
                .unwrap(),
            2000
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtPintype,
                "ignition_clamp"
            )
            .unwrap(),
            2009
        );
        // ADR-078: the numeric-string fallback is removed, so a bare number
        // that isn't a recognized pin type name no longer round-trips to
        // that number.
        let status =
            J2534Service::resolve_object_id(vci_service_interface::ObjectType::ObjtPintype, "2010")
                .expect_err(
                    "a numeric shortname absent from the pin type table should be rejected",
                );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("PDU_ERR_INVALID_PARAMETERS"));
    }

    /// ADR-079: `OBJT_IO_CTRL` now resolves through `map_ioctl_name`'s private
    /// `PDU_IOCTL_BASE` namespace -- superseding ADR-078's "always rejects"
    /// pin for this one object type only. A name matching none of the 25
    /// commands still rejects with `PDU_ERR_INVALID_PARAMETERS`, including a
    /// bare numeric string (there is still no numeric fallback).
    #[test]
    fn resolve_object_id_objt_io_ctrl_resolves_known_names_and_rejects_others() {
        use super::super::PDU_IOCTL_BASE;

        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtIoCtrl,
                "PDU_IOCTL_RESET",
            )
            .unwrap(),
            PDU_IOCTL_BASE + 0x01
        );
        // Case-insensitive, matching every other map_*_name helper's convention.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtIoCtrl,
                "pdu_ioctl_read_ignition_sense_state",
            )
            .unwrap(),
            PDU_IOCTL_BASE + 0x11
        );

        let status =
            J2534Service::resolve_object_id(vci_service_interface::ObjectType::ObjtIoCtrl, "35")
                .expect_err("a shortname matching none of the 25 commands should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("PDU_ERR_INVALID_PARAMETERS"));
    }

    /// ADR-069: `GetObjectId(OBJT_RESOURCE, ...)` resolves through the
    /// resources table first, same as `CreateComLogicalLink`/
    /// `GetResourceStatus`.
    #[test]
    fn resolve_object_id_objt_resource_resolves_through_table_first() {
        // Table-only name (no legacy map_protocol_name alias at all):
        // previously returned 0.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtResource,
                "ISO_OBD_on_K_Line",
            )
            .unwrap(),
            0x0213
        );
        // Direct table protocol_name match: previously returned the raw
        // legacy protocol value (6) via map_protocol_name, not the table ID.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtResource,
                "ISO_15765_2",
            )
            .unwrap(),
            0x0206
        );
        // Direct table config_name match.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtResource,
                "SCI_B_TRANS",
            )
            .unwrap(),
            0x0225
        );
        // Legacy-only alias (not a table protocol_name/config_name) still
        // resolves, now via the table lookup for that protocol instead of
        // the raw legacy value.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtResource,
                "ISO15765",
            )
            .unwrap(),
            0x0206
        );
        // Ambiguous table name (matches 4 rows with different ChannelProtocols):
        // rejected, consistent with CreateComLogicalLink's own rejection.
        let status = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtResource,
            "SAE_J2610_SCI",
        )
        .expect_err("an ambiguous resource name should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("SCI_A_ENGINE"));
        // Spec correction: the four SAE_J2610_on_SAE_J2610_SCI rows share one
        // ChannelProtocol but differ in hw_protocol_override, so this name is
        // now ambiguous too (GetObjectId has no pin data to narrow with).
        let status = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtResource,
            "SAE_J2610_on_SAE_J2610_SCI",
        )
        .expect_err("an ambiguous resource name should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        // ADR-078: an unrecognized name is rejected with
        // PDU_ERR_INVALID_PARAMETERS, superseding the prior numeric-fallback/0
        // behavior.
        let status = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtResource,
            "not_a_resource",
        )
        .expect_err("an unrecognized resource shortname should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("PDU_ERR_INVALID_PARAMETERS"));
        // A bare number that isn't a known resource ID/legacy protocol also
        // no longer round-trips to that number.
        let status = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtResource,
            "9999",
        )
        .expect_err("an unmapped numeric resource shortname should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("PDU_ERR_INVALID_PARAMETERS"));
    }

    /// A1-2 fix (ADR-069 convention extended to `OBJT_PROTOCOL`):
    /// `GetObjectId(OBJT_PROTOCOL, ...)` resolves through the resources
    /// table first, same as `OBJT_RESOURCE`.
    #[test]
    fn resolve_object_id_objt_protocol_resolves_through_table_first() {
        use super::ChannelProtocol;
        // Table-only protocol_name with no legacy map_protocol_name alias at
        // all -- "sae_j1850_vpw" is not among map_protocol_name's J1850VPW
        // aliases ("j1850vpw"/"j1850_vpw"/"j1850-vpw"/"vpw"): previously this
        // name failed outright (not a number, so the numeric-parse fallback
        // rejected it).
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtProtocol,
                "SAE_J1850_VPW",
            )
            .unwrap(),
            j2534_0404::J1850VPW
        );
        // Case-insensitive, like the table-first ObjtResource arm.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtProtocol,
                "sae_j1850_vpw",
            )
            .unwrap(),
            j2534_0404::J1850VPW
        );
        // "SAE_J2610_SCI" matches all four SCI configuration rows
        // (0x0222-0x0225), which differ in `ChannelProtocol`
        // (SCI_A_ENGINE/SCI_A_TRANS/SCI_B_ENGINE/SCI_B_TRANS):
        // find_protocol_for_name rejects this as genuinely ambiguous (the
        // rows differ in protocol identity, not just hw_protocol_override),
        // so OBJT_PROTOCOL now has two distinct failure modes -- ambiguous
        // (this case) is invalid_argument, while unrecognized (below) is
        // still not_found.
        let status = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtProtocol,
            "SAE_J2610_SCI",
        )
        .expect_err("an ambiguous protocol shortname should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        // Codex-review regression fix (PR #115): the four
        // "SAE_J2610_on_SAE_J2610_SCI" rows (0x021E-0x0221) share one
        // ChannelProtocol (0x0160) and differ only in hw_protocol_override
        // (irrelevant to protocol identity), so this must resolve
        // unambiguously, unlike OBJT_RESOURCE's stricter
        // find_table_row_by_name (which would reject it, since it also
        // requires matching hw_protocol_override). This name previously
        // worked (via map_protocol_name) before this fix first shipped, and
        // reusing find_table_row_by_name directly briefly regressed it.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtProtocol,
                "SAE_J2610_on_SAE_J2610_SCI",
            )
            .unwrap(),
            ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI.value()
        );
        // A genuinely unrecognized name is unchanged by this fix: it still
        // falls through the table lookup and map_protocol_name to the
        // numeric-parse fallback, which rejects it with not_found (OBJT_PROTOCOL's
        // documented carve-out, distinct from every other resolve_object_id arm).
        let status = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtProtocol,
            "not_a_protocol_at_all",
        )
        .expect_err("an unrecognized protocol shortname should be rejected");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    /// A1-2 fix: `GetObjectId(OBJT_BUSTYPE, ...)` resolves through the
    /// resources table's `bus_type_name` first (via `find_bustype_id_by_name`),
    /// before falling back to the legacy `map_bustype_name`/`map_protocol_name`
    /// aliasing -- mirroring `OBJT_RESOURCE`'s ADR-069 table-first behavior.
    #[test]
    fn resolve_object_id_objt_bustype_resolves_through_table_first() {
        // Table bus_type_name that previously mismatched entirely: J2534's
        // SCI_MODE hardware ID (map_bustype_name's answer for
        // "SAE_J2610_UART") is not this table's opaque bus_type_id.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "SAE_J2610_UART",
            )
            .unwrap(),
            0x0308
        );
        // Table bus_type_names that previously hit PDU_ERR_INVALID_PARAMETERS:
        // map_bustype_name deliberately returns None for these (physical
        // layer only, transport protocol not determinable from the bus name
        // alone), and map_protocol_name has no alias for the bare bus name
        // either.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "ISO_9141_2_UART",
            )
            .unwrap(),
            0x0303
        );
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "ISO_14230_1_UART",
            )
            .unwrap(),
            0x0302
        );
        // The auto-detecting SAE_J1850 bus (ADR-070): previously failed
        // outright (no map_bustype_name/map_protocol_name alias covers the
        // bare "SAE_J1850" name), now resolves to its table bus_type_id.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "SAE_J1850",
            )
            .unwrap(),
            0x0307
        );
        // Case-insensitive, like find_bustype_id_by_name's other callers.
        assert_eq!(
            J2534Service::resolve_object_id(
                vci_service_interface::ObjectType::ObjtBustype,
                "sae_j1850",
            )
            .unwrap(),
            0x0307
        );
        // An unrecognized name is still rejected with
        // PDU_ERR_INVALID_PARAMETERS, unchanged.
        let status = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtBustype,
            "not_a_bustype",
        )
        .expect_err("an unrecognized bus type shortname should be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("PDU_ERR_INVALID_PARAMETERS"));
    }

    /// A1-2 fix: the `GetObjectId(OBJT_BUSTYPE) -> GetResourceIds` discovery
    /// round trip the audit finding is named after -- resolving a canonical
    /// bus_type_name to its table bus_type_id, then feeding that id back into
    /// `GetResourceIds`, now actually finds the rows it names.
    #[test]
    fn resolve_object_id_objt_bustype_round_trips_into_resolve_resource_ids() {
        let bus_type_id = J2534Service::resolve_object_id(
            vci_service_interface::ObjectType::ObjtBustype,
            "SAE_J2610_UART",
        )
        .unwrap();

        let resource_data = vci_service_interface::ResourceData {
            bus_type: Some(vci_service_interface::resource_data::BusType::BusTypeId(
                bus_type_id,
            )),
            ..Default::default()
        };
        let resource_ids = J2534Service::resolve_resource_ids_from_data(&resource_data).unwrap();
        assert!(
            !resource_ids.is_empty(),
            "expected at least one resource row for bus_type_id {bus_type_id:#06x}"
        );
    }

    #[test]
    fn map_pintype_name_returns_none_for_unknown_name() {
        assert_eq!(J2534Service::map_pintype_name("CAN_H"), None);
        assert_eq!(J2534Service::map_pintype_name(""), None);
        assert_eq!(J2534Service::map_pintype_name("UNKNOWN_PIN"), None);
    }

    #[test]
    fn resolve_resource_ids_from_data_filters_by_protocol_name() {
        let resource_data = vci_service_interface::ResourceData {
            dlc_pin_data: vec![],
            bus_type: None,
            protocol: Some(
                vci_service_interface::resource_data::Protocol::ProtocolName(
                    "ISO15765".to_string(),
                ),
            ),
        };
        // "ISO15765" is a legacy alias (via `map_protocol_name`), not a table
        // `protocol_name`; it resolves to `ChannelProtocol::ISO15765`, which
        // (ADR-164/Phase 4, extended by ADR-168/Phase 6) now matches three
        // table rows sharing that identity: the native dual-wire `ISO_15765_2`
        // row (0x0206), its SWCAN sibling `ISO_15765_2_SWCAN` (0x022B), and
        // its FTCAN sibling `ISO_15765_2_FTCAN` (0x0235, resources.rs).
        // `GetResourceIds` is a discovery/enumeration RPC (unlike
        // `CreateComLogicalLink`'s pick-exactly-one resolution) -- with no
        // `bus_type` selector narrowing the query, correctly listing every
        // distinct physical resource implementing this protocol is the
        // intended behavior, not a regression (the same "multiple matches"
        // shape this function already produces for e.g. `"SAE_J2610_SCI"`'s
        // four rows).
        assert_eq!(
            J2534Service::resolve_resource_ids_from_data(&resource_data).unwrap(),
            vec![0x0206, 0x022B, 0x0235]
        );
    }

    #[test]
    fn resolve_resource_ids_from_data_returns_empty_on_conflicting_selectors() {
        let resource_data = vci_service_interface::ResourceData {
            dlc_pin_data: vec![],
            bus_type: Some(vci_service_interface::resource_data::BusType::BusTypeName(
                "SAE_J1850_VPW".to_string(),
            )),
            protocol: Some(
                vci_service_interface::resource_data::Protocol::ProtocolName(
                    "ISO15765".to_string(),
                ),
            ),
        };
        assert!(
            J2534Service::resolve_resource_ids_from_data(&resource_data)
                .unwrap()
                .is_empty()
        );
    }

    /// Regression test (Codex review, PR #64): a non-canonical `bus_type_name`
    /// alias for SAE J1708 (`"sae-j1708-uart"`, hyphenated) does not exactly
    /// match row 0x023C's own `bus_type_name` ("SAE_J1708_UART"), so
    /// `candidates_matching_bustype` falls through to `map_bustype_name`'s
    /// legacy hw-id fallback. Before ADR-175/Phase 11's fix, that fallback
    /// still returned the pre-Phase-11 placeholder mapping to ISO9141,
    /// silently routing this alias to the wrong (or, since a bare bus-type
    /// selector alone still finds SOME row, an outright empty/incorrect)
    /// result instead of the new J1708 row. Also proves the canonical exact
    /// name ("SAE_J1708_UART") already resolves correctly via the table
    /// match that runs before this fallback is ever consulted.
    #[test]
    fn resolve_resource_ids_from_data_resolves_j1708_bustype_aliases_to_the_new_row() {
        for alias in ["SAE_J1708_UART", "sae-j1708-uart", "j1708_uart"] {
            let resource_data = vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: Some(vci_service_interface::resource_data::BusType::BusTypeName(
                    alias.to_string(),
                )),
                protocol: None,
            };
            assert_eq!(
                J2534Service::resolve_resource_ids_from_data(&resource_data).unwrap(),
                vec![0x023C],
                "bus_type_name alias {alias:?} should resolve to the new SAE J1708 row (0x023C)"
            );
        }
    }

    /// Regression test (Codex review, PR #64, round 6): `GetResourceIds`
    /// with a DWFTCAN alias that doesn't exactly match the table row's own
    /// underscore-separated `bus_type_name` (e.g. the hyphenated
    /// "iso-11898-3-dwftcan") used to fall through to the generic CAN hw-id
    /// fallback, which matches every row sharing that raw hw id -- both the
    /// ten DWCAN rows AND the ten DWFTCAN rows -- instead of just the
    /// DWFTCAN family the caller actually asked for.
    #[test]
    fn resolve_resource_ids_from_data_disambiguates_dwftcan_alias_from_dwcan_rows() {
        use super::resources;

        for alias in ["iso-11898-3-dwftcan", "iso11898_3_dwftcan"] {
            let resource_data = vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: Some(vci_service_interface::resource_data::BusType::BusTypeName(
                    alias.to_string(),
                )),
                protocol: None,
            };
            let ids = J2534Service::resolve_resource_ids_from_data(&resource_data).unwrap();
            assert!(
                !ids.is_empty(),
                "alias {alias:?} should resolve to at least the DWFTCAN rows"
            );
            for &id in &ids {
                let row = resources::resource_table()
                    .iter()
                    .find(|row| row.resource_id == id)
                    .unwrap();
                assert_eq!(
                    row.bus_type_name, "ISO_11898_3_DWFTCAN",
                    "alias {alias:?} must only match DWFTCAN rows, not DWCAN rows sharing its hw id"
                );
            }
        }
    }

    #[test]
    fn resolve_resource_ids_from_data_filters_by_pintype() {
        let resource_data = vci_service_interface::ResourceData {
            dlc_pin_data: vec![vci_service_interface::PinData {
                dlc_pin_number: 6,
                dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                    "HI".to_string(),
                )),
            }],
            bus_type: None,
            protocol: None,
        };
        let ids = J2534Service::resolve_resource_ids_from_data(&resource_data).unwrap();
        assert!(ids.contains(&0x0201)); // ISO_11898_RAW (CAN)
        assert!(ids.contains(&0x0206)); // ISO_15765_2
        assert!(!ids.contains(&0x0210)); // ISO_9141_2
    }

    #[test]
    fn resolve_resource_ids_from_data_rejects_invalid_pintype_name() {
        let resource_data = vci_service_interface::ResourceData {
            dlc_pin_data: vec![vci_service_interface::PinData {
                dlc_pin_number: 6,
                dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                    "NOT_A_PIN".to_string(),
                )),
            }],
            bus_type: None,
            protocol: None,
        };
        assert!(J2534Service::resolve_resource_ids_from_data(&resource_data).is_err());
    }

    #[test]
    fn resolve_resource_ids_from_data_filters_by_pin_number() {
        let resource_data = vci_service_interface::ResourceData {
            dlc_pin_data: vec![vci_service_interface::PinData {
                dlc_pin_number: 6,
                dlc_pin_type: None,
            }],
            bus_type: None,
            protocol: None,
        };
        let ids = J2534Service::resolve_resource_ids_from_data(&resource_data).unwrap();
        assert!(ids.contains(&0x0201)); // ISO_11898_RAW (CAN)
        assert!(ids.contains(&0x0206)); // ISO_15765_2
        assert!(!ids.contains(&0x0210)); // ISO_9141_2
    }

    #[test]
    fn resolve_resource_ids_from_data_returns_empty_when_pin_number_conflicts_with_protocol() {
        let resource_data = vci_service_interface::ResourceData {
            dlc_pin_data: vec![vci_service_interface::PinData {
                dlc_pin_number: 7,
                dlc_pin_type: None,
            }],
            bus_type: None,
            protocol: Some(
                vci_service_interface::resource_data::Protocol::ProtocolName(
                    "ISO15765".to_string(),
                ),
            ),
        };
        assert!(
            J2534Service::resolve_resource_ids_from_data(&resource_data)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn resolve_resource_ids_from_data_returns_empty_when_pintype_and_pin_number_conflict() {
        let resource_data = vci_service_interface::ResourceData {
            dlc_pin_data: vec![vci_service_interface::PinData {
                dlc_pin_number: 7,
                dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                    "HI".to_string(),
                )),
            }],
            bus_type: None,
            protocol: None,
        };
        assert!(
            J2534Service::resolve_resource_ids_from_data(&resource_data)
                .unwrap()
                .is_empty()
        );
    }

    // ── ADR-156 Decision 2: SAE J2534-2 clause 6 Pin Selection ──────────────

    fn pin(number: u32, type_name: &str) -> vci_service_interface::PinData {
        vci_service_interface::PinData {
            dlc_pin_number: number,
            dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                type_name.to_string(),
            )),
        }
    }

    fn untyped_pin(number: u32) -> vci_service_interface::PinData {
        vci_service_interface::PinData {
            dlc_pin_number: number,
            dlc_pin_type: None,
        }
    }

    #[test]
    fn resolve_pin_selection_is_a_noop_for_empty_or_default_pins() {
        use super::ChannelProtocol;

        assert_eq!(
            J2534Service::resolve_pin_selection(ChannelProtocol::CAN, None, &[], true, None)
                .unwrap(),
            None,
            "empty dlc_pin_data is always a no-op"
        );
        // CAN's default pins are 6 (HI) / 14 (LOW) -- supplying exactly those
        // (in either order, and regardless of opt-in) must not trigger Pin
        // Selection at all.
        assert_eq!(
            J2534Service::resolve_pin_selection(
                ChannelProtocol::CAN,
                None,
                &[pin(14, "LOW"), pin(6, "HI")],
                false,
                None,
            )
            .unwrap(),
            None,
            "pins matching the default set (any order) must not trigger Pin Selection"
        );
        // Same pin numbers as the default, but untyped -- clause 6.3.3.2's
        // auto-connect-to-default-pins behavior for a caller that supplies
        // bare pin numbers without typing them.
        assert_eq!(
            J2534Service::resolve_pin_selection(
                ChannelProtocol::CAN,
                None,
                &[untyped_pin(6), untyped_pin(14)],
                false,
                None,
            )
            .unwrap(),
            None,
            "untyped pins matching the default numbers must not trigger Pin Selection"
        );
    }

    /// Regression test: CAN's default pin *numbers* (6, 14) supplied with
    /// their primary/secondary roles swapped (6 typed `LOW`, 14 typed `HI`
    /// -- the opposite of CAN's actual default wiring) must NOT be treated
    /// as "default, no Pin Selection" just because the requested pin-number
    /// set matches the default pin-number set. A number-only-set comparison
    /// wrongly returns `Ok(None)` here; the fix resolves this to `CAN_PS`
    /// with pin 14 as primary/HI and pin 6 as secondary/LOW.
    #[test]
    fn resolve_pin_selection_detects_swapped_default_pin_roles() {
        use super::ChannelProtocol;

        let result = J2534Service::resolve_pin_selection(
            ChannelProtocol::CAN,
            None,
            &[pin(6, "LOW"), pin(14, "HI")],
            true,
            None,
        )
        .unwrap();
        assert_eq!(
            result,
            Some((j2534_0404::CAN, j2534_0404::PROTOCOL_CAN_PS, 0x0000_0E06)),
            "swapped pin roles on the default pin numbers must resolve to CAN_PS with the \
             caller's actual (swapped) polarity, not be silently treated as default wiring"
        );
    }

    /// Regression test: a `dlc_pin_data` with a duplicate default pin number
    /// (deduplicated numbers happen to equal the default set) must not be
    /// silently accepted as "default, no Pin Selection" -- it must fall
    /// through to `compute_pin_select`'s own validation, which rejects it as
    /// malformed (more than 2 pins here, since the duplicate isn't
    /// collapsed).
    #[test]
    fn resolve_pin_selection_rejects_duplicate_default_pin_numbers() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::CAN,
            None,
            &[untyped_pin(6), untyped_pin(6), untyped_pin(14)],
            true,
            None,
        )
        .expect_err(
            "a duplicate default pin number must not be silently accepted as default wiring",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// The spec's own clause 6.3.1 example: CAN on pins 3 (HI) and 11 (LOW)
    /// (a common medium-speed CAN wiring) differs from CAN's default (6/14),
    /// so an opted-in module resolves to `CAN_PS` with `pin_select =
    /// 0x0000030B`.
    #[test]
    fn resolve_pin_selection_computes_pin_select_for_an_in_scope_protocol() {
        use super::ChannelProtocol;

        let result = J2534Service::resolve_pin_selection(
            ChannelProtocol::CAN,
            None,
            &[pin(3, "HI"), pin(11, "LOW")],
            true,
            None,
        )
        .unwrap();
        assert_eq!(
            result,
            Some((j2534_0404::CAN, j2534_0404::PROTOCOL_CAN_PS, 0x0000_030B))
        );
    }

    /// Same non-default pins as above, but the module has not opted into
    /// J2534-2 (ADR-156 Decision 4/clause 5) -- rejected rather than
    /// silently honored or silently ignored.
    #[test]
    fn resolve_pin_selection_rejects_non_default_pins_when_not_opted_in() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::CAN,
            None,
            &[pin(3, "HI"), pin(11, "LOW")],
            false,
            None,
        )
        .expect_err("a non-opted-in module must reject a non-default pin request");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16, Codex review
    /// finding on PR #102): a non-opted-in module must be rejected even
    /// though a plain Ethernet_NDIS connect supplies no `dlc_pin_data` at
    /// all -- before this fix, `resolve_pin_selection` had no dedicated arm
    /// for this protocol, so it fell through to the generic tail's
    /// unconditional `Ok(None)`, letting a non-opted-in module's
    /// `PassThruConnect(PROTOCOL_ETHERNET_NDIS)` through with no opt-in
    /// check at all (the Discovery capability check is explicitly a no-op
    /// for a non-opted-in module too, so nothing else in the connect path
    /// would have caught this). Mirrors
    /// `resolve_pin_selection_rejects_non_default_pins_when_not_opted_in`'s
    /// CAN case above, and the Analog Inputs arm's identical structure this
    /// protocol's own arm was modeled on.
    #[test]
    fn resolve_pin_selection_rejects_ethernet_ndis_when_not_opted_in() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::ETHERNET_NDIS,
            None,
            &[],
            false,
            None,
        )
        .expect_err("a non-opted-in module must reject an Ethernet_NDIS resource");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// The same request, opted in, canonicalizes to `Ok(None)` -- clause 24
    /// has no pin concept at all, so there is never a real `pin_select` to
    /// compute.
    #[test]
    fn resolve_pin_selection_accepts_ethernet_ndis_with_no_pins_when_opted_in() {
        use super::ChannelProtocol;

        let result = J2534Service::resolve_pin_selection(
            ChannelProtocol::ETHERNET_NDIS,
            None,
            &[],
            true,
            None,
        )
        .expect("an opted-in module with no dlc_pin_data should be accepted");
        assert_eq!(result, None);
    }

    /// A protocol outside ADR-156 Decision 2's clause 6.3.1 Table 1 scope
    /// (here, a synthetic raw `ChannelProtocol` id with no `_PS` mapping and
    /// no known default pins at all) rejects a non-default pin request even
    /// when opted in, rather than silently ignoring the caller's pins.
    #[test]
    fn resolve_pin_selection_rejects_an_out_of_scope_protocol() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::from_raw(0xDEAD),
            None,
            &[untyped_pin(3)],
            true,
            None,
        )
        .expect_err("a protocol with no _PS variant must reject a non-default pin request");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Regression test (found by Codex review, PR #28, a later round): a
    /// caller can name a `_PS` hardware protocol id directly via
    /// `protocol_id` (e.g. `PROTOCOL_CAN_PS`) rather than requesting
    /// non-default pins on the base `CAN` id -- `resolve_protocol_id`'s
    /// table fallback preserves that raw id unchanged
    /// (`ChannelProtocol::from_raw`), since no `resources` table row exists
    /// for a `_PS` id. Without `dlc_pin_data` to compute a `pin_select`
    /// from, this must be rejected rather than silently connecting a
    /// channel whose DLC pins can never be assigned.
    #[test]
    fn resolve_pin_selection_rejects_a_raw_ps_protocol_id_with_no_pins() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::from_raw(j2534_0404::PROTOCOL_CAN_PS),
            None,
            &[],
            true,
            None,
        )
        .expect_err("a raw _PS protocol id with no dlc_pin_data must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// ADR-179/Phase 5: SAE J2534-2 clause 16 SAE J1939's own `_PS` arm --
    /// unlike UART Echo Byte/Honda DIAG-H/J1708 (each of which falls back to
    /// a resource-table row's own hardcoded default pin when
    /// `resource_row.is_some()`), J1939's two resource-table rows carry no
    /// default pins at all (`dlc_pins: &[]`, ADR-179 Decision 2), so
    /// `dlc_pin_data` is required unconditionally -- with or without a
    /// matched resource-table row.
    #[test]
    fn resolve_pin_selection_rejects_j1939_with_no_pins_even_with_a_matched_resource_row() {
        use super::ChannelProtocol;

        let row = resources::find_by_resource_id(0x025D).expect("J1939 row 0x025D should exist");

        // No resource_row: rejected, matching every other standalone _PS
        // protocol's own no-pins/no-row rejection.
        let status =
            J2534Service::resolve_pin_selection(ChannelProtocol::J1939_PS, None, &[], true, None)
                .expect_err("J1939 with no dlc_pin_data and no resource row must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);

        // A matched resource_row: STILL rejected -- the row itself defines
        // no default pins (unlike UART Echo Byte/Honda DIAG-H/J1708's own
        // resource_row.is_some() fallback).
        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::J1939_PS,
            Some(row),
            &[],
            true,
            None,
        )
        .expect_err(
            "J1939 with no dlc_pin_data must be rejected even with a matched resource \
                     row, since its own rows carry no default pins",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Opted out of SAE J2534-2 -- rejected regardless of pins, mirroring
    /// every other standalone `_PS` protocol's own opt-in gate.
    #[test]
    fn resolve_pin_selection_rejects_j1939_when_not_opted_in() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::J1939_PS,
            None,
            &[pin(6, "HI"), pin(14, "LOW")],
            false,
            None,
        )
        .expect_err("J1939 must be rejected when the module has not opted into J2534-2");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Explicit `dlc_pin_data` resolves through the generic
    /// `compute_pin_select` path -- no closed-set validation, matching
    /// J1708's own reasoning (clause 6.3.3.2 documents no J1962 pin table
    /// for J1939 either).
    #[test]
    fn resolve_pin_selection_computes_pin_select_for_j1939_with_explicit_pins() {
        use super::ChannelProtocol;

        let result = J2534Service::resolve_pin_selection(
            ChannelProtocol::J1939_PS,
            None,
            &[pin(6, "HI"), pin(14, "LOW")],
            true,
            None,
        )
        .unwrap();
        assert_eq!(
            result,
            Some((
                j2534_0404::PROTOCOL_J1939_PS,
                j2534_0404::PROTOCOL_J1939_PS,
                0x0000_060E,
            ))
        );
    }

    /// ADR-158/Phase 3a: a directly-named SAE J2534-2 clause 21 CAN FD
    /// (`PROTOCOL_FD_CAN_PS`) hardware protocol id is rejected outright --
    /// unlike every other family's own `_PS`/`_CHx` pair (each a legitimate
    /// direct-naming route when opted in), CAN FD has no valid direct-naming
    /// route at all for EITHER its `_PS` id or its own `_CHx` ids (ADR-213
    /// Decision item 4 -- `_CHx` support for this family is reached only via
    /// `apply_fd_mode`'s index-aware promotion, never by naming
    /// `FD_CAN_CHx` directly), so this must reject even when opted in and
    /// even with `dlc_pin_data` supplied.
    #[test]
    fn resolve_pin_selection_rejects_a_direct_fd_can_protocol_id() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::from_raw(j2534_0404::PROTOCOL_FD_CAN_PS),
            None,
            &[],
            true,
            None,
        )
        .expect_err("a direct CAN FD protocol id must be rejected, not silently normalized");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::from_raw(j2534_0404::PROTOCOL_FD_CAN_PS),
            None,
            &[pin(6, "HI"), pin(14, "LOW")],
            true,
            None,
        )
        .expect_err(
            "a direct CAN FD protocol id must be rejected even with dlc_pin_data supplied, not \
             silently canonicalized to a plain CAN connect",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// End-to-end via `parse_protocol_id_from_resource`'s `RscData::ProtocolId`
    /// route: a directly-named `PROTOCOL_FD_CAN_PS` id is rejected with
    /// `invalid_argument`, never silently resolved to a plain CAN connect
    /// (ADR-158).
    #[test]
    fn parse_protocol_id_from_resource_rejects_a_direct_fd_can_protocol_id() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                    j2534_0404::PROTOCOL_FD_CAN_PS,
                )),
            },
        );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
            Err(status) => status,
            Ok(_) => panic!("a direct CAN FD protocol id must be rejected"),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Same raw `_PS` id, but with `dlc_pin_data` supplied -- and, notably,
    /// supplying exactly CAN's normal default pins (6/14). This must
    /// canonicalize to `Ok(None)`, the same as requesting those same pins on
    /// the base `CAN` id: an earlier version of this function always
    /// computed a real `pin_select` here, reasoning that the `_PS` channel
    /// starts pin-unassigned regardless of what the pins equal -- true, but
    /// it gave one physical wiring two different `ChannelKey`s, splitting
    /// physical-resource lock scope across them (Codex review / design-advisor,
    /// PR #28, a later round -- see this function's own doc comment).
    #[test]
    fn resolve_pin_selection_canonicalizes_a_raw_ps_protocol_id_with_default_pins() {
        use super::ChannelProtocol;

        let result = J2534Service::resolve_pin_selection(
            ChannelProtocol::from_raw(j2534_0404::PROTOCOL_CAN_PS),
            None,
            &[pin(6, "HI"), pin(14, "LOW")],
            true,
            None,
        )
        .unwrap();
        assert_eq!(
            result, None,
            "a raw _PS protocol id with pins matching the base protocol's usual defaults must \
             canonicalize to the base identity (Ok(None)), sharing a ChannelKey with an \
             ordinary default-pins connect, not compute a distinct pin_select"
        );
    }

    /// Same raw `_PS` id with pins supplied, but the module has not opted
    /// into J2534-2 -- rejected the same way a non-default pin request on
    /// the base id is (ADR-156 Decision 4/clause 5), not silently honored
    /// just because the caller named the `_PS` id directly.
    #[test]
    fn resolve_pin_selection_rejects_a_raw_ps_protocol_id_when_not_opted_in() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::from_raw(j2534_0404::PROTOCOL_CAN_PS),
            None,
            &[pin(6, "HI"), pin(14, "LOW")],
            false,
            None,
        )
        .expect_err("a non-opted-in module must reject a raw _PS protocol id with pins too");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Regression test for an edge-case-hunter finding on the PR #98 round 3
    /// `_CHx`-compound-name fix: a caller combining GM UART's `_CHx` suffix
    /// (`requested_index: Some(_)`) with explicit `dlc_pin_data` must be
    /// rejected outright, not silently accepted with the pins discarded.
    /// Before this fix, the new `requested_index.is_some()` bypass at the
    /// top of the `PROTOCOL_GM_UART_PS` arm returned `Ok(None)`
    /// unconditionally, and `resolve_channel_selection`'s own mutual-
    /// exclusion check couldn't catch the gap because
    /// `resources::is_ps_protocol_id` never covers the standalone-row
    /// families (GM UART included).
    #[test]
    fn resolve_pin_selection_rejects_gm_uart_chx_combined_with_explicit_pins() {
        use super::ChannelProtocol;

        let row = resources::find_by_resource_id(0x0260).expect("GM UART row 0x0260 must exist");

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::GM_UART_PS,
            Some(row),
            &[pin(9, "K")],
            true,
            Some(1),
        )
        .expect_err(
            "a _CHx suffix combined with explicit dlc_pin_data must be rejected, not silently \
             accepted with the pins discarded",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// ADR-206's own analog of `resolve_pin_selection_rejects_gm_uart_chx_
    /// combined_with_explicit_pins` above: J1939's own `requested_index.
    /// is_some()` bypass, added by ADR-206 to mirror GM UART's, must reject
    /// a caller combining a `_CHx` suffix with explicit `dlc_pin_data`
    /// rather than silently accepting it with the pins discarded -- the
    /// same PR #98 round 3 reasoning GM UART's own bypass already
    /// documents.
    #[test]
    fn resolve_pin_selection_rejects_j1939_chx_combined_with_explicit_pins() {
        use super::ChannelProtocol;

        let row = resources::find_by_resource_id(0x025D).expect("J1939 row 0x025D must exist");

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::J1939_PS,
            Some(row),
            &[pin(6, "HI"), pin(14, "LOW")],
            true,
            Some(1),
        )
        .expect_err(
            "a _CHx suffix combined with explicit dlc_pin_data must be rejected, not silently \
             accepted with the pins discarded",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// ADR-212's own analog of `resolve_pin_selection_rejects_gm_uart_chx_
    /// combined_with_explicit_pins` above: Single Wire CAN's own
    /// `requested_index.is_some()` bypass, added by ADR-212 to close the same
    /// masking gap ADR-211 already closed for FT-CAN, must reject a caller
    /// combining a `_CHx` suffix with explicit `dlc_pin_data` rather than
    /// silently accepting it with the pins discarded.
    #[test]
    fn resolve_pin_selection_rejects_sw_can_chx_combined_with_explicit_pins() {
        use super::ChannelProtocol;

        let row = resources::find_by_resource_id(0x0226).expect("SW-CAN row 0x0226 must exist");

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::CAN,
            Some(row),
            &[pin(1, "HI")],
            true,
            Some(1),
        )
        .expect_err(
            "a _CHx suffix combined with explicit dlc_pin_data must be rejected, not silently \
             accepted with the pins discarded",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn compute_pin_select_packs_a_single_pin_with_zero_secondary() {
        // Single-wire protocols (e.g. J1850VPW) supply only one pin.
        assert_eq!(
            J2534Service::compute_pin_select(&[pin(2, "PLUS")]).unwrap(),
            0x0000_0200
        );
        // A single untyped pin is fine too -- there's no secondary to
        // disambiguate against.
        assert_eq!(
            J2534Service::compute_pin_select(&[untyped_pin(2)]).unwrap(),
            0x0000_0200
        );
    }

    /// A single pin explicitly typed secondary (LOW/L/RX/MINUS) cannot be
    /// packed as if it were the single-wire primary pin -- there is no
    /// secondary byte for it to pair with, so silently packing it into `PP`
    /// would misrepresent the caller's explicit request (Codex review, PR
    /// #28, 5th gap in this pipeline; ADR-156's Corrections list the prior
    /// four).
    #[test]
    fn compute_pin_select_rejects_a_single_pin_explicitly_typed_secondary() {
        let status = J2534Service::compute_pin_select(&[pin(3, "LOW")])
            .expect_err("a single secondary-typed pin has no primary to pair with");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// A single pin with an unrecognized/garbage type name is rejected
    /// rather than silently packed -- `compute_pin_select`'s single-pin
    /// branch must resolve (and validate) the pin's type via
    /// `resolve_pin_type_id`, not skip that call entirely.
    #[test]
    fn compute_pin_select_rejects_a_single_pin_with_an_unrecognized_type_name() {
        let status = J2534Service::compute_pin_select(&[pin(3, "NOT_A_REAL_PIN_TYPE")])
            .expect_err("an unrecognized pin-type name must be rejected, not silently ignored");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// A single pin explicitly typed as a recognized but non-primary role
    /// (`IGN`/`SINGLE`/`PROGV`, none of which are LOW/L/RX/MINUS either) must
    /// also be rejected -- an earlier version of this fix only blocked the
    /// four secondary roles, silently packing any other recognized type
    /// (including these non-communication roles) as if it were primary
    /// (Codex review, PR #28, follow-up to the 5th gap: an allow-list of
    /// primary roles, not a block-list of secondary ones).
    #[test]
    fn compute_pin_select_rejects_a_single_pin_typed_a_non_primary_non_secondary_role() {
        for type_name in ["IGN", "SINGLE", "PROGV"] {
            let status = J2534Service::compute_pin_select(&[pin(3, type_name)]).expect_err(
                "a single pin typed with a recognized but non-primary role must still be \
                 rejected, not silently packed as primary",
            );
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    /// A single pin explicitly typed primary (HI/K/TX/PLUS) still packs
    /// correctly -- unchanged behavior, mirrored against
    /// `compute_pin_select_packs_a_single_pin_with_zero_secondary`'s
    /// untyped case.
    #[test]
    fn compute_pin_select_packs_a_single_pin_explicitly_typed_primary() {
        assert_eq!(
            J2534Service::compute_pin_select(&[pin(3, "HI")]).unwrap(),
            0x0000_0300
        );
    }

    #[test]
    fn compute_pin_select_rejects_two_untyped_pins() {
        J2534Service::compute_pin_select(&[untyped_pin(3), untyped_pin(11)])
            .expect_err("two pins need typing to know which is primary/secondary");
    }

    #[test]
    fn compute_pin_select_rejects_more_than_two_pins() {
        J2534Service::compute_pin_select(&[pin(6, "HI"), pin(14, "LOW"), pin(2, "PLUS")])
            .expect_err("Pin Selection supports at most one primary and one secondary pin");
    }

    #[test]
    fn compute_pin_select_rejects_a_wildcard_pin_number() {
        J2534Service::compute_pin_select(&[vci_service_interface::PinData {
            dlc_pin_number: 0,
            dlc_pin_type: Some(vci_service_interface::pin_data::DlcPinType::DlcPinTypeName(
                "HI".to_string(),
            )),
        }])
        .expect_err("pin_select needs a concrete pin number, not a type-only wildcard");
    }

    /// SAE J2534-2 clause 6.3.3.2 Table 3: `CONFIG_J1962_PINS`'s PP/SS bytes
    /// are each valid only in `0x00`-`0x10` (0-16 decimal).
    #[test]
    fn compute_pin_select_rejects_a_pin_number_outside_the_valid_range() {
        J2534Service::compute_pin_select(&[pin(17, "HI")])
            .expect_err("pin 17 is outside Table 3's 0x00-0x10 J1962 pin range");
        J2534Service::compute_pin_select(&[pin(255, "HI")])
            .expect_err("pin 255 is outside Table 3's 0x00-0x10 J1962 pin range");
    }

    /// SAE J2534-2 clause 6.3.3.2 Table 3 excludes pins 4, 5, and 16 from
    /// `CONFIG_J1962_PINS` regardless of protocol -- 16 (`0x10`) is notable
    /// since it sits at the very top of the otherwise-valid numeric range,
    /// so range-validity alone does not imply legality.
    #[test]
    fn compute_pin_select_rejects_each_excluded_j1962_pin() {
        for excluded in [4, 5, 16] {
            J2534Service::compute_pin_select(&[pin(excluded, "HI")]).expect_err(&format!(
                "pin {excluded} is one of Table 3's excluded J1962 pins"
            ));
        }
    }

    /// SAE J2534-2 clause 6.3.3.2 Table 3: PP must differ from SS (except
    /// for the `0x0000` "no selection made" sentinel this function never
    /// produces, since a wildcard/zero pin number is already rejected
    /// earlier) -- two differently-typed pins that happen to name the same
    /// physical pin number must still be rejected.
    #[test]
    fn compute_pin_select_rejects_identical_primary_and_secondary_pin_numbers() {
        J2534Service::compute_pin_select(&[pin(6, "HI"), pin(6, "LOW")])
            .expect_err("primary and secondary pin numbers must differ per Table 3");
    }

    /// End-to-end via `parse_protocol_id_from_resource`'s `RscData::ProtocolId`
    /// route: dlc_pin_data was previously never even inspected on this route
    /// (only `ProtocolName` matching a table row narrowed candidates), so
    /// this is a pure behavioral addition, not a change to any existing
    /// resolution.
    #[test]
    fn parse_protocol_id_from_resource_resolves_pin_selection_via_protocol_id() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![pin(3, "HI"), pin(11, "LOW")],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                    j2534_0404::CAN,
                )),
            },
        );
        let (protocol, _row, pin_selection, _channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::CAN);
        assert_eq!(
            pin_selection,
            Some((j2534_0404::CAN, j2534_0404::PROTOCOL_CAN_PS, 0x0000_030B))
        );
    }

    /// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16,
    /// `edge-case-hunter` finding on PR #102 round 2): a non-opted-in
    /// module must be rejected even when Ethernet_NDIS is named via
    /// `RscData::ProtocolName` -- the route this fix closes. Unlike a
    /// `ProtocolId`/`ResourceId` selector, this route resolves the table
    /// row directly (`table_row_matched = true`) with a protocol excluded
    /// from `row_needs_dynamic_pin_selection` (clause 24 has no pin
    /// concept), so before this fix it skipped `resolve_pin_selection` --
    /// this crate's only opt-in checkpoint for this protocol -- entirely,
    /// reaching `resolve_channel_selection`'s `_CHx`-only-scoped opt-in
    /// check's no-op early return with no rejection at all.
    #[test]
    fn parse_protocol_id_from_resource_rejects_ethernet_ndis_via_protocol_name_when_not_opted_in() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(
                    vci_service_interface::resource_data::Protocol::ProtocolName(
                        "ETHERNET_NDIS".to_string(),
                    ),
                ),
            },
        );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, false) {
            Err(status) => status,
            Ok(_) => {
                panic!("a non-opted-in module must reject Ethernet_NDIS named by ProtocolName")
            }
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// The same request, opted in, resolves successfully -- confirming the
    /// fix above doesn't just reject everything on this route.
    #[test]
    fn parse_protocol_id_from_resource_resolves_ethernet_ndis_via_protocol_name_when_opted_in() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(
                    vci_service_interface::resource_data::Protocol::ProtocolName(
                        "ETHERNET_NDIS".to_string(),
                    ),
                ),
            },
        );
        let (protocol, _row, pin_selection, _channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol, super::ChannelProtocol::ETHERNET_NDIS);
        assert_eq!(pin_selection, None);
    }

    /// `edge-case-hunter` finding on PR #102 round 2, item 3: before the fix
    /// above, this same `ProtocolName` route reached `find_table_row_by_name`
    /// (which DOES run `retain_rows_matching_all_pins` on `dlc_pin_data`) but
    /// then skipped `resolve_pin_selection` entirely, silently discarding any
    /// non-empty `dlc_pin_data` instead of surfacing
    /// `resolve_pin_selection`'s own explicit "clause 24 has no pin concept,
    /// reject any dlc_pin_data outright" rule. Now that this route reaches
    /// `resolve_pin_selection` too, non-empty `dlc_pin_data` is rejected the
    /// same way it already was via the `ProtocolId` route.
    #[test]
    fn parse_protocol_id_from_resource_rejects_ethernet_ndis_dlc_pin_data_via_protocol_name() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![pin(3, "PLUS")],
                bus_type: None,
                protocol: Some(
                    vci_service_interface::resource_data::Protocol::ProtocolName(
                        "ETHERNET_NDIS".to_string(),
                    ),
                ),
            },
        );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
            Err(status) => status,
            Ok(_) => panic!(
                "Ethernet_NDIS named by ProtocolName with non-empty dlc_pin_data must be rejected"
            ),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15,
    /// `edge-case-hunter` finding, PR #102 round-2 close-out): the identical
    /// gap the Ethernet_NDIS tests above pin, for its sibling protocol also
    /// excluded from `row_needs_dynamic_pin_selection`. Before this fix, a
    /// non-opted-in module naming an Analog Input resource via
    /// `RscData::ProtocolName` skipped `resolve_pin_selection` -- this
    /// crate's only opt-in checkpoint for this protocol -- entirely,
    /// reaching `resolve_channel_selection`'s `_CHx`-only-scoped opt-in
    /// check's no-op early return with no rejection at all.
    #[test]
    fn parse_protocol_id_from_resource_rejects_analog_in_via_protocol_name_when_not_opted_in() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(
                    vci_service_interface::resource_data::Protocol::ProtocolName(
                        "ANALOG_IN_1".to_string(),
                    ),
                ),
            },
        );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, false) {
            Err(status) => status,
            Ok(_) => {
                panic!("a non-opted-in module must reject Analog Inputs named by ProtocolName")
            }
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// The same request, opted in, resolves successfully -- confirming the
    /// fix above doesn't just reject everything on this route.
    #[test]
    fn parse_protocol_id_from_resource_resolves_analog_in_via_protocol_name_when_opted_in() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(
                    vci_service_interface::resource_data::Protocol::ProtocolName(
                        "ANALOG_IN_1".to_string(),
                    ),
                ),
            },
        );
        let (protocol, _row, pin_selection, _channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol, super::ChannelProtocol::ANALOG_IN);
        assert_eq!(pin_selection, None);
    }

    /// ADR-157 Bug 1 regression: a pin-selected `RscData::ProtocolId`
    /// resolving to one of the four `SAE_J2610_on_SAE_J2610_SCI` rows
    /// (0x021E-0x0221) must resolve `resolve_pin_selection`'s returned base
    /// hw protocol id to that row's own `hw_protocol_override` (the exact
    /// SCI variant, e.g. `SCI_A_ENGINE`), not the shared `ChannelProtocol`'s
    /// lossy `j2534_protocol_id()` (`SCI_MODE`, ADR-023). Resource id
    /// 0x021E is the `SCI_A_ENGINE` configuration (default pins (6, TX)/(7,
    /// RX)); supplying `SCI_A_TRANS`'s pins (14, TX)/(7, RX) instead
    /// requests non-default pins and triggers Pin Selection.
    #[test]
    fn parse_protocol_id_from_resource_pin_selection_preserves_sci_variant() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![pin(14, "TX"), pin(7, "RX")],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                    0x021E,
                )),
            },
        );
        let (protocol, row, pin_selection, _channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(
            protocol.value(),
            super::ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI.value()
        );
        assert_eq!(row.unwrap().resource_id, 0x021E);
        assert_eq!(
            pin_selection,
            Some((
                j2534_0404::SCI_A_ENGINE,
                j2534_0404::PROTOCOL_J2610_PS,
                0x0000_0E07,
            )),
            "the base hw protocol id must be the exact SCI variant (SCI_A_ENGINE), not SCI_MODE"
        );
    }

    /// A `protocol_name` that matches a `resources` table row by name (e.g.
    /// the canonical `"ISO_15765_2"`) keeps `find_table_row_by_name`'s
    /// pre-existing all-or-nothing pin-narrowing contract unchanged -- Pin
    /// Selection is not reachable through it, even for an opted-in module
    /// (see `parse_protocol_id_from_resource`'s own doc comment for why).
    /// Mirrors the integration-level pin
    /// `create_com_logical_link_rejects_pin_narrowing_a_unique_name_to_zero_rows`
    /// (`tests/grpc_mock/resources.rs`).
    #[test]
    fn parse_protocol_id_from_resource_does_not_extend_pin_selection_to_a_table_row_name() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![untyped_pin(7)],
                bus_type: None,
                protocol: Some(
                    vci_service_interface::resource_data::Protocol::ProtocolName(
                        "ISO_15765_2".to_string(),
                    ),
                ),
            },
        );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
            Err(status) => status,
            Ok(_) => panic!("a table-row name's own pin-narrowing contract must still reject this"),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Regression test (Codex review, PR #28, a later round on the same
    /// `resolve_pin_selection` fix): the bare `ResourceId` variant has no
    /// `dlc_pin_data` field at all, but `id` can still numerically equal a
    /// raw `_PS` hardware protocol id via `resolve_protocol_id`'s
    /// `ChannelProtocol::from_raw` fallback -- this route must reject that,
    /// same as the `RscData::ProtocolId` route with no pins supplied, rather
    /// than silently connecting an unpinned `_PS` channel it can never
    /// assign pins to.
    #[test]
    fn parse_protocol_id_from_resource_rejects_a_raw_ps_resource_id() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::ResourceId(
            j2534_0404::PROTOCOL_CAN_PS,
        );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
            Err(status) => status,
            Ok(_) => panic!(
                "a raw _PS resource_id must be rejected -- this route has no way to ever supply its pins"
            ),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Same regression as above, via `ResourceName`'s numeric-string
    /// fallback (`resolve_protocol_name` re-entering `resolve_protocol_id`
    /// for a name that parses as `u32`) -- the other route with no
    /// `dlc_pin_data` field at all.
    #[test]
    fn parse_protocol_id_from_resource_rejects_a_raw_ps_resource_name() {
        let resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceName(
                j2534_0404::PROTOCOL_CAN_PS.to_string(),
            );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
            Err(status) => status,
            Ok(_) => panic!(
                "a raw _PS resource_name must be rejected -- this route has no way to ever supply its pins"
            ),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Sanity check: an ordinary (non-`_PS`) `ResourceId` -- the overwhelming
    /// common case, and everything this variant supported before the two
    /// regression tests above -- must still resolve exactly as before,
    /// `pin_selection: None`, no rejection.
    #[test]
    fn parse_protocol_id_from_resource_resource_id_unaffected_for_a_base_protocol() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::ResourceId(
            j2534_0404::CAN,
        );
        let (protocol, _row, pin_selection, _channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::CAN);
        assert_eq!(pin_selection, None);
    }

    /// Regression test (design-advisor, PR #28, after a 3rd/4th independent
    /// instance of the same normalization gap surfaced): a raw `_PS`
    /// protocol id named directly via `protocol_id`, with genuinely
    /// non-default pins (so it actually resolves to a real `pin_select`
    /// rather than canonicalizing -- see the sibling default-pins test
    /// below), must produce a `protocol` (`LogicalLinkState`'s stored
    /// `ChannelProtocol`) that is the BASE identity (`ChannelProtocol::CAN`),
    /// not `ChannelProtocol::from_raw` of the raw `_PS` value -- restoring
    /// the invariant every Plane B consumer of `link.protocol`
    /// (`is_can_family()`/`unique_id_params`/`rpc_connect_com_logical_link`'s
    /// `base_proto_id`/etc.) relies on.
    #[test]
    fn parse_protocol_id_from_resource_normalizes_a_raw_ps_protocol_id() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![pin(3, "HI"), pin(11, "LOW")],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                    j2534_0404::PROTOCOL_CAN_PS,
                )),
            },
        );
        let (protocol, _row, pin_selection, _channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(
            protocol.value(),
            j2534_0404::CAN,
            "a directly-named _PS protocol id must normalize to its base ChannelProtocol, not \
             stay wrapped as ChannelProtocol::from_raw(_PS id)"
        );
        assert_eq!(
            pin_selection,
            Some((j2534_0404::CAN, j2534_0404::PROTOCOL_CAN_PS, 0x0000_030B)),
            "pin_selection's own hw ids are unaffected by the protocol-field normalization"
        );
    }

    /// Sibling of the test above: a directly-named `_PS` id whose pins equal
    /// the base protocol's defaults canonicalizes to the base identity
    /// entirely (`pin_selection: None`) -- the `resolve_pin_selection`-level
    /// regression this mirrors is
    /// `resolve_pin_selection_canonicalizes_a_raw_ps_protocol_id_with_default_pins`;
    /// this test additionally confirms `protocol` itself lands on the base
    /// `ChannelProtocol` through the full `parse_protocol_id_from_resource`
    /// call, not just `resolve_pin_selection` in isolation.
    #[test]
    fn parse_protocol_id_from_resource_canonicalizes_a_raw_ps_protocol_id_with_default_pins() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![pin(6, "HI"), pin(14, "LOW")],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                    j2534_0404::PROTOCOL_CAN_PS,
                )),
            },
        );
        let (protocol, _row, pin_selection, _channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::CAN);
        assert_eq!(pin_selection, None);
    }

    /// Same regression, for the SAE J2610 SCI case specifically (design-advisor's
    /// answer to "what's the correct ChannelProtocol for the representative
    /// collapse"): `PROTOCOL_J2610_PS` named directly normalizes to
    /// `ChannelProtocol::SCI_A_ENGINE` -- the same representative
    /// `resources::base_protocol_id` already collapses to -- NOT
    /// `ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI` (0x0160), whose own
    /// `j2534_protocol_id()` is the unrelated `SCI_MODE` TX-flag value
    /// (`protocol.rs`), which would disagree with
    /// `base_hw_protocol_override = SCI_A_ENGINE` and reopen the exact
    /// mismatch ADR-157's first Correction fixed. Uses `SCI_A_TRANS`'s
    /// default pins (14/TX, 7/RX) rather than `SCI_A_ENGINE`'s own (6/7) --
    /// the latter would canonicalize to `Ok(None)` (the sibling
    /// default-pins-canonicalization fix), which is not what this test is
    /// pinning.
    #[test]
    fn parse_protocol_id_from_resource_normalizes_a_raw_j2610_ps_protocol_id() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![pin(14, "TX"), pin(7, "RX")],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                    j2534_0404::PROTOCOL_J2610_PS,
                )),
            },
        );
        let (protocol, _row, pin_selection, _channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::SCI_A_ENGINE);
        assert_eq!(
            pin_selection.map(|(base, _, _)| base),
            Some(j2534_0404::SCI_A_ENGINE),
            "protocol and pin_selection's base hw id must agree on the same representative \
             SCI variant"
        );
    }

    // ── ADR-201: general clause 6 Pin Selection per-bus completeness ────────

    /// `CAN`/`ISO15765` (standard differential CAN HI/LOW) must reject a
    /// single primary-only pin: `resources::SecondaryPinRequirement::Required`
    /// closes the gap ADR-168's Seventh correction/Accepted residual left
    /// open in this general fallback path (the FT-CAN arm's own closed
    /// two-pair check, a few lines earlier in this function, does not cover
    /// plain `CAN_PS`/`ISO15765_PS`).
    #[test]
    fn resolve_pin_selection_rejects_a_single_primary_pin_for_can_and_iso15765() {
        use super::ChannelProtocol;

        for protocol in [ChannelProtocol::CAN, ChannelProtocol::ISO15765] {
            let status =
                J2534Service::resolve_pin_selection(protocol, None, &[pin(3, "HI")], true, None)
                    .expect_err("a single primary-only pin must be rejected for a dual-wire bus");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    /// `J1850PWM` (Ford SCP, differential PLUS/MINUS) must likewise reject a
    /// single primary-only pin selection.
    #[test]
    fn resolve_pin_selection_rejects_a_single_primary_pin_for_j1850pwm() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::J1850PWM,
            None,
            &[pin(2, "PLUS")],
            true,
            None,
        )
        .expect_err("a single primary-only pin must be rejected for J1850PWM");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// All four SAE J2610 SCI ids need both Tx and Rx to function as a
    /// bidirectional diagnostic link (no one-pin SCI shape is defined in
    /// either spec) -- each must reject a single primary-typed (Tx) pin.
    #[test]
    fn resolve_pin_selection_rejects_a_single_primary_pin_for_every_sci_id() {
        use super::ChannelProtocol;

        for (protocol, tx_pin) in [
            (ChannelProtocol::SCI_A_ENGINE, 6),
            (ChannelProtocol::SCI_A_TRANS, 14),
            (ChannelProtocol::SCI_B_ENGINE, 12),
            (ChannelProtocol::SCI_B_TRANS, 9),
        ] {
            let status = J2534Service::resolve_pin_selection(
                protocol,
                None,
                &[pin(tx_pin, "TX")],
                true,
                None,
            )
            .expect_err("a single primary-typed pin must be rejected for every SCI id");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    /// K-line's own secondary pin is genuinely optional -- unlike the
    /// dual-wire buses above, `ISO9141`/`ISO14230` must ACCEPT a single
    /// primary-only (K) pin selection, packing a zero secondary byte.
    #[test]
    fn resolve_pin_selection_accepts_a_single_k_line_pin_for_iso9141_and_iso14230() {
        use super::ChannelProtocol;

        for protocol in [ChannelProtocol::ISO9141, ChannelProtocol::ISO14230] {
            let result =
                J2534Service::resolve_pin_selection(protocol, None, &[pin(7, "K")], true, None)
                    .unwrap();
            assert!(matches!(result, Some((_, _, 0x0000_0700))));
        }
    }

    /// `edge-case-hunter` coverage gap (ADR-201 review round): every other
    /// `Optional` acceptance test above uses a single (K-only) pin, and the
    /// only two-pin acceptance test targets `Required`/CAN -- a non-default
    /// two-pin (K+L) selection for a `SecondaryPinRequirement::Optional`
    /// protocol never exercised the new check's `Optional` catch-all arm at
    /// all (a *default* two-pin K+L selection short-circuits earlier, via
    /// `dlc_pin_data_matches_defaults`, before reaching this check).
    #[test]
    fn resolve_pin_selection_accepts_a_full_non_default_two_pin_selection_for_k_line() {
        use super::ChannelProtocol;

        for protocol in [ChannelProtocol::ISO9141, ChannelProtocol::ISO14230] {
            let result = J2534Service::resolve_pin_selection(
                protocol,
                None,
                &[pin(7, "K"), pin(8, "L")],
                true,
                None,
            )
            .unwrap();
            assert!(matches!(result, Some((_, _, 0x0000_0708))));
        }
    }

    /// Over-rejection guard: without this test, a check that rejected every
    /// dual-wire CAN pin selection outright (not just single-pin ones) would
    /// still pass every "rejects" case above. CAN must still accept a full,
    /// valid, non-default two-pin selection.
    #[test]
    fn resolve_pin_selection_accepts_a_full_two_pin_selection_for_can() {
        use super::ChannelProtocol;

        let result = J2534Service::resolve_pin_selection(
            ChannelProtocol::CAN,
            None,
            &[pin(3, "HI"), pin(11, "LOW")],
            true,
            None,
        )
        .unwrap();
        assert!(matches!(result, Some((_, _, 0x0000_030B))));
    }

    /// `J1850VPW` (GM/Chrysler Class 2) is genuinely single-wire -- no
    /// secondary pin exists for this bus at all, unlike `J1850PWM` -- so a
    /// two-pin selection must be rejected.
    #[test]
    fn resolve_pin_selection_rejects_a_two_pin_selection_for_j1850vpw() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_pin_selection(
            ChannelProtocol::J1850VPW,
            None,
            &[pin(2, "PLUS"), pin(10, "MINUS")],
            true,
            None,
        )
        .expect_err("a two-pin selection must be rejected for single-wire J1850VPW");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// `J1850VPW` must still accept a single non-default primary-only pin
    /// selection (its own default is pin 2; pin 3 exercises the non-default
    /// path without tripping the defaults-match short-circuit).
    #[test]
    fn resolve_pin_selection_accepts_a_single_non_default_pin_for_j1850vpw() {
        use super::ChannelProtocol;

        let result = J2534Service::resolve_pin_selection(
            ChannelProtocol::J1850VPW,
            None,
            &[pin(3, "PLUS")],
            true,
            None,
        )
        .unwrap();
        assert!(matches!(result, Some((_, _, 0x0000_0300))));
    }

    // ── ADR-156 Decision 3/Phase 2b: `resolve_channel_selection`/
    // `parse_protocol_id_from_resource`'s `_CHx` (Additional Channels) tests ──

    /// ADR-178's proto field route is gone, but `requested_index: None` (no
    /// compound `_CHx`-suffixed name was supplied) plus a plain (non-`_CHx`)
    /// directly-named protocol is still always a no-op here, regardless of
    /// opt-in (there is nothing to gate for a request that names no
    /// Additional Channels qualifier at all).
    #[test]
    fn resolve_channel_selection_is_a_noop_for_a_non_chx_protocol() {
        use super::ChannelProtocol;

        assert_eq!(
            J2534Service::resolve_channel_selection(ChannelProtocol::CAN, None, None, None, true)
                .unwrap(),
            None
        );
        // Also a no-op when not opted in, since there is nothing to gate.
        assert_eq!(
            J2534Service::resolve_channel_selection(ChannelProtocol::CAN, None, None, None, false)
                .unwrap(),
            None
        );
    }

    /// ADR-158 Corrections item 4 (`edge-case-hunter` Finding 3): a
    /// `resources` table row whose `hw_protocol_override` directly names a
    /// SAE J2534-2 clause 21 CAN FD (`_PS`) hardware protocol id must be
    /// rejected here too, not just via `resolve_pin_selection`'s own check
    /// -- this is the table-row-matched resolution route, which never calls
    /// `resolve_pin_selection` at all. Uses a non-`_CHx` row specifically
    /// because that is exactly the case the pre-fix no-op check (`!already_chx`)
    /// would otherwise short-circuit straight to `Ok(None)`, silently
    /// accepting the FD id with no rejection at all -- `is_fd_protocol_id`
    /// is not `is_chx_protocol_id`, so `already_chx` alone never caught it.
    #[test]
    fn resolve_channel_selection_rejects_a_resource_row_naming_fd_can_ps() {
        use super::{ChannelProtocol, resources};

        static FD_ROW: resources::ResourceDef = resources::ResourceDef {
            resource_id: 0,
            protocol_name: "TEST_FD_CAN_PS_ROW",
            config_name: None,
            protocol: ChannelProtocol::CAN,
            hw_protocol_override: Some(j2534_0404::PROTOCOL_FD_CAN_PS),
            bus_type_id: 0,
            bus_type_name: "",
            dlc_pins: &[],
        };

        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::CAN,
            Some(&FD_ROW),
            None,
            None,
            true,
        )
        .expect_err(
            "a resource row whose hw_protocol_override directly names PROTOCOL_FD_CAN_PS \
                     must be rejected, not silently connected via the non-_CHx no-op path",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// A caller naming a `_CHx` hardware protocol id directly (via
    /// `protocol_id`, decomposed through `resolve_protocol_id`'s
    /// `ChannelProtocol::from_raw` fallback) is accepted and decomposed to
    /// `(base, index)` -- unlike a bare `_PS` id (which is rejected for
    /// missing pin data), a `_CHx` id is fully self-describing.
    #[test]
    fn resolve_channel_selection_decomposes_a_directly_named_chx_id() {
        use super::ChannelProtocol;

        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let result = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(raw_chx),
            None,
            None,
            None,
            true,
        )
        .unwrap();
        assert_eq!(result, Some((j2534_0404::CAN, raw_chx, 5)));
    }

    /// ADR-211 regression: a directly-named `_CHx` id from Fault-Tolerant
    /// CAN's own CAN-collapse family must decompose to the true `CAN`/
    /// `ISO15765` base, not the intermediate `PROTOCOL_FT_CAN_PS`/
    /// `PROTOCOL_FT_ISO15765_PS` value `resources::chx_base_protocol_id`
    /// returns raw. This is the `already_chx` branch's own analog of
    /// `resolve_channel_selection_decomposes_a_directly_named_chx_id` just
    /// above -- that test uses a plain CAN `_CHx` id, for which
    /// `chx_base_protocol_id` already returns the true base, so it could not
    /// have caught this bug; Fault-Tolerant CAN is the one already-shipped
    /// family whose `_CHx` block is keyed on its own qualifying `_PS` id
    /// instead of a true base (see `resources::chx_base_protocol_id`'s own
    /// doc comment). Found by edge-case-hunter review of ADR-211's
    /// implementation: the compound-`_CHx`-name and `_PS`-direct-naming
    /// routes were both fixed to recurse through `base_protocol_id`, but this
    /// third route (`already_chx`) was missed.
    #[test]
    fn resolve_channel_selection_decomposes_a_directly_named_ft_chx_id_to_the_true_base() {
        use super::ChannelProtocol;

        let raw_ft_can_ch1 = j2534_0404_sys::bindings::PROTOCOL_FT_CAN_CH1;
        let result = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(raw_ft_can_ch1),
            None,
            None,
            None,
            true,
        )
        .unwrap();
        assert_eq!(result, Some((j2534_0404::CAN, raw_ft_can_ch1, 1)));

        let raw_ft_iso15765_ch1 = j2534_0404_sys::bindings::PROTOCOL_FT_ISO15765_CH1;
        let result = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(raw_ft_iso15765_ch1),
            None,
            None,
            None,
            true,
        )
        .unwrap();
        assert_eq!(result, Some((j2534_0404::ISO15765, raw_ft_iso15765_ch1, 1)));
    }

    /// The clause-5 opt-in gate rejects a directly-named, in-scope `_CHx` id
    /// on a non-opted-in module -- mirrors
    /// `resolve_pin_selection_rejects_a_raw_ps_protocol_id_when_not_opted_in`'s
    /// equivalent gate for `_PS`.
    #[test]
    fn resolve_channel_selection_rejects_a_directly_named_chx_id_when_not_opted_in() {
        use super::ChannelProtocol;

        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(raw_chx),
            None,
            None,
            None,
            false,
        )
        .expect_err("a non-opted-in module must reject a directly-named in-scope _CHx id");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("has not opted into J2534-2"),
            "got: {}",
            status.message()
        );
    }

    // `resolve_channel_selection_rejects_a_directly_named_out_of_scope_chx_id`/
    // `..._when_not_opted_in` (ADR-206-era) used to pin the "`_CHx` id inside
    // the full clause-24 region but from a still-out-of-scope family" branch
    // via a CAN-FD `_CHx` example. ADR-213 closed the last remaining
    // out-of-scope family in that region (CAN FD/ISO15765-on-CAN-FD), so
    // every id `resources::is_chx_protocol_id` recognizes now also resolves
    // via `resources::chx_base_protocol_id` -- there is no longer any real
    // id left to construct this scenario with (the CAN-FD example these
    // tests used is now itself in-scope, and no other family remains).
    // Removed rather than repurposed with a synthetic id: the second test's
    // own "opt-in gate precedes the family-support rejection" point cannot
    // be demonstrated with a CAN-FD id either way, since `is_fd_protocol_id`'s
    // own guard (above) rejects a directly-named FD id unconditionally,
    // regardless of opt-in -- a different precedence claim than the one
    // these tests were written to pin. The defensive `chx_base_protocol_id`
    // `None` branch itself is unchanged and still reachable in principle
    // (kept for the same forward-looking-symmetry reason this codebase
    // keeps other now-unreachable-in-practice guards elsewhere) if
    // `resources::is_chx_protocol_id`'s own region is ever widened ahead of
    // `chx_base_protocol_id`'s own BLOCKS table again.

    // ── `split_chx_suffix` (the compound-name grammar's own one-shot
    // suffix-split parser) tests -- edge-case-hunter, PR #67 round 2:
    // leading zeros, u32 overflow, empty head, and no-backtrack behavior
    // weren't directly exercised anywhere else in this suite. ──

    #[test]
    fn split_chx_suffix_accepts_a_leading_zero_index() {
        assert_eq!(
            J2534Service::split_chx_suffix("CAN_CH003"),
            Some(("CAN", 3))
        );
    }

    #[test]
    fn split_chx_suffix_rejects_a_u32_overflowing_index() {
        assert_eq!(J2534Service::split_chx_suffix("CAN_CH99999999999"), None);
    }

    #[test]
    fn split_chx_suffix_rejects_an_empty_head() {
        assert_eq!(J2534Service::split_chx_suffix("_CH5"), None);
    }

    #[test]
    fn split_chx_suffix_matches_the_rightmost_ch_when_it_has_a_valid_suffix() {
        // The rightmost "_CH" ("...BAR_CH5") has an all-digit suffix, so it
        // matches even though an earlier "_CH" ("FOO_CH_BAR...") exists --
        // `rfind` always tries the rightmost occurrence first.
        assert_eq!(
            J2534Service::split_chx_suffix("FOO_CH_BAR_CH5"),
            Some(("FOO_CH_BAR", 5))
        );
    }

    #[test]
    fn split_chx_suffix_does_not_backtrack_past_a_rightmost_ch_with_no_valid_suffix() {
        // The rightmost "_CH" ("...FOO_CH_BAR", suffix "_BAR") is not a
        // valid digit suffix, and this function does not backtrack to try
        // an earlier "_CH" in the string -- it is a one-shot fallback, not
        // a general grammar.
        assert_eq!(J2534Service::split_chx_suffix("FOO_CH_BAR"), None);
    }

    #[test]
    fn split_chx_suffix_is_case_insensitive_on_the_ch_marker() {
        assert_eq!(
            J2534Service::split_chx_suffix("SCI_B_TRANS_ch3"),
            Some(("SCI_B_TRANS", 3))
        );
    }

    #[test]
    fn split_chx_suffix_rejects_no_ch_marker_at_all() {
        assert_eq!(J2534Service::split_chx_suffix("PLAIN_NAME"), None);
    }

    // ── `requested_index` (the compound-name grammar's own resolution
    // route, ADR-178's ADR-156-scoping fix) tests, at the
    // `resolve_channel_selection` unit level -- reinstated in adapted form
    // from the `channel_index` field route's own pre-ADR-178 test suite. ──

    #[test]
    fn resolve_channel_selection_computes_chx_id_for_a_requested_index() {
        use super::ChannelProtocol;

        let result = J2534Service::resolve_channel_selection(
            ChannelProtocol::CAN,
            None,
            Some(5),
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            result,
            Some((j2534_0404::CAN, j2534_0404::PROTOCOL_CAN_CH1 + 4, 5))
        );
    }

    #[test]
    fn resolve_channel_selection_rejects_a_requested_index_when_not_opted_in() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::CAN,
            None,
            Some(5),
            None,
            false,
        )
        .expect_err("a non-opted-in module must reject a requested_index request");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn resolve_channel_selection_rejects_an_out_of_range_requested_index() {
        use super::ChannelProtocol;

        for bad_index in [0, 129, u32::MAX] {
            let status = J2534Service::resolve_channel_selection(
                ChannelProtocol::CAN,
                None,
                Some(bad_index),
                None,
                true,
            )
            .expect_err("an out-of-range requested_index must be rejected");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    /// A protocol outside ADR-156 Decision 3's seven in-scope families (here,
    /// a synthetic raw `ChannelProtocol` id with no `_CHx` mapping) rejects a
    /// `requested_index` request even when opted in.
    #[test]
    fn resolve_channel_selection_rejects_a_requested_index_for_an_out_of_scope_protocol() {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(0xDEAD),
            None,
            Some(1),
            None,
            true,
        )
        .expect_err("a protocol with no _CHx variant must reject a requested_index request");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// A `requested_index` combined with a genuine, non-default Pin
    /// Selection request (`pin_selection.is_some()`) is mutually exclusive
    /// -- rejected regardless of opt-in.
    #[test]
    fn resolve_channel_selection_rejects_a_requested_index_combined_with_pin_selection() {
        use super::ChannelProtocol;

        let pin_selection = Some((j2534_0404::CAN, j2534_0404::PROTOCOL_CAN_PS, 0x0000_030B));
        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::CAN,
            None,
            Some(5),
            pin_selection,
            true,
        )
        .expect_err(
            "a requested_index combined with a real Pin Selection request must be rejected",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// The `requested_index` route is exactly where the reinstated
    /// `is_ps_protocol_id(raw_hw_protocol_id)` disjunct in the
    /// mutual-exclusion check actually fires: `raw_hw_protocol_id` here
    /// comes from an arbitrary resolved base/qualified resource (unlike the
    /// `already_chx` path, where it can never be `_PS`-shaped), so a caller
    /// naming a `_PS` id whose pins canonicalize to defaults (`pin_selection:
    /// None`, PR #28's `9db653f`) combined with a `requested_index` must
    /// still be rejected.
    #[test]
    fn resolve_channel_selection_rejects_a_ps_protocol_combined_with_a_requested_index_even_when_pins_canonicalize()
     {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(j2534_0404::PROTOCOL_CAN_PS),
            None,
            Some(5),
            None,
            true,
        )
        .expect_err(
            "a directly-named _PS id combined with a requested_index must be rejected even when \
             its pins canonicalize to defaults",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Same bug, the SAE J2610/SCI variant: `PROTOCOL_J2610_PS` canonicalizes
    /// against the representative `SCI_A_ENGINE`'s own defaults exactly like
    /// any other `_PS` id.
    #[test]
    fn resolve_channel_selection_rejects_a_j2610_ps_protocol_combined_with_a_requested_index_even_when_pins_canonicalize()
     {
        use super::ChannelProtocol;

        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(j2534_0404::PROTOCOL_J2610_PS),
            None,
            Some(3),
            None,
            true,
        )
        .expect_err(
            "a directly-named J2610 _PS id combined with a requested_index must be rejected \
             even when its pins canonicalize to defaults",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Double qualification -- a directly-named `_CHx` id AND a
    /// `requested_index` -- is rejected even when the decomposed index would
    /// agree with the requested one. Reachable via `resolve_channel_selection`
    /// itself (unit level) regardless of whether any current
    /// `parse_protocol_id_from_resource` caller can construct it; see the
    /// end-to-end test below for a caller-reachable construction (a compound
    /// name whose head is itself the decimal string of a raw `_CHx` id).
    #[test]
    fn resolve_channel_selection_rejects_double_qualification_when_indices_agree() {
        use super::ChannelProtocol;

        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(raw_chx),
            None,
            Some(5),
            None,
            true,
        )
        .expect_err("naming a _CHx id directly AND supplying a requested_index must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Same as above, but the two routes disagree on the index -- still
    /// rejected, not silently resolved to either one.
    #[test]
    fn resolve_channel_selection_rejects_double_qualification_when_indices_disagree() {
        use super::ChannelProtocol;

        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(raw_chx),
            None,
            Some(9),
            None,
            true,
        )
        .expect_err("disagreeing _CHx id and requested_index must still be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// The clause-5 opt-in gate is checked before the double-qualification
    /// rejection (edge-case-hunter finding 4, Phase 2b review, carried
    /// forward through the ADR-178 field-route removal and this compound-name
    /// reinstatement): a non-opted-in module given BOTH a directly-named
    /// `_CHx` id AND a `requested_index` (a double-qualification trigger on
    /// its own) must report the "not opted into J2534-2" rejection, not the
    /// double-qualification one.
    #[test]
    fn resolve_channel_selection_reports_opt_in_rejection_before_double_qualification() {
        use super::ChannelProtocol;

        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let status = J2534Service::resolve_channel_selection(
            ChannelProtocol::from_raw(raw_chx),
            None,
            Some(5),
            None,
            false,
        )
        .expect_err("a non-opted-in module with both qualification routes must still be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("has not opted into J2534-2"),
            "the opt-in rejection must win over the double-qualification one when both apply, \
             got: {}",
            status.message()
        );
    }

    // ── Compound-name grammar (`"<name>_CH<n>"`) end-to-end tests, via
    // `parse_protocol_id_from_resource`. ──

    /// The core case: `"SCI_B_TRANS_CH3"` resolves to the exact SAE J2610 SCI
    /// variant plus its clause 7 index -- the regression this whole fix
    /// exists for. `SCI_B_TRANS` is one of four native hardware ids that all
    /// collapse onto the single shared `_CHx` numeric block
    /// (`resources::chx_block_base`'s SCI arm), so a bare numeric `_CHx` id
    /// can never express which variant a caller means; the compound-name
    /// grammar's head resolves through the exact same table-row lookup a
    /// bare `"SCI_B_TRANS"` request uses, preserving the exact variant.
    #[test]
    fn parse_protocol_id_from_resource_resolves_compound_chx_suffix_via_resource_name() {
        let resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceName(
                "SCI_B_TRANS_CH3".to_string(),
            );
        let (protocol, row, pin_selection, channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::SCI_B_TRANS);
        assert!(row.is_some(), "SCI_B_TRANS should match a table row");
        assert_eq!(pin_selection, None);
        assert_eq!(
            channel_selection,
            Some((
                j2534_0404::SCI_B_TRANS,
                j2534_0404::PROTOCOL_J2610_CH1 + 2,
                3
            ))
        );
    }

    /// Same core case via `RscData::ProtocolName`.
    #[test]
    fn parse_protocol_id_from_resource_resolves_compound_chx_suffix_via_protocol_name() {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(
                    vci_service_interface::resource_data::Protocol::ProtocolName(
                        "SCI_B_TRANS_CH3".to_string(),
                    ),
                ),
            },
        );
        let (protocol, row, pin_selection, channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::SCI_B_TRANS);
        assert!(row.is_some(), "SCI_B_TRANS should match a table row");
        assert_eq!(pin_selection, None);
        assert_eq!(
            channel_selection,
            Some((
                j2534_0404::SCI_B_TRANS,
                j2534_0404::PROTOCOL_J2610_CH1 + 2,
                3
            ))
        );
    }

    /// Lowercase suffix (`"_ch3"`) resolves identically -- the whole compound
    /// string is matched case-insensitively.
    #[test]
    fn parse_protocol_id_from_resource_resolves_compound_chx_suffix_case_insensitively() {
        let resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceName(
                "sci_b_trans_ch3".to_string(),
            );
        let (protocol, _row, _pin_selection, channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::SCI_B_TRANS);
        assert_eq!(
            channel_selection,
            Some((
                j2534_0404::SCI_B_TRANS,
                j2534_0404::PROTOCOL_J2610_CH1 + 2,
                3
            ))
        );
    }

    /// A compound name on a non-SCI, unambiguous family (`"ISO_15765_2_CH5"`)
    /// produces the exact same tuple the numeric direct-`_CHx`-id route
    /// would, since the base id there is already unambiguous (no collapse
    /// like SAE J2610's).
    #[test]
    fn parse_protocol_id_from_resource_resolves_compound_chx_suffix_on_a_non_sci_family() {
        let resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceName(
                "ISO_15765_2_CH5".to_string(),
            );
        let (protocol, _row, pin_selection, channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::ISO15765);
        assert_eq!(pin_selection, None);
        assert_eq!(
            channel_selection,
            Some((
                j2534_0404::ISO15765,
                j2534_0404::PROTOCOL_ISO15765_CH1 + 4,
                5
            ))
        );

        // Confirms the "same tuple as the direct numeric _CHx route" claim.
        let raw_chx = j2534_0404::PROTOCOL_ISO15765_CH1 + 4;
        let direct_resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceId(raw_chx);
        let (_protocol, _row, _pin_selection, direct_channel_selection) =
            J2534Service::parse_protocol_id_from_resource(direct_resource, true).unwrap();
        assert_eq!(channel_selection, direct_channel_selection);
    }

    /// A compound name whose head resolves to a `_PS`-shaped id with default
    /// pins (`pin_selection` canonicalizes to `None`, so only the reinstated
    /// `is_ps_protocol_id` disjunct catches this) is rejected -- two
    /// irreconcilable connect identities (clause 6 `_PS` and clause 7
    /// `_CHx`), even though `pin_selection` alone reports "nothing to see
    /// here." Only reachable via `RscData::ProtocolName`, since `_PS`
    /// canonicalization needs real `dlc_pin_data`, which `ResourceName`
    /// cannot carry.
    #[test]
    fn parse_protocol_id_from_resource_rejects_a_compound_chx_suffix_whose_head_is_a_ps_id_with_default_pins()
     {
        let name = format!("{}_CH3", j2534_0404::PROTOCOL_CAN_PS);
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![pin(6, "HI"), pin(14, "LOW")],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolName(name)),
            },
        );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
            Err(status) => status,
            Ok(_) => panic!(
                "a compound name whose head is a _PS id with default-canonicalizing pins must \
                 be rejected, not silently connect as a clause 7 Additional Channel"
            ),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// A compound name whose head is ITSELF the decimal string of a raw
    /// `_CHx` id is double qualification, reachable end-to-end: the head
    /// resolves through `resolve_protocol_name`'s numeric-string fallback to
    /// `ChannelProtocol::from_raw(<that _CHx id>)`, so `already_chx` and
    /// `requested_index.is_some()` are both true once
    /// `resolve_channel_selection` runs.
    #[test]
    fn parse_protocol_id_from_resource_rejects_a_compound_chx_suffix_whose_head_is_already_a_chx_id()
     {
        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let name = format!("{raw_chx}_CH3");
        let resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceName(name);
        let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
            Err(status) => status,
            Ok(_) => panic!(
                "a compound name whose head is already a _CHx id must be rejected as double \
                 qualification"
            ),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Out-of-range suffix indices (`_CH0`, `_CH129`) are rejected via the
    /// same `1..=128` range `resources::chx_protocol_id` enforces for every
    /// other route.
    #[test]
    fn parse_protocol_id_from_resource_rejects_an_out_of_range_compound_chx_suffix() {
        for suffix_name in ["CAN_CH0", "CAN_CH129"] {
            let resource =
                vci_service_interface::create_com_logical_link_request::Resource::ResourceName(
                    suffix_name.to_string(),
                );
            let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
                Err(status) => status,
                Ok(_) => panic!("an out-of-range compound suffix index must be rejected"),
            };
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    /// An unresolvable head (`"NOT_A_REAL_PROTOCOL_CH3"`) reports the same
    /// "not a valid protocol name" error every other unresolvable name does
    /// -- the compound-name fallback does not manufacture a different error
    /// for an unresolvable head.
    #[test]
    fn parse_protocol_id_from_resource_rejects_a_compound_chx_suffix_with_an_unresolvable_head() {
        let resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceName(
                "NOT_A_REAL_PROTOCOL_CH3".to_string(),
            );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
            Err(status) => status,
            Ok(_) => panic!("an unresolvable compound-name head must be rejected"),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// The compound-name route requires the clause-5 J2534-2 opt-in, exactly
    /// like every other Additional Channels route.
    #[test]
    fn parse_protocol_id_from_resource_rejects_a_compound_chx_suffix_when_not_opted_in() {
        let resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceName(
                "SCI_B_TRANS_CH3".to_string(),
            );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, false) {
            Err(status) => status,
            Ok(_) => panic!("a non-opted-in module must reject a compound _CHx-suffixed name"),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// A canonical table-row name with a non-default disambiguating pin
    /// combined with a compound `_CHx` suffix still resolves both:
    /// `dlc_pin_data` here disambiguates a specific `SAE_J2610_SCI` row
    /// configuration (row IDENTIFICATION, not clause-6 Pin Selection --
    /// `pin_selection` stays `None` regardless, ADR-156 Decision 2 scoping),
    /// while the compound suffix's index resolves normally alongside it.
    /// True negative for the mutual-exclusion widening: this must never trip
    /// the `is_ps_protocol_id`-based rejection.
    #[test]
    fn parse_protocol_id_from_resource_resolves_compound_chx_suffix_alongside_pin_disambiguated_table_row_name()
     {
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![pin(9, "TX"), pin(15, "RX")],
                bus_type: None,
                protocol: Some(
                    vci_service_interface::resource_data::Protocol::ProtocolName(
                        "SAE_J2610_on_SAE_J2610_SCI_CH2".to_string(),
                    ),
                ),
            },
        );
        let (_protocol, row, pin_selection, channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert!(
            row.is_some(),
            "SAE_J2610_on_SAE_J2610_SCI + SCI_B_TRANS's pins should match exactly one row"
        );
        assert_eq!(
            pin_selection, None,
            "dlc_pin_data here disambiguates a table row, not a Pin Selection request"
        );
        assert_eq!(
            channel_selection,
            Some((
                j2534_0404::SCI_B_TRANS,
                j2534_0404::PROTOCOL_J2610_CH1 + 1,
                2
            )),
            "the compound suffix's index must still resolve normally alongside pin-disambiguated \
             row identification"
        );
    }

    /// End-to-end via `parse_protocol_id_from_resource`'s `RscData::ProtocolId`
    /// route, naming the `_CHx` id directly via `protocol_id` -- decomposed
    /// and accepted, with `protocol` normalized to the base id.
    #[test]
    fn parse_protocol_id_from_resource_resolves_channel_selection_via_direct_chx_protocol_id() {
        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                    raw_chx,
                )),
            },
        );
        let (protocol, _row, pin_selection, channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(
            protocol.value(),
            j2534_0404::CAN,
            "protocol must normalize to the base id, never the raw _CHx value"
        );
        assert_eq!(pin_selection, None);
        assert_eq!(channel_selection, Some((j2534_0404::CAN, raw_chx, 5)));
    }

    /// Bare `ResourceId`/`ResourceName` routes (no `RscData`, so no
    /// `channel_index` field at all) can still numerically name a `_CHx` id
    /// directly -- decomposed and accepted the same way, unlike a bare `_PS`
    /// id on the same routes (rejected for missing pins).
    #[test]
    fn parse_protocol_id_from_resource_decomposes_a_bare_chx_resource_id() {
        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceId(raw_chx);
        let (protocol, _row, pin_selection, channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::CAN);
        assert_eq!(pin_selection, None);
        assert_eq!(channel_selection, Some((j2534_0404::CAN, raw_chx, 5)));
    }

    #[test]
    fn parse_protocol_id_from_resource_decomposes_a_bare_chx_resource_name() {
        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let resource =
            vci_service_interface::create_com_logical_link_request::Resource::ResourceName(
                raw_chx.to_string(),
            );
        let (protocol, _row, pin_selection, channel_selection) =
            J2534Service::parse_protocol_id_from_resource(resource, true).unwrap();
        assert_eq!(protocol.value(), j2534_0404::CAN);
        assert_eq!(pin_selection, None);
        assert_eq!(channel_selection, Some((j2534_0404::CAN, raw_chx, 5)));
    }

    /// A directly-named `_CHx` id combined with non-default `dlc_pin_data`
    /// is rejected the same way, extended to the direct-naming route.
    #[test]
    fn parse_protocol_id_from_resource_rejects_direct_chx_naming_with_non_default_pins() {
        let raw_chx = j2534_0404::PROTOCOL_CAN_CH1 + 4; // _CH5
        let resource = vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![pin(3, "HI"), pin(11, "LOW")],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                    raw_chx,
                )),
            },
        );
        let status = match J2534Service::parse_protocol_id_from_resource(resource, true) {
            Err(status) => status,
            Ok(_) => {
                panic!("a directly-named _CHx id with non-default dlc_pin_data must be rejected")
            }
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}
