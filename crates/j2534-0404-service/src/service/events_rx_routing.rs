use super::*;

/// Which of a [`UniqueRespIdKey`]'s configured fields matched an inbound
/// frame's CAN ID (`UniqueRespIdKey::matched`'s own precedence order: USDT,
/// then UUDT, then J1939 SA).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchKind {
    Usdt,
    Uudt,
    J1939Sa,
    Tp20RxId,
    Tp20TxId,
}

/// ADR-222: disambiguates a numeric CAN ID's configured width when the SAME
/// numeric id is configured at two different widths (11-bit vs. 29-bit) by
/// different `UniqueRespIdTable` entries sharing one physical channel --
/// either on the same table (a same-entry-set collision) or across two
/// CLLs sharing one raw-CAN `CAN_ID_BOTH` channel (ADR-065's own motivating
/// scenario). `Any` (the default, and the ONLY value `build_cll_rx_entries`
/// ever produces for an id it saw at just one width) matches a frame of
/// either width -- a provable no-op relative to the pre-ADR-222
/// numeric-only comparison. `Bits11`/`Bits29` are only ever produced for a
/// CONTENDED id (seen at both widths somewhere on the channel), and match
/// only a frame actually carrying that width (`RxStatus` bit 8,
/// `CAN_29BIT_ID_STATUS`, threaded in as `frame_is_29bit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CanIdWidthGate {
    Any,
    Bits11,
    Bits29,
}

impl CanIdWidthGate {
    pub(super) fn accepts(self, frame_is_29bit: bool) -> bool {
        match self {
            Self::Any => true,
            Self::Bits11 => !frame_is_29bit,
            Self::Bits29 => frame_is_29bit,
        }
    }

    /// ADR-222 (Codex review finding, PR #141): the canonical width
    /// component for the software-ISO-TP reassembly map's key -- derived
    /// from the matched entry's OWN gate, never from a frame's raw
    /// `frame_is_29bit` bit directly. An uncontended id (`Any`) must
    /// reassemble under ONE stable key regardless of a device's per-frame
    /// `RxStatus` bit-8 noise across a single segmented transfer (e.g. an
    /// honestly-tagged FirstFrame followed by untagged ConsecutiveFrames)
    /// -- the same width-blind tolerance `accepts` already grants an `Any`
    /// gate for routing; keying reassembly by the raw per-frame bit instead
    /// would silently split one transfer's segments across two map slots
    /// and drop the response, a regression from the pre-ADR-222
    /// CAN-ID-only key. Only a genuinely contended id (`Bits11`/`Bits29`)
    /// needs two independent reassembly slots -- keyed by its own fixed
    /// configured width, which for any frame that reached this point is
    /// already guaranteed equal to that frame's own bit (`accepts` having
    /// returned `true`), so deriving from the gate instead of the frame
    /// changes nothing for the contended case.
    pub(super) fn reassembly_key_width(self) -> bool {
        matches!(self, Self::Bits29)
    }
}

/// Decodes a `CP_Can*Format` UNUM32 bitfield's Table B.13 bit 1 (CAN ID
/// Type): `true` for a 29-bit identifier, `false` for 11-bit or when the
/// ComParam is absent. Mirrors `tx_header::can_29bit_id`/
/// `rpc_link::CanIdFormat::extended_can_id`, which decode the same bit for
/// TX composition and `FLOW_CONTROL_FILTER` construction respectively --
/// not reused directly (both are private to their own modules) since this
/// is a third, RX-matching-side, consumer of the same Table B.13 bit
/// (ADR-222).
fn can_29bit_id(format_raw: Option<u32>) -> bool {
    format_raw.is_some_and(|raw| raw & 0x02 != 0)
}

/// ADR-222 round 4 (design-advisor audit, PR #141): which frame
/// population(s) a `CP_CanRespUSDTId`/`CP_CanRespUUDTId` value's width
/// actually needs to be tracked -- and later resolved -- against.
/// `process_frame_for_entry` compares a numeric id against exactly two
/// distinct frame populations: `iso` (ISO15765-tagged frames, the USDT/
/// ISO-tagged `Hardware` arm and the `SoftwareIsoTp` arm) and `can`
/// (non-ISO15765-tagged frames, a raw-CAN `PASS_FILTER`/companion-channel
/// match). A field that is only ever compared against ONE of those
/// populations (e.g. a native-mixed entry's own UUDT id, which is only ever
/// reached via the raw-CAN-tagged arm) must only contend against OTHER
/// values seen in that SAME population -- contending it against a value only
/// ever compared in the other population is a false collision that can
/// wrongly turn an `Any` gate into a strict width check for a numeric id
/// that, in routing terms, was never actually shared with anything (the
/// round-4 finding this replaces the single combined `widths_seen` map to
/// fix). `Both` (the pre-round-4 behavior) is still correct for every field
/// that genuinely IS compared against both populations depending on how the
/// frame carrying it happens to be tagged (e.g. a plain single-channel
/// entry's own USDT/UUDT ids, or a software-ISO-TP entry's ids). `Neither`
/// is for a field that never contributes to routing at all for its own CLL
/// under the current mode (a dual-channel-mode primary's own UUDT field,
/// which can only ever be served by its separate ADR-046 companion channel,
/// never by the primary channel this classification is computed for).
///
/// `width_domains` computes BOTH fields' domains for one link in one call --
/// see its own doc comment -- so `build_cll_rx_entries` never has two
/// independently-derived answers for the same link that could disagree
/// (ADR-217 round 2's "two searches disagree" bug class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WidthDomain {
    Iso,
    Can,
    Both,
    Neither,
}

impl WidthDomain {
    /// `true` iff `id` is contended within the population(s) this domain
    /// covers -- consults `iso_widths_seen`/`can_widths_seen` per `self`,
    /// `||`-combining both for `Both` (contended in EITHER population is
    /// enough, since `matched`'s USDT-then-UUDT sequence means a value
    /// shared across both tiers is exactly as ambiguous as a same-tier
    /// collision -- ADR-222's original cross-field reasoning, now scoped by
    /// domain instead of applied unconditionally).
    fn contended(
        self,
        iso_widths_seen: &HashMap<u32, HashSet<bool>>,
        can_widths_seen: &HashMap<u32, HashSet<bool>>,
        id: u32,
    ) -> bool {
        let iso_contended = || iso_widths_seen.get(&id).is_some_and(|w| w.len() > 1);
        let can_contended = || can_widths_seen.get(&id).is_some_and(|w| w.len() > 1);
        match self {
            Self::Iso => iso_contended(),
            Self::Can => can_contended(),
            Self::Both => iso_contended() || can_contended(),
            Self::Neither => false,
        }
    }

    /// Records `id`'s width into whichever of `iso_widths_seen`/
    /// `can_widths_seen` this domain covers (both, for `Both`; neither, for
    /// `Neither`).
    fn record(
        self,
        iso_widths_seen: &mut HashMap<u32, HashSet<bool>>,
        can_widths_seen: &mut HashMap<u32, HashSet<bool>>,
        id: u32,
        is_29bit: bool,
    ) {
        if matches!(self, Self::Iso | Self::Both) {
            iso_widths_seen.entry(id).or_default().insert(is_29bit);
        }
        if matches!(self, Self::Can | Self::Both) {
            can_widths_seen.entry(id).or_default().insert(is_29bit);
        }
    }
}

/// ADR-222 round 4 (design-advisor audit, PR #141): `true` iff `l` is a
/// native-mixed-family-eligible link -- the exact predicate
/// `build_cll_rx_entries`'s own `RxEntryKind::Hardware { native_mixed }`
/// construction used to compute inline (and `width_domains` below now also
/// needs, for the SAME link, to classify its two CAN-ID fields'
/// contention domains) -- extracted so both call sites read one definition
/// instead of two copies that could drift apart.
fn is_native_mixed_link(l: &LogicalLinkState, mode_is_native_mixed: bool) -> bool {
    mode_is_native_mixed
        && resources::base_protocol_id(l.hw_protocol_id) == j2534_0404::ISO15765
        && !link_is_qualified(l)
}

/// ADR-222 round 6 (design-advisor audit, PR #141): `true` iff `l` carries a
/// connect-time resource qualifier (SAE J2534-2 Pin Selection
/// (`pin_select`) or Additional Channels (`channel_index`)) or is
/// FD-substituted (`resources::is_fd_protocol_id`) -- the exact "qualified"
/// disqualifier `rpc_link.rs`'s `install_point_to_point_fc_filters` callers
/// already `||`-combine into their own `qualified` argument at each of that
/// function's three call sites (see its own doc comment). `is_native_mixed_
/// link` already needed this predicate's negation inline; extracted so
/// `is_dual_channel_link` below reuses the identical definition instead of
/// re-deriving it independently (the ADR-217-round-2 "two searches
/// disagree" discipline).
fn link_is_qualified(l: &LogicalLinkState) -> bool {
    l.pin_select.is_some()
        || l.channel_index.is_some()
        || resources::is_fd_protocol_id(l.hw_protocol_id)
}

