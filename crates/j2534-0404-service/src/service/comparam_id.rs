//! Type-level separation between D-PDU ComParam IDs and native J2534 config
//! parameter IDs. See ADR-027 and ADR-028.

#[cfg(test)]
use super::PARAM_ANALOG_SAMPLE_RATE;
#[cfg(test)]
use super::PARAM_N_BR;
use super::resources;
#[cfg(test)]
use super::{PARAM_ACCESS_TIMING_ECU, PARAM_ACCESS_TIMING_OVERRIDE};
use super::{
    PARAM_ANALOG_ACTIVE_CHANNELS, PARAM_ANALOG_AVERAGING_METHOD, PARAM_ANALOG_READINGS_PER_MSG,
    PARAM_ANALOG_SAMPLES_PER_READING, PARAM_CAN_FILLER_BYTE, PARAM_CANFD_TX_MAX_DATA_LENGTH,
    PARAM_CHANGE_SPEED_CTRL, PARAM_CHANGE_SPEED_RATE, PARAM_CHANGE_SPEED_RES_CTRL, PARAM_N_BS,
    PARAM_N_CR, PARAM_N_CS, PARAM_TP20_BROADCAST_INTERVAL, PARAM_UEB_T0_MIN, PARAM_UEB_T1_MAX,
    PARAM_UEB_T2_MAX, PARAM_UEB_T3_MAX, PARAM_UEB_T4_MIN, PARAM_UEB_T5_MAX, PARAM_UEB_T6_MAX,
    PARAM_UEB_T7_MAX, PARAM_UEB_T7_MIN, PARAM_UEB_T9_MIN,
};
#[cfg(test)]
use super::{
    PARAM_TP20_APPLICATION_TYPE, PARAM_TP20_BROADCAST_ADDRESS, PARAM_TP20_CHANNEL_SETUP_CAN_ID,
    PARAM_TP20_DESTINATION_ADDRESS, PARAM_TP20_PASSIVE_IDENTIFIER, PARAM_TP20_PASSIVE_RX_ID,
    PARAM_TP20_RX_ID_PROPOSAL, PARAM_TP20_TX_ID_PROPOSAL,
};
#[cfg(test)]
use super::{PARAM_W1_MIN, PARAM_W2_MIN, PARAM_W3_MIN, PARAM_W4_MAX};

/// A D-PDU-style ComParam ID, as received via `SetComParam`/`GetComParam`
/// and stored in `ComParamSet`.
///
/// Distinct from the plain `u32` that
/// `j2534_0404::J2534Api0404::set_config_u32`/`get_config_u32` accept (a
/// native J2534 config parameter ID), even though the two ID spaces
/// currently overlap numerically for the great majority of parameters that
/// map 1:1 onto a J2534 native config parameter (see `to_j2534_config_id`).
/// **Exception (ADR-159/Phase 3b):** on an `FD_ISO15765_PS` link
/// specifically, `CP_CANFDTxMaxDataLength`/`CP_Cr`/`CP_CanFillerByte`
/// translate to a *different* native config id than the one they numerically
/// share a value with everywhere else -- `to_j2534_config_id`'s first
/// non-identity translations. This type exists so a ComParam ID can never be
/// passed directly to `set_config`/`get_config` without going through an
/// explicit, reviewable translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComParamId(pub(super) u32);

impl ComParamId {
    /// Whether this ComParam ID falls in SAE J2534-1 §7.2.14.3/§7.3.2's
    /// tool-manufacturer-reserved range (`0x0001_0000..=0xFFFF_FFFF`,
    /// ADR-219 Context), used identically for both `IoctlID` and
    /// `ConfigParameterID`. `to_j2534_config_id`'s identity-passthrough
    /// catch-all, `rpc_link.rs`'s unstaged-vendor-read gate, and
    /// `comparam_support::is_param_allowed`'s unconditional-admit early
    /// return (ADR-219 amendment) all share this one boundary check instead
    /// of each repeating the `0x10000` literal.
    ///
    /// **Invariant this depends on:** no service-minted `ComParamId` (every
    /// `PARAM_*` constant in `service_params.rs`) may ever be `>= 0x10000`
    /// -- all of them currently sit in `0x8001`-`0x80E3`, well below this
    /// boundary (enforced by
    /// `comparam_support::tests::all_service_minted_comparam_ids_are_below_vendor_boundary`).
    /// If that invariant were ever violated, `is_param_allowed`'s vendor
    /// early return would silently shadow that id's protocol-specific
    /// exclusion, admitting it on every protocol regardless of the
    /// allowlist that id would otherwise be subject to.
    pub(super) fn is_vendor(self) -> bool {
        self.0 >= 0x0001_0000
    }

