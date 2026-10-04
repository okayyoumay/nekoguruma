use std::ops::RangeInclusive;

/// Service-level channel protocol identifier.
///
/// Wraps a `u32` but distinguishes between J2534 hardware protocol IDs and
/// application-layer protocol stacks that share the same J2534 channel type.
/// For example, `ISO_15765_3_ON_ISO_15765_2` and `ISO_14229_3_ON_ISO_15765_2`
/// both use J2534's `ISO15765` (0x06) channel but require different handling at
/// the service level.
///
/// # Encoding
///
/// - Values 0x01–0x0A: native J2534 protocol IDs — `value()` equals the J2534
///   hardware ID returned by `j2534_protocol_id()`.
/// - Values 0x0100+: service-level extended protocols — `j2534_protocol_id()`
///   maps them down to the underlying J2534 channel type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelProtocol(u32);

impl ChannelProtocol {
    // ── Native J2534 protocol IDs (service value == J2534 hardware ID) ─────────
    pub const J1850VPW: Self = Self(1);
    pub const J1850PWM: Self = Self(2);
    pub const ISO9141: Self = Self(3);
    pub const ISO14230: Self = Self(4);
    pub const CAN: Self = Self(5);
    pub const ISO15765: Self = Self(6);
    pub const SCI_A_ENGINE: Self = Self(7);
    pub const SCI_A_TRANS: Self = Self(8);
    pub const SCI_B_ENGINE: Self = Self(9);
    pub const SCI_B_TRANS: Self = Self(10);

    // ── SAE J2534-2 clause 12 UART Echo Byte (ADR-170/Phase 9) ──────────────────
    // A wholly new, standalone protocol -- unlike CAN FD/Single Wire CAN/
    // Fault-Tolerant CAN (ADR-158/159/164/168), which each reuse the existing
    // CAN/ISO15765 `ChannelProtocol` identity via a resource row's
    // `hw_protocol_override`, clause 12 defines no relationship to any
    // protocol this service already models (Honda ABS/VSA per SAE J2809, or
    // KWP1281 per SAE J2818, over a K-line UART byte-echo handshake). Clause
    // 12.3.3.1.4's table defines no unqualified base id either -- only
    // `_PS`/`_CHx` -- so this raw J2534 hardware protocol id IS the
    // service-level identity, self-mapping through `j2534_protocol_id()`
    // below the same way `ISO9141`/`J1850PWM` (the native ids above) do, not
    // a "service-level extended" (0x0100+) id that maps DOWN to a different
    // hardware channel.
    pub const UART_ECHO_BYTE_PS: Self = Self(0x0000800A);

    // ── SAE J2534-2 clause 13 Honda DIAG-H (ADR-174/Phase 10) ───────────────────
    // A second wholly standalone protocol, the same shape as UART Echo Byte
    // just above (not the FD/SW/FT-CAN `hw_protocol_override` pattern):
    // clause 13 defines Honda's proprietary "92 Hm/2" message format over the
    // single-wire, half-duplex "DIAG-H" UART physical layer with no
    // relationship to any protocol this service already models. Clause 13
    // defines no unqualified base id either -- only `_PS`/`_CHx` -- so this
    // raw J2534 hardware protocol id IS the service-level identity,
    // self-mapping through `j2534_protocol_id()` below the same way
    // `UART_ECHO_BYTE_PS` does.
    pub const HONDA_DIAGH_PS: Self = Self(0x0000800B);

    // ── SAE J2534-2 clause 17 SAE J1708 (ADR-175/Phase 11) ──────────────────────
    // A third wholly standalone protocol, the same shape as UART Echo Byte and
    // Honda DIAG-H above (not the FD/SW/FT-CAN `hw_protocol_override`
    // pattern): clause 17 defines the SAE J1708 heavy-duty-truck serial bus
    // with no relationship to any protocol this service already models.
    // Clause 17 defines no unqualified base id either -- only `_PS`/`_CHx` --
    // so this raw J2534 hardware protocol id IS the service-level identity,
    // self-mapping through `j2534_protocol_id()` below the same way
    // `HONDA_DIAGH_PS`/`UART_ECHO_BYTE_PS` do.
    pub const J1708_PS: Self = Self(0x0000800D);

    // ── SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5) ──────────────
    // A fourth wholly standalone protocol, the same shape as UART Echo Byte,
    // Honda DIAG-H, and SAE J1708 above (not the FD/SW/FT-CAN
    // `hw_protocol_override` pattern): clause 16 defines the SAE J1939
    // heavy-duty-vehicle protocol (250 kbps, 29-bit CAN identifiers, and its
    // own device-owned address claim/defend negotiation) with no
    // CAN-family/`ChannelProtocol::CAN`/`ISO15765` relationship this service
    // reuses -- ADR-179's Context section explains why SWCAN/FT-CAN's
    // bus-type-variant shape does not fit here even though J1939 physically
    // rides a CAN transceiver. Clause 16 defines no unqualified base id
    // either -- only `_PS`/`_CHx` -- so this raw J2534 hardware protocol id
    // IS the service-level identity, self-mapping through
    // `j2534_protocol_id()` below the same way `J1708_PS`/`HONDA_DIAGH_PS`/
    // `UART_ECHO_BYTE_PS` do.
    pub const J1939_PS: Self = Self(0x0000800C);

    // ── SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a) ──────────────────
    // A fifth wholly standalone protocol, the same shape as UART Echo Byte,
    // Honda DIAG-H, SAE J1708, and SAE J1939 above (not the FD/SW/FT-CAN
    // `hw_protocol_override` pattern): clause 19 defines the VW/Audi TP2.0
    // (SAE J2819) connection-oriented transport protocol, with no
    // relationship to any protocol this service already models -- ADR-188's
    // Context section confirms neither ISO 22900-2 edition defines a
    // TP2.0/J2819/VWTP equivalent. Clause 19 defines no unqualified base id
    // either -- only `_PS`/`_CHx` -- so this raw J2534 hardware protocol id
    // IS the service-level identity, self-mapping through
    // `j2534_protocol_id()` below the same way `J1939_PS`/`J1708_PS`/
    // `HONDA_DIAGH_PS`/`UART_ECHO_BYTE_PS` do.
    pub const TP2_0_PS: Self = Self(0x0000_800E);