/// ADR-222 round 6 (design-advisor audit, PR #141): `true` iff `l` is the
/// primary channel of a `DualChannel`-mode CLL that actually gets an
/// ADR-046 companion channel -- mirrors `rpc_link.rs`'s own `install_point_
/// to_point_fc_filters`' `dual_channel` local (`effective_can_mode ==
/// CanChannelMode::DualChannel && !qualified`) exactly, using the same
/// `link_is_qualified` a qualified link never gets a companion channel
/// (ADR-157), and therefore still needs its own point-to-point UUDT
/// fallback filter considered eligible below, same as any other
/// non-dual-channel link.
fn is_dual_channel_link(l: &LogicalLinkState, can_channel_mode: CanChannelMode) -> bool {
    can_channel_mode == CanChannelMode::DualChannel && !link_is_qualified(l)
}

/// ADR-222 round 4 (design-advisor audit, PR #141): the `(usdt_domain,
/// uudt_domain)` [`WidthDomain`] pair for `l`'s own `CP_CanRespUSDTId`/
/// `CP_CanRespUUDTId` fields, when building `channel_id`'s own RX entries --
/// the single source of truth `build_cll_rx_entries` calls ONCE per link and
/// reuses for both contributing to `iso_widths_seen`/`can_widths_seen` and
/// resolving every gate drawn from them (the `UniqueRespIdKey` construction,
/// the `SoftwareIsoTp` `FcPair` gate, and the `usdt_addressing_by_id`/
/// `uudt_addressing_by_id` tables) -- never recomputed independently at each
/// site (ADR-217 round 2's "two searches disagree" bug class).
///
/// `native_mixed_link` is the caller's own `is_native_mixed_link(l, ...)`
/// result, threaded in rather than recomputed here so a caller that also
/// needs it for `RxEntryKind::Hardware`'s own field gets the identical
/// value, not a second independently-evaluated one.
///
/// - `l.channel_id == Some(channel_id)` and `l.software_isotp`: both fields
///   are `Both` -- a software-ISO-TP link's own USDT/UUDT ids are compared
///   against whichever population each individual raw frame happens to be
///   tagged, same as the pre-round-4 combined-map model.
/// - `l.channel_id == Some(channel_id)` and `native_mixed_link`: USDT is
///   `Iso` (only ever compared by the ISO-tagged `Hardware` arm), UUDT is
///   `Can` (only ever compared by the raw-CAN-tagged `Hardware` arm --
///   `route_frame_uudt_only`, never reached with an ISO-tagged frame for a
///   native-mixed entry, `process_frame_for_entry`'s own `uudt_eligible =
///   !native_mixed` gate).
/// - `l.channel_id == Some(channel_id)` and `l.uudt_channel_id.is_some()`
///   (a dual-channel-mode primary): USDT is `Both` (still compared by
///   whichever tag a frame on the primary channel carries), UUDT is
///   `Neither` -- a dual-channel primary's own UUDT field is never served by
///   the primary channel at all (ADR-046: only the separate companion
///   channel ever delivers UUDT), and the routing change below additionally
///   disables the UUDT tier entirely for this case.
/// - `l.channel_id == Some(channel_id)`, none of the above (plain single-
///   channel/ADR-041/qualified/FD/KWP/J1939 primary): both fields `Both`.
/// - `l.uudt_channel_id == Some(channel_id)` (this link's own ADR-046
///   companion channel): USDT is `Neither` (a companion channel never
///   carries USDT traffic), UUDT is `Both` (compared the same way regardless
///   of which raw tag the companion channel's own frames carry).
fn width_domains(
    l: &LogicalLinkState,
    channel_id: ChannelId,
    native_mixed_link: bool,
) -> (WidthDomain, WidthDomain) {
    if l.channel_id == Some(channel_id) {
        if l.software_isotp {
            (WidthDomain::Both, WidthDomain::Both)
        } else if native_mixed_link {
            (WidthDomain::Iso, WidthDomain::Can)
        } else if l.uudt_channel_id.is_some() {
            (WidthDomain::Both, WidthDomain::Neither)
        } else {
            (WidthDomain::Both, WidthDomain::Both)
        }
    } else if l.uudt_channel_id == Some(channel_id) {
        (WidthDomain::Neither, WidthDomain::Both)
    } else {
        (WidthDomain::Neither, WidthDomain::Neither)
    }
}

/// ADR-222, domain-scoped by round 4 (design-advisor audit, PR #141):
/// resolves one numeric CAN id's [`CanIdWidthGate`] from `iso_widths_seen`/
/// `can_widths_seen` (the two per-channel contention maps -- see
/// [`WidthDomain`]'s own doc comment for why there are two, not one
/// combined map) via `domain`, this specific field's own [`WidthDomain`]
/// (`usdt_domain` for `CP_CanRespUSDTId`, `uudt_domain` for
/// `CP_CanRespUUDTId` -- see `width_domains`). `is_29bit` is the width THIS
/// specific entry's own Format ComParam decodes to -- only consulted when
/// `id` is contended within `domain`'s own population(s); an uncontended id
/// always resolves to `Any` regardless of `is_29bit`.
fn resolve_width_gate(
    iso_widths_seen: &HashMap<u32, HashSet<bool>>,
    can_widths_seen: &HashMap<u32, HashSet<bool>>,
    domain: WidthDomain,
    id: u32,
    is_29bit: bool,
) -> CanIdWidthGate {
    if domain.contended(iso_widths_seen, can_widths_seen, id) {
        if is_29bit {
            CanIdWidthGate::Bits29
        } else {
            CanIdWidthGate::Bits11
        }
    } else {
        CanIdWidthGate::Any
    }
}