    /// Translates this ComParam ID to its corresponding J2534 native config
    /// parameter ID, if one exists **for `hw_protocol_id`** (ADR-158: the
    /// channel's actual, un-normalized -- ADR-157 Plane A -- hardware
    /// protocol ID the value would be applied to, e.g.
    /// `LogicalLinkState::hw_protocol_id`; may be a `_PS`/`_CHx`/FD variant).
    ///
    /// Returns `None` for service-level ComParam IDs (0x8000-0x80FF), any
    /// other ID with no J2534 `SET_CONFIG`/`GET_CONFIG` equivalent for any
    /// protocol, and any native config ID not supported on `hw_protocol_id`
    /// specifically -- these must never reach `PassThruIoctl`. This is the
    /// single point where a `ComParamId` may become a raw J2534 config ID;
    /// callers must not otherwise unwrap `.0` to feed `set_config`/
    /// `get_config`.
    ///
    /// The per-protocol support matrix (ADR-028), keyed by the base protocol
    /// id (`resources::base_protocol_id(hw_protocol_id)`, ADR-157 Plane B --
    /// this is the "ordinary family-arm dispatch" every row below except the
    /// CAN one uses unchanged) is:
    ///
    /// | CONFIG ID | Supported protocols |
    /// |---|---|
    /// | `DATA_RATE`, `LOOPBACK` | all |
    /// | `NODE_ADDRESS`, `NETWORK_LINE` | J1850PWM |
    /// | `P1_MAX`, `P3_MIN`, `P4_MIN`, `W1`-`W4`, `TIDLE`, `TINIL`, `TWUP`, `PARITY`, `DATA_BITS`, `FIVE_BAUD_MOD` | ISO9141, ISO14230 |
    /// | `W0` | ISO9141 |
    /// | `W5` | ISO14230 |
    /// | `BIT_SAMPLE_POINT`, `SYNC_JUMP_WIDTH` | CAN, but NOT an FD_CAN_PS link (ADR-158, narrow supersession of ADR-157: SAE J2534-2 clause 21.3.2.5.1 makes these two read-only on CAN FD -- checked directly against the raw `hw_protocol_id`, since `base_protocol_id`'s FD arm would otherwise collapse the distinction away before this function ever saw it), and also TP2_0_PS (ADR-188/Phase 7 Stage 7a: TP2.0 physically rides a CAN transceiver and allowlists both, `comparam_support::is_tp20_param`, even though it self-identifies rather than normalizing onto CAN via `base_protocol_id`) |
    /// | `T1_MAX`-`T5_MAX` | SCI_A_ENGINE, SCI_A_TRANS, SCI_B_ENGINE, SCI_B_TRANS |
    /// | `ISO15765_BS`, `ISO15765_STMIN`, `BS_TX`, `STMIN_TX`, `ISO15765_WFT_MAX` | ISO15765 |
    ///
    /// `P1_MIN`, `P2_MIN`, `P2_MAX`, `P3_MAX`, `P4_MAX` have no J2534
    /// `SET_CONFIG`/`GET_CONFIG` support for any protocol and always
    /// translate to `None`.
    ///
    /// **Non-identity exception (ADR-159/Phase 3b):** on an
    /// `FD_ISO15765_PS` link specifically -- checked against the raw
    /// `hw_protocol_id`, before the table above's `base_protocol_id`
    /// normalization, mirroring the FD/`BIT_SAMPLE_POINT` check's own
    /// ordering -- three ComParams translate to a *different* native config
    /// id than the one the table above (or the identity fallback) would
    /// otherwise give them: `CP_CANFDTxMaxDataLength` -> `CONFIG_FD_ISO15765_TX_DATA_LENGTH`,
    /// `CP_Cr` -> `CONFIG_N_CR_MAX`, `CP_CanFillerByte` -> `CONFIG_ISO15765_PAD_VALUE`.
    /// Every other ComParam ID, and these three on every other hardware id,
    /// still resolve exactly as the table above and the identity/`None`
    /// contract otherwise describe.
    ///
    /// **Non-identity exception (ADR-179/Phase 5):** on a J1939_PS/_CHx link
    /// specifically (`resources::is_j1939_protocol_id`) -- checked against
    /// the raw `hw_protocol_id`, the same "before `base_protocol_id`
    /// normalization" ordering the FD_ISO15765_PS exception above uses --
    /// five ComParams translate to a native `CONFIG_J1939_*` id
    /// **non-ordinally** (ADR-179's Context section table): `CP_Cr`
    /// (`PARAM_N_CR`) -> `CONFIG_J1939_T1`, `CP_T5Max` (native `T5_MAX`) ->
    /// `CONFIG_J1939_T2`, `CP_T4Max` (native `T4_MAX`) -> `CONFIG_J1939_T3`,
    /// `CP_Bs` (`PARAM_N_BS`) -> `CONFIG_J1939_T4`, `CP_Cs` (`PARAM_N_CS`) ->
    /// `CONFIG_J1939_BRDCST_MIN_DELAY`. `CP_T3Max` (native `T3_MAX`) and
    /// `CP_Br` (`PARAM_N_BR`) have no native counterpart at all and stay
    /// `None` even on a J1939 link (Table 60 defines neither).
    pub(super) fn to_j2534_config_id(self, hw_protocol_id: u32) -> Option<u32> {
        // ADR-219 Decision item 3: an unrecognized id `>= 0x10000` (checked
        // via `is_vendor()`) is the SAE J2534-1 §7.2.14.3/§7.3.2
        // tool-manufacturer `ConfigParameterID` range -- forwarded to native
        // `SET_CONFIG`/`GET_CONFIG` by identity (the same unchanged-value
        // passthrough shape `DATA_RATE`/`LOOPBACK` below already use, not a
        // new pattern), for every `hw_protocol_id`. Checked ahead of every
        // other branch, mirroring the ADR-158 FD check just below.
        if self.is_vendor() {
            return Some(self.0);
        }
        // ADR-158: checked first, against the raw (un-normalized) id -- once
        // `base_protocol_id` below collapses an FD_CAN_PS link onto plain
        // CAN, the FD distinction these two params care about is gone.
        if resources::is_fd_protocol_id(hw_protocol_id)
            && matches!(
                self.0,
                j2534_0404::BIT_SAMPLE_POINT | j2534_0404::SYNC_JUMP_WIDTH
            )
        {
            return None;
        }
        // ADR-159/Phase 3b: this function's own first non-identity
        // translations. Checked against the raw `hw_protocol_id` too, before
        // `base_protocol_id` below collapses `PROTOCOL_FD_ISO15765_PS`/its
        // own `_CHx` block onto plain `ISO15765` -- these three ComParams
        // translate differently on an FD_ISO15765_PS (or, as of ADR-213, an
        // FD_ISO15765 `_CHx`) link specifically, not on ISO15765 in general.
        // ADR-213/Round 3 (edge-case-hunter finding, PR review): widened
        // from a bare `_PS` exact-match to also recognize the `_CH1..128`
        // range -- without this, a `_CHx`-connected FD ISO15765 link (now
        // reachable via `apply_fd_mode`'s index-aware promotion) silently
        // fell through to the generic identity/`None` table for all three
        // ComParams, so `SET_CONFIG` was never issued for TX data length,
        // N_Cr timing, or the pad byte, with no error surfaced.
        if hw_protocol_id == j2534_0404::PROTOCOL_FD_ISO15765_PS
            || (j2534_0404::PROTOCOL_FD_ISO15765_CH1..=j2534_0404::PROTOCOL_FD_ISO15765_CH128)
                .contains(&hw_protocol_id)
        {
            if self == PARAM_CANFD_TX_MAX_DATA_LENGTH {
                return Some(j2534_0404::CONFIG_FD_ISO15765_TX_DATA_LENGTH);
            }
            if self == PARAM_N_CR {
                return Some(j2534_0404::CONFIG_N_CR_MAX);
            }
            if self == PARAM_CAN_FILLER_BYTE {
                return Some(j2534_0404::CONFIG_ISO15765_PAD_VALUE);
            }
        }
        // ADR-164 Decision 2 (SAE J2534-2 clause 9 Single Wire CAN, Phase 4):
        // three of the five `CP_ChangeSpeed*` ComParams translate to native
        // SWCAN CONFIG ids on an SW link specifically -- checked against the
        // raw `hw_protocol_id` (an SW row's own `hw_protocol_override` IS
        // the `_PS` id directly, so there is no `base_protocol_id`
        // normalization step to place this before, unlike the FD checks
        // above). `CP_ChangeSpeedMsg`/`CP_ChangeSpeedTxDelay` have no
        // documented native mapping (ISO 22900-2:2009 Annex A.1.2 Table A.3
        // omits them) and intentionally fall through to the `_ => false` arm
        // below, on every hardware id including SW. ADR-212/Round 2: re-keyed
        // from `is_sw_protocol_id` to the family-wide `is_sw_family_protocol_
        // id` so this translation still fires for a `_CHx`-connected SW link
        // -- the raw-`hw_protocol_id`, no-normalization reasoning above still
        // holds, this predicate is just now family-wide rather than
        // `_PS`-only.
        if resources::is_sw_family_protocol_id(hw_protocol_id) {
            if self == PARAM_CHANGE_SPEED_RATE {
                return Some(j2534_0404::CONFIG_SW_CAN_HS_DATA_RATE);
            }
            if self == PARAM_CHANGE_SPEED_CTRL {
                return Some(j2534_0404::CONFIG_SW_CAN_SPEEDCHANGE_ENABLE);
            }
            if self == PARAM_CHANGE_SPEED_RES_CTRL {
                return Some(j2534_0404::CONFIG_SW_CAN_RES_SWITCH);
            }
        }
        // ADR-174/Phase 10 (Codex review, PR #63 round 3): Honda DIAG-H's own
        // ComParam allowlist (`comparam_support::is_honda_diagh_param`)
        // permits exactly `LOOPBACK`/`P1_MAX`/`P3_MIN`/`P4_MIN` -- but
        // `base_protocol_id` does not collapse `HONDA_DIAGH_PS` onto either
        // `ISO9141` or `ISO14230` (it self-maps, per `resources::
        // base_protocol_id`'s own test), so the shared P1_MAX/P3_MIN/P4_MIN
        // match arm below never recognized it, silently dropping every
        // staged value -- the seeded 20ms/55ms/5ms defaults and any client
        // `SetComParam` override alike -- instead of forwarding it via
        // `PassThruIoctl(SET_CONFIG)`. Clause 13.3.1's own "reuses ISO9141
        // timing parameters" text is exactly this: `CONFIG_P1_MAX`/`_P3_MIN`/
        // `_P4_MIN` are J2534-1's own generic native timing CONFIG ids, not
        // an ISO9141-specific mechanism, so translating them for
        // `HONDA_DIAGH_PS` too is a direct application of that clause, not a
        // new one. Checked against the raw `hw_protocol_id` (same reason as
        // the FD/SW blocks above) and scoped to exactly these three params,
        // not the wider W1-W4/TIDLE/TINIL/TWUP/PARITY/DATA_BITS/
        // FIVE_BAUD_MOD group the shared match arm below also covers --
        // clause 13's own closed allowlist never permits those others for
        // this protocol, so lumping `HONDA_DIAGH_PS` into that arm directly
        // would incorrectly claim translation support this protocol's
        // allowlist can never actually stage.
        //
        // ADR-208 Decision item 7: checked against
        // `resources::is_honda_diagh_family_protocol_id`, not the narrower
        // `is_honda_diagh_protocol_id` arm-gate predicate -- this
        // translation is for a *connected link's* raw `hw_protocol_id`
        // (same reason as the FD/SW/J1939 blocks around this one), and
        // P1/P3/P4 timing semantics apply identically whether the link is
        // `_PS`- or `_CHx`-connected (clause 7 Additional Channels leaves
        // clause 13's own timing allowlist untouched). Left on the narrow
        // predicate, a `_CHx`-connected Honda DIAG-H link would silently
        // lose its P1/P3/P4 native timing translation -- `SetComParam`/the
        // seeded Working defaults would stop reaching hardware for the
        // `_CHx` route only. This is a genuinely new gap class ADR-207
        // never hit: UART Echo Byte/GM UART have no non-ordinal
        // `comparam_id.rs` translation block of their own to compare
        // against, and SAE J1939's analogous block just below happens to
        // already be safe only because `is_j1939_protocol_id` was
        // independently written range-inclusive from its own inception
        // (predating `_CHx` support entirely), not because anyone
        // deliberately widened it for this reason.
        if resources::is_honda_diagh_family_protocol_id(hw_protocol_id)
            && matches!(
                self.0,
                j2534_0404::P1_MAX | j2534_0404::P3_MIN | j2534_0404::P4_MIN
            )
        {
            return Some(self.0);
        }
        // ADR-179/Phase 5 (SAE J2534-2 clause 16 SAE J1939), Decision 5: five
        // ComParams translate to native CONFIG_J1939_* targets on a J1939
        // link specifically -- checked against the raw `hw_protocol_id`
        // (J1939_PS/_CHx self-identify, like Honda DIAG-H/J1708 above, so
        // there is no `base_protocol_id` normalization step to place this
        // before). Non-ordinal: ISO 22900-2's own CP_Cr/CP_T5Max/CP_T4Max/
        // CP_Bs/CP_Cs entries map to native J1939_T1/_T2/_T3/_T4/
        // _BRDCST_MIN_DELAY respectively (ADR-179's Context table, verified
        // directly against ISO 22900-2:2022) -- NOT an ordinal T1<->T1/
        // T2<->T2/... correspondence. `CP_T3Max` (now the native `T3_MAX`
        // ComParamId, see its retirement in `service_params.rs`) and `CP_Br`
        // (`PARAM_N_BR`) have no native counterpart at all (Table 60 defines
        // none for either) and are intentionally absent here -- an accepted
        // residual, ADR-179 Decision 5.
        if resources::is_j1939_protocol_id(hw_protocol_id) {
            if self == PARAM_N_CR {
                return Some(j2534_0404::CONFIG_J1939_T1);
            }
            if self == ComParamId(j2534_0404::T5_MAX) {
                return Some(j2534_0404::CONFIG_J1939_T2);
            }
            if self == ComParamId(j2534_0404::T4_MAX) {
                return Some(j2534_0404::CONFIG_J1939_T3);
            }
            if self == PARAM_N_BS {
                return Some(j2534_0404::CONFIG_J1939_T4);
            }
            if self == PARAM_N_CS {
                return Some(j2534_0404::CONFIG_J1939_BRDCST_MIN_DELAY);
            }
        }
        // ADR-192/Phase 7 Stage 7c: `PARAM_TP20_BROADCAST_INTERVAL` translates
        // to the native `CONFIG_TP2_0_T_BR_INT` (`0x8044`) on a TP2.0 link --
        // a non-ordinal translation (the ComParam id `0x80D1` does not equal
        // the native config id numerically), so it needs its own explicit arm
        // here rather than falling into the generic identity-mapped match
        // below, the same shape the J1939 block above uses for its own
        // non-ordinal translations. `PARAM_TP20_BROADCAST_ADDRESS` is
        // deliberately NOT handled here -- it is service-level-only (see the
        // catch-all's doc comment below) and always falls through to `None`.
        // ADR-210 Decision item 8: re-keyed from the narrow
        // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id` -- this
        // checks the raw, un-normalized `hw_protocol_id` parameter directly
        // (unlike the `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` block below, which
        // already checks the normalized `j2534_protocol_id` and needs no
        // change), so a `_CHx`-connected TP2.0 link would otherwise silently
        // lose this translation.
        if resources::is_tp2_0_family_protocol_id(hw_protocol_id)
            && self == PARAM_TP20_BROADCAST_INTERVAL
        {
            return Some(j2534_0404::CONFIG_TP2_0_T_BR_INT);
        }
        // ADR-216 Decision item 4: the four writable Analog Inputs ComParams
        // translate 1:1 onto their native `CONFIG_*` ids on an ANALOG_IN_x
        // link specifically -- checked against the raw `hw_protocol_id`
        // (ANALOG_IN_x ids self-identify, like J1939/TP2.0 above, so there is
        // no `base_protocol_id` normalization step to place this before).
        // The three read-only Analog Inputs ComParams (SAMPLE_RESOLUTION/
        // INPUT_RANGE_LOW/INPUT_RANGE_HIGH) are deliberately NOT handled
        // here -- they are never staged into a SET_CONFIG batch in the first
        // place (`rpc_set_com_param` rejects any SetComParam attempt against
        // them outright, ADR-216 Decision item 5), so they always fall
        // through to the catch-all `None` below, same as every other
        // service-level-only ComParam.
        if resources::is_analog_in_protocol_id(hw_protocol_id) {
            if self == PARAM_ANALOG_ACTIVE_CHANNELS {
                return Some(j2534_0404::CONFIG_ACTIVE_CHANNELS);
            }
            if self == PARAM_ANALOG_SAMPLES_PER_READING {
                return Some(j2534_0404::CONFIG_SAMPLES_PER_READING);
            }
            if self == PARAM_ANALOG_READINGS_PER_MSG {
                return Some(j2534_0404::CONFIG_READINGS_PER_MSG);
            }
            if self == PARAM_ANALOG_AVERAGING_METHOD {
                return Some(j2534_0404::CONFIG_AVERAGING_METHOD);
            }
        }
        // ADR-216 Decision item 4: the ten UEB timing ComParams translate 1:1
        // onto their native `CONFIG_UEB_*` ids on a UART Echo Byte link --
        // keyed on the FAMILY-WIDE `is_uart_echo_byte_family_protocol_id`
        // predicate (not the narrower `_PS`-only
        // `is_uart_echo_byte_protocol_id`), mirroring the Honda DIAG-H/TP2.0
        // blocks above: ADR-213's own Round 3 fix already established that a
        // translation arm keyed on a bare `_PS` id silently loses its
        // translation for a `_CHx`-connected link of the same family, and
        // UEB has shipped `_CHx` support since ADR-207.
        if resources::is_uart_echo_byte_family_protocol_id(hw_protocol_id) {
            if self == PARAM_UEB_T0_MIN {
                return Some(j2534_0404::CONFIG_UEB_T0_MIN);
            }
            if self == PARAM_UEB_T1_MAX {
                return Some(j2534_0404::CONFIG_UEB_T1_MAX);
            }
            if self == PARAM_UEB_T2_MAX {
                return Some(j2534_0404::CONFIG_UEB_T2_MAX);
            }
            if self == PARAM_UEB_T3_MAX {
                return Some(j2534_0404::CONFIG_UEB_T3_MAX);
            }
            if self == PARAM_UEB_T4_MIN {
                return Some(j2534_0404::CONFIG_UEB_T4_MIN);
            }
            if self == PARAM_UEB_T5_MAX {
                return Some(j2534_0404::CONFIG_UEB_T5_MAX);
            }
            if self == PARAM_UEB_T6_MAX {
                return Some(j2534_0404::CONFIG_UEB_T6_MAX);
            }
            if self == PARAM_UEB_T7_MIN {
                return Some(j2534_0404::CONFIG_UEB_T7_MIN);
            }
            if self == PARAM_UEB_T7_MAX {
                return Some(j2534_0404::CONFIG_UEB_T7_MAX);
            }
            if self == PARAM_UEB_T9_MIN {
                return Some(j2534_0404::CONFIG_UEB_T9_MIN);
            }
        }
        let j2534_protocol_id = resources::base_protocol_id(hw_protocol_id);
        let supported = match self.0 {
            j2534_0404::DATA_RATE | j2534_0404::LOOPBACK => true,
            j2534_0404::NODE_ADDRESS | j2534_0404::NETWORK_LINE => {
                j2534_protocol_id == j2534_0404::J1850PWM
            }
            j2534_0404::P1_MAX
            | j2534_0404::P3_MIN
            | j2534_0404::P4_MIN
            | j2534_0404::W1
            | j2534_0404::W2
            | j2534_0404::W3
            | j2534_0404::W4
            | j2534_0404::TIDLE
            | j2534_0404::TINIL
            | j2534_0404::TWUP
            | j2534_0404::PARITY
            | j2534_0404::DATA_BITS
            | j2534_0404::FIVE_BAUD_MOD => {
                matches!(
                    j2534_protocol_id,
                    j2534_0404::ISO9141 | j2534_0404::ISO14230
                )
            }
            j2534_0404::W0 => j2534_protocol_id == j2534_0404::ISO9141,
            j2534_0404::W5 => j2534_protocol_id == j2534_0404::ISO14230,
            j2534_0404::BIT_SAMPLE_POINT | j2534_0404::SYNC_JUMP_WIDTH => {
                // ADR-188/Phase 7 Stage 7a: TP2_0_PS self-identifies (no
                // `base_protocol_id` normalization onto CAN, see that
                // function's own doc comment), so it needs its own arm here
                // even though it physically rides a CAN transceiver.
                matches!(
                    j2534_protocol_id,
                    j2534_0404::CAN | j2534_0404::PROTOCOL_TP2_0_PS
                )
            }
            j2534_0404::T1_MAX
            | j2534_0404::T2_MAX
            | j2534_0404::T3_MAX
            | j2534_0404::T4_MAX
            | j2534_0404::T5_MAX => matches!(
                j2534_protocol_id,
                j2534_0404::SCI_A_ENGINE
                    | j2534_0404::SCI_A_TRANS
                    | j2534_0404::SCI_B_ENGINE
                    | j2534_0404::SCI_B_TRANS
            ),
            j2534_0404::ISO15765_BS
            | j2534_0404::ISO15765_STMIN
            | j2534_0404::BS_TX
            | j2534_0404::STMIN_TX
            | j2534_0404::ISO15765_WFT_MAX => j2534_protocol_id == j2534_0404::ISO15765,
            // P1_MIN, P2_MIN, P2_MAX, P3_MAX, P4_MAX, and every service-level
            // ComParam ID: no J2534 SET_CONFIG/GET_CONFIG support for any protocol.
            // This catch-all also covers ADR-190/Phase 7 Stage 7b's two minted
            // TP2.0 passive-connection ComParams (`PARAM_TP20_PASSIVE_IDENTIFIER`,
            // `PARAM_TP20_PASSIVE_RX_ID`) *deliberately*, not as an oversight:
            // even though native `CONFIG_TP2_0_IDENTIFER`/`_RXIDPASSIVE`
            // (`0x804E`/`0x804F`) constants already exist, routing these two
            // ComParams through this generic per-ComParam translation pipeline
            // would let ordinary ComParam application (connect-time, or a
            // `CoptUpdateparam` on an already-active link) arm or re-arm the
            // device-side passive listener outside ADR-190 section 1's own
            // arm/disarm lifecycle and section 2's exclusivity gate.
            // `handle_start_comm`'s passive arm issues the two native
            // `SET_CONFIG` calls itself, directly, exactly when arming.
            // Also covers ADR-192/Phase 7 Stage 7c's `PARAM_TP20_BROADCAST_ADDRESS`:
            // service-level-only (there is no native `SET_CONFIG` parameter
            // for a per-message address at all), read directly by the
            // `CoptSendrecv` TX-flags/addressing resolution path instead.
            //
            // Note: an id `>= 0x10000` never reaches this match at all --
            // `is_vendor()` returns `Some(self.0)` above, before this
            // function's own `hw_protocol_id` normalization even runs.
            _ => false,
        };
        supported.then_some(self.0)
    }
}