    // ── SAE J2534-2 clause 11 GM UART Protocol (SAE J2740, ADR-189/Phase 8) ────
    // A sixth wholly standalone protocol, the same shape as UART Echo Byte,
    // Honda DIAG-H, SAE J1708, SAE J1939, and TP2.0 above (not the FD/SW/
    // FT-CAN `hw_protocol_override` pattern): clause 11 defines a
    // master/slave UART bus with its own poll-message/poll-response bus-
    // mastership handshake, with no relationship to any protocol this
    // service already models. Clause 11 defines no unqualified base id
    // either -- only `_PS`/`_CHx` -- so this raw J2534 hardware protocol id
    // IS the service-level identity, self-mapping through
    // `j2534_protocol_id()` below the same way `TP2_0_PS`/`J1939_PS`/
    // `J1708_PS`/`HONDA_DIAGH_PS`/`UART_ECHO_BYTE_PS` do. Unlike those five,
    // though, GM UART also participates in the clause 7 Additional Channels
    // `_CHx` arithmetic mapping (ADR-189 Decision 2) -- the first standalone
    // protocol to combine both. `0x00008009` -- the header's own native
    // `PROTOCOL_GM_UART_PS` value -- NOT the next free slot after
    // `TP2_0_PS`'s `0x0000800E`: clause 11 is numbered before clause 12
    // (UART Echo Byte, `0x0000800A`) in SAE J2534-2's own `_PS` id
    // allocation, unrelated to this service's own phase implementation
    // order (GM UART shipped last, Phase 8, but its native id was minted
    // first).
    pub const GM_UART_PS: Self = Self(0x0000_8009);

    // ── SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16) ──────────────────
    // A seventh wholly standalone protocol, the same shape as UART Echo
    // Byte, Honda DIAG-H, SAE J1708, SAE J1939, TP2.0, and GM UART above
    // (not the FD/SW/FT-CAN `hw_protocol_override` pattern): clause 24
    // binds a J2534 channel to an NDIS/RNDIS Ethernet adapter, with no
    // relationship to any protocol this service already models -- the
    // actual Ethernet payload traffic is routed by the spec itself outside
    // the J2534 API entirely (ADR-194 Context/Decision), so this identity
    // exists purely for the ordinary connect/disconnect channel lifecycle
    // plus one channel-scoped info IOCTL. Clause 24 defines no unqualified
    // base id either -- only this raw `PROTOCOL_ETHERNET_NDIS` value --
    // so this raw J2534 hardware protocol id IS the service-level
    // identity, self-mapping through `j2534_protocol_id()` below the same
    // way `GM_UART_PS`/`TP2_0_PS`/`J1939_PS`/`J1708_PS`/`HONDA_DIAGH_PS`/
    // `UART_ECHO_BYTE_PS` do. Unlike those six, clause 24 defines no
    // `_PS`/`_CHx` pin-selection mechanics at all (pin usage is selected by
    // connect flag, `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2`, not
    // `J1962_PINS`-style resolution), so this id never participates in
    // `names::resolve_pin_selection`.
    pub const ETHERNET_NDIS: Self = Self(0x0000_8013);

    // ── Service-level extended protocols (0x0100+) ──────────────────────────────
    // ISO 15765-2 channel variants
    pub const ISO_15765_3_ON_ISO_15765_2: Self = Self(0x0100);
    pub const ISO_14229_3_ON_ISO_15765_2: Self = Self(0x0101);
    pub const ISO_14230_3_ON_ISO_15765_2: Self = Self(0x0102);
    pub const SAE_J2190_ON_ISO_15765_2: Self = Self(0x0103);
    pub const ISO_15031_5_ON_ISO_15765_4: Self = Self(0x0104);
    pub const ISO_14229_3_ON_ISO_15765_2_WITH_ISO_11783_5: Self = Self(0x0105);
    // ISO_14229_3/ISO_15765_3 used standalone (not qualified "_ON_ISO_15765_2"):
    // ISO_14229_3 supersedes ISO_15765_3 and offers the same feature set, so
    // both map to the same underlying ISO15765 (0x06) channel as
    // their "_ON_ISO_15765_2" counterparts above.
    pub const ISO_14229_3: Self = Self(0x0106);
    pub const ISO_15765_3: Self = Self(0x0107);
    // ISO 11783-5 (J1939-derived CAN channel)
    pub const ISO_11783_12_ON_ISO_11783_5: Self = Self(0x0110);
    // ISO 9141-2 channel variants
    pub const SAE_J2190_ON_ISO_9141_2: Self = Self(0x0120);
    pub const ISO_15031_5_ON_ISO_9141_2: Self = Self(0x0121);
    // Combined ISO9141/ISO14230 K-line bus (ISO_9141_2_UART_and_ISO_14230_1_UART)
    pub const ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4: Self = Self(0x0122);
    pub const SAE_J2190_ON_ISO_9141_2_AND_ISO_14230_2: Self = Self(0x0123);
    // ISO 14230-2 / 14230-4 channel variants
    pub const ISO_14230_3_ON_ISO_14230_2: Self = Self(0x0130);
    pub const SAE_J2190_ON_ISO_14230_2: Self = Self(0x0131);
    pub const ISO_15031_5_ON_ISO_14230_4: Self = Self(0x0132);
    // SAE J1850 PWM channel variants
    pub const SAE_J2190_ON_SAE_J1850_PWM: Self = Self(0x0140);
    pub const ISO_15031_5_ON_SAE_J1850_PWM: Self = Self(0x0141);
    // SAE J1850 VPW channel variants
    pub const SAE_J2190_ON_SAE_J1850_VPW: Self = Self(0x0150);
    pub const ISO_15031_5_ON_SAE_J1850_VPW: Self = Self(0x0151);
    // SAE J1850 bus-agnostic variants (used with the auto-detecting
    // `SAE_J1850` bus type, 0x0307): the underlying flavor -- VPW or PWM --
    // is not fixed by the resource row and is instead resolved at
    // `ConnectComLogicalLink` by an active-probe sequence (ADR-070).
    pub const SAE_J2190_ON_SAE_J1850: Self = Self(0x0152);
    pub const ISO_15031_5_ON_SAE_J1850: Self = Self(0x0153);
    // SAE J2610 SCI channel (maps to the TX_FLAG_SCI_MODE quirk). Its
    // resource-table rows (the four `SAE_J2610_on_SAE_J2610_SCI`
    // configurations, spec correction) each override the actual connect
    // protocol via `resources::ResourceDef::hw_protocol_override`
    // (`SCI_A_ENGINE`/`_A_TRANS`/`_B_ENGINE`/`_B_TRANS`, selected by DLC pin
    // wiring) rather than connecting with `j2534_protocol_id()`'s `SCI_MODE`
    // below -- `SCI_MODE` is therefore no longer used as a connect protocol
    // by any table row. Legacy direct creation (a raw/extended
    // `ChannelProtocol` value with no matching resource row) still connects
    // via `j2534_protocol_id()` unchanged, i.e. `SCI_MODE`.
    pub const SAE_J2610_ON_SAE_J2610_SCI: Self = Self(0x0160);
    // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): one shared
    // identity across the 32 independent, read-only native `PROTOCOL_ANALOG_IN_x`
    // ids (`resources.rs`'s 32 new rows, `0x023D`-`0x025C`, each overriding
    // onto its own native id via `hw_protocol_override`) -- structurally the
    // SAE J2610 SCI shape just above (one shared `ChannelProtocol` fanning
    // out to N native ids via resource-row overrides), not the UART Echo
    // Byte/Honda DIAG-H/J1708 shape (a single `_PS` id that IS the identity
    // directly), since clause 10 defines 32 distinct enumerated ids rather
    // than one. `j2534_protocol_id()` below maps this to
    // `PROTOCOL_ANALOG_IN_1` as an arbitrary-but-valid default (the first
    // channel, mirroring ADR-164's own "first-listed as practical default"
    // precedent) -- reached only by a legacy direct creation of this exact
    // extended id with no matching resource row, since every one of the 32
    // table rows always overrides via its own `hw_protocol_override`.
    pub const ANALOG_IN: Self = Self(0x0170);