/// One `active_unique_resp_id_table` entry's CAN-ID-routable fields,
/// distilled for RX matching (`CllRxEntry::unique_resp_ids`) -- replaces the
/// earlier raw `(uid, usdt, uudt)` 3-tuple (ADR-184) with a named struct that
/// has a slot for `CP_J1939SourceAddress` too.
pub(super) struct UniqueRespIdKey {
    pub(super) unique_resp_identifier: u32,
    /// `CP_CanRespUSDTId`.
    pub(super) can_resp_usdt_id: Option<u32>,
    /// ADR-222, domain-scoped by round 4 (design-advisor audit, PR #141):
    /// this entry's own `can_resp_usdt_id` width gate, computed once per
    /// poll pass by `build_cll_rx_entries` from whichever of the channel-wide
    /// `iso_widths_seen`/`can_widths_seen` contention maps this link's own
    /// USDT [`WidthDomain`] says to consult (`width_domains`; usually `Both`,
    /// checking either map -- see that function's own doc comment for the
    /// narrower cases). `Any` unless `can_resp_usdt_id`'s numeric value is
    /// configured at two different widths somewhere WITHIN that domain's own
    /// population(s), whether by another entry's USDT id or a UUDT id.
    /// Inert (never consulted) when `can_resp_usdt_id` is `None`.
    pub(super) usdt_width_gate: CanIdWidthGate,
    /// `CP_CanRespUUDTId`.
    pub(super) can_resp_uudt_id: Option<u32>,
    /// ADR-222, domain-scoped by round 4 (design-advisor audit, PR #141):
    /// the `can_resp_uudt_id` counterpart to `usdt_width_gate` -- resolved
    /// the same way, but via this link's own UUDT [`WidthDomain`] (which can
    /// differ from the USDT field's own domain -- e.g. a native-mixed
    /// entry's USDT is `Iso`-only while its UUDT is `Can`-only).
    pub(super) uudt_width_gate: CanIdWidthGate,
    /// `CP_J1939SourceAddress` (ADR-184): the responding ECU's SAE J1939
    /// source address, spec range `[0, 0xFFFF]` per ISO 22900-2's own NOTE
    /// on this ComParam (the extended range covers an 11-bit-CAN-ID
    /// URID-assignment mode this codebase never reaches -- J1939 CLLs always
    /// connect flat `CAN_29BIT_ID`, `rpc_link.rs`'s own `connect_flags`
    /// derivation -- so a value above `0xFF` can configure but can never
    /// match a real frame; an accepted residual, not enforced here).
    pub(super) j1939_source_address: Option<u32>,
    /// SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a): this CLL's
    /// own established connection's `requested_rx_id`, when connected --
    /// unlike `j1939_source_address` (a client-`SetUniqueRespIdTable`-
    /// configured value), this is never client-configured through the
    /// table at all; `build_cll_rx_entries` synthesizes one entry per
    /// TP2.0 CLL directly from `LogicalLinkState::tp20_connection`, since
    /// the RX-ID is already a structural property of the connection
    /// itself (clause 19's own one-CLL-one-connection-one-RX-ID model),
    /// with no addressing-table configuration concept to mirror. Matching
    /// this field is gated on `!is_tx_side` (Codex review, PR #97, round 12
    /// fix: an earlier version matched it unconditionally -- symmetric to
    /// the bug `tp20_tx_id`'s own round-10 fix closed on the OTHER tier --
    /// which meant a TX-side echo of a DIFFERENT connection's own write
    /// could also match this entry whenever that connection's TX-ID
    /// happened to equal this CLL's own RX-ID, delivering that other
    /// connection's transmit echo here too).
    pub(super) tp20_rx_id: Option<u32>,
    /// SAE J2534-2 clause 19 TP2.0 (Codex review, PR #97): this CLL's own
    /// established connection's `established_tx_id`, when connected -- a
    /// `CP_Loopback`-enabled write's device-generated echo carries the
    /// written frame's own 4-byte address prefix unchanged (mirrors a real
    /// frame's `Data[0..3]`, but holds the TX-ID we wrote with, not an RX-ID
    /// any peer ever addresses us with), so it never matches `tp20_rx_id`
    /// and was previously dropped for the originating CLL (or, worse,
    /// misdelivered to a sibling CLL whose own `requested_rx_id` happens to
    /// equal this TX-ID). No real inbound frame from a peer ECU can ever
    /// carry our own TX-ID as its address (clause 19's connections address
    /// each direction by the receiver's own RX-ID). Matching this field is
    /// gated on the frame's own `is_tx_side` flag (Codex review, PR #97,
    /// round 10 fix: an earlier version matched it unconditionally, which
    /// meant a GENUINE inbound frame addressed to a sibling connection's own
    /// RX-ID -- not a loopback echo at all -- could also match this entry
    /// whenever that sibling's RX-ID happened to equal this connection's own
    /// TX-ID, delivering the sibling's real traffic here too. `matched`
    /// only ever checks this field when the caller says the frame IS a
    /// TX-side one (`RxStatus`'s `TX_MSG_TYPE`/`TX_INDICATION` bits), never
    /// for ordinary received content.
    pub(super) tp20_tx_id: Option<u32>,
    /// `CP_EcuRespSourceAddress` (ADR-203): the responding ECU's KWP
    /// (ISO9141/ISO14230) or SAE J1850 (VPW/PWM) source address, `[0,
    /// 0xFF]` (a single wire byte). Unlike every other field on this
    /// struct, this is NOT consulted by `UniqueRespIdKey::matched` -- KWP/
    /// J1850 frames routinely have no CAN id at all, so `matched`'s
    /// `can_id: u32` parameter would be meaningless for them. Instead,
    /// `route_frame` checks this field directly, in its own protocol-gated
    /// branch, before ever reaching `matched`. Also a legal, deliberately
    /// inert SURPLUS param on a `CAN_UNIQUE_ID_UNUM32` entry (documented in
    /// `docs/implementation-notes.md`'s 2022-edition-delta-audit section) --
    /// `build_cll_rx_entries` only ever populates this field for a KWP/
    /// J1850 CLL (see its own doc comment on the gate), so a CAN CLL's
    /// surplus `CP_EcuRespSourceAddress`-only entry keeps reading as
    /// CAN-ID-routeless and correctly falls back to the no-table wildcard,
    /// exactly as before this ADR.
    pub(super) ecu_resp_source_addr: Option<u32>,
}

impl UniqueRespIdKey {
    /// Returns which field (if any) matches `can_id`, in USDT -> UUDT ->
    /// J1939 SA precedence -- mirrors the pre-ADR-184 USDT-then-UUDT `find`
    /// order for the two CAN fields, with J1939 SA appended as the new third
    /// tier (no existing CLL configures both CAN and J1939 fields on the
    /// same entry, so precedence between J1939 SA and the CAN fields is
    /// never actually contended in practice).
    ///
    /// SA matching (ADR-184): SAE J1939-21's 29-bit CAN identifier packs
    /// priority/EDP/DP (3 bits), PDU Format (8 bits), PDU Specific (8 bits),
    /// and the source address (the low 8 bits) into the 29-bit id,
    /// right-justified into the 32-bit big-endian read `frame_can_id`
    /// carries -- so `can_id & 0xFF` IS the source address byte (equivalent
    /// to reading raw frame byte 3 of the clause 16.4.3 5-byte prefix, the
    /// derivation the now-removed `tx_header::j1939_source_address` used).
    /// Never matches a configured `j1939_source_address` above `0xFF`: no
    /// real frame's low byte can exceed `0xFF` (see this struct's own field
    /// doc comment on the dead extended-range NOTE).
    ///
    /// `is_tx_side` (Codex review, PR #97, rounds 10 and 12): `true` for a
    /// frame carrying `RxStatus`'s `TX_MSG_TYPE`/`TX_INDICATION` bit (this
    /// device's own transmit echo/completion notice, ADR-099's own
    /// definition) -- makes the `tp20_rx_id`/`tp20_tx_id` tiers mutually
    /// exclusive: `tp20_tx_id` only eligible when `is_tx_side` (round 10),
    /// `tp20_rx_id` only eligible when `!is_tx_side` (round 12, closing the
    /// symmetric leak the round-10 fix alone left open -- see each field's
    /// own doc comment).
    ///
    /// `uudt_eligible` (ADR-217 Codex-review fix, PR #132 round 2): `false`
    /// makes the UUDT tier never match (fall through to the lower-precedence
    /// tiers, or `None`), same gating shape as `is_tx_side`'s TP2.0 tiers --
    /// see `route_frame_with_role`'s own doc comment for why this exists.
    ///
    /// `frame_is_29bit` (ADR-222): the frame's own `RxStatus` bit 8
    /// (`CAN_29BIT_ID_STATUS`) -- gates the USDT/UUDT tiers only, via each
    /// field's own `usdt_width_gate`/`uudt_width_gate` (`CanIdWidthGate::
    /// accepts`). A provable no-op on an uncontended id (gate `Any`
    /// unconditionally accepts), so every pre-ADR-222 caller/table shape is
    /// unaffected. `j1939_source_address`/`tp20_rx_id`/`tp20_tx_id` are
    /// deliberately NOT gated by this -- out of scope per ADR-222 (J1939
    /// CLLs always connect flat `CAN_29BIT_ID`, never `CAN_ID_BOTH`; TP2.0's
    /// ids are structural, not Format-ComParam-configured).
    fn matched(
        &self,
        can_id: u32,
        is_tx_side: bool,
        uudt_eligible: bool,
        frame_is_29bit: bool,
    ) -> Option<MatchKind> {
        if self.can_resp_usdt_id == Some(can_id) && self.usdt_width_gate.accepts(frame_is_29bit) {
            Some(MatchKind::Usdt)
        } else if uudt_eligible
            && self.can_resp_uudt_id == Some(can_id)
            && self.uudt_width_gate.accepts(frame_is_29bit)
        {
            Some(MatchKind::Uudt)
        } else if self
            .j1939_source_address
            .is_some_and(|sa| sa == can_id & 0xFF)
        {
            Some(MatchKind::J1939Sa)
        } else if !is_tx_side && self.tp20_rx_id == Some(can_id) {
            Some(MatchKind::Tp20RxId)
        } else if is_tx_side && self.tp20_tx_id == Some(can_id) {
            Some(MatchKind::Tp20TxId)
        } else {
            None
        }
    }
}