/// Converts a ComParam value into the raw value to write via `PassThruIoctl
/// SET_CONFIG` for the native J2534 CONFIG ID `config_id` (as returned by
/// `ComParamId::to_j2534_config_id`), applying any unit conversion between
/// D-PDU ComParam encoding and native J2534 encoding.
///
/// - `STMIN_TX`: ISO 15765-2 STmin byte encoding, see
///   `stmin_override_to_stmin_tx` (ADR-037).
/// - `P1_MAX`: ISO 22900-2's 1 us `CP_*` resolution converts to J2534's
///   native 0.5 ms resolution via `us_to_half_ms` (ADR-072), clamped to the
///   native `$0001`-`$FFFF` range -- SAE J2534-1 Figure 30's `P1_MAX` row is
///   the only one of this whole timing group with a nonzero floor.
/// - `P3_MIN`, `P4_MIN`: same `us_to_half_ms` conversion, clamped to
///   `$0000`-`$FFFF` (Figure 30's floor for these two is `0`, so only the
///   `0xFFFF` ceiling needs enforcing).
/// - `W0`-`W5`, `TIDLE`, `TINIL`, `TWUP`, `T1_MAX`-`T5_MAX`,
///   `CONFIG_J1939_T1`-`_T4`/`_BRDCST_MIN_DELAY` (ADR-179/Phase 5): ISO
///   22900-2's 1 us `CP_*` resolution converts to J2534's native 1 ms
///   resolution via `us_to_ms` (ADR-072), clamped to `$0000`-`$FFFF` (Figure
///   30, and SAE J2534-2 clause 16's own value tables for the `CONFIG_J1939_*`
///   targets, all document a `0` floor for this group -- only the `0xFFFF`
///   ceiling needs enforcing).
/// - `CONFIG_FD_ISO15765_TX_DATA_LENGTH` (ADR-159/Phase 3b): `CP_CANFDTxMaxDataLength`'s
///   FD_ISO15765_PS target -- floors a staged `0` (unset) to `8`.
/// - `CONFIG_N_CR_MAX` (ADR-159/Phase 3b): `CP_Cr`'s FD_ISO15765_PS target --
///   ISO 22900-2's 1 us `CP_*` resolution converts to Table 97's native ms
///   resolution via `us_to_ms`, clamped to the native `$0001`-`$FFFF` range.
/// - Every other native CONFIG ID (including `CONFIG_ISO15765_PAD_VALUE`,
///   `CP_CanFillerByte`'s FD_ISO15765_PS target) uses identical units/
///   encoding on both sides, so the value passes through unchanged.
pub(crate) fn to_j2534_config_value(config_id: u32, value: u32) -> u32 {
    match config_id {
        j2534_0404::STMIN_TX => stmin_override_to_stmin_tx(value),
        // Figure 30's only nonzero floor in this timing group.
        j2534_0404::P1_MAX => us_to_half_ms(value).clamp(1, 0xFFFF),
        j2534_0404::P3_MIN | j2534_0404::P4_MIN => us_to_half_ms(value).min(0xFFFF),
        j2534_0404::W0
        | j2534_0404::W1
        | j2534_0404::W2
        | j2534_0404::W3
        | j2534_0404::W4
        | j2534_0404::W5
        | j2534_0404::TIDLE
        | j2534_0404::TINIL
        | j2534_0404::TWUP
        | j2534_0404::T1_MAX
        | j2534_0404::T2_MAX
        | j2534_0404::T3_MAX
        | j2534_0404::T4_MAX
        | j2534_0404::T5_MAX
        // ADR-179/Phase 5: CONFIG_J1939_T1/_T2/_T3/_T4/_BRDCST_MIN_DELAY
        // share the same 1 us -> 1 ms ADR-072 conversion.
        | j2534_0404::CONFIG_J1939_T1
        | j2534_0404::CONFIG_J1939_T2
        | j2534_0404::CONFIG_J1939_T3
        | j2534_0404::CONFIG_J1939_T4
        | j2534_0404::CONFIG_J1939_BRDCST_MIN_DELAY => us_to_ms(value).min(0xFFFF),
        // ADR-159/Phase 3b: `CP_CANFDTxMaxDataLength`'s FD_ISO15765_PS
        // target -- a staged `0` (unset) floors to `8`, the same floor
        // `fd_mode_staged`'s own doc comment and `fd_can_tx_message_size_range`
        // already establish for an unset TX_DL, rather than letting Table
        // 97's native default (64) silently win over an explicit-but-small
        // staged value.
        j2534_0404::CONFIG_FD_ISO15765_TX_DATA_LENGTH => value.max(8),
        // `CP_Cr`'s FD_ISO15765_PS target -- ISO 22900-2's µs resolution
        // converts to Table 97's native ms resolution and range
        // (`$0001`-`$FFFF`).
        j2534_0404::CONFIG_N_CR_MAX => us_to_ms(value).clamp(1, 0xFFFF),
        _ => value,
    }
}

/// Rounds a microsecond value (ISO 22900-2 `CP_*` resolution) to the nearest
/// native J2534 0.5 ms step -- `P1_MAX`/`P3_MIN`/`P4_MIN`'s resolution
/// (ADR-072). Round-to-nearest via a saturating add, matching
/// `stmin_override_to_stmin_tx`'s rounding policy (ADR-037); saturates
/// rather than panicking on `value_us` near `u32::MAX`.
pub(super) fn us_to_half_ms(value_us: u32) -> u32 {
    value_us.saturating_add(250) / 500
}

/// Rounds a microsecond value (ISO 22900-2 `CP_*` resolution) to the nearest
/// native J2534 1 ms step -- `W0`-`W5`, `TIDLE`, `TINIL`, `TWUP`, and
/// `T1_MAX`-`T5_MAX`'s resolution (ADR-072). Round-to-nearest via a
/// saturating add, matching `stmin_override_to_stmin_tx`'s rounding policy
/// (ADR-037); saturates rather than panicking on `value_us` near
/// `u32::MAX`.
pub(crate) fn us_to_ms(value_us: u32) -> u32 {
    value_us.saturating_add(500) / 1000
}

/// `CP_UartConfig` (ISO 22900-2) values that are fully representable in
/// J2534 v04.04's `DATA_BITS`/`PARITY` `SET_CONFIG` params (ADR-071).
///
/// `CP_UartConfig` encodes data bits × parity × stop bits as `0..=17`: the
/// value is divided into 6 groups of 3 by `data bits, stop bits`
/// (`7N/7O/7E` ×1 stop, `7N/7O/7E` ×2 stop, `8N/8O/8E` ×1 stop, `8N/8O/8E`
/// ×2 stop, `9N/9O/9E` ×1 stop, `9N/9O/9E` ×2 stop), with parity (N/O/E)
/// cycling within each group of 3. J2534 v04.04 has no stop-bit `SET_CONFIG`
/// param (implicitly 1 stop bit) and `DATA_BITS` only distinguishes 7 vs. 8
/// data bits (no 9), so only the two 1-stop-bit, 7/8-data-bit groups --
/// values `0,1,2` (7N1/7O1/7E1) and `6,7,8` (8N1/8O1/8E1) -- are
/// representable; every other value (2-stop-bit or 9-data-bit encodings, or
/// out of the `0..=17` range) is rejected.
pub(crate) const UART_CONFIG_ACCEPTED: [u32; 6] = [0, 1, 2, 6, 7, 8];

/// Returns `true` if `value` is one of the 6 `CP_UartConfig` encodings J2534
/// v04.04 can represent (see [`UART_CONFIG_ACCEPTED`]).
pub(crate) fn is_valid_uart_config(value: u32) -> bool {
    UART_CONFIG_ACCEPTED.contains(&value)
}

/// Decodes an accepted `CP_UartConfig` value (see [`is_valid_uart_config`])
/// into the native J2534 `DATA_BITS` value (`0` = 8 data bits, `1` = 7 data
/// bits). Returns `None` for a value outside [`UART_CONFIG_ACCEPTED`].
pub(super) fn uart_config_to_data_bits(value: u32) -> Option<u32> {
    match value {
        0..=2 => Some(1), // 7 data bits
        6..=8 => Some(0), // 8 data bits
        _ => None,
    }
}