    /// Returns the raw service-level protocol ID.
    pub const fn value(self) -> u32 {
        self.0
    }

    /// Wraps a raw numeric ID as a `ChannelProtocol` without validation.
    pub const fn from_raw(id: u32) -> Self {
        Self(id)
    }

    /// Returns the J2534 hardware protocol ID used when calling `PassThruConnect`,
    /// building `PassThruMessage`, and forming the `ChannelKey`.
    ///
    /// For native J2534 protocols (IDs 1–10) this equals `value()`.
    /// For extended service-level protocols (0x0100+) this maps down to the
    /// underlying J2534 channel type (e.g. `ISO_14229_3_ON_ISO_15765_2` → ISO15765).
    pub fn j2534_protocol_id(self) -> u32 {
        match self.0 {
            // ISO 15765-2 channel group (incl. standalone ISO_14229_3/ISO_15765_3)
            // → ISO15765 (0x06)
            0x0100..=0x0107 => j2534_0404::ISO15765,
            // ISO 11783-5 channel → CAN (0x05)
            0x0110 => j2534_0404::CAN,
            // ISO 9141-2 channel group (incl. combined ISO9141/ISO14230 K-line
            // variants) → ISO9141 (0x03)
            0x0120..=0x0123 => j2534_0404::ISO9141,
            // ISO 14230-2/4 channel group → ISO14230 (0x04)
            0x0130..=0x0132 => j2534_0404::ISO14230,
            // SAE J1850 PWM channel group → J1850PWM (0x02)
            0x0140 | 0x0141 => j2534_0404::J1850PWM,
            // SAE J1850 VPW channel group (incl. bus-agnostic variants)
            // → J1850VPW (0x01). For 0x0152/0x0153 specifically (the
            // `SAE_J1850` bus-agnostic variants) this is only the *initial
            // candidate*: `ConnectComLogicalLink` may override it to
            // J1850PWM after the VPW/PWM auto-detect probe finds a
            // PWM-only responder, carrying the result in
            // `LogicalLinkState::hw_protocol_id` rather than here (ADR-070,
            // mirroring the software-ISO-TP divergence mechanism of ADR-046).
            0x0150..=0x0153 => j2534_0404::J1850VPW,
            // SAE J2610 SCI → SCI_MODE TX-flag (existing quirk)
            0x0160 => j2534_0404::SCI_MODE,
            // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): the
            // shared `ANALOG_IN` identity's own arbitrary-but-valid default
            // -- see that constant's own doc comment. Every one of the 32
            // resource-table rows overrides this via `hw_protocol_override`,
            // so this arm is only reached by a legacy direct creation of
            // this exact extended id with no matching row.
            0x0170 => j2534_0404::PROTOCOL_ANALOG_IN_1,
            // Native J2534 IDs, UART_ECHO_BYTE_PS (ADR-170/Phase 9),
            // HONDA_DIAGH_PS (ADR-174/Phase 10), J1708_PS (ADR-175/Phase
            // 11), J1939_PS (ADR-179/Phase 5), TP2_0_PS (ADR-188/Phase 7
            // Stage 7a), GM_UART_PS (ADR-189/Phase 8), and ETHERNET_NDIS
            // (ADR-194/Phase 16) -- none has an unqualified base id to map
            // down to, so all seven self-identify -- and any unknown values
            // pass through as-is (including the 32 `PROTOCOL_ANALOG_IN_x`
            // ids themselves, when named directly).
            id => id,
        }
    }

    /// Returns `true` for protocols that use a CAN or ISO 15765 J2534 channel.
    pub fn is_can_family(self) -> bool {
        matches!(
            self.j2534_protocol_id(),
            j2534_0404::CAN | j2534_0404::ISO15765
        )
    }

    /// Returns `true` for protocols that use an ISO 9141 or ISO 14230 J2534 channel.
    pub fn is_kwp_family(self) -> bool {
        matches!(
            self.j2534_protocol_id(),
            j2534_0404::ISO9141 | j2534_0404::ISO14230
        )
    }

    /// Returns `true` for protocols that use a SAE J1850 VPW or PWM J2534
    /// channel. Distinct from `is_kwp_family` -- ADR-125's `CP_P3Min`
    /// floor gate depends on this split, since `CP_P3Min` (native
    /// `P3_MIN`) has no defined value for J1850 in Table B.10/B.19 or
    /// J2534-1 (scoped there to the ISO 9141 and ISO 14230 protocol IDs),
    /// even though Annex I.1.4's RC21/RC23 request-time handling covers
    /// both J1850 and K-line.
    pub fn is_j1850_family(self) -> bool {
        matches!(
            self.j2534_protocol_id(),
            j2534_0404::J1850PWM | j2534_0404::J1850VPW
        )
    }

    /// Returns `true` for the SAE J1939 J2534 channel (`J1939_PS`, ADR-179/
    /// Phase 5) -- self-identifies (no unqualified base id to normalize
    /// down to, per this impl's own `j2534_protocol_id` doc comment).
    pub fn is_j1939_family(self) -> bool {
        self.j2534_protocol_id() == j2534_0404::PROTOCOL_J1939_PS
    }