/// Returns the `unique_resp_identifier` to use when delivering a frame with
/// `frame_can_id` to `entry`, or `None` when the frame should be dropped for
/// this CLL (CAN ID not in the routing table). `is_tx_side` -- see
/// `UniqueRespIdKey::matched`'s own doc comment -- gates the TP2.0
/// `tp20_tx_id` tier only; every other tier is unaffected by it.
///
/// `frame_source_addr` (ADR-203): the frame's own KWP/J1850 source-address
/// byte (`kline_j1850_source_addr`, `events.rs`), consulted ONLY for a KWP/
/// J1850 entry (`entry.header_protocol`) -- routed through a dedicated
/// protocol-gated branch, placed after the `unique_resp_ids.is_empty()`
/// wildcard check (so a KWP/J1850 CLL with no SA-keyed entries still
/// wildcard-delivers, unchanged) but before the generic CAN-ID `.find()`
/// below. This branch is deliberately NOT routed through
/// `UniqueRespIdKey::matched` -- see that method's own doc comment and
/// `UniqueRespIdKey::ecu_resp_source_addr`'s field doc for why. A `None`
/// result here (no source byte parsed, or no configured entry's
/// `ecu_resp_source_addr` matches it) means DROP the frame for this CLL --
/// the same "explicit table, no match" behavior the CAN/J1939 tiers below
/// already have, per ISO 22900-2:2022 §8.4.28.7.3's unmatched-response
/// model (paraphrased): a response is only ever delivered as `PDU_ID_UNDEF`
/// via an explicitly configured catch-all entry, never as an ad hoc
/// wildcard fallback once a table exists.
///
/// `is_content_frame` (Codex review finding, this PR): a KWP/J1850 entry
/// takes ONE of two paths depending on this flag, and NEITHER ever falls
/// through to the generic CAN-ID `.find()` below -- a dedicated early
/// `return`, not a conditional guard on the SA branch alone. A content
/// frame (a real message body) is matched against `ecu_resp_source_addr` as
/// described above. A non-content frame (a SOM/RX_BREAK/TxDone/loopback
/// indication, ADR-097/ADR-098) is delivered UNCONDITIONALLY at the
/// wildcard `0`, restoring the exact pre-ADR-203 behavior for these frames
/// regardless of the SA table's contents -- they are not "unmatched ECU
/// responses" ISO 22900-2:2022 §8.4.28.7.3's unmatched-response model
/// governs at all, so that model's "drop on no match" contract only ever
/// applies to a content frame. Two DIFFERENT bugs, both real, are why this
/// is a dedicated early return rather than a plain `is_content_frame &&`
/// guard on the SA branch that lets a `false` case fall through to the
/// CAN-ID `.find()` below:
/// - Indication frames on ISO9141/ISO14230/J1850 routinely carry no data at
///   all (an empty SOM/RX_BREAK payload), so `frame_source_addr` is always
///   `None` for them -- without gating the SA branch at all, every such
///   indication on a CLL with ANY SA-keyed entry would be silently dropped
///   by the `frame_source_addr.and_then` below before `bind_frame`'s own
///   `indication_suppressed` policy (which, notably, never suppresses
///   `RX_BREAK`) ever got a chance to run.
/// - `frame_can_id` (`events.rs`'s `poll_rx_inner`) is computed generically
///   from the frame's own first 4 raw bytes with NO protocol check --
///   `None` only when `data.len() < 4`. A loopback/TxDone indication
///   (`is_content_frame == false`, but carrying the real echoed
///   header+payload bytes of whatever was transmitted, `ADR-098`) is
///   routinely `>= 4` bytes for any realistic K-line/J1850 request, so
///   `frame_can_id` is `Some(<K-line header bytes misread as a u32>)` for
///   it, NOT `None` -- falling through to the CAN-ID `.find()` below would
///   search `entry.unique_resp_ids` (populated only with SA-keyed entries
///   for a KWP/J1850 CLL, never CAN-only fields) via `UniqueRespIdKey::
///   matched`, which can never succeed, dropping the echo instead of
///   delivering it at `0` -- silently violating the same ADR-098 "loopback/
///   TxDone must never be filtered from delivery" invariant this whole gate
///   exists to protect, just for a different indication subtype than the
///   empty-payload SOM/RX_BREAK case above.
///
/// No `RawMode` gate (ADR-196 Decision item 3 already establishes that
/// `route_frame`'s own URID-table matching stays active under RawMode; that
/// ADR's bypass convention applies to ComParam-derived TX composition and
/// RX header splitting only) and no `is_tx_side` gate on the SA branch
/// itself (mirrors ADR-184's ungated J1939 SA tier: a TX-side echo's own
/// byte-2 value is the tester's own address, which structurally cannot
/// match a real `CP_EcuRespSourceAddress` entry, so a genuine content-frame
/// echo -- e.g. a loopback frame that also happens to be tagged content --
/// is naturally dropped in table mode with no special-case echo detection
/// needed; `is_tx_side` gates the pre-existing TP2.0 `tp20_tx_id` tier
/// below only, unrelated to KWP/J1850).
/// Combines what were previously two independent searches -- `route_frame`'s
/// own uid lookup and `route_frame_matched_uudt`'s separate "was it a UUDT
/// tier match" lookup -- into ONE, returning `(unique_resp_identifier,
/// uudt_routed)` (ADR-217 Codex-review fix, PR #132 round 2). Two mirrored
/// searches over the same `unique_resp_ids` list, computed independently,
/// can disagree if only one of them is updated for a new gating rule -- this
/// exact class of bug is what round 2's finding was: a fix that gated
/// `route_frame`'s own uid search but left `route_frame_matched_uudt`
/// computing its own unaware answer would have reintroduced the round-1
/// addressing-table bug by feeding `CllRxEntry::rx_addressing_table` a
/// `uudt_routed` that no longer agreed with which key `route_frame` actually
/// used. A single combined search makes agreement structural: whichever
/// `MatchKind` wins is recorded in the SAME closure invocation that resolves
/// the uid.
///
/// `uudt_eligible` (ADR-217 Codex-review fix, PR #132 round 2): `false`
/// disables the UUDT tier for this call, via `UniqueRespIdKey::matched`'s own
/// `uudt_eligible` gate. `route_frame`'s call site (below) always passes
/// `true`, preserving every pre-existing caller's exact behavior -- this
/// parameter exists for `process_frame_for_entry`'s own native-mixed-aware
/// call site (`events.rs`) to pass `false` for an ISO15765-tagged frame on a
/// native-mixed entry: under native-mixed, a UUDT id is served by a
/// `PASS_FILTER`, never the ADR-041 `FLOW_CONTROL_FILTER` workaround
/// (`rpc_link.rs`), so the ONLY way a UUDT-configured entry's frames ever
/// arrive is CAN-tagged, via `route_frame_uudt_only`'s own dedicated arm
/// (`process_frame_for_entry`'s FIRST `RxEntryKind::Hardware` arm) -- an
/// ISO15765-tagged frame whose CAN ID happens to equal a native-mixed
/// entry's own UUDT id is always some OTHER entry's USDT interpretation
/// (ADR-217's own new dual-delivery collision case), never a legitimate
/// UUDT delivery for THIS entry. Every other caller (non-native-mixed
/// `Hardware`/`Companion` via `route_frame`'s ADR-041 single-channel
/// workaround, which genuinely does need the UUDT tier reachable via an
/// ISO15765-tagged frame) is unaffected -- `uudt_eligible: true` there is a
/// provable no-op for the pre-`ALL_FRAMES` behavior this function replaces.
/// `frame_is_29bit` (ADR-222): threaded straight to `UniqueRespIdKey::
/// matched` -- see that method's own doc comment. Not consulted by the
/// KWP/J1850 `ecu_resp_source_addr` branch above (CAN-width-less).
pub(super) fn route_frame_with_role(
    entry: &CllRxEntry,
    frame_can_id: Option<u32>,
    frame_source_addr: Option<u8>,
    is_content_frame: bool,
    is_tx_side: bool,
    uudt_eligible: bool,
    frame_is_29bit: bool,
) -> Option<(u32, bool)> {
    if entry.unique_resp_ids.is_empty() {
        return Some((0, false)); // no-table mode — deliver unconditionally
    }
    if matches!(
        entry.header_protocol,
        j2534_0404::ISO9141 | j2534_0404::ISO14230 | j2534_0404::J1850PWM | j2534_0404::J1850VPW
    ) {
        if !is_content_frame {
            return Some((0, false)); // indication frame: always wildcard-deliver
        }
        return frame_source_addr.and_then(|sa| {
            entry
                .unique_resp_ids
                .iter()
                .find(|k| k.ecu_resp_source_addr == Some(u32::from(sa)))
                .map(|k| (k.unique_resp_identifier, false))
        });
    }
    match frame_can_id {
        Some(can_id) => entry.unique_resp_ids.iter().find_map(|key| {
            key.matched(can_id, is_tx_side, uudt_eligible, frame_is_29bit)
                .map(|kind| (key.unique_resp_identifier, kind == MatchKind::Uudt))
        }),
        None => Some((0, false)), // frame too short for CAN ID extraction; deliver without routing
    }
}

/// Test-only (ADR-217 Codex-review fix, PR #132 round 2): the single
/// production call site that used to call this function
/// (`process_frame_for_entry`'s ISO-tagged `RxEntryKind::Hardware` arm,
/// `events.rs`) now calls `route_frame_with_role` directly, since it needs
/// BOTH the uid and the `uudt_routed` role from one call and a real
/// `uudt_eligible` value (`!native_mixed`) rather than this wrapper's
/// hardcoded `true`. Kept as a thin wrapper, not deleted, purely so this
/// crate's existing `route_frame`-named unit tests (`events_bind_frame_tests.rs`,
/// `events_build_cll_rx_entries_tests.rs`) keep exercising `matched()`'s core
/// per-entry precedence through a simpler uid-only API, unchanged from every
/// one of their ~35 pre-ADR-217 call sites (`uudt_eligible: true` here is a
/// provable no-op relative to this function's pre-ADR-217 behavior).
/// ADR-222: `frame_is_29bit` is hardcoded `false` here -- no existing test
/// exercising this wrapper configures a contended id, so every gate any of
/// them reach is `Any` regardless of what this constant would otherwise
/// select; existing callers' behavior is unaffected.
#[cfg(test)]
pub(super) fn route_frame(
    entry: &CllRxEntry,
    frame_can_id: Option<u32>,
    frame_source_addr: Option<u8>,
    is_content_frame: bool,
    is_tx_side: bool,
) -> Option<u32> {
    route_frame_with_role(
        entry,
        frame_can_id,
        frame_source_addr,
        is_content_frame,
        is_tx_side,
        true,
        false,
    )
    .map(|(uid, _)| uid)
}