/// Decodes an accepted `CP_UartConfig` value (see [`is_valid_uart_config`])
/// into the native J2534 `PARITY` value (`0` = no parity, `1` = odd, `2` =
/// even). Returns `None` for a value outside [`UART_CONFIG_ACCEPTED`].
pub(crate) fn uart_config_to_parity(value: u32) -> Option<u32> {
    match value {
        0 | 6 => Some(0), // N
        1 | 7 => Some(1), // O
        2 | 8 => Some(2), // E
        _ => None,
    }
}

/// Returns `true` if `value` is a valid native J2534 `PARITY` value (`0`
/// no parity, `1` odd, `2` even -- J2534 v04.04 range `0..=2`, default `0`).
/// Used to range-check an explicit `CP_Parity` `SetComParam` (ADR-027's
/// numeric overlap between `CP_Parity` and the native `PARITY` config ID).
pub(crate) fn is_valid_parity(value: u32) -> bool {
    value <= 2
}

/// Returns `true` if `value` is a valid native J2534 `BS_TX` value (SAE
/// J2534-1 Figure 30): `0x00..=0xFF`, or the `0xFFFF` sentinel meaning the
/// vehicle-reported flow-control value applies. Used to range-check an
/// explicit `CP_BlockSizeOverride` `SetComParam` (`CP_BlockSizeOverride` is
/// a direct-reuse mapping onto native `BS_TX`, `names.rs`) -- ISO 22900-2's
/// Table B.14 gives `CP_BlockSizeOverride` the full contiguous `[0, 0xFFFF]`
/// range, but nothing in `0x100..0xFFFE` is a valid native `BS_TX` value.
pub(crate) fn is_valid_block_size_override(value: u32) -> bool {
    value <= 0xFF || value == 0xFFFF
}

/// Returns `true` if `value` is a valid `CP_InitializationSettings` value:
/// `1` (5-baud init), `2` (fast-init), or `3` (no init sequence) -- the
/// project's authoritative definition for this ComParam (see
/// `events::select_init_sequence`, which drives `CoptStartcomm`'s K-line
/// init selection from it). Everything else is rejected by `SetComParam`
/// rather than silently falling back to the legacy heuristic at init time.
pub(crate) fn is_valid_init_settings(value: u32) -> bool {
    matches!(value, 1..=3)
}

/// `CP_CANFDTxMaxDataLength` (D-PDU) valid encodings per SAE J2534-2 Table
/// 90/97 (ADR-158): `0` (unset -- Classic CAN, or "use `CP_Baudrate`"'s
/// counterpart for TX length), Classic CAN's own max payload `8` (ambiguous
/// alone -- only a decisive FD-mode signal paired with a nonzero
/// `CP_CANFDBaudrate`; see `rpc_link::J2534Service::apply_fd_mode`'s trigger
/// rule), and CAN FD's DLC-encoded `12`/`16`/`20`/`24`/`32`/`48`/`64`-byte
/// payload lengths.
pub(crate) const CANFD_TX_MAX_DATA_LENGTH_ACCEPTED: [u32; 9] = [0, 8, 12, 16, 20, 24, 32, 48, 64];

/// Returns `true` if `value` is one of
/// [`CANFD_TX_MAX_DATA_LENGTH_ACCEPTED`].
pub(crate) fn is_valid_canfd_tx_max_data_length(value: u32) -> bool {
    CANFD_TX_MAX_DATA_LENGTH_ACCEPTED.contains(&value)
}

/// Rounds `len` (a CAN FD data-portion byte count `> 8`) up to the smallest
/// entry of [`CANFD_TX_MAX_DATA_LENGTH_ACCEPTED`] that is `>= len` -- SAE
/// J2534-2 Table 91 only accepts these DLC-encoded lengths on the wire, so
/// ISO 22900-2's `CP_CANFDTxMaxDataLength` NOTE 5 (ADR-158 correction)
/// requires the D-PDU API to pad a client's arbitrary-length payload up to
/// one of them with `CP_CanFillerByte`, rather than rejecting it outright.
/// Callers must ensure `len <= CANFD_TX_MAX_DATA_LENGTH_ACCEPTED`'s max
/// (`64`) -- checked by the TX size-range validation this always runs
/// after, so this never needs to return a value it can't find.
pub(crate) fn fd_can_padded_data_len(len: usize) -> usize {
    CANFD_TX_MAX_DATA_LENGTH_ACCEPTED
        .into_iter()
        .map(|accepted| accepted as usize)
        .find(|&accepted| accepted >= len)
        .expect("caller must range-check len against CANFD_TX_MAX_DATA_LENGTH_ACCEPTED's max first")
}

/// Expands a `(config_id, value)` list -- already produced by mapping each
/// Working ComParam through `to_j2534_config_id`, **before**
/// `to_j2534_config_value` has run (see `apply_j2534_params`/
/// `apply_params_to_hardware`, which convert units only after this and
/// `expand_tidle` (ADR-072) have both run) -- so that a `DATA_BITS` entry
/// originating from `CP_UartConfig`'s combined encoding (ADR-027's numeric
/// overlap between `CP_UartConfig` and the native `DATA_BITS` config ID)
/// becomes the two native entries J2534 v04.04 actually needs: `DATA_BITS`
/// and `PARITY` (ADR-071). Neither is affected by unit conversion (both are
/// small enum-like values, not timing values), so running before or after
/// `to_j2534_config_value` would be equivalent for this function alone --
/// but it still runs before, for a single consistent pipeline order with
/// `expand_tidle`.
///
/// **Precedence:** if `configs` already contains an explicit `PARITY` entry
/// (from `CP_Parity`, ADR-027's other overlap), that entry wins and the
/// `CP_UartConfig`-derived parity is dropped instead of appended --
/// deterministic, independent of the source `HashMap`'s iteration order.
///
/// Every value reaching this function is expected to already be validated by
/// `SetComParam` (see [`is_valid_uart_config`]/[`is_valid_parity`]); a
/// `DATA_BITS` entry whose value is not a valid `CP_UartConfig` encoding is
/// dropped rather than forwarded unmodified (should be unreachable).
pub(crate) fn expand_uart_config(configs: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    let has_explicit_parity = configs.iter().any(|&(id, _)| id == j2534_0404::PARITY);
    let mut expanded = Vec::with_capacity(configs.len() + 1);
    for (config_id, value) in configs {
        if config_id != j2534_0404::DATA_BITS {
            expanded.push((config_id, value));
            continue;
        }
        match (
            uart_config_to_data_bits(value),
            uart_config_to_parity(value),
        ) {
            (Some(data_bits), Some(parity)) => {
                expanded.push((j2534_0404::DATA_BITS, data_bits));
                if !has_explicit_parity {
                    expanded.push((j2534_0404::PARITY, parity));
                }
            }
            _ => {
                // Not a valid CP_UartConfig encoding; SetComParam should
                // have already rejected it, so drop it defensively rather
                // than forwarding a raw, meaningless DATA_BITS value.
            }
        }
    }
    expanded
}

/// Expands a `(config_id, value)` list -- already produced by mapping each
/// Working ComParam through `to_j2534_config_id`, **before**
/// `to_j2534_config_value` has run (see `apply_j2534_params`/
/// `apply_params_to_hardware`, which convert units only after this
/// expansion, so the derived entry this function adds is converted exactly
/// once, alongside every other entry, rather than zero or two times) -- so
/// that a `TIDLE` entry (`CP_TIdle`) also reaches the K-line idle timer that
/// actually applies on `j2534_protocol_id`: `W0` on ISO9141, `W5` on
/// ISO14230 (ADR-072). `CP_TIdle`'s single ComParam value is, per the ISO
/// 22900-2 -> J2534 timing table, the source for both `TIDLE` itself and
/// whichever of `W0`/`W5` the protocol uses.
///
/// **Precedence:** if `configs` already contains an explicit `W0`/`W5` entry
/// (from `SetComParam` on that native-overlapping ID directly, not derived
/// from `TIDLE`), that entry wins and the `TIDLE`-derived one is dropped
/// instead of appended -- deterministic, independent of the source
/// `HashMap`'s iteration order (mirrors `expand_uart_config`'s explicit-
/// `PARITY`-wins precedence, ADR-071).
///
/// No entry is added when `j2534_protocol_id` is neither `ISO9141` nor
/// `ISO14230` (`W0`/`W5` have no `SET_CONFIG` support on any other protocol,
/// ADR-028) or when `configs` has no `TIDLE` entry at all.
pub(crate) fn expand_tidle(configs: Vec<(u32, u32)>, hw_protocol_id: u32) -> Vec<(u32, u32)> {
    // ADR-158 correction: self-normalize from the raw (Plane A) id, exactly
    // like `to_j2534_config_id` does, so callers never need to choose which
    // id to pass. `base_protocol_id` is identity on already-base ids, so
    // this is correct regardless of whether the caller passes a raw or base
    // id -- in particular it makes `_PS` variants (e.g. `ISO9141_PS`,
    // `ISO14230_PS`) match here exactly as their base counterparts do.
    let derived_id = match resources::base_protocol_id(hw_protocol_id) {
        j2534_0404::ISO9141 => j2534_0404::W0,
        j2534_0404::ISO14230 => j2534_0404::W5,
        _ => return configs,
    };
    let Some(&(_, tidle_value)) = configs.iter().find(|&&(id, _)| id == j2534_0404::TIDLE) else {
        return configs;
    };
    if configs.iter().any(|&(id, _)| id == derived_id) {
        return configs;
    }
    let mut expanded = configs;
    expanded.push((derived_id, tidle_value));
    expanded
}