    /// Returns `true` for protocols that use an SCI J2534 channel (SCI-A/B or SAE J2610).
    pub fn is_sci_family(self) -> bool {
        matches!(
            self.j2534_protocol_id(),
            j2534_0404::SCI_A_ENGINE
                | j2534_0404::SCI_A_TRANS
                | j2534_0404::SCI_B_ENGINE
                | j2534_0404::SCI_B_TRANS
                | j2534_0404::SCI_MODE
        )
    }

    /// Returns `true` for the two `SAE_J1850` bus-agnostic protocols
    /// (`SAE_J2190_ON_SAE_J1850` / `ISO_15031_5_ON_SAE_J1850`, values
    /// 0x0152/0x0153). These are the only resource-table protocols that
    /// connect via the auto-detecting `SAE_J1850` bus type (0x0307) and
    /// therefore require the VPW/PWM probe at `ConnectComLogicalLink`
    /// instead of a fixed flavor (ADR-070).
    ///
    /// Deliberately checks `self` directly rather than `j2534_protocol_id()`,
    /// since 0x0150/0x0151 (the VPW-only bus-agnostic-looking variants) also
    /// map to `J1850VPW` but are fixed-flavor resources, not on this bus.
    pub(super) fn needs_j1850_autodetect(self) -> bool {
        matches!(
            self,
            Self::SAE_J2190_ON_SAE_J1850 | Self::ISO_15031_5_ON_SAE_J1850
        )
    }

    /// Returns `true` when `CP_ModifyTiming`'s KWP mechanism (ADR-146: ISO
    /// 14230-2 SID 0x83/0xC3 Access Timing Parameter service) applies to
    /// this protocol -- ISO 22900-2's `CP_ModifyTiming` description scopes
    /// this specifically to the ISO 14230-3/ISO 14230-4 diagnostic
    /// SERVICES layers (Table B.10's applicability columns, corroborated by
    /// both editions' per-protocol default-ComParam lists), not merely
    /// "uses the ISO14230 J2534 hardware channel."
    ///
    /// Deliberately checks `self` directly rather than `j2534_protocol_id()`
    /// (Codex review, PR #22, round 3 -- the bug `needs_j1850_autodetect`'s
    /// own doc comment above already warns about): `j2534_protocol_id()`
    /// collapses `SAE_J2190_ON_ISO_14230_2` (SAE J2190 services, not
    /// KWP2000's own ISO 14230-3 services) onto the same hardware channel
    /// as the genuine KWP variants below, which would wrongly enable this
    /// mechanism for it.
    ///
    /// `ISO_15031_5_ON_ISO_14230_4` (OBD/ISO 15031-5 services over ISO
    /// 14230-4 transport) IS included, even though its services layer
    /// isn't ISO 14230-3 -- both spec editions' Table B.10 mark the ISO
    /// 14230-4 column applicable too, and the `CP_ModifyTiming` description
    /// scopes the 0x83/0xC3 mechanism to the ISO 14230-2 data link, which
    /// ISO 14230-4 (an application-layer profile riding the same data
    /// link) shares. `ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4` (the
    /// combined-bus K-line variant, 0x0122) is excluded: which half of its
    /// combined bus is actually in use is only known post-init-probe, not
    /// at COP-creation time when this is evaluated -- matches this
    /// mechanism's existing behavior, since it never fired for this
    /// protocol before this fix either.
    ///
    /// Bare `ISO14230` (a raw native `CreateComLogicalLink`, no
    /// resource-table row) is included: among every services layer that
    /// CAN run on an ISO14230 channel, only ISO 14230-3/-4 have
    /// `CP_ModifyTiming` applicability defined at all, so a client
    /// enabling it on a bare link is asserting the one stack where it has
    /// defined meaning.
    pub(super) fn kwp_access_timing_applies(self) -> bool {
        matches!(
            self,
            Self::ISO14230 | Self::ISO_14230_3_ON_ISO_14230_2 | Self::ISO_15031_5_ON_ISO_14230_4
        )
    }

    /// Returns `true` when `CP_ModifyTiming`'s UDS mechanism (ADR-150: ISO
    /// 15765-3/14229-3 SID 0x10/0x50 DiagnosticSessionControl) applies to
    /// this protocol -- ISO 22900-2's `CP_ModifyTiming` description and
    /// Table B.10 scope this to the ISO 15765-3/ISO 14229-3 diagnostic
    /// SERVICES layer specifically, not merely "uses the ISO15765 J2534
    /// hardware channel."
    ///
    /// Deliberately checks `self` directly rather than `j2534_protocol_id()`
    /// for the identical reason `kwp_access_timing_applies` does (Codex
    /// review, PR #22, round 3): that would wrongly sweep in
    /// `ISO_14230_3_ON_ISO_15765_2` (KWP2000 services over CAN transport --
    /// its 0x50 response is a KWP StartDiagnosticSession reply, not UDS P2
    /// timing bytes), `SAE_J2190_ON_ISO_15765_2`, and
    /// `ISO_15031_5_ON_ISO_15765_4` (OBD services) -- all three share the
    /// ISO15765 hardware channel but run a services layer this mechanism
    /// does not understand. `ISO_14230_3_ON_ISO_15765_2` gets NEITHER
    /// mechanism (an accepted residual, not an oversight): its services
    /// layer genuinely is KWP2000's own, but the 0x83/0xC3 mechanism's own
    /// ISO 22900-2 scope is the ISO 14230-2 data link specifically, which
    /// this protocol does not run on, and ADR-146's derivation writes
    /// K-line-only ComParams (`CP_P3Min`/`CP_P4Min`) meaningless on an
    /// ISO15765 channel.
    ///
    /// Bare `ISO15765` is included for the same "only defined stack on a
    /// bare link" reasoning `kwp_access_timing_applies` documents.
    pub(super) fn uds_session_timing_applies(self) -> bool {
        matches!(
            self,
            Self::ISO15765
                | Self::ISO_15765_3_ON_ISO_15765_2
                | Self::ISO_14229_3_ON_ISO_15765_2
                | Self::ISO_14229_3_ON_ISO_15765_2_WITH_ISO_11783_5
                | Self::ISO_14229_3
                | Self::ISO_15765_3
        )
    }