/// Companion-channel routing: only `CP_CanRespUUDTId` matches are delivered.
/// J1939 has no UUDT concept (no dual-channel-mode Companion path ever opens
/// for a J1939 CLL) and no `CP_CanRespUSDTId` either, so this function is
/// deliberately NOT routed through `UniqueRespIdKey::matched` (ADR-184) --
/// it keeps its own field-specific check, unaffected by SA matching.
///
/// `frame_is_29bit` (ADR-222): gates the `can_resp_uudt_id` comparison via
/// the matched key's own `uudt_width_gate` -- see `CanIdWidthGate::accepts`.
pub(super) fn route_frame_uudt_only(
    entry: &CllRxEntry,
    frame_can_id: Option<u32>,
    frame_is_29bit: bool,
) -> Option<u32> {
    let can_id = frame_can_id?;
    entry
        .unique_resp_ids
        .iter()
        .find(|key| {
            key.can_resp_uudt_id == Some(can_id) && key.uudt_width_gate.accepts(frame_is_29bit)
        })
        .map(|key| key.unique_resp_identifier)
}

/// ADR-150 (ADR-184 follow-up): `true` iff `route_frame`'s own routing
/// decision for `frame_can_id` on `entry` (the `RxEntryKind::Hardware` arm)
/// was made via a `CP_CanRespUUDTId` match rather than `CP_CanRespUSDTId` or
/// a J1939 `CP_J1939SourceAddress` match -- mirrors `route_frame`'s own
/// `find`/`UniqueRespIdKey::matched` (same per-entry precedence, same
/// first-matching-entry selection), but reports WHICH tier actually matched
/// instead of just the resolved `unique_resp_identifier`. `false` for the
/// no-table wildcard (empty `unique_resp_ids`, `route_frame`'s own "deliver
/// unconditionally" case), for a too-short frame with no CAN ID, and now
/// also (by construction, not by a special case) for an SA-only match --
/// `MatchKind::J1939Sa` is simply not `MatchKind::Uudt`.
///
/// Test-only (ADR-217 Codex-review fix, PR #132 round 2), same reasoning as
/// `route_frame`'s own doc comment: the one production call site that used
/// to pair this function with a separate `route_frame` call now resolves
/// BOTH the uid and this function's own answer from a single
/// `route_frame_with_role` search instead (that function does NOT call this
/// one -- it reimplements the combined search inline). Kept only for its
/// existing unit tests, which pin `matched()`'s own UUDT-tier-detection
/// precedence in isolation from the uid.
#[cfg(test)]
pub(super) fn route_frame_matched_uudt(
    entry: &CllRxEntry,
    frame_can_id: Option<u32>,
    is_tx_side: bool,
) -> bool {
    if entry.unique_resp_ids.is_empty() {
        return false;
    }
    let Some(can_id) = frame_can_id else {
        return false;
    };
    // `uudt_eligible: true` -- see `route_frame`'s own doc comment above:
    // this function has no production caller at all any more, only tests
    // exercising it directly, so this stays a provable no-op relative to
    // this function's pre-ADR-217 behavior. `frame_is_29bit: false` for the
    // same reason `route_frame`'s own wrapper hardcodes it (ADR-222): no
    // existing test here configures a contended id.
    entry
        .unique_resp_ids
        .iter()
        .find_map(|key| key.matched(can_id, is_tx_side, true, false))
        == Some(MatchKind::Uudt)
}