/// Converts `CP_StMinOverride` (ISO 22900-2) — a microsecond value, with
/// `0xFFFFFFFF` meaning the vehicle-reported value applies — into the
/// SAE J2534-1 `STMIN_TX` encoding (ADR-037):
///
/// - `0xFFFF` — use the value reported by the vehicle (sentinel).
/// - `0x00`-`0x7F` — 0-127 ms, 1 ms resolution.
/// - `0xF1`-`0xF9` — 100-900 us, 100 us resolution.
///
/// `CP_StMinOverride`'s 1 us resolution is finer than either J2534 range, so
/// a value that doesn't land exactly on a representable step is rounded to
/// the nearest one (a tie between the two ranges favors the larger
/// candidate). Values above the representable maximum (127 ms) are clamped
/// to `0x7F`.
pub(super) fn stmin_override_to_stmin_tx(value_us: u32) -> u32 {
    const SENTINEL_ISO22900: u32 = 0xFFFF_FFFF;
    const SENTINEL_J2534: u32 = 0xFFFF;
    const MIN_US_STEP: u32 = 1; // 0xF1 == 100 us
    const MAX_US_STEP: u32 = 9; // 0xF9 == 900 us
    const MAX_MS_STEP: u32 = 0x7F; // 127 ms

    if value_us == SENTINEL_ISO22900 {
        return SENTINEL_J2534;
    }

    // Nearest 100 us step, clamped to the representable [100, 900] us range.
    let us_step = (value_us.saturating_add(50) / 100).clamp(MIN_US_STEP, MAX_US_STEP);
    let us_candidate = us_step * 100;

    // Nearest 1 ms step, clamped to the representable [0, 127] ms range.
    let ms_step = (value_us.saturating_add(500) / 1000).min(MAX_MS_STEP);
    let ms_candidate = ms_step * 1000;

    let us_diff = value_us.abs_diff(us_candidate);
    let ms_diff = value_us.abs_diff(ms_candidate);

    if us_diff < ms_diff || (us_diff == ms_diff && us_candidate > ms_candidate) {
        0xF0 + us_step
    } else {
        ms_step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn universal_params_supported_on_every_protocol() {
        for proto in [
            j2534_0404::J1850VPW,
            j2534_0404::J1850PWM,
            j2534_0404::ISO9141,
            j2534_0404::ISO14230,
            j2534_0404::CAN,
            j2534_0404::ISO15765,
            j2534_0404::SCI_A_ENGINE,
        ] {
            assert_eq!(
                ComParamId(j2534_0404::DATA_RATE).to_j2534_config_id(proto),
                Some(j2534_0404::DATA_RATE)
            );
            assert_eq!(
                ComParamId(j2534_0404::LOOPBACK).to_j2534_config_id(proto),
                Some(j2534_0404::LOOPBACK)
            );
        }
    }

    #[test]
    fn node_address_and_network_line_only_for_j1850pwm() {
        assert_eq!(
            ComParamId(j2534_0404::NODE_ADDRESS).to_j2534_config_id(j2534_0404::J1850PWM),
            Some(j2534_0404::NODE_ADDRESS)
        );
        assert_eq!(
            ComParamId(j2534_0404::NODE_ADDRESS).to_j2534_config_id(j2534_0404::CAN),
            None
        );
        assert_eq!(
            ComParamId(j2534_0404::NETWORK_LINE).to_j2534_config_id(j2534_0404::ISO9141),
            None
        );
    }

    #[test]
    fn kwp_timing_params_only_for_iso9141_and_iso14230() {
        for id in [
            j2534_0404::P1_MAX,
            j2534_0404::P3_MIN,
            j2534_0404::P4_MIN,
            j2534_0404::TIDLE,
        ] {
            assert_eq!(
                ComParamId(id).to_j2534_config_id(j2534_0404::ISO9141),
                Some(id)
            );
            assert_eq!(
                ComParamId(id).to_j2534_config_id(j2534_0404::ISO14230),
                Some(id)
            );
            assert_eq!(ComParamId(id).to_j2534_config_id(j2534_0404::CAN), None);
        }
        // ADR-174/Phase 10 (Codex review, PR #63 round 3): TIDLE stays
        // exclusive to ISO9141/ISO14230 -- clause 13's closed ComParam
        // allowlist never permits it for HONDA_DIAGH_PS, unlike
        // P1_MAX/P3_MIN/P4_MIN (see
        // `honda_diagh_reuses_p1_max_p3_min_p4_min_but_not_the_wider_kwp_timing_group`
        // below).
        assert_eq!(
            ComParamId(j2534_0404::TIDLE).to_j2534_config_id(j2534_0404::PROTOCOL_HONDA_DIAGH_PS),
            None
        );
    }

    #[test]
    fn honda_diagh_reuses_p1_max_p3_min_p4_min_but_not_the_wider_kwp_timing_group() {
        // Codex review, PR #63 round 3: `base_protocol_id` does not collapse
        // HONDA_DIAGH_PS onto ISO9141/ISO14230 (it self-maps), so the shared
        // ISO9141/ISO14230 match arm alone never recognized it -- every
        // staged P1_MAX/P3_MIN/P4_MIN value (the seeded defaults and any
        // client SetComParam override alike) was silently dropped instead of
        // reaching the native adapter via PassThruIoctl(SET_CONFIG).
        for id in [j2534_0404::P1_MAX, j2534_0404::P3_MIN, j2534_0404::P4_MIN] {
            assert_eq!(
                ComParamId(id).to_j2534_config_id(j2534_0404::PROTOCOL_HONDA_DIAGH_PS),
                Some(id),
                "P1_MAX/P3_MIN/P4_MIN (clause 13.3.1's reused ISO9141 timing values) must \
                 translate for HONDA_DIAGH_PS"
            );
        }
        // W1 is part of the wider KWP timing group the shared match arm
        // covers, but clause 13's own closed allowlist never permits it for
        // this protocol (comparam_support::is_honda_diagh_param) -- scoping
        // the fix to exactly P1_MAX/P3_MIN/P4_MIN, not the whole group,
        // keeps this function's claims consistent with what can actually be
        // staged.
        assert_eq!(
            ComParamId(j2534_0404::W1).to_j2534_config_id(j2534_0404::PROTOCOL_HONDA_DIAGH_PS),
            None
        );
    }

    #[test]
    fn w0_only_iso9141_w5_only_iso14230() {
        assert_eq!(
            ComParamId(j2534_0404::W0).to_j2534_config_id(j2534_0404::ISO9141),
            Some(j2534_0404::W0)
        );
        assert_eq!(
            ComParamId(j2534_0404::W0).to_j2534_config_id(j2534_0404::ISO14230),
            None
        );
        assert_eq!(
            ComParamId(j2534_0404::W5).to_j2534_config_id(j2534_0404::ISO14230),
            Some(j2534_0404::W5)
        );
        assert_eq!(
            ComParamId(j2534_0404::W5).to_j2534_config_id(j2534_0404::ISO9141),
            None
        );
        // W1-W4 are shared between both KWP protocols.
        assert_eq!(
            ComParamId(j2534_0404::W1).to_j2534_config_id(j2534_0404::ISO9141),
            Some(j2534_0404::W1)
        );
        assert_eq!(
            ComParamId(j2534_0404::W1).to_j2534_config_id(j2534_0404::ISO14230),
            Some(j2534_0404::W1)
        );
    }

    #[test]
    fn analog_sample_rate_has_no_native_config_translation() {
        // ADR-178 line 137-138: `CP_AnalogSampleRate` is service-level only
        // (staged via SetComParam, resolved at ConnectComLogicalLink time,
        // never forwarded through the generic per-protocol SET_CONFIG path)
        // -- the same catch-all `None` shape `PARAM_CANFD_BAUDRATE` already
        // relies on for its own "service-level only" status. This function
        // has no translation arm for it, so it falls through to the `_ =>
        // None` catch-all deliberately, not as an oversight.
        assert_eq!(
            PARAM_ANALOG_SAMPLE_RATE.to_j2534_config_id(j2534_0404::PROTOCOL_ANALOG_IN_1),
            None
        );
    }

    /// ADR-188/Phase 7 Stage 7a (extended by ADR-190/Phase 7 Stage 7b): the
    /// five active-connection TP2.0 ComParams and the two minted passive-
    /// connection ComParams are all service-level-only (consumed directly by
    /// `handle_start_comm`'s own arm logic, never forwarded to a native
    /// `SET_CONFIG` via this generic pipeline), falling through to the
    /// `_ => None` catch-all deliberately, the same shape
    /// `analog_sample_rate_has_no_native_config_translation` above pins for
    /// `CP_AnalogSampleRate`.
    #[test]
    fn tp20_minted_comparams_have_no_native_config_translation() {
        for id in [
            PARAM_TP20_CHANNEL_SETUP_CAN_ID,
            PARAM_TP20_DESTINATION_ADDRESS,
            PARAM_TP20_TX_ID_PROPOSAL,
            PARAM_TP20_RX_ID_PROPOSAL,
            PARAM_TP20_APPLICATION_TYPE,
            PARAM_TP20_PASSIVE_IDENTIFIER,
            PARAM_TP20_PASSIVE_RX_ID,
            PARAM_TP20_BROADCAST_ADDRESS,
        ] {
            assert_eq!(id.to_j2534_config_id(j2534_0404::PROTOCOL_TP2_0_PS), None);
        }
    }

    /// ADR-192/Phase 7 Stage 7c: `PARAM_TP20_BROADCAST_INTERVAL` is a
    /// non-ordinal translation to native `CONFIG_TP2_0_T_BR_INT`, on a TP2.0
    /// link only -- it must not translate for an unrelated protocol.
    #[test]
    fn tp20_broadcast_interval_translates_to_native_t_br_int_on_tp2_0_only() {
        assert_eq!(
            PARAM_TP20_BROADCAST_INTERVAL.to_j2534_config_id(j2534_0404::PROTOCOL_TP2_0_PS),
            Some(j2534_0404::CONFIG_TP2_0_T_BR_INT)
        );
        assert_eq!(
            PARAM_TP20_BROADCAST_INTERVAL.to_j2534_config_id(j2534_0404::CAN),
            None
        );
    }

    /// ADR-210 Decision item 8: `PARAM_TP20_BROADCAST_INTERVAL` must
    /// translate identically to its `_PS` sibling on a `_CHx`-connected
    /// TP2.0 link -- before the fix, this block was keyed on the narrow
    /// `is_tp2_0_protocol_id`, so a `_CHx` id would have silently lost this
    /// translation (falling through to the generic match's catch-all `_ =>
    /// false` arm, wrongly returning `None`).
    #[test]
    fn tp20_broadcast_interval_translates_to_native_t_br_int_on_tp2_0_chx_too() {
        assert_eq!(
            PARAM_TP20_BROADCAST_INTERVAL.to_j2534_config_id(j2534_0404::PROTOCOL_TP2_0_CH1),
            Some(j2534_0404::CONFIG_TP2_0_T_BR_INT)
        );
        assert_eq!(
            PARAM_TP20_BROADCAST_INTERVAL.to_j2534_config_id(j2534_0404::PROTOCOL_TP2_0_CH128),
            Some(j2534_0404::CONFIG_TP2_0_T_BR_INT)
        );
    }

    /// ADR-188/Phase 7 Stage 7a: `CP_BitSamplePoint`/`CP_SyncJumpWidth`
    /// translate natively on a TP2_0_PS link too, even though it
    /// self-identifies rather than normalizing onto CAN via
    /// `base_protocol_id`.
    #[test]
    fn tp2_0_ps_gets_native_can_bit_timing_translation() {
        assert_eq!(
            ComParamId(j2534_0404::BIT_SAMPLE_POINT)
                .to_j2534_config_id(j2534_0404::PROTOCOL_TP2_0_PS),
            Some(j2534_0404::BIT_SAMPLE_POINT)
        );
        assert_eq!(
            ComParamId(j2534_0404::SYNC_JUMP_WIDTH)
                .to_j2534_config_id(j2534_0404::PROTOCOL_TP2_0_PS),
            Some(j2534_0404::SYNC_JUMP_WIDTH)
        );
    }

    #[test]
    fn can_bit_timing_only_for_can_not_iso15765() {
        assert_eq!(
            ComParamId(j2534_0404::BIT_SAMPLE_POINT).to_j2534_config_id(j2534_0404::CAN),
            Some(j2534_0404::BIT_SAMPLE_POINT)
        );
        assert_eq!(
            ComParamId(j2534_0404::BIT_SAMPLE_POINT).to_j2534_config_id(j2534_0404::ISO15765),
            None
        );
        assert_eq!(
            ComParamId(j2534_0404::SYNC_JUMP_WIDTH).to_j2534_config_id(j2534_0404::ISO15765),
            None
        );
    }

    // ── ADR-158/Phase 3a: CAN FD read-only bit-timing params ────────────────

    /// SAE J2534-2 clause 21.3.2.5.1: `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH`
    /// become read-only (`to_j2534_config_id` -> `None`) on an FD_CAN_PS
    /// link -- the exact matrix cells the ADR-158 contract change to
    /// `to_j2534_config_id` affects.
    #[test]
    fn bit_timing_params_not_supported_on_fd_can() {
        assert_eq!(
            ComParamId(j2534_0404::BIT_SAMPLE_POINT)
                .to_j2534_config_id(j2534_0404::PROTOCOL_FD_CAN_PS),
            None
        );
        assert_eq!(
            ComParamId(j2534_0404::SYNC_JUMP_WIDTH)
                .to_j2534_config_id(j2534_0404::PROTOCOL_FD_CAN_PS),
            None
        );
    }

    /// The same two params stay supported, unchanged, on plain Classic CAN
    /// -- ADR-158 narrows ADR-157's translation contract only for an
    /// FD_CAN_PS id, never for Classic CAN itself.
    #[test]
    fn bit_timing_params_still_supported_on_classic_can() {
        assert_eq!(
            ComParamId(j2534_0404::BIT_SAMPLE_POINT).to_j2534_config_id(j2534_0404::CAN),
            Some(j2534_0404::BIT_SAMPLE_POINT)
        );
        assert_eq!(
            ComParamId(j2534_0404::SYNC_JUMP_WIDTH).to_j2534_config_id(j2534_0404::CAN),
            Some(j2534_0404::SYNC_JUMP_WIDTH)
        );
    }

    /// Every other param's dispatch is unaffected by the FD check -- a
    /// universal param still translates on an FD_CAN_PS id (via
    /// `base_protocol_id`'s normal normalization for the ordinary
    /// family-arm dispatch).
    #[test]
    fn non_bit_timing_params_unaffected_by_fd_can() {
        assert_eq!(
            ComParamId(j2534_0404::DATA_RATE).to_j2534_config_id(j2534_0404::PROTOCOL_FD_CAN_PS),
            Some(j2534_0404::DATA_RATE)
        );
    }

    /// ADR-159/Phase 3b: the same clause 21.3.2.5.1 read-only suppression
    /// (clause 22.3.2.6.1's identical rule) comes free on `FD_ISO15765_PS`
    /// through the widened `is_fd_protocol_id` gate this function already
    /// checks first -- no code change to this check itself was needed.
    #[test]
    fn bit_timing_params_not_supported_on_fd_iso15765() {
        assert_eq!(
            ComParamId(j2534_0404::BIT_SAMPLE_POINT)
                .to_j2534_config_id(j2534_0404::PROTOCOL_FD_ISO15765_PS),
            None
        );
        assert_eq!(
            ComParamId(j2534_0404::SYNC_JUMP_WIDTH)
                .to_j2534_config_id(j2534_0404::PROTOCOL_FD_ISO15765_PS),
            None
        );
    }

    // ── ADR-159/Phase 3b: FD_ISO15765_PS non-identity translations ──────────

    /// Regression guard (ADR-159 Consequences): every arm that pre-dates this
    /// ADR still round-trips as a plain identity mapping on an ordinary
    /// hardware id -- confirms this ADR's three new non-identity cases are
    /// additive, not a silent change to any existing identity arm.
    #[test]
    fn every_pre_existing_identity_arm_still_round_trips_as_identity() {
        let cases: &[(u32, u32)] = &[
            (j2534_0404::DATA_RATE, j2534_0404::CAN),
            (j2534_0404::LOOPBACK, j2534_0404::ISO15765),
            (j2534_0404::NODE_ADDRESS, j2534_0404::J1850PWM),
            (j2534_0404::P1_MAX, j2534_0404::ISO9141),
            (j2534_0404::W0, j2534_0404::ISO9141),
            (j2534_0404::W5, j2534_0404::ISO14230),
            (j2534_0404::BIT_SAMPLE_POINT, j2534_0404::CAN),
            (j2534_0404::T1_MAX, j2534_0404::SCI_A_ENGINE),
            (j2534_0404::ISO15765_BS, j2534_0404::ISO15765),
        ];
        for &(comparam, hw_protocol_id) in cases {
            assert_eq!(
                ComParamId(comparam).to_j2534_config_id(hw_protocol_id),
                Some(comparam),
                "comparam {comparam:#x} on hw_protocol_id {hw_protocol_id:#x} should still be \
                 identity-mapped"
            );
        }
    }

    /// Regression guard, the companion case ADR-159's table calls out
    /// explicitly: `CP_CANFDTxMaxDataLength`/`CP_Cr`/`CP_CanFillerByte` were
    /// `None` (unsupported for native forwarding) on every hardware id
    /// *before* this ADR -- never identity. This must stay true everywhere
    /// except `FD_ISO15765_PS` (asserted separately below), including on
    /// plain ISO15765 and on `FD_CAN_PS`.
    #[test]
    fn fd_iso15765_ps_specific_comparams_stay_unsupported_everywhere_else() {
        let cases: &[(super::ComParamId, u32)] = &[
            (PARAM_CANFD_TX_MAX_DATA_LENGTH, j2534_0404::CAN),
            (PARAM_CANFD_TX_MAX_DATA_LENGTH, j2534_0404::ISO15765),
            (
                PARAM_CANFD_TX_MAX_DATA_LENGTH,
                j2534_0404::PROTOCOL_FD_CAN_PS,
            ),
            (PARAM_N_CR, j2534_0404::CAN),
            (PARAM_N_CR, j2534_0404::ISO15765),
            (PARAM_N_CR, j2534_0404::PROTOCOL_FD_CAN_PS),
            (PARAM_CAN_FILLER_BYTE, j2534_0404::CAN),
            (PARAM_CAN_FILLER_BYTE, j2534_0404::ISO15765),
            (PARAM_CAN_FILLER_BYTE, j2534_0404::PROTOCOL_FD_CAN_PS),
        ];
        for &(comparam, hw_protocol_id) in cases {
            assert_eq!(
                comparam.to_j2534_config_id(hw_protocol_id),
                None,
                "comparam {comparam:?} on hw_protocol_id {hw_protocol_id:#x} should remain \
                 unsupported (None), not identity"
            );
        }
    }

    /// ADR-159's three new non-identity id translations, on `FD_ISO15765_PS`
    /// specifically.
    #[test]
    fn fd_iso15765_ps_translates_three_comparams_non_identically() {
        assert_eq!(
            PARAM_CANFD_TX_MAX_DATA_LENGTH.to_j2534_config_id(j2534_0404::PROTOCOL_FD_ISO15765_PS),
            Some(j2534_0404::CONFIG_FD_ISO15765_TX_DATA_LENGTH)
        );
        assert_eq!(
            PARAM_N_CR.to_j2534_config_id(j2534_0404::PROTOCOL_FD_ISO15765_PS),
            Some(j2534_0404::CONFIG_N_CR_MAX)
        );
        assert_eq!(
            PARAM_CAN_FILLER_BYTE.to_j2534_config_id(j2534_0404::PROTOCOL_FD_ISO15765_PS),
            Some(j2534_0404::CONFIG_ISO15765_PAD_VALUE)
        );
    }

    /// ADR-159's `to_j2534_config_value` conversions: the TX_DL floor-at-8
    /// (a staged `0` floors up, not down) and the N_Cr µs->ms conversion
    /// with its `0xFFFF` clamp boundary. `CONFIG_ISO15765_PAD_VALUE` is
    /// identity (falls into the `_ => value` arm), asserted here too for
    /// completeness.
    #[test]
    fn fd_iso15765_ps_config_value_conversions() {
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_FD_ISO15765_TX_DATA_LENGTH, 0),
            8
        );
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_FD_ISO15765_TX_DATA_LENGTH, 5),
            8
        );
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_FD_ISO15765_TX_DATA_LENGTH, 64),
            64
        );

        // 1_000_000 us == 1000 ms, ISO 22900-2's default CP_Cr value.
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_N_CR_MAX, 1_000_000),
            1000
        );
        // Clamped at the low end: any nonzero value converts to at least 1.
        assert_eq!(to_j2534_config_value(j2534_0404::CONFIG_N_CR_MAX, 1), 1);
        assert_eq!(to_j2534_config_value(j2534_0404::CONFIG_N_CR_MAX, 0), 1);
        // Clamped at the high end: a value converting past 0xFFFF ms clamps
        // down to 0xFFFF rather than wrapping/truncating.
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_N_CR_MAX, 0xFFFF_FFFF),
            0xFFFF
        );

        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_ISO15765_PAD_VALUE, 0x55),
            0x55
        );
    }

    // ── ADR-179/Phase 5: J1939 non-ordinal ComParam translations ────────────

    /// Regression guard, mirroring `fd_iso15765_ps_specific_comparams_stay_
    /// unsupported_everywhere_else`: the five ComParams ADR-179 translates on
    /// a J1939 link must stay `None` on every other hardware id (asserted
    /// separately below on J1939 itself), including on plain CAN/ISO15765
    /// where none of them had a native translation before this ADR either.
    #[test]
    fn j1939_specific_comparams_stay_unsupported_everywhere_else() {
        let cases: &[(super::ComParamId, u32)] = &[
            (PARAM_N_CR, j2534_0404::CAN),
            (PARAM_N_CR, j2534_0404::ISO15765),
            (ComParamId(j2534_0404::T5_MAX), j2534_0404::CAN),
            (ComParamId(j2534_0404::T4_MAX), j2534_0404::CAN),
            (PARAM_N_BS, j2534_0404::CAN),
            (PARAM_N_CS, j2534_0404::CAN),
        ];
        for &(comparam, hw_protocol_id) in cases {
            assert_eq!(
                comparam.to_j2534_config_id(hw_protocol_id),
                None,
                "comparam {comparam:?} on hw_protocol_id {hw_protocol_id:#x} should remain \
                 unsupported (None), not identity"
            );
        }
    }

    /// ADR-179's five new non-ordinal ComParam translations, on
    /// `PROTOCOL_J1939_PS` specifically -- pinned against the Context
    /// section's table (`CP_Cr`->T1, `CP_T5Max`->T2, `CP_T4Max`->T3,
    /// `CP_Bs`->T4, `CP_Cs`->BRDCST_MIN_DELAY, NOT an ordinal
    /// T1<->T1/T2<->T2/... correspondence).
    #[test]
    fn j1939_ps_translates_five_comparams_non_ordinally() {
        assert_eq!(
            PARAM_N_CR.to_j2534_config_id(j2534_0404::PROTOCOL_J1939_PS),
            Some(j2534_0404::CONFIG_J1939_T1)
        );
        assert_eq!(
            ComParamId(j2534_0404::T5_MAX).to_j2534_config_id(j2534_0404::PROTOCOL_J1939_PS),
            Some(j2534_0404::CONFIG_J1939_T2)
        );
        assert_eq!(
            ComParamId(j2534_0404::T4_MAX).to_j2534_config_id(j2534_0404::PROTOCOL_J1939_PS),
            Some(j2534_0404::CONFIG_J1939_T3)
        );
        assert_eq!(
            PARAM_N_BS.to_j2534_config_id(j2534_0404::PROTOCOL_J1939_PS),
            Some(j2534_0404::CONFIG_J1939_T4)
        );
        assert_eq!(
            PARAM_N_CS.to_j2534_config_id(j2534_0404::PROTOCOL_J1939_PS),
            Some(j2534_0404::CONFIG_J1939_BRDCST_MIN_DELAY)
        );
        // Table 60's own two documented gaps: neither has a native slot at
        // all, even on a J1939 link.
        assert_eq!(
            ComParamId(j2534_0404::T3_MAX).to_j2534_config_id(j2534_0404::PROTOCOL_J1939_PS),
            None
        );
        assert_eq!(
            PARAM_N_BR.to_j2534_config_id(j2534_0404::PROTOCOL_J1939_PS),
            None
        );
    }

    /// ADR-179's `to_j2534_config_value` conversions for the five
    /// `CONFIG_J1939_*` targets: the shared 1 us -> 1 ms ADR-072 conversion,
    /// pinned against ADR-179's own default values (Context section table).
    #[test]
    fn j1939_config_value_conversions() {
        // CP_Cr default: 750_000 us == 750 ms.
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_J1939_T1, 750_000),
            750
        );
        // CP_T5Max/CP_T4Max default: 1_250_000 us == 1250 ms.
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_J1939_T2, 1_250_000),
            1250
        );
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_J1939_T3, 1_250_000),
            1250
        );
        // CP_Bs default: 1_050_000 us == 1050 ms.
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_J1939_T4, 1_050_000),
            1050
        );
        // CP_Cs default: 50_000 us == 50 ms.
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_J1939_BRDCST_MIN_DELAY, 50_000),
            50
        );
    }

    // ── J2534-1 audit P2 fix: Figure-30 16-bit timing clamps ────────────────

    /// `P1_MAX` alone in this group has a nonzero Figure-30 floor (`0x1`):
    /// a small microsecond input must clamp up to `1`, not silently convert
    /// to `0` (which a conformant device rejects, per the P2 backlog entry
    /// this fixes). A huge input must clamp down to `0xFFFF` rather than
    /// silently exceeding the native 16-bit range, and a mid-range input
    /// converts normally, unaffected by either clamp.
    #[test]
    fn p1_max_config_value_clamps_to_nonzero_16bit_range() {
        // 100 us is nonzero but rounds to 0 half-ms steps; must clamp to 1,
        // not 0.
        assert_eq!(to_j2534_config_value(j2534_0404::P1_MAX, 100), 1);
        // A huge input must clamp to 0xFFFF rather than overflowing/
        // truncating the native 16-bit field.
        assert_eq!(
            to_j2534_config_value(j2534_0404::P1_MAX, 1_000_000_000),
            0xFFFF
        );
        // Mid-range input converts normally: 20_000 us == 20 ms == 40 half-ms
        // steps.
        assert_eq!(to_j2534_config_value(j2534_0404::P1_MAX, 20_000), 40);
    }

    /// `P3_MIN`/`P4_MIN` share `P1_MAX`'s conversion but Figure 30 gives them
    /// a `0` floor, not `1` -- a small input must still convert to `0`
    /// exactly, unlike `P1_MAX`. A huge input still clamps to `0xFFFF`.
    #[test]
    fn p3_min_and_p4_min_config_value_floor_stays_zero() {
        for id in [j2534_0404::P3_MIN, j2534_0404::P4_MIN] {
            // 100 us rounds to 0 half-ms steps; P3_MIN/P4_MIN's floor really
            // is 0, so this must stay 0, not get bumped to 1 like P1_MAX.
            assert_eq!(to_j2534_config_value(id, 100), 0);
            assert_eq!(to_j2534_config_value(id, 1_000_000_000), 0xFFFF);
        }
    }

    /// A representative `W*`/`T*` param (1 ms resolution group): a huge
    /// input clamps to `0xFFFF` instead of silently exceeding the native
    /// 16-bit range.
    #[test]
    fn w_and_t_group_config_value_clamps_to_16bit_ceiling() {
        assert_eq!(to_j2534_config_value(j2534_0404::W1, 1_000_000_000), 0xFFFF);
        assert_eq!(
            to_j2534_config_value(j2534_0404::T1_MAX, 1_000_000_000),
            0xFFFF
        );
    }

    /// A representative `CONFIG_J1939_*` param: a huge input clamps to
    /// `0xFFFF` too, per SAE J2534-2 clause 16's own value tables (same
    /// `0x0`-`0xFFFF` shape as the Figure-30 rows above).
    #[test]
    fn j1939_config_value_clamps_to_16bit_ceiling() {
        assert_eq!(
            to_j2534_config_value(j2534_0404::CONFIG_J1939_T1, 1_000_000_000),
            0xFFFF
        );
    }

    // ── ADR-158/Phase 3a: CP_CANFDTxMaxDataLength range validation ──────────

    #[test]
    fn canfd_tx_max_data_length_accepts_documented_encodings() {
        for value in CANFD_TX_MAX_DATA_LENGTH_ACCEPTED {
            assert!(
                is_valid_canfd_tx_max_data_length(value),
                "{value} should be valid"
            );
        }
    }

    #[test]
    fn canfd_tx_max_data_length_rejects_undocumented_values() {
        for value in [1, 4, 7, 9, 11, 40, 65, u32::MAX] {
            assert!(
                !is_valid_canfd_tx_max_data_length(value),
                "{value} should be rejected"
            );
        }
    }

    /// ADR-158 correction (Codex review PR #30 round 4): `fd_can_padded_data_len`
    /// rounds up to the smallest [`CANFD_TX_MAX_DATA_LENGTH_ACCEPTED`] entry
    /// `>= len` -- an already-accepted length (including the two Classic
    /// values `0`/`8`, though callers only invoke this for `len > 8`) must
    /// round to itself.
    #[test]
    fn fd_can_padded_data_len_rounds_up_to_the_nearest_accepted_length() {
        assert_eq!(fd_can_padded_data_len(9), 12);
        assert_eq!(fd_can_padded_data_len(12), 12);
        assert_eq!(fd_can_padded_data_len(13), 16);
        assert_eq!(fd_can_padded_data_len(40), 48);
        assert_eq!(fd_can_padded_data_len(48), 48);
        assert_eq!(fd_can_padded_data_len(64), 64);
        for accepted in CANFD_TX_MAX_DATA_LENGTH_ACCEPTED {
            assert_eq!(fd_can_padded_data_len(accepted as usize), accepted as usize);
        }
    }

    #[test]
    fn iso15765_flow_control_only_for_iso15765_not_can() {
        for id in [
            j2534_0404::ISO15765_BS,
            j2534_0404::ISO15765_STMIN,
            j2534_0404::BS_TX,
            j2534_0404::STMIN_TX,
            j2534_0404::ISO15765_WFT_MAX,
        ] {
            assert_eq!(
                ComParamId(id).to_j2534_config_id(j2534_0404::ISO15765),
                Some(id)
            );
            assert_eq!(ComParamId(id).to_j2534_config_id(j2534_0404::CAN), None);
        }
    }

    /// Pins the invariant `rpc_link.rs`'s `SetComParam` `CP_BlockSizeOverride`
    /// range-check gate relies on: no raw-id-only branch above the
    /// `base_protocol_id` normalization intercepts `BS_TX`, so passing a
    /// qualified `FD_ISO15765_PS` raw id here still normalizes down to plain
    /// `ISO15765` and translates identically to passing plain `ISO15765`
    /// directly. If a future change ever added a raw-id-conditioned override
    /// for `BS_TX` (mirroring the FD_ISO15765_PS/SW/Honda/J1939 precedents
    /// above), this test would need to change too -- which is the point.
    #[test]
    fn bs_tx_translation_unaffected_by_qualified_iso15765_variant() {
        assert_eq!(
            ComParamId(j2534_0404::BS_TX).to_j2534_config_id(j2534_0404::ISO15765),
            ComParamId(j2534_0404::BS_TX).to_j2534_config_id(j2534_0404::PROTOCOL_FD_ISO15765_PS),
        );
    }

    #[test]
    fn sci_timing_only_for_sci_family() {
        for id in [
            j2534_0404::T1_MAX,
            j2534_0404::T2_MAX,
            j2534_0404::T3_MAX,
            j2534_0404::T4_MAX,
            j2534_0404::T5_MAX,
        ] {
            assert_eq!(
                ComParamId(id).to_j2534_config_id(j2534_0404::SCI_A_ENGINE),
                Some(id)
            );
            assert_eq!(
                ComParamId(id).to_j2534_config_id(j2534_0404::SCI_B_TRANS),
                Some(id)
            );
            assert_eq!(ComParamId(id).to_j2534_config_id(j2534_0404::CAN), None);
        }
    }

    #[test]
    fn unsupported_p_timers_never_translate_for_any_protocol() {
        // P1_MIN, P2_MIN, P2_MAX, P3_MAX, P4_MAX have no J2534 SET_CONFIG
        // support for any protocol per the supported-CONFIG table.
        for id in [
            j2534_0404::P1_MIN,
            j2534_0404::P2_MIN,
            j2534_0404::P2_MAX,
            j2534_0404::P3_MAX,
            j2534_0404::P4_MAX,
        ] {
            for proto in [
                j2534_0404::J1850VPW,
                j2534_0404::J1850PWM,
                j2534_0404::ISO9141,
                j2534_0404::ISO14230,
                j2534_0404::CAN,
                j2534_0404::ISO15765,
            ] {
                assert_eq!(ComParamId(id).to_j2534_config_id(proto), None);
            }
        }
    }

    #[test]
    fn service_level_param_has_no_j2534_config_id() {
        // 0x8002 == PARAM_TESTER_PRESENT_INTERVAL_US, a service-only param.
        assert_eq!(ComParamId(0x8002).to_j2534_config_id(j2534_0404::CAN), None);
    }

    #[test]
    fn w_min_max_project_ids_never_forward_to_hardware() {
        // ADR-181: CP_W1Min/CP_W2Min/CP_W3Min/CP_W4Max have no native J2534
        // register at all (SAE J2534-1 Figure 30 defines only a single
        // MAX-side register for W1-W3 and a single MIN-side register for
        // W4) -- these four project-minted ids must fall through this
        // function's catch-all on every protocol, confirmed here for both
        // protocols that otherwise accept the rest of the W-timer group.
        for proto in [j2534_0404::ISO9141, j2534_0404::ISO14230] {
            for id in [PARAM_W1_MIN, PARAM_W2_MIN, PARAM_W3_MIN, PARAM_W4_MAX] {
                assert_eq!(id.to_j2534_config_id(proto), None);
            }
        }
    }

    #[test]
    fn unrecognized_id_is_never_forwarded_to_hardware() {
        // A hypothetical ComParam ID not in the whitelist above (and not a
        // known service param either) must not be treated as a native
        // config ID -- this is the regression test for the bug where an
        // unrecognized ID would previously fall through a blacklist check
        // and be forwarded to `PassThruIoctl SET_CONFIG` unchecked.
        assert_eq!(ComParamId(0x8999).to_j2534_config_id(j2534_0404::CAN), None);
    }

    // ── stmin_override_to_stmin_tx ──────────────────────────────────────────

    #[test]
    fn stmin_sentinel_maps_to_j2534_sentinel() {
        assert_eq!(stmin_override_to_stmin_tx(0xFFFF_FFFF), 0xFFFF);
    }

    #[test]
    fn stmin_exact_ms_values_map_to_range1() {
        assert_eq!(stmin_override_to_stmin_tx(0), 0x00);
        assert_eq!(stmin_override_to_stmin_tx(1_000), 0x01);
        assert_eq!(stmin_override_to_stmin_tx(127_000), 0x7F);
    }

    #[test]
    fn stmin_exact_100us_multiples_map_to_range2() {
        assert_eq!(stmin_override_to_stmin_tx(100), 0xF1);
        assert_eq!(stmin_override_to_stmin_tx(500), 0xF5);
        assert_eq!(stmin_override_to_stmin_tx(900), 0xF9);
    }

    #[test]
    fn stmin_above_max_ms_clamps_to_0x7f() {
        assert_eq!(stmin_override_to_stmin_tx(130_000), 0x7F);
        assert_eq!(stmin_override_to_stmin_tx(u32::MAX - 1), 0x7F);
    }

    #[test]
    fn stmin_rounds_to_nearest_representable_step() {
        // 51 us is closer to the 100 us step than to 0 ms.
        assert_eq!(stmin_override_to_stmin_tx(51), 0xF1);
        // 49 us is closer to 0 ms than to the 100 us step.
        assert_eq!(stmin_override_to_stmin_tx(49), 0x00);
        // 150 us is closer to the 200 us step than to the 100 us step.
        assert_eq!(stmin_override_to_stmin_tx(150), 0xF2);
        // 899 us is closer to the 900 us step than to the 1 ms step.
        assert_eq!(stmin_override_to_stmin_tx(899), 0xF9);
    }

    #[test]
    fn stmin_tie_between_ranges_favors_larger_candidate() {
        // 50 us is equidistant from 0 ms (0) and the 100 us step (0xF1);
        // the tie favors the larger candidate (100 us).
        assert_eq!(stmin_override_to_stmin_tx(50), 0xF1);
        // 950 us is equidistant from the 900 us step (0xF9) and 1 ms (0x01);
        // the tie favors the larger candidate (1 ms).
        assert_eq!(stmin_override_to_stmin_tx(950), 0x01);
    }

    // ── CP_UartConfig decode/validation (ADR-071) ───────────────────────────

    #[test]
    fn uart_config_full_decode_table() {
        // (CP_UartConfig value, expected DATA_BITS, expected PARITY)
        let table = [
            (0, 1, 0), // 7N1
            (1, 1, 1), // 7O1
            (2, 1, 2), // 7E1
            (6, 0, 0), // 8N1
            (7, 0, 1), // 8O1
            (8, 0, 2), // 8E1
        ];
        for (value, expected_data_bits, expected_parity) in table {
            assert!(is_valid_uart_config(value), "{value} should be valid");
            assert_eq!(
                uart_config_to_data_bits(value),
                Some(expected_data_bits),
                "DATA_BITS for CP_UartConfig={value}"
            );
            assert_eq!(
                uart_config_to_parity(value),
                Some(expected_parity),
                "PARITY for CP_UartConfig={value}"
            );
        }
    }

    #[test]
    fn uart_config_rejects_unrepresentable_values() {
        // 3, 5: 7 data bits, 2 stop bits (no J2534 stop-bit param).
        // 9, 11: 8 data bits, 2 stop bits.
        // 12, 17: 9 data bits (DATA_BITS only encodes 7/8).
        // 18, u32::MAX: outside the 0..=17 CP_UartConfig range entirely.
        for value in [3, 5, 9, 11, 12, 17, 18, u32::MAX] {
            assert!(!is_valid_uart_config(value), "{value} should be rejected");
            assert_eq!(uart_config_to_data_bits(value), None);
            assert_eq!(uart_config_to_parity(value), None);
        }
    }

    #[test]
    fn parity_valid_range_is_0_to_2() {
        assert!(is_valid_parity(0));
        assert!(is_valid_parity(1));
        assert!(is_valid_parity(2));
        assert!(!is_valid_parity(3));
        assert!(!is_valid_parity(u32::MAX));
    }

    #[test]
    fn block_size_override_valid_range_is_0_to_ff_plus_sentinel() {
        assert!(is_valid_block_size_override(0));
        assert!(is_valid_block_size_override(0xFF));
        assert!(!is_valid_block_size_override(0x100));
        assert!(!is_valid_block_size_override(0xFFFE));
        assert!(is_valid_block_size_override(0xFFFF));
    }

    #[test]
    fn init_settings_valid_range_is_1_to_3() {
        assert!(!is_valid_init_settings(0));
        assert!(is_valid_init_settings(1));
        assert!(is_valid_init_settings(2));
        assert!(is_valid_init_settings(3));
        assert!(!is_valid_init_settings(4));
        assert!(!is_valid_init_settings(u32::MAX));
    }

    // ── expand_uart_config ───────────────────────────────────────────────────

    #[test]
    fn expand_uart_config_splits_data_bits_and_parity() {
        let configs = vec![(j2534_0404::DATA_BITS, 8), (j2534_0404::P1_MAX, 20)];
        let expanded = expand_uart_config(configs);
        assert!(expanded.contains(&(j2534_0404::DATA_BITS, 0)));
        assert!(expanded.contains(&(j2534_0404::PARITY, 2)));
        assert!(expanded.contains(&(j2534_0404::P1_MAX, 20)));
        assert_eq!(expanded.len(), 3);
    }

    #[test]
    fn expand_uart_config_explicit_parity_wins() {
        // CP_UartConfig=8 (8E1, PARITY=2) plus an explicit CP_Parity=1 (odd):
        // the explicit entry must win, not the CP_UartConfig-derived one.
        let configs = vec![(j2534_0404::DATA_BITS, 8), (j2534_0404::PARITY, 1)];
        let expanded = expand_uart_config(configs);
        assert!(expanded.contains(&(j2534_0404::DATA_BITS, 0)));
        assert!(expanded.contains(&(j2534_0404::PARITY, 1)));
        assert_eq!(
            expanded
                .iter()
                .filter(|&&(id, _)| id == j2534_0404::PARITY)
                .count(),
            1
        );
    }

    #[test]
    fn expand_uart_config_passthrough_when_no_data_bits_entry() {
        let configs = vec![(j2534_0404::P1_MAX, 20), (j2534_0404::W1, 60)];
        let expanded = expand_uart_config(configs.clone());
        assert_eq!(expanded, configs);
    }

    #[test]
    fn expand_uart_config_drops_invalid_data_bits_value() {
        // Should be unreachable in practice (SetComParam validates first),
        // but must not forward a meaningless raw value if it ever happens.
        let configs = vec![(j2534_0404::DATA_BITS, 12), (j2534_0404::P1_MAX, 20)];
        let expanded = expand_uart_config(configs);
        assert_eq!(expanded, vec![(j2534_0404::P1_MAX, 20)]);
    }

    // ── us_to_half_ms / us_to_ms (ADR-072) ──────────────────────────────────

    #[test]
    fn us_to_half_ms_exact_multiples() {
        assert_eq!(us_to_half_ms(20_000), 40);
        assert_eq!(us_to_half_ms(0), 0);
    }

    #[test]
    fn us_to_half_ms_rounds_to_nearest_step() {
        // 20_250 us is exactly halfway between the 40 and 41 half-ms steps
        // (20_000 and 20_500 us); the tie rounds up (integer division of the
        // saturating-add offset).
        assert_eq!(us_to_half_ms(20_250), 41);
        // 20_249 us is closer to the 40 step (20_000 us) than to 41
        // (20_500 us).
        assert_eq!(us_to_half_ms(20_249), 40);
    }

    #[test]
    fn us_to_half_ms_saturates_at_u32_max() {
        // saturating_add(u32::MAX, 250) pins at u32::MAX rather than
        // overflowing/panicking, then divides by 500.
        assert_eq!(us_to_half_ms(u32::MAX), u32::MAX / 500);
    }

    #[test]
    fn us_to_ms_exact_multiples() {
        assert_eq!(us_to_ms(300_000), 300);
        assert_eq!(us_to_ms(0), 0);
    }

    #[test]
    fn us_to_ms_rounds_to_nearest_step() {
        assert_eq!(us_to_ms(300_500), 301);
        assert_eq!(us_to_ms(300_499), 300);
    }

    #[test]
    fn us_to_ms_saturates_at_u32_max() {
        // saturating_add(u32::MAX, 500) pins at u32::MAX rather than
        // overflowing/panicking, then divides by 1000.
        assert_eq!(us_to_ms(u32::MAX), u32::MAX / 1000);
    }

    // ── to_j2534_config_value dispatch (ADR-072) ────────────────────────────

    #[test]
    fn to_j2534_config_value_converts_half_ms_timing_params() {
        for id in [j2534_0404::P1_MAX, j2534_0404::P3_MIN, j2534_0404::P4_MIN] {
            assert_eq!(to_j2534_config_value(id, 20_000), 40, "config id {id:#x}");
        }
    }

    #[test]
    fn to_j2534_config_value_converts_ms_timing_params() {
        for id in [
            j2534_0404::W0,
            j2534_0404::W1,
            j2534_0404::W2,
            j2534_0404::W3,
            j2534_0404::W4,
            j2534_0404::W5,
            j2534_0404::TIDLE,
            j2534_0404::TINIL,
            j2534_0404::TWUP,
            j2534_0404::T1_MAX,
            j2534_0404::T2_MAX,
            j2534_0404::T3_MAX,
            j2534_0404::T4_MAX,
            j2534_0404::T5_MAX,
        ] {
            assert_eq!(to_j2534_config_value(id, 300_000), 300, "config id {id:#x}");
        }
    }

    #[test]
    fn to_j2534_config_value_still_converts_stmin_tx() {
        assert_eq!(to_j2534_config_value(j2534_0404::STMIN_TX, 500), 0xF5);
    }

    #[test]
    fn to_j2534_config_value_passes_through_unrelated_id() {
        assert_eq!(
            to_j2534_config_value(j2534_0404::DATA_RATE, 500_000),
            500_000
        );
    }

    // ── expand_tidle (ADR-072) ───────────────────────────────────────────────

    #[test]
    fn expand_tidle_derives_w0_on_iso9141() {
        let configs = vec![(j2534_0404::TIDLE, 300_000), (j2534_0404::P1_MAX, 20_000)];
        let expanded = expand_tidle(configs, j2534_0404::ISO9141);
        assert!(expanded.contains(&(j2534_0404::W0, 300_000)));
        assert!(!expanded.iter().any(|&(id, _)| id == j2534_0404::W5));
        assert_eq!(expanded.len(), 3);
    }

    #[test]
    fn expand_tidle_derives_w5_on_iso14230() {
        let configs = vec![(j2534_0404::TIDLE, 300_000)];
        let expanded = expand_tidle(configs, j2534_0404::ISO14230);
        assert!(expanded.contains(&(j2534_0404::W5, 300_000)));
        assert!(!expanded.iter().any(|&(id, _)| id == j2534_0404::W0));
        assert_eq!(expanded.len(), 2);
    }

    #[test]
    fn expand_tidle_explicit_w0_wins_on_iso9141() {
        let configs = vec![(j2534_0404::TIDLE, 300_000), (j2534_0404::W0, 400_000)];
        let expanded = expand_tidle(configs, j2534_0404::ISO9141);
        assert_eq!(
            expanded
                .iter()
                .filter(|&&(id, _)| id == j2534_0404::W0)
                .count(),
            1
        );
        assert!(expanded.contains(&(j2534_0404::W0, 400_000)));
    }

    #[test]
    fn expand_tidle_explicit_w5_wins_on_iso14230() {
        let configs = vec![(j2534_0404::TIDLE, 300_000), (j2534_0404::W5, 400_000)];
        let expanded = expand_tidle(configs, j2534_0404::ISO14230);
        assert_eq!(
            expanded
                .iter()
                .filter(|&&(id, _)| id == j2534_0404::W5)
                .count(),
            1
        );
        assert!(expanded.contains(&(j2534_0404::W5, 400_000)));
    }

    #[test]
    fn expand_tidle_passthrough_when_no_tidle_entry() {
        let configs = vec![(j2534_0404::P1_MAX, 20_000), (j2534_0404::W1, 60_000)];
        let expanded = expand_tidle(configs.clone(), j2534_0404::ISO9141);
        assert_eq!(expanded, configs);
    }

    #[test]
    fn expand_tidle_passthrough_on_unrelated_protocol() {
        let configs = vec![(j2534_0404::TIDLE, 300_000)];
        let expanded = expand_tidle(configs.clone(), j2534_0404::CAN);
        assert_eq!(expanded, configs);
    }

    // ADR-158 correction: `expand_tidle` must self-normalize a raw (Plane A)
    // `_PS` id exactly like it does a base id -- this is the regression test
    // for the Stage 3a diff that silently broke `CP_TIdle` derivation on
    // `ISO9141_PS`/`ISO14230_PS` links (see ADR-158's Corrections section).
    #[test]
    fn expand_tidle_derives_w0_on_iso9141_ps() {
        let configs = vec![(j2534_0404::TIDLE, 300_000)];
        let expanded = expand_tidle(configs, j2534_0404::PROTOCOL_ISO9141_PS);
        assert!(expanded.contains(&(j2534_0404::W0, 300_000)));
    }

    #[test]
    fn expand_tidle_derives_w5_on_iso14230_ps() {
        let configs = vec![(j2534_0404::TIDLE, 300_000)];
        let expanded = expand_tidle(configs, j2534_0404::PROTOCOL_ISO14230_PS);
        assert!(expanded.contains(&(j2534_0404::W5, 300_000)));
    }

    // ── ADR-219 Decision item 3: vendor ConfigParameterID identity passthrough ──

    #[test]
    fn vendor_config_id_translates_identically_on_any_protocol() {
        for hw_protocol_id in [
            j2534_0404::CAN,
            j2534_0404::ISO15765,
            j2534_0404::ISO9141,
            j2534_0404::PROTOCOL_J1939_PS,
            0xFFFF_FFFF, // a genuinely unrecognized hardware id too
        ] {
            for vendor_id in [0x0001_0000u32, 0x0001_0001, 0xFFFF_FFFF] {
                assert_eq!(
                    ComParamId(vendor_id).to_j2534_config_id(hw_protocol_id),
                    Some(vendor_id),
                    "vendor ComParamId {vendor_id:#x} should identity-translate on \
                     hw_protocol_id {hw_protocol_id:#x}"
                );
            }
        }
    }

    #[test]
    fn just_below_vendor_range_stays_unsupported() {
        // 0xFFFF is the top of the SAE J2534-2 range (Context) -- one below
        // the ADR-219 vendor boundary must NOT identity-translate.
        assert_eq!(
            ComParamId(0x0000_FFFF).to_j2534_config_id(j2534_0404::CAN),
            None
        );
    }

    /// ADR-219 Context's "Critical collision fact": these two service-minted
    /// ComParams share their numeric value with native
    /// `CONFIG_TP2_0_IDENTIFER`/`_RXIDPASSIVE` in the `0x8000`-`0xFFFF`
    /// window -- the exact reason the vendor boundary is `0x10000`, not
    /// `0x8000`. Must stay `None` (never routed to native SET_CONFIG/
    /// GET_CONFIG), unaffected by this ADR's new `>= 0x10000` arm.
    #[test]
    fn access_timing_ecu_and_override_stay_unsupported_after_vendor_passthrough() {
        for hw_protocol_id in [j2534_0404::CAN, j2534_0404::PROTOCOL_TP2_0_PS] {
            assert_eq!(
                PARAM_ACCESS_TIMING_ECU.to_j2534_config_id(hw_protocol_id),
                None
            );
            assert_eq!(
                PARAM_ACCESS_TIMING_OVERRIDE.to_j2534_config_id(hw_protocol_id),
                None
            );
        }
    }
}