    /// SAE J2534-1 per-protocol length range for a `PassThruMessage.Data`
    /// buffer transmitted to the J2534 library (`PassThruWriteMsgs`).
    ///
    /// | Protocol | Min Tx | Max Tx | Notes |
    /// |---|---|---|---|
    /// | CAN | 4 | 12 | 4-byte CAN ID + up to 8 data bytes |
    /// | ISO15765 | 4 | 4099 | 4-byte CAN ID + up to 4095 data bytes |
    /// | ISO15765 (extended addressing) | 5 | 4100 | 4-byte CAN ID + 1 AE byte + up to 4095 data bytes |
    /// | J1850PWM | 3 | 10 | 3 header bytes + up to 7 data bytes |
    /// | J1850VPW | 1 | 4128 | |
    /// | ISO9141 | 1 | 4128 | |
    /// | ISO14230 | 1 | 259 | 1-4 header bytes + up to 255 data bytes |
    /// | SCI | 1 | 4128 | |
    /// | UART_ECHO_BYTE_PS | 4 | 256 | SAE J2534-2 clause 12.4.2 Table 38 (ADR-170/Phase 9) -- no addressing-mode split |
    /// | HONDA_DIAGH_PS | 3 | 255 | SAE J2534-2 clause 13.4.3 Table 48 (ADR-174/Phase 10) -- no addressing-mode split |
    /// | J1708_PS | 1 | 4095 | SAE J2534-2 clause 17.4.3 Table 67 (ADR-175/Phase 11) -- no addressing-mode split |
    /// | J1939_PS | 5 | 1790 | SAE J2534-2 clause 16.4.3 Table 62 (ADR-179/Phase 5) -- no addressing-mode split; the 5-byte minimum is the CP_MessagePriority/CP_J1939DataPage/CP_J1939PDUFormat/CP_J1939PDUSpecific-or-target-address header plus destination byte `tx_header.rs` composes (deferred to a later phase), not client payload alone |
    /// | TP2_0_PS | 4 | 4096 | SAE J2534-2 clause 19 Table 80 (ADR-188/Phase 7 Stage 7a) -- no addressing-mode split; the 4-byte minimum is the established TX-ID `tx_header.rs` prepends, not client payload alone |
    /// | GM_UART_PS | 3 | 170 | SAE J2534-2 clause 11 Table 33 (ADR-189/Phase 8) -- no addressing-mode split; the 3-byte minimum is the destination/source/length header, up to 167 data bytes |
    ///
    /// `extended_addressing` only affects ISO15765 (a one-byte Address
    /// Extension widens both bounds by 1); it is ignored for every other
    /// protocol, including `UART_ECHO_BYTE_PS`/`HONDA_DIAGH_PS`/`J1708_PS`/
    /// `J1939_PS`/`GM_UART_PS` (clauses 12, 13, 17, 16, and 11 each define a
    /// single addressing mode, no Normal/Extended split). ISO14230's "Manual
    /// Checksum" 260-byte variant is NOT represented as a row here -- this
    /// function returns its ordinary 1..=259 row unconditionally; ADR-198
    /// Phase 2's call sites (`rpc_primitive::resolve_send_recv_tx`/the
    /// `CoptStartcomm` fast-init size check) widen the returned range's end
    /// by 1 themselves when the effective native `ISO9141_NO_CHECKSUM`
    /// connect flag applies, rather than this function growing a third
    /// boolean parameter for a single-protocol special case (`extended_
    /// addressing` and the RawMode-manual-checksum bit are independent axes
    /// that never combine -- ISO14230 has no addressing-mode split at all).
    /// `UART_ECHO_BYTE_PS`'s RX-side range
    /// (clause 12.4.2 Table 38: 3..=256), `HONDA_DIAGH_PS`'s RX-side range
    /// (clause 13.4.3 Table 48: 1..=255), `J1708_PS`'s RX-side range (clause
    /// 17.4.3 Table 67: 1..=4095, the same as its TX-side range), `J1939_PS`'s
    /// RX-side range (clause 16.4.3 Table 62: 5..=1790, also the same as its
    /// TX-side range), and `GM_UART_PS`'s RX-side range (clause 11 Table 33:
    /// 3..=170, also the same as its TX-side range) need no entry here or
    /// anywhere else in this service -- there is no RX-side message-size
    /// validation function for any protocol; `PassThruReadMsgs` results are
    /// forwarded as-is (ADR-170 verification
    /// note).
    pub(super) fn tx_message_size_range(self, extended_addressing: bool) -> RangeInclusive<usize> {
        match self.j2534_protocol_id() {
            j2534_0404::CAN => 4..=12,
            j2534_0404::ISO15765 => {
                if extended_addressing {
                    5..=4100
                } else {
                    4..=4099
                }
            }
            j2534_0404::J1850PWM => 3..=10,
            j2534_0404::J1850VPW => 1..=4128,
            j2534_0404::ISO9141 => 1..=4128,
            j2534_0404::ISO14230 => 1..=259,
            j2534_0404::SCI_A_ENGINE
            | j2534_0404::SCI_A_TRANS
            | j2534_0404::SCI_B_ENGINE
            | j2534_0404::SCI_B_TRANS
            | j2534_0404::SCI_MODE => 1..=4128,
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS => 4..=256,
            j2534_0404::PROTOCOL_HONDA_DIAGH_PS => 3..=255,
            j2534_0404::PROTOCOL_J1708_PS => 1..=4095,
            j2534_0404::PROTOCOL_J1939_PS => 5..=1790,
            j2534_0404::PROTOCOL_TP2_0_PS => 4..=4096,
            j2534_0404::PROTOCOL_GM_UART_PS => 3..=170,
            // Unknown/unmapped protocol IDs: fall back to the PASSTHRU_MSG
            // struct's own data-buffer capacity rather than rejecting outright.
            _ => 1..=j2534_0404::MAX_MESSAGE_DATA,
        }
    }
}