/// Snapshots the routing state for all CLLs receiving from `channel_id` —
/// either as their primary channel or as their dual-channel-mode UUDT
/// companion channel (ADR-046).
///
/// Reads each CLL's `active_unique_resp_id_table`, not Working: RX routing
/// must match what is actually installed on hardware (the ISO15765
/// `FLOW_CONTROL_FILTER`s built from the Active table) and what TX/COP
/// snapshots resolve against, not a staged-but-not-yet-promoted table
/// (ADR-068).
///
/// `set_seq_snapshot` (ADR-147 sixth amendment, pre-read capture for the
/// batch anchor): a CLL-handle -> `LogicalLinkState::error_set_seq` map,
/// captured by the caller (`poll_rx_inner`) BEFORE this pass's
/// `PassThruReadMsgs` call even starts, under its own separate
/// `logical_links` lock acquisition -- NOT re-read live here. Each
/// `CllRxEntry::set_seq_at_read` is stamped straight from this map
/// (`Some(seq)` for a handle present in it); a handle this function needs
/// but that is ABSENT from the map -- connected strictly after the pre-read
/// snapshot was taken but before this function runs, a narrow window -- is
/// stamped `None` instead, which `queue_error_class_to_apply` treats as
/// "always discard `Positive`" for that CLL this pass.
///
/// `can_channel_mode` (ADR-160/Phase 3c; widened to the family by ADR-217;
/// widened from a pre-collapsed `mode_is_native_mixed: bool` to the full
/// enum by ADR-222 round 6): the poll cycle's own
/// `effective_can_channel_mode()` snapshot (`poll_rx_inner`, which has
/// `ctx.service` and therefore `effective_can_channel_mode()` -- this plain
/// associated function does not), read ONCE by the caller and threaded
/// through so this function has a single source of truth for "what mode is
/// this poll pass running under" rather than deriving `mode_is_native_mixed`
/// and a separate `DualChannel` fact from two independent reads that could
/// disagree. `is_native_mixed_link`/`width_domains` derive `mode_is_native_
/// mixed` from `can_channel_mode.is_native_mixed_family()` (`true` under
/// either `NativeMixed` or `NativeMixedAllFrames`, since both sub-modes need
/// the same per-frame `ProtocolID` routing branch active, ADR-217 Decision
/// item 3); `is_dual_channel_link` derives the `DualChannel` fact the same
/// way. A `Hardware`-kind entry's own `native_mixed` field additionally
/// requires the entry's own channel to be ISO15765-family and not
/// FD-substituted (`l.hw_protocol_id`), AND unqualified
/// (`!link_is_qualified(l)`, ADR-217 Codex-review fix PR #132 round 2 --
/// previously missing here, though harmless before this round since every
/// qualified link's own frames are always ISO15765-tagged and this field's
/// only other reader, `process_frame_for_entry`'s raw-CAN-tagged arm, never
/// fired for one anyway; round 2's new `uudt_eligible`-gated ISO-tagged arm
/// below makes the omission load-bearing, since a qualified link on a
/// native-mixed module never gets clause 8 `SET_CONFIG`'d or a
/// `PASS_FILTER` at connect time -- `rpc_link.rs`'s own `!qualified` term on
/// every native-mixed call site, mirrored here) -- so a raw-CAN,
/// FD-substituted, or qualified link on a native-mixed-family-configured
/// module still gets `native_mixed: false`, exactly like every link does
/// under the other modes.
///
/// ADR-222 round 6 (design-advisor audit, PR #141): also used to compute
/// `dual_channel_link` per visited link in the contention-map collection
/// loop below -- see `point_to_point_filter_eligibility`'s own doc comment
/// (`rpc_link.rs`) for why the collection loop needs to know this in
/// addition to `native_mixed_link`.
///
/// `primitives` (ADR-204 Codex review, PR #116, round 3): locked ONCE here,
/// nested inside the already-held `logical_links` guard (this crate's
/// documented `logical_links -> primitives` lock order, never the reverse --
/// no deadlock risk, since no caller of this function holds `primitives`
/// itself), so each `CllRxEntry::cop_tags` snapshot is captured from the
/// SAME critical section, at the SAME instant, as `registrants` itself --
/// see that field's own doc comment for why this closes the removal race a
/// later, separately-locked `primitives` lookup was vulnerable to.
pub(super) async fn build_cll_rx_entries(
    channel_id: ChannelId,
    logical_links: &Arc<Mutex<HashMap<u32, LogicalLinkState>>>,
    primitives: &Arc<Mutex<HashMap<u32, CopEntry>>>,
    set_seq_snapshot: &HashMap<u32, u64>,
    can_channel_mode: CanChannelMode,
) -> Vec<CllRxEntry> {
    let mode_is_native_mixed = can_channel_mode.is_native_mixed_family();
    let links = logical_links.lock().await;
    let prims = primitives.lock().await;
    // ADR-222, domain-scoped by round 4 (design-advisor audit, PR #141):
    // per-channel numeric-CAN-ID-width contention, computed ONCE per poll
    // pass (not read per-frame) across every CLL sharing this physical
    // `channel_id` -- the SAME membership test the main pass below applies
    // (`l.channel_id == Some(channel_id) || l.uudt_channel_id ==
    // Some(channel_id)`), so a cross-CLL collision on a shared raw-CAN
    // `CAN_ID_BOTH` channel (ADR-065's own motivating scenario) is caught
    // exactly like a same-table collision. TWO maps, `iso_widths_seen`
    // (ISO15765-tagged frames) and `can_widths_seen` (non-ISO15765-tagged,
    // raw-CAN-matched frames) -- NOT one combined map across both
    // `CP_CanRespUSDTId` and `CP_CanRespUUDTId` regardless of which frame
    // population each field is actually compared against (an earlier
    // version of this code did exactly that, reasoning that
    // `UniqueRespIdKey::matched` compares the SAME raw frame `can_id`
    // against both fields in sequence; true for a plain single-channel
    // entry, but NOT for e.g. a native-mixed entry, whose USDT id is only
    // ever compared by the ISO-tagged arm and whose UUDT id is only ever
    // compared by the raw-CAN-tagged arm -- combining them regardless
    // falsely contends the two fields against each other even though no
    // single frame population ever compares them against the same value,
    // design-advisor audit finding, round 4). Each field's own [`WidthDomain`]
    // (`width_domains`, computed once per link and reused below for gate
    // resolution too) decides which map(s) it contributes to -- `Both` for
    // a field genuinely compared against either population, `Iso`/`Can` for
    // one compared against only one, `Neither` for a field that never
    // contributes to routing for its own CLL under the current mode (a
    // dual-channel primary's own UUDT field).
    let mut iso_widths_seen: HashMap<u32, HashSet<bool>> = HashMap::new();
    let mut can_widths_seen: HashMap<u32, HashSet<bool>> = HashMap::new();
    for l in links
        .values()
        .filter(|l| l.channel_id == Some(channel_id) || l.uudt_channel_id == Some(channel_id))
    {
        let native_mixed_link = is_native_mixed_link(l, mode_is_native_mixed);
        let (usdt_domain, uudt_domain) = width_domains(l, channel_id, native_mixed_link);
        // ADR-222 round 6 (design-advisor audit, PR #141): whether `l` is a
        // native ISO15765 hardware channel at all -- only on such a channel
        // does a table entry's field need to actually clear `rpc_link.rs`'s
        // `install_point_to_point_fc_filters` go/no-go decision (via
        // `point_to_point_filter_eligibility`) before it can contribute a
        // genuine width observation below. A software-ISO-TP link's own
        // `hw_protocol_id` bases to `CAN`, not `ISO15765` (`rpc_link.rs`'s
        // own `software_isotp` derivation), so it never matches this test --
        // every non-native channel (software-ISO-TP, raw CAN, the ADR-046
        // companion) stays wide-open/trivially-eligible below, unchanged
        // from pre-round-6 behavior.
        let native_hardware_link = l.channel_id == Some(channel_id)
            && resources::base_protocol_id(l.hw_protocol_id) == j2534_0404::ISO15765;
        let dual_channel_link = is_dual_channel_link(l, can_channel_mode);
        for e in &l.active_unique_resp_id_table {
            let elig = if native_hardware_link {
                point_to_point_filter_eligibility(e, dual_channel_link, native_mixed_link)
            } else {
                PointToPointFilterEligibility {
                    usdt: true,
                    uudt: true,
                }
            };
            // ADR-222 round 6: `usdt_resp_id`/`uudt_resp_id` (the
            // sentinel-aware helpers, excluding ISO 22900-2 Table 76's
            // `0xFFFFFFFF` "not used" marker) replace this loop's earlier
            // raw `e.params.unum32.get(...)` reads -- applied unconditionally
            // (including on non-native-hardware channels, where `elig` is
            // always `{true, true}`), for consistency with
            // `point_to_point_filter_eligibility`'s own use of them.
            // Functionally inert either way: `0xFFFFFFFF` exceeds any real
            // 29-bit CAN ID, so a sentinel value could never collide with a
            // real frame's numeric id regardless of which lookup is used;
            // `UniqueRespIdKey`'s own gate-resolution site below still reads
            // the raw, unfiltered value (edge-case-hunter finding, this PR).
            if elig.usdt
                && let Some(id) = usdt_resp_id(e)
            {
                let is_29bit =
                    can_29bit_id(e.params.unum32.get(&PARAM_CAN_RESP_USDT_FORMAT).copied());
                usdt_domain.record(&mut iso_widths_seen, &mut can_widths_seen, id, is_29bit);
            }
            if elig.uudt
                && let Some(id) = uudt_resp_id(e)
            {
                let is_29bit =
                    can_29bit_id(e.params.unum32.get(&PARAM_CAN_RESP_UUDT_FORMAT).copied());
                uudt_domain.record(&mut iso_widths_seen, &mut can_widths_seen, id, is_29bit);
            }
        }
    }
    links
        .iter()
        .filter_map(|(&h, l)| {
            // ADR-222 round 4 (design-advisor audit, PR #141): computed ONCE
            // per link and reused for every gate-resolution site below
            // (`FcPair`, `UniqueRespIdKey`, the addressing tables) -- see
            // `width_domains`'s own doc comment.
            let native_mixed_link = is_native_mixed_link(l, mode_is_native_mixed);
            let (usdt_domain, uudt_domain) = width_domains(l, channel_id, native_mixed_link);
            let kind = if l.channel_id == Some(channel_id) {
                if l.software_isotp {
                    RxEntryKind::SoftwareIsoTp(IsoTpRxContext {
                        reassembly: Arc::clone(&l.isotp_rx),
                        fc_pairs: l
                            .active_unique_resp_id_table
                            .iter()
                            .filter_map(|e| {
                                let usdt_can_id = *e.params.unum32.get(&PARAM_CAN_RESP_USDT_ID)?;
                                let phys_req_can_id =
                                    *e.params.unum32.get(&PARAM_CAN_PHYS_REQ_ID)?;
                                let usdt_format_raw =
                                    e.params.unum32.get(&PARAM_CAN_RESP_USDT_FORMAT).copied();
                                let rx_addressing = Addressing::from_format(
                                    usdt_format_raw,
                                    e.params
                                        .unum32
                                        .get(&PARAM_CAN_RESP_USDT_EXT_ADDR)
                                        .copied()
                                        .unwrap_or(0) as u8,
                                );
                                let fc_tx_addressing = Addressing::from_format(
                                    e.params.unum32.get(&PARAM_CAN_PHYS_REQ_FORMAT).copied(),
                                    e.params
                                        .unum32
                                        .get(&PARAM_CAN_PHYS_REQ_EXT_ADDR)
                                        .copied()
                                        .unwrap_or(0) as u8,
                                );
                                // ADR-222 round 4: `usdt_domain` -- this
                                // link's own USDT [`WidthDomain`], computed
                                // once above and reused here, exactly like
                                // the `UniqueRespIdKey` construction below
                                // uses it -- not a second, independently
                                // computed classification. Always `Both` in
                                // practice (a software-ISO-TP link never
                                // takes the `native_mixed`/`uudt_on_companion`
                                // branches `width_domains` special-cases),
                                // but reading the shared variable instead of
                                // hardcoding `WidthDomain::Both` here keeps
                                // this one source of truth.
                                let usdt_width_gate = resolve_width_gate(
                                    &iso_widths_seen,
                                    &can_widths_seen,
                                    usdt_domain,
                                    usdt_can_id,
                                    can_29bit_id(usdt_format_raw),
                                );
                                Some(FcPair {
                                    usdt_can_id,
                                    usdt_width_gate,
                                    rx_addressing,
                                    phys_req_can_id,
                                    fc_tx_addressing,
                                })
                            })
                            .collect(),
                        block_size: l.active.isotp_rx_fc().0,
                        st_min: l.active.isotp_rx_fc().1,
                        framing: l.active.isotp_framing(0),
                        n_cr: Duration::from_millis(l.active.isotp_n_cr_timeout_ms() as u64),
                    })
                } else {
                    RxEntryKind::Hardware {
                        native_mixed: native_mixed_link,
                        // ADR-222 round 4 (design-advisor audit, PR #141):
                        // `true` for a dual-channel-mode primary channel --
                        // see `RxEntryKind::Hardware`'s own doc comment for
                        // why this disables the UUDT tier on the primary.
                        uudt_on_companion: l.uudt_channel_id.is_some(),
                    }
                }
            } else if l.uudt_channel_id == Some(channel_id) {
                RxEntryKind::Companion
            } else {
                return None;
            };
            // Codex review finding (PR #72 round 1, ADR-179/Phase 5): a
            // `filter_map`, not a plain `map` -- a `UniqueRespIdTable` entry
            // that configures none of `CP_CanRespUSDTId`/`CP_CanRespUUDTId`/
            // `CP_J1939SourceAddress` (e.g. the pre-existing gap for KWP/
            // J1850's own `CP_EcuRespSourceAddress`-keyed entries, which this
            // field's shape has no slot for either) used to still add a
            // fully-`None` entry here -- which made `route_frame`'s
            // `unique_resp_ids.is_empty()` no-table-wildcard check false (the
            // vec was non-empty) while its own CAN-ID match `.find()` could
            // NEVER match an all-`None` entry, so every frame was silently
            // dropped for that CLL instead of delivered. Filtering these
            // entries out entirely restores the correct fallback: a table
            // with no CAN-ID-routable entries at all now correctly reads as
            // "no table" (deliver unconditionally), the same no-table
            // wildcard mode KWP/J1850 already rely on.
            //
            // `CP_J1939SourceAddress` (ADR-184: wired into real routing, not
            // just filtered-in-or-out here) is read the same way as the two
            // CAN fields -- a J1939-only entry (both CAN fields absent, SA
            // present) now survives this filter AND participates in
            // `UniqueRespIdKey::matched`'s SA tier, giving multiple J1939
            // CLLs sharing one physical channel real per-CLL disambiguation
            // by source address when a client explicitly configures a table
            // for it (previously deferred; the RPC-acceptance side needed to
            // land first -- see `comparam_support::unique_id_params`'s own
            // `J1939_PS` branch).
            // ADR-203: `CP_EcuRespSourceAddress` is retained into
            // `ecu_resp_source_addr` ONLY for a KWP (ISO9141/ISO14230) or
            // J1850 (VPW/PWM) CLL (`sa_routable`) -- NOT unconditionally.
            // `CP_EcuRespSourceAddress` is ALSO a legal, deliberately inert
            // SURPLUS param on a `CAN_UNIQUE_ID_UNUM32` entry (a CAN client
            // can legally configure a table entry containing only this
            // param, which currently means nothing to CAN's own USDT/UUDT/
            // J1939-SA routing and must keep being filtered out, keeping
            // that CLL in wildcard mode). Retaining it unconditionally
            // would flip a CAN CLL's SA-only surplus entry from "filtered
            // out -> wildcard delivery" to "retained but unmatchable by any
            // of CAN's own tiers -> every frame silently dropped" --
            // resurrecting the exact PR #72 round-1 bug class this same
            // filter's own comment above already describes.
            let sa_routable = matches!(
                resources::base_protocol_id(l.hw_protocol_id),
                j2534_0404::ISO9141
                    | j2534_0404::ISO14230
                    | j2534_0404::J1850PWM
                    | j2534_0404::J1850VPW
            );
            let mut unique_resp_ids: Vec<UniqueRespIdKey> = l
                .active_unique_resp_id_table
                .iter()
                .filter_map(|e| {
                    let usdt = e.params.unum32.get(&PARAM_CAN_RESP_USDT_ID).copied();
                    let uudt = e.params.unum32.get(&PARAM_CAN_RESP_UUDT_ID).copied();
                    let j1939_sa = e.params.unum32.get(&PARAM_J1939_SOURCE_ADDRESS).copied();
                    let ecu_sa = sa_routable
                        .then(|| e.params.unum32.get(&PARAM_ECU_RESP_SOURCE_ADDR).copied())
                        .flatten();
                    // ADR-222 round 4: gate resolution uses this link's own
                    // `usdt_domain`/`uudt_domain` (computed once above,
                    // reused here) against the per-channel contention maps
                    // -- `Any` for every uncontended id within that domain
                    // (the common case, a provable no-op).
                    let usdt_width_gate = usdt.map_or(CanIdWidthGate::Any, |id| {
                        resolve_width_gate(
                            &iso_widths_seen,
                            &can_widths_seen,
                            usdt_domain,
                            id,
                            can_29bit_id(e.params.unum32.get(&PARAM_CAN_RESP_USDT_FORMAT).copied()),
                        )
                    });
                    let uudt_width_gate = uudt.map_or(CanIdWidthGate::Any, |id| {
                        resolve_width_gate(
                            &iso_widths_seen,
                            &can_widths_seen,
                            uudt_domain,
                            id,
                            can_29bit_id(e.params.unum32.get(&PARAM_CAN_RESP_UUDT_FORMAT).copied()),
                        )
                    });
                    (usdt.is_some() || uudt.is_some() || j1939_sa.is_some() || ecu_sa.is_some())
                        .then_some(UniqueRespIdKey {
                            unique_resp_identifier: e.unique_resp_identifier,
                            can_resp_usdt_id: usdt,
                            usdt_width_gate,
                            can_resp_uudt_id: uudt,
                            uudt_width_gate,
                            j1939_source_address: j1939_sa,
                            tp20_rx_id: None,
                            tp20_tx_id: None,
                            ecu_resp_source_addr: ecu_sa,
                        })
                })
                .collect();
            // ADR-188/Phase 7 Stage 7a (the "ADR-184 routing shape" ADR-188
            // itself names): a synthetic entry for a TP2.0 CLL, appended (not
            // filtered from the table above -- TP2.0 has no
            // `UniqueRespIdTable` addressing concept at all) so this CLL's
            // own routing key participates in `route_frame`'s `find` the same
            // way a J1939 SA-only entry does, and so `unique_resp_ids` is
            // ALWAYS non-empty for a TP2.0 CLL -- preventing `route_frame`'s
            // own "empty table -> deliver unconditionally" wildcard fallback.
            //
            // Codex review fix (PR #97): this must push an entry for EVERY
            // TP2.0 CLL, not only one whose connection has reached
            // `Established` -- a CLL that is merely `Requested`, `Lost`, or
            // has no `tp20_connection` yet previously contributed nothing to
            // `unique_resp_ids`, leaving it empty and falling straight into
            // `route_frame`'s empty-table wildcard, which broadcast every
            // frame on the shared physical channel to it -- including frames
            // actually addressed to an established SIBLING CLL's own
            // connection (a cross-CLL data leak). When `Established`, push
            // the real entry (`tp20_rx_id: Some(conn.requested_rx_id)`, as
            // before -- plus `tp20_tx_id: conn.established_tx_id`, Codex
            // review fix, PR #97: routes a `CP_Loopback`-enabled write's own
            // device-generated echo, addressed by our TX-ID rather than our
            // RX-ID, back to this same CLL instead of dropping it or
            // misdelivering it to whichever sibling's RX-ID happens to equal
            // this TX-ID -- see `UniqueRespIdKey::tp20_tx_id`'s own doc
            // comment). Otherwise push a deliberately UNMATCHABLE sentinel
            // (all fields `None`) -- `UniqueRespIdKey::matched` can never
            // return `Some` for an all-`None` entry, so `route_frame`'s
            // `.find(...)` correctly returns `None` (frame dropped for this
            // CLL) instead of ever reaching the empty-table wildcard branch.
            // `unique_resp_identifier: 0` mirrors `route_frame`'s own
            // no-real-uid sentinel; it is otherwise unused on the sentinel
            // entry since that entry can never match.
            // ADR-210 Decision item 13: re-keyed from the narrow
            // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id` --
            // `l.hw_protocol_id` is a live link's raw id, so a
            // `_CHx`-connected TP2.0 CLL would otherwise get NO synthetic
            // routing entry at all here, falling into `route_frame`'s
            // empty-table wildcard and receiving every frame on the shared
            // physical channel instead of just its own connection's.
            if resources::is_tp2_0_family_protocol_id(l.hw_protocol_id) {
                let established = l
                    .tp20_connection
                    .filter(|conn| conn.phase == Tp20ConnectionPhase::Established);
                let tp20_rx_id = established.map(|conn| conn.requested_rx_id);
                let tp20_tx_id = established.and_then(|conn| conn.established_tx_id);
                unique_resp_ids.push(UniqueRespIdKey {
                    unique_resp_identifier: 0,
                    can_resp_usdt_id: None,
                    // ADR-222: inert on this all-`None`-CAN-field sentinel
                    // (never consulted, since `can_resp_usdt_id`/
                    // `can_resp_uudt_id` are both `None`) but still filled in
                    // -- the struct has no `Default`, and `Any` is the
                    // correct value even if it somehow were consulted.
                    usdt_width_gate: CanIdWidthGate::Any,
                    can_resp_uudt_id: None,
                    uudt_width_gate: CanIdWidthGate::Any,
                    j1939_source_address: None,
                    tp20_rx_id,
                    tp20_tx_id,
                    ecu_resp_source_addr: None,
                });
            }
            // ADR-171 (this PR's own follow-up Decision item): `header_protocol`
            // must NOT be a single `l.protocol.j2534_protocol_id()` read for
            // every `kind` -- for the `SAE_J1850` bus-agnostic resources
            // (`0x021A`/`0x021C`/`0x021D`), that value is only the VPW *initial
            // probe candidate* (ADR-070) and stays permanently `J1850VPW` even
            // after the connect-time auto-detect probe lands on PWM, which is
            // recorded separately in `l.hw_protocol_id`. Since ADR-171 made
            // `header_footer_len`'s J1850 split flavor-sensitive (PWM derives
            // its footer from `ExtraDataIndex`; VPW never does), routing a
            // PWM-detected link through the VPW arm now silently drops IFR
            // footer bytes -- so `RxEntryKind::Hardware`/`Companion` derive
            // `header_protocol` from the detected `l.hw_protocol_id` instead,
            // normalized through `resources::base_protocol_id` (needed for the
            // `_PS`/`_CHx` qualified ISO15765 ids `header_footer_len`'s raw
            // numeric match expects -- the old `j2534_protocol_id()` path
            // incidentally already provided this for ISO15765-family links,
            // but never provided it for J1850's auto-detect case). A
            // software-ISO-TP link keeps `l.protocol.j2534_protocol_id()`
            // instead (ADR-051/ADR-046 invariant): its logical ISO15765
            // identity must govern the header/footer split, not the raw CAN
            // `hw_protocol_id` underneath -- `hw_protocol_id` for a
            // software-ISO-TP link is plain `CAN`, which is not what should
            // key this split.
            //
            // This leaves the `usdt_addressing_by_id`/`uudt_addressing_by_id`
            // gate just below (`header_protocol == j2534_0404::ISO15765`)
            // provably invariant
            // across this change: every ISO15765-family case -- qualified or
            // not, software-ISO-TP or not -- still collapses to plain
            // `ISO15765` either way (`base_protocol_id` normalizes a
            // `_PS`/`_CHx` qualified id the same way `j2534_protocol_id()`
            // already did for the software-ISO-TP arm). Only J1850/SCI-family
            // cases can differ in value between the two derivations, and
            // those never satisfied this gate before or after.
            let header_protocol = match kind {
                RxEntryKind::SoftwareIsoTp(_) => l.protocol.j2534_protocol_id(),
                RxEntryKind::Hardware { .. } | RxEntryKind::Companion => {
                    resources::base_protocol_id(l.hw_protocol_id)
                }
            };
            // ADR-217 Codex-review fix: split into two role-specific tables
            // rather than one flat, role-blind `Vec` -- see
            // `CllRxEntry::usdt_addressing_by_id`/`uudt_addressing_by_id`'s
            // own doc comments for why a single table was wrong (it let one
            // colliding entry's `Addressing` bleed into the other role's
            // delivery). Each `map` below is unchanged from the old
            // `usdt`/`uudt` closures except for which `Vec` it feeds.
            let (usdt_addressing_by_id, uudt_addressing_by_id) = if header_protocol
                == j2534_0404::ISO15765
            {
                let usdt = l
                    .active_unique_resp_id_table
                    .iter()
                    .filter_map(|e| {
                        e.params
                            .unum32
                            .get(&PARAM_CAN_RESP_USDT_ID)
                            .copied()
                            .map(|id| {
                                let format_raw =
                                    e.params.unum32.get(&PARAM_CAN_RESP_USDT_FORMAT).copied();
                                let addressing = Addressing::from_format(
                                    format_raw,
                                    e.params
                                        .unum32
                                        .get(&PARAM_CAN_RESP_USDT_EXT_ADDR)
                                        .copied()
                                        .unwrap_or(0) as u8,
                                );
                                // ADR-222 round 4: `usdt_domain`, this
                                // link's own USDT `WidthDomain`, computed
                                // once above and reused here.
                                let gate = resolve_width_gate(
                                    &iso_widths_seen,
                                    &can_widths_seen,
                                    usdt_domain,
                                    id,
                                    can_29bit_id(format_raw),
                                );
                                (id, gate, addressing)
                            })
                    })
                    .collect();
                let uudt = l
                    .active_unique_resp_id_table
                    .iter()
                    .filter_map(|e| {
                        e.params
                            .unum32
                            .get(&PARAM_CAN_RESP_UUDT_ID)
                            .copied()
                            .map(|id| {
                                let format_raw =
                                    e.params.unum32.get(&PARAM_CAN_RESP_UUDT_FORMAT).copied();
                                let addressing = Addressing::from_format(
                                    format_raw,
                                    e.params
                                        .unum32
                                        .get(&PARAM_CAN_RESP_UUDT_EXT_ADDR)
                                        .copied()
                                        .unwrap_or(0) as u8,
                                );
                                // ADR-222 round 4: `uudt_domain`, this
                                // link's own UUDT `WidthDomain`, computed
                                // once above and reused here (can differ
                                // from `usdt_domain` -- e.g. a native-mixed
                                // link's `Can`-only UUDT domain vs. its
                                // `Iso`-only USDT domain).
                                let gate = resolve_width_gate(
                                    &iso_widths_seen,
                                    &can_widths_seen,
                                    uudt_domain,
                                    id,
                                    can_29bit_id(format_raw),
                                );
                                (id, gate, addressing)
                            })
                    })
                    .collect();
                (usdt, uudt)
            } else {
                (Vec::new(), Vec::new())
            };
            // `open_tp_discards` (ADR-137 fourth Codex-review fix / round-4
            // restructure) lives independently of `tester_present_state`: a
            // discard window's lifetime is per-*send*, not per-*configuration*,
            // so it is no longer read out of whichever `TesterPresentState`
            // variant happens to be current. Filtered here (read-only, not
            // pruned in place -- `l` is only immutably borrowed at this point)
            // to the entries still inside their own `CP_P2Max` window
            // (`now < r.window.until`), same liveness rule the old
            // per-variant `discard_until`/`residual` reads used. Every
            // surviving entry becomes a `TesterPresentDiscard`; `bind_frame`
            // discards a frame matching ANY of them, not just the first --
            // each entry is an independently elicited send, not a
            // precedence chain.
            let now = tokio::time::Instant::now();
            let tester_present_discard: Vec<TesterPresentDiscard> = l
                .open_tp_discards
                .iter()
                .filter(|r| now < r.window.until)
                .map(|r| TesterPresentDiscard {
                    pos: r.window.pos.clone(),
                    neg: r.window.neg.clone(),
                    target_can_ids: r.target_can_ids,
                    tx_can_id: r.tx_can_id,
                })
                .collect();
            let registrant_baselines = l
                .registrants
                .iter()
                .map(|r| RegistrantBaseline {
                    cop_handle: r.cop_handle,
                    matches_got: r.matches_got,
                    pending_rc: r.pending_rc,
                    concat_segments_got: r.concat_segments_got,
                    timing_accumulator: r.timing_accumulator,
                })
                .collect();
            // ADR-204 Codex review (PR #116, round 3): see
            // `CllRxEntry::cop_tags`'s own doc comment -- captured from
            // `prims` (locked above, nested inside `links`) in this SAME
            // critical section as `registrants` itself, right above.
            let cop_tags: HashMap<u32, Vec<u8>> = l
                .registrants
                .iter()
                .filter_map(|r| {
                    prims
                        .get(&r.cop_handle)
                        .and_then(|e| e.cop_tag.clone())
                        .map(|tag| (r.cop_handle, tag))
                })
                .collect();
            Some(CllRxEntry {
                handle: h,
                rx_buf: Arc::clone(&l.rx_buf),
                unique_resp_ids,
                kind,
                header_protocol,
                // ADR-196 Decision item 3: mirrors `LogicalLinkState::
                // raw_mode` -- see `CllRxEntry::raw_mode`'s own doc comment.
                raw_mode: l.raw_mode,
                usdt_addressing_by_id,
                uudt_addressing_by_id,
                connect_generation: l.connect_generation,
                suspend_seq: None,
                // ADR-147 sixth amendment (pre-read capture for the batch
                // anchor): stamped from the CALLER's pre-read snapshot
                // (`poll_rx_inner`, captured before this pass's own
                // `PassThruReadMsgs` call even started), NOT re-read live
                // from `l.error_set_seq` here -- that live read would be
                // just as vulnerable to a concurrent bump landing in this
                // function's own `logical_links` critical section as the
                // fifth amendment's post-read capture was. `None` when this
                // handle is absent from the snapshot (connected in the
                // narrow window between the snapshot and this function
                // running) -- see `set_seq_snapshot`'s own doc comment on
                // this function.
                set_seq_at_read: set_seq_snapshot.get(&h).copied(),
                tester_present_discard,
                registrants: l.registrants.clone(),
                registrant_baselines,
                queue_error_class: None,
                start_msg_ind_enable: l.active.start_msg_ind_enable(),
                transmit_ind_enable: l.active.transmit_ind_enable(),
                cop_tags,
            })
        })
        .collect()
}