/// SAE J2534-2 clause 22 ISO15765-on-CAN-FD (ADR-159/Phase 3b) Table 98's TX
/// message size range on an `FD_ISO15765_PS` link: a 4-byte (5-byte under
/// extended addressing) header plus up to the shared 4128-byte maximum both
/// rows cap at. Deliberately a standalone function, not a
/// `ChannelProtocol::tx_message_size_range` row: that table is a pure SAE
/// J2534-1 per-protocol constant with its own lockstep test, while FD-ness is
/// link state, not a protocol constant -- the same reasoning
/// `rpc_link::fd_can_tx_message_size_range` (Stage 3a) already established,
/// mirrored here since clause 22's own range happens to be a fixed constant
/// rather than tracking a live staged ComParam the way clause 21's does.
pub(super) fn fd_iso15765_tx_message_size_range(
    extended_addressing: bool,
) -> RangeInclusive<usize> {
    if extended_addressing {
        5..=4128
    } else {
        4..=4128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_message_size_range_matches_sae_j2534_1_table() {
        assert_eq!(ChannelProtocol::CAN.tx_message_size_range(false), 4..=12);
        assert_eq!(
            ChannelProtocol::ISO15765.tx_message_size_range(false),
            4..=4099
        );
        assert_eq!(
            ChannelProtocol::ISO15765.tx_message_size_range(true),
            5..=4100
        );
        assert_eq!(
            ChannelProtocol::J1850PWM.tx_message_size_range(false),
            3..=10
        );
        assert_eq!(
            ChannelProtocol::J1850VPW.tx_message_size_range(false),
            1..=4128
        );
        assert_eq!(
            ChannelProtocol::ISO9141.tx_message_size_range(false),
            1..=4128
        );
        assert_eq!(
            ChannelProtocol::ISO14230.tx_message_size_range(false),
            1..=259
        );
        assert_eq!(
            ChannelProtocol::SCI_A_ENGINE.tx_message_size_range(false),
            1..=4128
        );
        assert_eq!(
            ChannelProtocol::SCI_A_TRANS.tx_message_size_range(false),
            1..=4128
        );
        assert_eq!(
            ChannelProtocol::SCI_B_ENGINE.tx_message_size_range(false),
            1..=4128
        );
        assert_eq!(
            ChannelProtocol::SCI_B_TRANS.tx_message_size_range(false),
            1..=4128
        );
    }

    /// ADR-159/Phase 3b: SAE J2534-2 Table 98's two rows, non-extended and
    /// extended addressing.
    #[test]
    fn fd_iso15765_tx_message_size_range_matches_sae_j2534_2_table_98() {
        assert_eq!(fd_iso15765_tx_message_size_range(false), 4..=4128);
        assert_eq!(fd_iso15765_tx_message_size_range(true), 5..=4128);
    }

    /// ADR-170/Phase 9: SAE J2534-2 clause 12.4.2 Table 38's TX range, no
    /// addressing-mode split (`extended_addressing` is ignored).
    #[test]
    fn uart_echo_byte_ps_tx_message_size_range_matches_sae_j2534_2_table_38() {
        assert_eq!(
            ChannelProtocol::UART_ECHO_BYTE_PS.tx_message_size_range(false),
            4..=256
        );
        assert_eq!(
            ChannelProtocol::UART_ECHO_BYTE_PS.tx_message_size_range(true),
            4..=256
        );
    }

    /// ADR-174/Phase 10: SAE J2534-2 clause 13.4.3 Table 48's TX range, no
    /// addressing-mode split (`extended_addressing` is ignored).
    #[test]
    fn honda_diagh_ps_tx_message_size_range_matches_sae_j2534_2_table_48() {
        assert_eq!(
            ChannelProtocol::HONDA_DIAGH_PS.tx_message_size_range(false),
            3..=255
        );
        assert_eq!(
            ChannelProtocol::HONDA_DIAGH_PS.tx_message_size_range(true),
            3..=255
        );
    }

    /// ADR-175/Phase 11: SAE J2534-2 clause 17.4.3 Table 67's TX range, no
    /// addressing-mode split (`extended_addressing` is ignored).
    #[test]
    fn j1708_ps_tx_message_size_range_matches_sae_j2534_2_table_67() {
        assert_eq!(
            ChannelProtocol::J1708_PS.tx_message_size_range(false),
            1..=4095
        );
        assert_eq!(
            ChannelProtocol::J1708_PS.tx_message_size_range(true),
            1..=4095
        );
    }

    /// ADR-179/Phase 5: SAE J2534-2 clause 16.4.3 Table 62's TX range, no
    /// addressing-mode split (`extended_addressing` is ignored).
    #[test]
    fn j1939_ps_tx_message_size_range_matches_sae_j2534_2_table_62() {
        assert_eq!(
            ChannelProtocol::J1939_PS.tx_message_size_range(false),
            5..=1790
        );
        assert_eq!(
            ChannelProtocol::J1939_PS.tx_message_size_range(true),
            5..=1790
        );
    }

    /// ADR-188/Phase 7 Stage 7a: SAE J2534-2 clause 19 Table 80's TX range,
    /// no addressing-mode split (`extended_addressing` is ignored).
    #[test]
    fn tp2_0_ps_tx_message_size_range_matches_sae_j2534_2_table_80() {
        assert_eq!(
            ChannelProtocol::TP2_0_PS.tx_message_size_range(false),
            4..=4096
        );
        assert_eq!(
            ChannelProtocol::TP2_0_PS.tx_message_size_range(true),
            4..=4096
        );
    }

    /// ADR-189/Phase 8: SAE J2534-2 clause 11 Table 33's TX range, no
    /// addressing-mode split (`extended_addressing` is ignored).
    #[test]
    fn gm_uart_ps_tx_message_size_range_matches_sae_j2534_2_table_33() {
        assert_eq!(
            ChannelProtocol::GM_UART_PS.tx_message_size_range(false),
            3..=170
        );
        assert_eq!(
            ChannelProtocol::GM_UART_PS.tx_message_size_range(true),
            3..=170
        );
    }

    /// ADR-170/Phase 9: `UART_ECHO_BYTE_PS` self-identifies through
    /// `j2534_protocol_id()` -- no unqualified base id exists to map down to
    /// (contrast every 0x0100+ service-level extended protocol above).
    #[test]
    fn uart_echo_byte_ps_self_identifies_through_j2534_protocol_id() {
        assert_eq!(
            ChannelProtocol::UART_ECHO_BYTE_PS.j2534_protocol_id(),
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        );
        assert!(!ChannelProtocol::UART_ECHO_BYTE_PS.is_can_family());
        assert!(!ChannelProtocol::UART_ECHO_BYTE_PS.is_kwp_family());
        assert!(!ChannelProtocol::UART_ECHO_BYTE_PS.is_j1850_family());
        assert!(!ChannelProtocol::UART_ECHO_BYTE_PS.is_sci_family());
    }

    /// ADR-174/Phase 10: `HONDA_DIAGH_PS` self-identifies through
    /// `j2534_protocol_id()` -- no unqualified base id exists to map down to
    /// (contrast every 0x0100+ service-level extended protocol above).
    #[test]
    fn honda_diagh_ps_self_identifies_through_j2534_protocol_id() {
        assert_eq!(
            ChannelProtocol::HONDA_DIAGH_PS.j2534_protocol_id(),
            j2534_0404::PROTOCOL_HONDA_DIAGH_PS
        );
        assert!(!ChannelProtocol::HONDA_DIAGH_PS.is_can_family());
        assert!(!ChannelProtocol::HONDA_DIAGH_PS.is_kwp_family());
        assert!(!ChannelProtocol::HONDA_DIAGH_PS.is_j1850_family());
        assert!(!ChannelProtocol::HONDA_DIAGH_PS.is_sci_family());
    }

    /// ADR-175/Phase 11: `J1708_PS` self-identifies through
    /// `j2534_protocol_id()` -- no unqualified base id exists to map down to
    /// (contrast every 0x0100+ service-level extended protocol above).
    #[test]
    fn j1708_ps_self_identifies_through_j2534_protocol_id() {
        assert_eq!(
            ChannelProtocol::J1708_PS.j2534_protocol_id(),
            j2534_0404::PROTOCOL_J1708_PS
        );
        assert!(!ChannelProtocol::J1708_PS.is_can_family());
        assert!(!ChannelProtocol::J1708_PS.is_kwp_family());
        assert!(!ChannelProtocol::J1708_PS.is_j1850_family());
        assert!(!ChannelProtocol::J1708_PS.is_sci_family());
    }

    /// ADR-179/Phase 5: `J1939_PS` self-identifies through
    /// `j2534_protocol_id()` -- no unqualified base id exists to map down to
    /// (contrast every 0x0100+ service-level extended protocol above).
    /// Deliberately not CAN-family despite riding a CAN transceiver
    /// physically -- see `ChannelProtocol::J1939_PS`'s own doc comment
    /// (ADR-179 Context: "Standalone protocol, not a CAN bus-type variant").
    #[test]
    fn j1939_ps_self_identifies_through_j2534_protocol_id() {
        assert_eq!(
            ChannelProtocol::J1939_PS.j2534_protocol_id(),
            j2534_0404::PROTOCOL_J1939_PS
        );
        assert!(!ChannelProtocol::J1939_PS.is_can_family());
        assert!(!ChannelProtocol::J1939_PS.is_kwp_family());
        assert!(!ChannelProtocol::J1939_PS.is_j1850_family());
        assert!(!ChannelProtocol::J1939_PS.is_sci_family());
    }

    /// ADR-188/Phase 7 Stage 7a: `TP2_0_PS` self-identifies through
    /// `j2534_protocol_id()` -- no unqualified base id exists to map down to
    /// (contrast every 0x0100+ service-level extended protocol above).
    /// Deliberately not CAN-family despite riding a CAN transceiver
    /// physically -- see `ChannelProtocol::TP2_0_PS`'s own doc comment.
    #[test]
    fn tp2_0_ps_self_identifies_through_j2534_protocol_id() {
        assert_eq!(
            ChannelProtocol::TP2_0_PS.j2534_protocol_id(),
            j2534_0404::PROTOCOL_TP2_0_PS
        );
        assert!(!ChannelProtocol::TP2_0_PS.is_can_family());
        assert!(!ChannelProtocol::TP2_0_PS.is_kwp_family());
        assert!(!ChannelProtocol::TP2_0_PS.is_j1850_family());
        assert!(!ChannelProtocol::TP2_0_PS.is_sci_family());
    }

    /// ADR-189/Phase 8: `GM_UART_PS` self-identifies through
    /// `j2534_protocol_id()` -- no unqualified base id exists to map down to
    /// (contrast every 0x0100+ service-level extended protocol above).
    #[test]
    fn gm_uart_ps_self_identifies_through_j2534_protocol_id() {
        assert_eq!(
            ChannelProtocol::GM_UART_PS.j2534_protocol_id(),
            j2534_0404::PROTOCOL_GM_UART_PS
        );
        assert!(!ChannelProtocol::GM_UART_PS.is_can_family());
        assert!(!ChannelProtocol::GM_UART_PS.is_kwp_family());
        assert!(!ChannelProtocol::GM_UART_PS.is_j1850_family());
        assert!(!ChannelProtocol::GM_UART_PS.is_sci_family());
    }

    /// ADR-194/Phase 16: `ETHERNET_NDIS` self-identifies through
    /// `j2534_protocol_id()` -- no unqualified base id exists to map down to
    /// (contrast every 0x0100+ service-level extended protocol above).
    #[test]
    fn ethernet_ndis_self_identifies_through_j2534_protocol_id() {
        assert_eq!(
            ChannelProtocol::ETHERNET_NDIS.j2534_protocol_id(),
            j2534_0404::PROTOCOL_ETHERNET_NDIS
        );
        assert!(!ChannelProtocol::ETHERNET_NDIS.is_can_family());
        assert!(!ChannelProtocol::ETHERNET_NDIS.is_kwp_family());
        assert!(!ChannelProtocol::ETHERNET_NDIS.is_j1850_family());
        assert!(!ChannelProtocol::ETHERNET_NDIS.is_sci_family());
    }

    #[test]
    fn tx_message_size_range_resolves_through_extended_protocol_ids() {
        // Extended service-level IDs must map onto the same table rows as
        // their underlying J2534 channel type.
        assert_eq!(
            ChannelProtocol::ISO_14229_3_ON_ISO_15765_2.tx_message_size_range(false),
            ChannelProtocol::ISO15765.tx_message_size_range(false)
        );
        assert_eq!(
            ChannelProtocol::ISO_15031_5_ON_SAE_J1850_PWM.tx_message_size_range(false),
            ChannelProtocol::J1850PWM.tx_message_size_range(false)
        );
        assert_eq!(
            ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI.tx_message_size_range(false),
            1..=4128
        );
    }

    #[test]
    fn new_extended_protocol_ids_resolve_to_expected_j2534_hardware_id() {
        assert_eq!(
            ChannelProtocol::ISO_14229_3.j2534_protocol_id(),
            j2534_0404::ISO15765
        );
        assert_eq!(
            ChannelProtocol::ISO_15765_3.j2534_protocol_id(),
            j2534_0404::ISO15765
        );
        assert_eq!(
            ChannelProtocol::ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4.j2534_protocol_id(),
            j2534_0404::ISO9141
        );
        assert_eq!(
            ChannelProtocol::SAE_J2190_ON_ISO_9141_2_AND_ISO_14230_2.j2534_protocol_id(),
            j2534_0404::ISO9141
        );
        assert_eq!(
            ChannelProtocol::SAE_J2190_ON_SAE_J1850.j2534_protocol_id(),
            j2534_0404::J1850VPW
        );
        assert_eq!(
            ChannelProtocol::ISO_15031_5_ON_SAE_J1850.j2534_protocol_id(),
            j2534_0404::J1850VPW
        );
        assert!(ChannelProtocol::ISO_14229_3.is_can_family());
        assert!(ChannelProtocol::ISO_15765_3.is_can_family());
        assert!(ChannelProtocol::ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4.is_kwp_family());
        assert!(ChannelProtocol::SAE_J2190_ON_ISO_9141_2_AND_ISO_14230_2.is_kwp_family());
    }

    #[test]
    fn needs_j1850_autodetect_is_true_only_for_the_sae_j1850_bus_agnostic_pair() {
        assert!(ChannelProtocol::SAE_J2190_ON_SAE_J1850.needs_j1850_autodetect());
        assert!(ChannelProtocol::ISO_15031_5_ON_SAE_J1850.needs_j1850_autodetect());
        // The fixed-flavor VPW/PWM-qualified variants (0x0150/0x0151/0x0140/0x0141)
        // map to J1850VPW/J1850PWM too, but are not on the auto-detect bus.
        assert!(!ChannelProtocol::SAE_J2190_ON_SAE_J1850_VPW.needs_j1850_autodetect());
        assert!(!ChannelProtocol::ISO_15031_5_ON_SAE_J1850_VPW.needs_j1850_autodetect());
        assert!(!ChannelProtocol::SAE_J2190_ON_SAE_J1850_PWM.needs_j1850_autodetect());
        assert!(!ChannelProtocol::ISO_15031_5_ON_SAE_J1850_PWM.needs_j1850_autodetect());
        assert!(!ChannelProtocol::J1850VPW.needs_j1850_autodetect());
        assert!(!ChannelProtocol::J1850PWM.needs_j1850_autodetect());
    }

    #[test]
    fn extended_addressing_is_ignored_outside_iso15765() {
        assert_eq!(
            ChannelProtocol::CAN.tx_message_size_range(true),
            ChannelProtocol::CAN.tx_message_size_range(false)
        );
        assert_eq!(
            ChannelProtocol::ISO14230.tx_message_size_range(true),
            ChannelProtocol::ISO14230.tx_message_size_range(false)
        );
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ModifyTimingApplicability {
        Kwp,
        Uds,
        Neither,
    }

    /// Codex review, PR #22 round 3: an exhaustive table over every
    /// declared `ChannelProtocol` constant, pinning `CP_ModifyTiming`'s
    /// applicability classification for each one. `ChannelProtocol` is an
    /// open newtype (the compiler cannot force exhaustiveness the way a
    /// closed `enum` would), so THIS TABLE is the defense against a future
    /// constant silently inheriting the wrong bucket -- when adding a new
    /// `ChannelProtocol` constant, add a row here too.
    #[test]
    fn kwp_and_uds_session_timing_applicability_matches_the_exhaustive_table() {
        use ModifyTimingApplicability::{Kwp, Neither, Uds};

        let table = [
            (ChannelProtocol::J1850VPW, Neither),
            (ChannelProtocol::J1850PWM, Neither),
            (ChannelProtocol::ISO9141, Neither),
            (ChannelProtocol::ISO14230, Kwp),
            (ChannelProtocol::CAN, Neither),
            (ChannelProtocol::ISO15765, Uds),
            (ChannelProtocol::SCI_A_ENGINE, Neither),
            (ChannelProtocol::SCI_A_TRANS, Neither),
            (ChannelProtocol::SCI_B_ENGINE, Neither),
            (ChannelProtocol::SCI_B_TRANS, Neither),
            (ChannelProtocol::ISO_15765_3_ON_ISO_15765_2, Uds),
            (ChannelProtocol::ISO_14229_3_ON_ISO_15765_2, Uds),
            (ChannelProtocol::ISO_14230_3_ON_ISO_15765_2, Neither),
            (ChannelProtocol::SAE_J2190_ON_ISO_15765_2, Neither),
            (ChannelProtocol::ISO_15031_5_ON_ISO_15765_4, Neither),
            (
                ChannelProtocol::ISO_14229_3_ON_ISO_15765_2_WITH_ISO_11783_5,
                Uds,
            ),
            (ChannelProtocol::ISO_14229_3, Uds),
            (ChannelProtocol::ISO_15765_3, Uds),
            (ChannelProtocol::ISO_11783_12_ON_ISO_11783_5, Neither),
            (ChannelProtocol::SAE_J2190_ON_ISO_9141_2, Neither),
            (ChannelProtocol::ISO_15031_5_ON_ISO_9141_2, Neither),
            (
                ChannelProtocol::ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4,
                Neither,
            ),
            (
                ChannelProtocol::SAE_J2190_ON_ISO_9141_2_AND_ISO_14230_2,
                Neither,
            ),
            (ChannelProtocol::ISO_14230_3_ON_ISO_14230_2, Kwp),
            (ChannelProtocol::SAE_J2190_ON_ISO_14230_2, Neither),
            (ChannelProtocol::ISO_15031_5_ON_ISO_14230_4, Kwp),
            (ChannelProtocol::SAE_J2190_ON_SAE_J1850_PWM, Neither),
            (ChannelProtocol::ISO_15031_5_ON_SAE_J1850_PWM, Neither),
            (ChannelProtocol::SAE_J2190_ON_SAE_J1850_VPW, Neither),
            (ChannelProtocol::ISO_15031_5_ON_SAE_J1850_VPW, Neither),
            (ChannelProtocol::SAE_J2190_ON_SAE_J1850, Neither),
            (ChannelProtocol::ISO_15031_5_ON_SAE_J1850, Neither),
            (ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI, Neither),
            (ChannelProtocol::UART_ECHO_BYTE_PS, Neither),
            (ChannelProtocol::HONDA_DIAGH_PS, Neither),
            // ADR-188 Consequences: pre-existing gap backfilled alongside
            // this stage's own TP2_0_PS row -- J1708_PS, J1939_PS, and
            // ANALOG_IN were missing from this table despite its own
            // "add a row when adding a constant" contract above.
            (ChannelProtocol::J1708_PS, Neither),
            (ChannelProtocol::J1939_PS, Neither),
            (ChannelProtocol::ANALOG_IN, Neither),
            (ChannelProtocol::TP2_0_PS, Neither),
            (ChannelProtocol::GM_UART_PS, Neither),
            (ChannelProtocol::ETHERNET_NDIS, Neither),
        ];

        for (protocol, expected) in table {
            let actual = if protocol.kwp_access_timing_applies() {
                Kwp
            } else if protocol.uds_session_timing_applies() {
                Uds
            } else {
                Neither
            };
            assert_eq!(
                actual, expected,
                "{protocol:?} (raw {:#06x}): expected {expected:?}, got {actual:?} -- update \
                 either the table or the `..._applies()` predicates, and check both are still \
                 mutually exclusive (a protocol classified as both would be a bug in the \
                 predicates themselves)",
                protocol.0
            );
            assert!(
                !(protocol.kwp_access_timing_applies() && protocol.uds_session_timing_applies()),
                "{protocol:?} must not satisfy both predicates at once"
            );
        }
    }
}
