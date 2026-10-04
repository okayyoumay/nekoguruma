//! Static ISO 22900-2-style resource table.
//!
//! Resource IDs are drawn from a dedicated opaque namespace starting at
//! `0x0201` and bus type IDs from a companion namespace starting at `0x0301`
//! -- neither overlaps the raw/extended J2534 `ChannelProtocol` values used
//! elsewhere in this service. Both are opaque MDF-style handles (per
//! ADR-069): callers must resolve them through this table (or the selectors
//! in `names.rs`), never by reinterpreting the numeric value as a J2534
//! protocol ID directly.
//!
//! Each row binds one resource ID to a `(protocol, bus type, typed DLC pin
//! list, hardware protocol override)` tuple. Several rows intentionally share
//! the same `ChannelProtocol` under a distinct resource ID / `protocol_name`
//! (e.g. `ISO_OBD_on_K_Line` and `ISO_15031_5_on_ISO_9141_2_and_ISO_14230_4`
//! both resolve to the same combined-K-line channel) -- ISO 22900-2 gives
//! these distinct short names even though they share one underlying channel,
//! so the duplication here is intentional, not a bug. Legacy raw/extended
//! `ChannelProtocol` values remain independently valid inputs to
//! `CreateComLogicalLink`, resolved via `ChannelProtocol::from_raw` when a
//! value does not match any row's `resource_id` (see
//! `names::parse_protocol_id_from_resource`).
//!
//! `hw_protocol_override` (spec correction, ADR-069 amendment) lets a row's
//! *service-level* `ChannelProtocol` (identity, ComParam defaults) diverge
//! from the *hardware* protocol ID actually used for `PassThruConnect` --
//! needed because the four `SAE_J2610_on_SAE_J2610_SCI` rows all share one
//! `ChannelProtocol` (`0x0160`) for identity purposes, but each names a
//! distinct physical SCI configuration (`SCI_A_ENGINE`/`_A_TRANS`/
//! `_B_ENGINE`/`_B_TRANS`) selected by DLC pin wiring, not by `ChannelProtocol`
//! value. `None` means the row connects with its `ChannelProtocol`'s own
//! `j2534_protocol_id()`, as before.

use super::ChannelProtocol;
use super::discovery::DiscoveryCheck;

// ── Bus type IDs (0x0300 namespace) ─────────────────────────────────────────
pub(super) const BUSTYPE_ISO_11898_2_DWCAN: u32 = 0x0301;
pub(super) const BUSTYPE_ISO_14230_1_UART: u32 = 0x0302;
pub(super) const BUSTYPE_ISO_9141_2_UART: u32 = 0x0303;
pub(super) const BUSTYPE_ISO_9141_2_UART_AND_ISO_14230_1_UART: u32 = 0x0304;
pub(super) const BUSTYPE_SAE_J1850_PWM: u32 = 0x0305;
pub(super) const BUSTYPE_SAE_J1850_VPW: u32 = 0x0306;
// Renamed from `SAE_J1850_VPW_and_SAE_J1850_PWM` (spec correction): this bus
// auto-detects VPW vs PWM at `ConnectComLogicalLink` time instead of fixing
// one connect protocol per resource row (ADR-070). The old name was never
// released, so it is dropped entirely -- no legacy alias.
pub(super) const BUSTYPE_SAE_J1850: u32 = 0x0307;
pub(super) const BUSTYPE_SAE_J2610_UART: u32 = 0x0308;
// SAE J2534-2 clause 9 (Single Wire CAN/SWCAN/GMLAN, ADR-164/Phase 4): a
// third bus-type variant of several already-implemented CAN-family
// application-layer rows (see the `SAE_J2411_SWCAN`-tagged rows below),
// reusing their existing `ChannelProtocol` identity -- the same shape this
// table already uses to distinguish dual-wire CAN from fault-tolerant CAN
// under the same application-layer protocols (ISO 22900-2's ComParam value
// tables carry a distinct `SAE_J2411_SWCAN` bus-type column with its own
// defaults, `comparam_defaults::sae_j2411_swcan`).
pub(super) const BUSTYPE_SAE_J2411_SWCAN: u32 = 0x0309;
// SAE J2534-2 clause 20 (Fault-Tolerant CAN, ISO 11898-3, ADR-168/Phase 6): a
// fourth bus-type variant of several already-implemented CAN-family
// application-layer rows (see the `ISO_11898_3_DWFTCAN`-tagged rows below),
// reusing their existing `ChannelProtocol` identity -- the same shape this
// table already uses to distinguish dual-wire CAN from single-wire CAN under
// the same application-layer protocols (ISO 22900-2's ComParam value tables
// carry a distinct `ISO_11898_3_DWFTCAN` bus-type column with its own
// defaults, `comparam_defaults::bustype_default_params`).
pub(super) const BUSTYPE_ISO_11898_3_DWFTCAN: u32 = 0x030A;
// SAE J2534-2 clause 12 (UART Echo Byte Protocol, ADR-170/Phase 9): unlike
// every bus type above, this is not a variant of an existing CAN-family
// application-layer row -- clause 12 defines a wholly standalone protocol
// (Honda ABS/VSA per SAE J2809, or KWP1281 per SAE J2818) with its own
// `ChannelProtocol` identity (`ChannelProtocol::UART_ECHO_BYTE_PS`), so it
// gets its own single bus type rather than reusing an existing one.
pub(super) const BUSTYPE_UART_ECHO_BYTE: u32 = 0x030B;
// SAE J2534-2 clause 13 (Honda DIAG-H Protocol, ADR-174/Phase 10): the same
// shape as UART Echo Byte's bus type just above -- a wholly standalone
// protocol (Honda's proprietary "92 Hm/2" message format over the
// single-wire, half-duplex "DIAG-H" UART physical layer) with its own
// `ChannelProtocol` identity (`ChannelProtocol::HONDA_DIAGH_PS`), so it gets
// its own single bus type rather than reusing an existing one.
pub(super) const BUSTYPE_HONDA_DIAGH: u32 = 0x030C;
// SAE J2534-2 clause 17 (SAE J1708 Protocol, ADR-175/Phase 11): the same
// shape as UART Echo Byte's/Honda DIAG-H's bus types just above -- a wholly
// standalone protocol (the SAE J1708 heavy-duty-truck serial bus) with its
// own `ChannelProtocol` identity (`ChannelProtocol::J1708_PS`), so it gets
// its own single bus type rather than reusing an existing one.
pub(super) const BUSTYPE_SAE_J1708: u32 = 0x030D;
// SAE J2534-2 clause 10 (Analog Inputs, ADR-177/Phase 15): a synthetic bus
// type with NO ISO 22900-2 source at all, unlike every bus type above --
// ISO 22900-2 (both editions, checked directly) defines no resource or
// ComParam concept resembling multi-channel analog acquisition
// (`SET_PROG_VOLTAGE`/`READ_VBATT` are the only voltage-adjacent concepts,
// neither modeling a 32-channel acquisition resource). Flagged explicitly
// here rather than implying a spec basis that doesn't exist, mirroring
// J1708's own `PINS_SAE_J1708` doc comment precedent for documenting a
// non-spec-grounded editorial choice. Shared by all 32 Analog Input rows
// below (they have no per-row pin wiring to distinguish, unlike the SCI/CAN
// bus-type variants above) -- `rows_conflict`'s own `same_config` shortcut
// (identical bus type AND identical, empty `dlc_pins`) already treats any
// two of these 32 rows as non-conflicting, matching each channel's
// independent-subsystem design (ADR-177 Decision).
pub(super) const BUSTYPE_ANALOG_IN: u32 = 0x030E;
// SAE J2534-2 clause 16 (SAE J1939 Protocol, ADR-179/Phase 5): the same
// shape as UART Echo Byte's/Honda DIAG-H's/SAE J1708's bus types above -- a
// wholly standalone protocol (its own device-owned address claim/defend
// negotiation, 5-byte message framing, and unique-response-address rule,
// none of which is CAN's own) with its own `ChannelProtocol` identity
// (`ChannelProtocol::J1939_PS`), so it gets its own single bus type rather
// than reusing `BUSTYPE_ISO_11898_2_DWCAN`/`BUSTYPE_SAE_J2411_SWCAN`-style
// bus-type-variant treatment (ADR-179's own "Standalone protocol, not a CAN
// bus-type variant" Context discussion) even though it physically rides a
// CAN transceiver.
pub(super) const BUSTYPE_SAE_J1939: u32 = 0x030F;
// SAE J2534-2 clause 19 (TP2.0 Protocol, ADR-188/Phase 7 Stage 7a): the same
// shape as UART Echo Byte's/Honda DIAG-H's/SAE J1708's/SAE J1939's bus types
// above -- a wholly standalone protocol (VW/Audi's connection-oriented TP2.0
// transport, SAE J2819) with its own `ChannelProtocol` identity
// (`ChannelProtocol::TP2_0_PS`), so it gets its own single bus type. Named
// `TP2_0_DWCAN` rather than reusing `ISO_11898_2_DWCAN` -- TP2.0 has no
// ISO 22900-2 preset at all (ADR-188 Context), so this bus type is minted,
// not a spec-sanctioned dual-wire-CAN variant, the same `BUSTYPE_ANALOG_IN`
// minting precedent (ADR-177).
pub(super) const BUSTYPE_TP2_0: u32 = 0x0310;
// SAE J2534-2 clause 11 (GM UART Protocol, SAE J2740, ADR-189/Phase 8): the
// same shape as UART Echo Byte's/Honda DIAG-H's/SAE J1708's/SAE J1939's/
// TP2.0's bus types above -- a wholly standalone protocol (a master/slave
// UART bus with its own poll-message/poll-response bus-mastership handshake)
// with its own `ChannelProtocol` identity (`ChannelProtocol::GM_UART_PS`), so
// it gets its own single bus type rather than reusing an existing one. Unlike
// every standalone protocol before it, GM UART also participates in the
// clause 7 Additional Channels `_CHx` arithmetic mapping (ADR-189 Decision
// 2) -- this bus type covers the `_PS` resource row only, `_CHx` channels
// share the same `ChannelProtocol` identity via `base_protocol_id`'s
// normalization, not a second bus type.
pub(super) const BUSTYPE_GM_UART: u32 = 0x0311;
// SAE J2534-2 clause 24 (Ethernet_NDIS, ADR-194/Phase 16): the same
// standalone shape as UART Echo Byte's/Honda DIAG-H's/SAE J1708's/SAE
// J1939's/TP2.0's/GM UART's bus types above -- a wholly standalone
// protocol (binds a J2534 channel to an NDIS/RNDIS Ethernet adapter) with
// its own `ChannelProtocol` identity (`ChannelProtocol::ETHERNET_NDIS`),
// so it gets its own single bus type. Named `IEEE_802_3` -- unlike every
// standalone protocol's bus type above (each a project-minted name with no
// ISO 22900-2 precedent), this is the first one with a real ISO anchor:
// ISO 22900-2:2022 Table B.2 already names `IEEE_802_3` as DoIP's own
// physical-layer/BUSTYPE short name (verified directly against
// `iso22900-2-2022/ISO_22900-2_2022(en).md`'s Table B.2 row "ISO UDS on
// DoIP"), and clause 24's own Ethernet/NDIS physical layer is the same
// IEEE 802.3 Ethernet physical layer DoIP itself rides.
pub(super) const BUSTYPE_IEEE_802_3: u32 = 0x0312;

// ── Pin type IDs (ISO 22900-2 Annex B.2/B.5 logical pin types) ─────────────
// Matches `names::map_pintype_name`'s 2000-range values exactly; duplicated
// here (rather than depending on `names.rs`, which depends on this module)
// so `dlc_pins` below can be typed directly.
const PIN_HI: u32 = 2000;
const PIN_LOW: u32 = 2001;
const PIN_K: u32 = 2002;
const PIN_L: u32 = 2003;
const PIN_TX: u32 = 2004;
const PIN_RX: u32 = 2005;
const PIN_PLUS: u32 = 2006;
const PIN_MINUS: u32 = 2007;

// ── Default typed DLC pin lists per bus type / SCI configuration ───────────
// Each entry is (pin_number, pin_type_id). Matches
// `docs/j2534-0404-architecture.md`'s "Default DLC pin conventions" table.
const PINS_ISO_11898_2_DWCAN: &[(u32, u32)] = &[(6, PIN_HI), (14, PIN_LOW)];
const PINS_ISO_14230_1_UART: &[(u32, u32)] = &[(7, PIN_K), (15, PIN_L)];
const PINS_ISO_9141_2_UART: &[(u32, u32)] = &[(7, PIN_K), (15, PIN_L)];
const PINS_ISO_9141_2_UART_AND_ISO_14230_1_UART: &[(u32, u32)] = &[(7, PIN_K), (15, PIN_L)];
const PINS_SAE_J1850_PWM: &[(u32, u32)] = &[(2, PIN_PLUS), (10, PIN_MINUS)];
const PINS_SAE_J1850_VPW: &[(u32, u32)] = &[(2, PIN_PLUS)];
const PINS_SAE_J1850: &[(u32, u32)] = &[(2, PIN_PLUS), (10, PIN_MINUS)];
// SAE_J2610_UART: the pin wiring itself selects the J2534 SCI configuration
// (SCI_A_ENGINE/_A_TRANS/_B_ENGINE/_B_TRANS), so each configuration gets its
// own typed pin pair rather than one bus-wide list.
const PINS_SCI_A_ENGINE: &[(u32, u32)] = &[(6, PIN_TX), (7, PIN_RX)];
const PINS_SCI_A_TRANS: &[(u32, u32)] = &[(14, PIN_TX), (7, PIN_RX)];
const PINS_SCI_B_ENGINE: &[(u32, u32)] = &[(12, PIN_TX), (7, PIN_RX)];
const PINS_SCI_B_TRANS: &[(u32, u32)] = &[(9, PIN_TX), (15, PIN_RX)];
// SAE J2534-2 clause 9.2.1 (ADR-164/Phase 4): SWCAN uses pin 1 of the J1962
// connector only -- no secondary pin, unlike every dual-wire/fault-tolerant
// CAN row above. Reuses `PIN_HI`'s existing primary-role categorization
// (`default_pin_select_for_base`/`compute_pin_select`'s primary/secondary
// split) rather than inventing a new pin-type constant, matching ADR-164
// Decision 1.
const PINS_SAE_J2411_SWCAN: &[(u32, u32)] = &[(1, PIN_HI)];
// SAE J2534-2 clause 20.2.1 (ADR-168/Phase 6): Fault-Tolerant CAN is a
// genuine CAN-high/CAN-low differential pair, like dual-wire CAN, not a
// single-wire protocol like SWCAN -- pins 1 (HI) / 9 (LOW), the first-listed
// of clause 20.2.1's two documented pin-pair options (the other, pins 3/11,
// is reachable via ordinary Pin Selection override, not a second resource
// row), mirroring ADR-164's own precedent of picking the first-listed pin(s)
// as a practical default despite the clause saying "no default identified."
const PINS_ISO_11898_3_DWFTCAN: &[(u32, u32)] = &[(1, PIN_HI), (9, PIN_LOW)];
// SAE J2534-2 clause 12.2.2 (ADR-170/Phase 9): J1962 pin 7 (the VW/Audi
// convention), a single K-line pin -- no secondary pin, mirroring SWCAN's own
// single-pin shape (`PINS_SAE_J2411_SWCAN`) rather than a differential pair
// like `PINS_ISO_11898_3_DWFTCAN`. Typed `PIN_K` (not `PIN_HI`), matching
// this table's existing K-line rows (`PINS_ISO_9141_2_UART`,
// `PINS_ISO_14230_1_UART`) -- `PIN_K` is still categorized "primary" by
// `compute_pin_select`/`default_pin_select_for_base`'s own primary/secondary
// split (HI/K/TX/PLUS), so it packs the same way a lone primary pin does
// elsewhere in this table.
const PINS_UART_ECHO_BYTE: &[(u32, u32)] = &[(7, PIN_K)];
// SAE J2534-2 clause 13.2.4 (ADR-174/Phase 10): J1962 pin 14 -- the pin
// Honda's own diagnostic application always targets (clause 13.2.4's own
// text singles it out, including for 3-pin/5-pin-DLC vehicles reached
// through an adapter cable that transparently routes pin 14 to the
// adapter's own pin 1). A single-wire pin, no secondary role, mirroring
// SWCAN's/UART Echo Byte's own single-pin shape. Typed `PIN_K`, matching
// `PINS_UART_ECHO_BYTE`'s own choice -- both are single generic-signal pins
// with no HI/LO differential-pair concept, and `PIN_K` is still categorized
// "primary" by `compute_pin_select`/`default_pin_select_for_base`'s own
// primary/secondary split (HI/K/TX/PLUS), so it packs the same way a lone
// primary pin does elsewhere in this table.
const PINS_HONDA_DIAGH: &[(u32, u32)] = &[(14, PIN_K)];
// SAE J2534-2 clause 17 (SAE J1708 Protocol, ADR-175/Phase 11): J1962 pins
// 3/11, typed `PIN_PLUS`/`PIN_MINUS` -- the same differential-pair pin types
// already used for J1850 (`PINS_SAE_J1850_PWM`/`PINS_SAE_J1850`), matching
// J1708's own J1708+/J1708- differential-pair naming per clause 6.3.3.2's
// dedicated-connector pin table, even though this phase does not use that
// connector (it uses J1962 only -- see ADR-175's Context section). This pin
// pair has NO textual basis in clause 17 or clause 6 for the J1962 connector
// specifically -- it is an editorial convenience default only, matching a
// commonly-cited real-world heavy-truck scan-tool J1962 wiring convention for
// J1708, not sourced from the spec. This is the least spec-grounded default
// this codebase has picked so far; flagged explicitly for revisit if a
// primary SAE J1708 electrical-layer reference becomes available (see
// ADR-175's Consequences section and the Prioritized Backlog).
const PINS_SAE_J1708: &[(u32, u32)] = &[(3, PIN_PLUS), (11, PIN_MINUS)];
// SAE J2534-2 clause 19.2.2 (ADR-188/Phase 7 Stage 7a): J1962 pins 6/14, the
// only pair clause 19.2.2 documents -- a genuine CAN-high/CAN-low
// differential pair, so typed the same `PIN_HI`/`PIN_LOW` pair as dual-wire
// CAN's own `PINS_ISO_11898_2_DWCAN` above (TP2.0 physically rides a CAN
// transceiver even though it is not `is_can_family()`, ADR-188 Decision 1).
const PINS_TP2_0: &[(u32, u32)] = &[(6, PIN_HI), (14, PIN_LOW)];
// SAE J2534-2 clause 11.2.2 (ADR-189/Phase 8): J1962 pin 9 (primary) -- the
// same closed-two-single-pin shape Honda DIAG-H's own `PINS_HONDA_DIAGH`
// established (clause 13.2.4, pins 1/14), only the pin numbers differ (1/9,
// not 1/14). No spec-mandated default; pin 9 is an editorial convenience
// default, the same category of non-normative choice `PINS_HONDA_DIAGH`'s
// own doc comment documents for pin 14. Typed `PIN_K`, matching every other
// single-wire UART protocol row in this table (`PINS_UART_ECHO_BYTE`,
// `PINS_HONDA_DIAGH`) -- `PIN_K` is still categorized "primary" by
// `compute_pin_select`/`default_pin_select_for_base`'s own primary/secondary
// split (HI/K/TX/PLUS), so it packs the same way a lone primary pin does
// elsewhere in this table.
const PINS_GM_UART: &[(u32, u32)] = &[(9, PIN_K)];
// SAE J2534-2 clause 24.2.4 Table 103 (ADR-194/Phase 16): unlike every
// standalone protocol above (each with at most one differential pair plus,
// at most, a single control pin), clause 24 defines TWO independent
// differential pairs (Ethernet Tx/Rx) plus one single control pin
// (Activation Line) -- a genuinely new shape this table's typed-pin
// vocabulary has no dedicated pair-per-signal-group concept for. Reuses
// this table's two existing differential-pair type families to keep the
// two pairs distinct from each other: `PIN_PLUS`/`PIN_MINUS` (J1850's/
// J1708's own convention) for the Tx pair, `PIN_HI`/`PIN_LOW` (CAN's/
// TP2.0's own convention) for the Rx pair, and `PIN_K` (every standalone
// UART protocol's own lone-control-pin convention: Honda DIAG-H/UART Echo
// Byte/GM UART) for the Activation Line -- none of the three reused
// families is a genuine textual match for Ethernet signaling, but this
// mirrors `PINS_SAE_J1708`'s own precedent of an explicitly-flagged,
// non-spec-grounded typing choice rather than leaving a pin untyped.
// Table 103 lists Option 1 and Option 2 as alternate pin assignments for
// the Tx pair only (pins 3/11 vs. 1/9); the Rx pair (12/13) and Activation
// Line (8) are common to both. This row lists Option 1's pin set --
// mirroring ADR-164's/ADR-168's own "first-listed as practical default"
// precedent -- since a resource row carries exactly one typed pin list.
// Purely descriptive metadata: unlike every `_PS`/`_CHx` row's `dlc_pins`,
// this row's pins are never resolved into a native
// `SET_CONFIG(CONFIG_J1962_PINS)` call -- clause 24 has no `_PS`/`_CHx`
// pin-selection mechanics at all (pin usage is chosen by connect flag,
// `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2`, ADR-194 Decision), so this
// protocol never reaches `names::resolve_pin_selection`/
// `row_needs_dynamic_pin_selection`.
const PINS_ETHERNET_NDIS: &[(u32, u32)] = &[
    (3, PIN_PLUS),
    (8, PIN_K),
    (11, PIN_MINUS),
    (12, PIN_HI),
    (13, PIN_LOW),
];

/// One row of the static resource table.
///
/// `protocol_name` and `bus_type_name` are the canonical ISO 22900-2 short
/// names (preserving the standard's mixed-case connector words, e.g.
/// `"on"`/`"and"`); lookups against them are always case-insensitive (see
/// `names::resolve_resource_ids_from_data`).
pub(super) struct ResourceDef {
    pub resource_id: u32,
    /// Canonical ISO 22900-2 short name for this resource.
    pub protocol_name: &'static str,
    /// SCI configuration name (`SCI_A_ENGINE`/`SCI_A_TRANS`/`SCI_B_ENGINE`/
    /// `SCI_B_TRANS`) for the four `SAE_J2610_SCI` rows; `None` everywhere
    /// else -- including the four `SAE_J2610_on_SAE_J2610_SCI` rows, which
    /// also select a configuration (via `hw_protocol_override`) but must not
    /// set `config_name`, since a bare config name (e.g. `"SCI_A_ENGINE"`)
    /// must resolve uniquely to its `SAE_J2610_SCI` row.
    pub config_name: Option<&'static str>,
    pub protocol: ChannelProtocol,
    /// Overrides the J2534 hardware protocol ID used for `PassThruConnect`/
    /// `ChannelKey`/message building, when `Some`; `None` means use
    /// `protocol.j2534_protocol_id()` as before. See the module doc comment.
    pub hw_protocol_override: Option<u32>,
    pub bus_type_id: u32,
    pub bus_type_name: &'static str,
    /// Typed DLC pins: `(pin_number, pin_type_id)` pairs.
    pub dlc_pins: &'static [(u32, u32)],
}

/// The static resource table, in resource-ID order (0x0201..=0x0260).
static RESOURCE_TABLE: [ResourceDef; 97] = [
    ResourceDef {
        resource_id: 0x0201,
        protocol_name: "ISO_11898_RAW",
        config_name: None,
        protocol: ChannelProtocol::CAN,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    ResourceDef {
        resource_id: 0x0202,
        protocol_name: "ISO_14229_3",
        config_name: None,
        protocol: ChannelProtocol::ISO_14229_3,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    ResourceDef {
        resource_id: 0x0203,
        protocol_name: "ISO_14229_3_on_ISO_15765_2",
        config_name: None,
        protocol: ChannelProtocol::ISO_14229_3_ON_ISO_15765_2,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    ResourceDef {
        resource_id: 0x0204,
        protocol_name: "ISO_14230_3_on_ISO_15765_2",
        config_name: None,
        protocol: ChannelProtocol::ISO_14230_3_ON_ISO_15765_2,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    ResourceDef {
        resource_id: 0x0205,
        protocol_name: "ISO_15031_5_on_ISO_15765_4",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_15765_4,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    ResourceDef {
        resource_id: 0x0206,
        protocol_name: "ISO_15765_2",
        config_name: None,
        protocol: ChannelProtocol::ISO15765,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    ResourceDef {
        resource_id: 0x0207,
        protocol_name: "ISO_15765_3",
        config_name: None,
        protocol: ChannelProtocol::ISO_15765_3,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    ResourceDef {
        resource_id: 0x0208,
        protocol_name: "ISO_15765_3_on_ISO_15765_2",
        config_name: None,
        protocol: ChannelProtocol::ISO_15765_3_ON_ISO_15765_2,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    // Alias row: same ChannelProtocol as 0x0205 (ISO_15031_5_on_ISO_15765_4),
    // distinct resource ID / protocol_name per ISO 22900-2.
    ResourceDef {
        resource_id: 0x0209,
        protocol_name: "ISO_OBD_on_ISO_15765_4",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_15765_4,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    ResourceDef {
        resource_id: 0x020A,
        protocol_name: "SAE_J2190_on_ISO_15765_2",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2190_ON_ISO_15765_2,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_11898_2_DWCAN,
        bus_type_name: "ISO_11898_2_DWCAN",
        dlc_pins: PINS_ISO_11898_2_DWCAN,
    },
    ResourceDef {
        resource_id: 0x020B,
        protocol_name: "ISO_14230_3_on_ISO_14230_2",
        config_name: None,
        protocol: ChannelProtocol::ISO_14230_3_ON_ISO_14230_2,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_14230_1_UART,
        bus_type_name: "ISO_14230_1_UART",
        dlc_pins: PINS_ISO_14230_1_UART,
    },
    ResourceDef {
        resource_id: 0x020C,
        protocol_name: "ISO_14230_4",
        config_name: None,
        protocol: ChannelProtocol::ISO14230,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_14230_1_UART,
        bus_type_name: "ISO_14230_1_UART",
        dlc_pins: PINS_ISO_14230_1_UART,
    },
    ResourceDef {
        resource_id: 0x020D,
        protocol_name: "ISO_15031_5_on_ISO_14230_4",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_14230_4,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_14230_1_UART,
        bus_type_name: "ISO_14230_1_UART",
        dlc_pins: PINS_ISO_14230_1_UART,
    },
    ResourceDef {
        resource_id: 0x020E,
        protocol_name: "SAE_J2190_on_ISO_14230_2",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2190_ON_ISO_14230_2,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_14230_1_UART,
        bus_type_name: "ISO_14230_1_UART",
        dlc_pins: PINS_ISO_14230_1_UART,
    },
    ResourceDef {
        resource_id: 0x020F,
        protocol_name: "ISO_15031_5_on_ISO_9141_2",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_9141_2,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_9141_2_UART,
        bus_type_name: "ISO_9141_2_UART",
        dlc_pins: PINS_ISO_9141_2_UART,
    },
    ResourceDef {
        resource_id: 0x0210,
        protocol_name: "ISO_9141_2",
        config_name: None,
        protocol: ChannelProtocol::ISO9141,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_9141_2_UART,
        bus_type_name: "ISO_9141_2_UART",
        dlc_pins: PINS_ISO_9141_2_UART,
    },
    ResourceDef {
        resource_id: 0x0211,
        protocol_name: "SAE_J2190_on_ISO_9141_2",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2190_ON_ISO_9141_2,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_9141_2_UART,
        bus_type_name: "ISO_9141_2_UART",
        dlc_pins: PINS_ISO_9141_2_UART,
    },
    ResourceDef {
        resource_id: 0x0212,
        protocol_name: "ISO_15031_5_on_ISO_9141_2_and_ISO_14230_4",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_9141_2_UART_AND_ISO_14230_1_UART,
        bus_type_name: "ISO_9141_2_UART_and_ISO_14230_1_UART",
        dlc_pins: PINS_ISO_9141_2_UART_AND_ISO_14230_1_UART,
    },
    // Alias row: same ChannelProtocol as 0x0212.
    ResourceDef {
        resource_id: 0x0213,
        protocol_name: "ISO_OBD_on_K_Line",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_9141_2_AND_ISO_14230_4,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_9141_2_UART_AND_ISO_14230_1_UART,
        bus_type_name: "ISO_9141_2_UART_and_ISO_14230_1_UART",
        dlc_pins: PINS_ISO_9141_2_UART_AND_ISO_14230_1_UART,
    },
    ResourceDef {
        resource_id: 0x0214,
        protocol_name: "SAE_J2190_on_ISO_9141_2_and_ISO_14230_2",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2190_ON_ISO_9141_2_AND_ISO_14230_2,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_ISO_9141_2_UART_AND_ISO_14230_1_UART,
        bus_type_name: "ISO_9141_2_UART_and_ISO_14230_1_UART",
        dlc_pins: PINS_ISO_9141_2_UART_AND_ISO_14230_1_UART,
    },
    ResourceDef {
        resource_id: 0x0215,
        protocol_name: "ISO_15031_5_on_SAE_J1850_PWM",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_SAE_J1850_PWM,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1850_PWM,
        bus_type_name: "SAE_J1850_PWM",
        dlc_pins: PINS_SAE_J1850_PWM,
    },
    ResourceDef {
        resource_id: 0x0216,
        protocol_name: "SAE_J1850_PWM",
        config_name: None,
        protocol: ChannelProtocol::J1850PWM,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1850_PWM,
        bus_type_name: "SAE_J1850_PWM",
        dlc_pins: PINS_SAE_J1850_PWM,
    },
    ResourceDef {
        resource_id: 0x0217,
        protocol_name: "SAE_J2190_on_SAE_J1850_PWM",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2190_ON_SAE_J1850_PWM,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1850_PWM,
        bus_type_name: "SAE_J1850_PWM",
        dlc_pins: PINS_SAE_J1850_PWM,
    },
    ResourceDef {
        resource_id: 0x0218,
        protocol_name: "ISO_15031_5_on_SAE_J1850_VPW",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_SAE_J1850_VPW,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1850_VPW,
        bus_type_name: "SAE_J1850_VPW",
        dlc_pins: PINS_SAE_J1850_VPW,
    },
    ResourceDef {
        resource_id: 0x0219,
        protocol_name: "SAE_J1850_VPW",
        config_name: None,
        protocol: ChannelProtocol::J1850VPW,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1850_VPW,
        bus_type_name: "SAE_J1850_VPW",
        dlc_pins: PINS_SAE_J1850_VPW,
    },
    ResourceDef {
        resource_id: 0x021A,
        protocol_name: "SAE_J2190_on_SAE_J1850",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2190_ON_SAE_J1850,
        hw_protocol_override: None,
        // Moved from the VPW-only bus (0x0306) to the auto-detecting
        // SAE_J1850 bus (0x0307, spec correction): this resource's
        // `ChannelProtocol` is the bus-agnostic variant, so it belongs on the
        // bus that resolves VPW vs PWM at connect time (ADR-070), not on a
        // fixed-VPW bus. Resource ID and table order are unchanged.
        bus_type_id: BUSTYPE_SAE_J1850,
        bus_type_name: "SAE_J1850",
        dlc_pins: PINS_SAE_J1850,
    },
    ResourceDef {
        resource_id: 0x021B,
        protocol_name: "SAE_J2190_on_SAE_J1850_VPW",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2190_ON_SAE_J1850_VPW,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1850_VPW,
        bus_type_name: "SAE_J1850_VPW",
        dlc_pins: PINS_SAE_J1850_VPW,
    },
    ResourceDef {
        resource_id: 0x021C,
        protocol_name: "ISO_15031_5_on_SAE_J1850",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_SAE_J1850,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1850,
        bus_type_name: "SAE_J1850",
        dlc_pins: PINS_SAE_J1850,
    },
    // Alias row: same ChannelProtocol as 0x021C.
    ResourceDef {
        resource_id: 0x021D,
        protocol_name: "ISO_OBD_on_SAE_J1850",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_SAE_J1850,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1850,
        bus_type_name: "SAE_J1850",
        dlc_pins: PINS_SAE_J1850,
    },
    // ── SAE_J2610_on_SAE_J2610_SCI: four rows, one per SCI configuration ──
    // (spec correction). All four share one `ChannelProtocol` (identity,
    // ComParam defaults) but each names a distinct physical SCI
    // configuration via `hw_protocol_override` -- the DLC pin wiring is what
    // actually selects the configuration on real hardware, matching
    // `SAE_J2610_SCI`'s per-config pins below. `config_name` stays `None` on
    // all four: bare config names (e.g. `"SCI_A_ENGINE"`) must resolve
    // uniquely to their `SAE_J2610_SCI` row, not also match these.
    ResourceDef {
        resource_id: 0x021E,
        protocol_name: "SAE_J2610_on_SAE_J2610_SCI",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI,
        hw_protocol_override: Some(j2534_0404::SCI_A_ENGINE),
        bus_type_id: BUSTYPE_SAE_J2610_UART,
        bus_type_name: "SAE_J2610_UART",
        dlc_pins: PINS_SCI_A_ENGINE,
    },
    ResourceDef {
        resource_id: 0x021F,
        protocol_name: "SAE_J2610_on_SAE_J2610_SCI",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI,
        hw_protocol_override: Some(j2534_0404::SCI_A_TRANS),
        bus_type_id: BUSTYPE_SAE_J2610_UART,
        bus_type_name: "SAE_J2610_UART",
        dlc_pins: PINS_SCI_A_TRANS,
    },
    ResourceDef {
        resource_id: 0x0220,
        protocol_name: "SAE_J2610_on_SAE_J2610_SCI",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI,
        hw_protocol_override: Some(j2534_0404::SCI_B_ENGINE),
        bus_type_id: BUSTYPE_SAE_J2610_UART,
        bus_type_name: "SAE_J2610_UART",
        dlc_pins: PINS_SCI_B_ENGINE,
    },
    ResourceDef {
        resource_id: 0x0221,
        protocol_name: "SAE_J2610_on_SAE_J2610_SCI",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI,
        hw_protocol_override: Some(j2534_0404::SCI_B_TRANS),
        bus_type_id: BUSTYPE_SAE_J2610_UART,
        bus_type_name: "SAE_J2610_UART",
        dlc_pins: PINS_SCI_B_TRANS,
    },
    ResourceDef {
        resource_id: 0x0222,
        protocol_name: "SAE_J2610_SCI",
        config_name: Some("SCI_A_ENGINE"),
        protocol: ChannelProtocol::SCI_A_ENGINE,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J2610_UART,
        bus_type_name: "SAE_J2610_UART",
        dlc_pins: PINS_SCI_A_ENGINE,
    },
    ResourceDef {
        resource_id: 0x0223,
        protocol_name: "SAE_J2610_SCI",
        config_name: Some("SCI_A_TRANS"),
        protocol: ChannelProtocol::SCI_A_TRANS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J2610_UART,
        bus_type_name: "SAE_J2610_UART",
        dlc_pins: PINS_SCI_A_TRANS,
    },
    ResourceDef {
        resource_id: 0x0224,
        protocol_name: "SAE_J2610_SCI",
        config_name: Some("SCI_B_ENGINE"),
        protocol: ChannelProtocol::SCI_B_ENGINE,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J2610_UART,
        bus_type_name: "SAE_J2610_UART",
        dlc_pins: PINS_SCI_B_ENGINE,
    },
    ResourceDef {
        resource_id: 0x0225,
        protocol_name: "SAE_J2610_SCI",
        config_name: Some("SCI_B_TRANS"),
        protocol: ChannelProtocol::SCI_B_TRANS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J2610_UART,
        bus_type_name: "SAE_J2610_UART",
        dlc_pins: PINS_SCI_B_TRANS,
    },
    // ── SAE J2534-2 clause 9 Single Wire CAN (SWCAN/GMLAN, ADR-164/Phase 4) ──
    // Ten rows, mirroring every row above that currently sits on
    // `BUSTYPE_ISO_11898_2_DWCAN` (0x0201-0x020A) -- the full grep-derived
    // set ADR-164's Context section calls for, spanning raw CAN plus every
    // ISO15765-based diagnostic protocol naming era this table already
    // implements (KWP2000/UDS's "bare" and "_on_ISO_15765_2"-qualified
    // rows, Enhanced Diagnostics, OBD, and its alias). Each row reuses its
    // dual-wire sibling's own `ChannelProtocol` unchanged (ISO 22900-2 Annex
    // G models SWCAN as the SAME client-selectable application-layer
    // *identity* on a different bus type, not a new protocol) -- but each
    // gets its OWN `protocol_name` (dual-wire sibling's name + `_SWCAN`),
    // a deliberate correction from an earlier draft of this table that
    // reused the sibling's exact `protocol_name` string: `resources.rs`'s
    // `find_table_row_by_name` (`names.rs`) treats two rows sharing one
    // `protocol_name` with DIFFERING `hw_protocol_override` (`None` for the
    // dual-wire row, `Some(_PS_id)` here) as genuinely ambiguous unless the
    // caller supplies disambiguating `dlc_pin_data` -- correct for the four
    // `SAE_J2610_on_SAE_J2610_SCI` rows (four physically distinct wirings
    // with no sane default), but wrong for SWCAN: it would silently turn
    // every PRE-EXISTING, previously-unambiguous bare-name lookup of these
    // ten dual-wire protocol_names (e.g. `"ISO_15765_2"`, `"ISO_11898_RAW"`)
    // into a hard error for any caller not supplying pins -- proven by
    // `parse_protocol_id_from_resource_extends_channel_selection_to_a_table_row_name`/
    // `resolve_object_id_objt_resource_resolves_through_table_first`/
    // `resolve_resource_ids_from_data_filters_by_protocol_name`, three
    // pre-existing tests that failed under the reused-name draft. Distinct
    // `protocol_name`s keep every pre-existing name-only lookup unchanged
    // while still making each SWCAN row independently name-addressable
    // (ISO 22900-2 Annex G's own worked example lists its SWCAN resource
    // under its own distinct name, "Single-Wire CAN on Pin 1", not the
    // dual-wire resource's name -- this correction's `_SWCAN` suffix is
    // this table's own naming choice for that same distinctness, not a
    // transcription of the annex's exact wording). `bus_type_id`/`dlc_pins`
    // still disambiguate physically, exactly as before -- disambiguation by
    // name was never the only mechanism; a numeric `resource_id` always
    // round-trips unambiguously regardless. `hw_protocol_override` is
    // `PROTOCOL_SW_CAN_PS` for the raw-CAN row and `PROTOCOL_SW_ISO15765_PS`
    // for every ISO15765-based row -- clause 9 defines no unqualified base
    // SWCAN id at all (unlike every other bus-type variant in this table),
    // so the override stores the qualified `_PS` id directly rather than a
    // native base id (contrast the `SAE_J2610_on_SAE_J2610_SCI` rows above,
    // whose overrides are base SCI ids). See `is_sw_protocol_id`/
    // `base_protocol_id`'s new SW arms for how the rest of this service
    // treats these ids as their base CAN/ISO15765 family "for free".
    ResourceDef {
        resource_id: 0x0226,
        protocol_name: "ISO_11898_RAW_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::CAN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_CAN_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    ResourceDef {
        resource_id: 0x0227,
        protocol_name: "ISO_14229_3_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_14229_3,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_ISO15765_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    ResourceDef {
        resource_id: 0x0228,
        protocol_name: "ISO_14229_3_on_ISO_15765_2_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_14229_3_ON_ISO_15765_2,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_ISO15765_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    ResourceDef {
        resource_id: 0x0229,
        protocol_name: "ISO_14230_3_on_ISO_15765_2_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_14230_3_ON_ISO_15765_2,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_ISO15765_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    ResourceDef {
        resource_id: 0x022A,
        protocol_name: "ISO_15031_5_on_ISO_15765_4_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_15765_4,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_ISO15765_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    ResourceDef {
        resource_id: 0x022B,
        protocol_name: "ISO_15765_2_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO15765,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_ISO15765_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    ResourceDef {
        resource_id: 0x022C,
        protocol_name: "ISO_15765_3_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_15765_3,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_ISO15765_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    ResourceDef {
        resource_id: 0x022D,
        protocol_name: "ISO_15765_3_on_ISO_15765_2_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_15765_3_ON_ISO_15765_2,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_ISO15765_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    // Alias row: same ChannelProtocol as 0x022A, mirroring 0x0209
    // (ISO_OBD_on_ISO_15765_4)'s own alias relationship to 0x0205.
    ResourceDef {
        resource_id: 0x022E,
        protocol_name: "ISO_OBD_on_ISO_15765_4_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_15765_4,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_ISO15765_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    ResourceDef {
        resource_id: 0x022F,
        protocol_name: "SAE_J2190_on_ISO_15765_2_SWCAN",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2190_ON_ISO_15765_2,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_SW_ISO15765_PS),
        bus_type_id: BUSTYPE_SAE_J2411_SWCAN,
        bus_type_name: "SAE_J2411_SWCAN",
        dlc_pins: PINS_SAE_J2411_SWCAN,
    },
    // SAE J2534-2 clause 20 (Fault-Tolerant CAN, ISO 11898-3, ADR-168/Phase
    // 6): a fifth (alongside the four `SAE_J2610_on_SAE_J2610_SCI` variant
    // rows -- see the module doc comment) bus-type variant of the ten
    // dual-wire-CAN rows above, mirroring ADR-164's SWCAN naming correction
    // (each row gets its own distinct `protocol_name`, dual-wire sibling's
    // name + `_FTCAN`, avoiding ambiguity with both the ten pre-existing
    // dual-wire names and the ten SWCAN names) while reusing the dual-wire
    // sibling's `ChannelProtocol` identity unchanged. `hw_protocol_override`
    // is `PROTOCOL_FT_CAN_PS` for the raw-CAN row and
    // `PROTOCOL_FT_ISO15765_PS` for every ISO15765-based row -- clause 20
    // defines no unqualified base FTCAN id at all (same shape as clause 9
    // SWCAN), so the override stores the qualified `_PS` id directly. See
    // `is_ft_protocol_id`/`base_protocol_id`'s FT arms for how the rest of
    // this service treats these ids as their base CAN/ISO15765 family "for
    // free".
    ResourceDef {
        resource_id: 0x0230,
        protocol_name: "ISO_11898_RAW_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::CAN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_CAN_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    ResourceDef {
        resource_id: 0x0231,
        protocol_name: "ISO_14229_3_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_14229_3,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_ISO15765_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    ResourceDef {
        resource_id: 0x0232,
        protocol_name: "ISO_14229_3_on_ISO_15765_2_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_14229_3_ON_ISO_15765_2,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_ISO15765_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    ResourceDef {
        resource_id: 0x0233,
        protocol_name: "ISO_14230_3_on_ISO_15765_2_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_14230_3_ON_ISO_15765_2,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_ISO15765_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    ResourceDef {
        resource_id: 0x0234,
        protocol_name: "ISO_15031_5_on_ISO_15765_4_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_15765_4,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_ISO15765_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    ResourceDef {
        resource_id: 0x0235,
        protocol_name: "ISO_15765_2_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO15765,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_ISO15765_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    ResourceDef {
        resource_id: 0x0236,
        protocol_name: "ISO_15765_3_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_15765_3,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_ISO15765_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    ResourceDef {
        resource_id: 0x0237,
        protocol_name: "ISO_15765_3_on_ISO_15765_2_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_15765_3_ON_ISO_15765_2,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_ISO15765_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    // Alias row: same ChannelProtocol as 0x0234, mirroring 0x022E
    // (ISO_OBD_on_ISO_15765_4_SWCAN)'s own alias relationship to 0x022A.
    ResourceDef {
        resource_id: 0x0238,
        protocol_name: "ISO_OBD_on_ISO_15765_4_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::ISO_15031_5_ON_ISO_15765_4,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_ISO15765_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    ResourceDef {
        resource_id: 0x0239,
        protocol_name: "SAE_J2190_on_ISO_15765_2_FTCAN",
        config_name: None,
        protocol: ChannelProtocol::SAE_J2190_ON_ISO_15765_2,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_FT_ISO15765_PS),
        bus_type_id: BUSTYPE_ISO_11898_3_DWFTCAN,
        bus_type_name: "ISO_11898_3_DWFTCAN",
        dlc_pins: PINS_ISO_11898_3_DWFTCAN,
    },
    // SAE J2534-2 clause 12 (UART Echo Byte Protocol, ADR-170/Phase 9): one
    // bare-protocol row, not ten like SWCAN/FT-CAN above -- no OBD/
    // service-composite spec defines an equivalent over this protocol, so
    // there is nothing to mirror the way the ten CAN-family rows mirror
    // each other. `protocol` is `ChannelProtocol::UART_ECHO_BYTE_PS`
    // directly (not an existing CAN/ISO15765 identity), and
    // `hw_protocol_override` is `None` -- unlike SWCAN/FT-CAN, there is no
    // separate base id for this row's own identity to diverge from; the
    // `_PS` id IS the protocol identity (see `protocol.rs`'s own doc
    // comment on the constant). `protocol_name` is a project-chosen
    // descriptive name -- no ISO 22900-2 short name exists to alias (a
    // search of both available editions for "Honda"/"VSA"/"KWP1281"/
    // "J2809"/"J2818" returns zero matches, ADR-170 Context).
    ResourceDef {
        resource_id: 0x023A,
        protocol_name: "UART_ECHO_BYTE",
        config_name: None,
        protocol: ChannelProtocol::UART_ECHO_BYTE_PS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_UART_ECHO_BYTE,
        bus_type_name: "UART_ECHO_BYTE_UART",
        dlc_pins: PINS_UART_ECHO_BYTE,
    },
    // SAE J2534-2 clause 13 (Honda DIAG-H Protocol, ADR-174/Phase 10): one
    // bare-protocol row, the same shape as UART Echo Byte's row above -- no
    // OBD/service-composite spec defines an equivalent over this protocol.
    // `protocol` is `ChannelProtocol::HONDA_DIAGH_PS` directly, and
    // `hw_protocol_override` is `None` -- the `_PS` id IS the protocol
    // identity (see `protocol.rs`'s own doc comment on the constant).
    // `protocol_name` is a project-chosen descriptive name -- no ISO 22900-2
    // short name exists to alias (a search of both available editions for
    // "DIAG-H"/"92 Hm"/"Honda" returns zero matches, ADR-174 Context).
    ResourceDef {
        resource_id: 0x023B,
        protocol_name: "HONDA_DIAGH",
        config_name: None,
        protocol: ChannelProtocol::HONDA_DIAGH_PS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_HONDA_DIAGH,
        bus_type_name: "HONDA_DIAGH_UART",
        dlc_pins: PINS_HONDA_DIAGH,
    },
    // SAE J2534-2 clause 17 (SAE J1708 Protocol, ADR-175/Phase 11): one
    // bare-protocol row, the same shape as UART Echo Byte's/Honda DIAG-H's
    // rows above -- no OBD/service-composite spec defines an equivalent over
    // this protocol. `protocol` is `ChannelProtocol::J1708_PS` directly, and
    // `hw_protocol_override` is `None` -- the `_PS` id IS the protocol
    // identity (see `protocol.rs`'s own doc comment on the constant).
    // `protocol_name` is a project-chosen descriptive name -- ISO 22900-2
    // defines no SAE J1708 short name to alias. `bus_type_name` reuses
    // `comparam_defaults.rs`'s existing but previously-unwired
    // `sae_j1708_uart()` bustype default.
    ResourceDef {
        resource_id: 0x023C,
        protocol_name: "SAE_J1708",
        config_name: None,
        protocol: ChannelProtocol::J1708_PS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1708,
        bus_type_name: "SAE_J1708_UART",
        dlc_pins: PINS_SAE_J1708,
    },
    // SAE J2534-2 clause 10 (Analog Inputs, ADR-177/Phase 15): 32 static
    // rows, one per independent, read-only native `PROTOCOL_ANALOG_IN_x` id
    // -- the same "N distinct native ids -> N rows differing only by
    // `hw_protocol_override`" shape the SAE_J2610_on_SAE_J2610_SCI block
    // above and the SWCAN/FT-CAN blocks use, not a new resource-model
    // mechanism (ADR-177 Decision, rejecting both a `_CHx`-style arithmetic
    // index mapping and a one-row-plus-mandatory-index-field alternative).
    // Unlike every block above, though: all 32 share ONE `ChannelProtocol`
    // (`ChannelProtocol::ANALOG_IN`, `protocol.rs`'s own doc comment) for
    // identity purposes, and NONE has any `dlc_pins`/pin table at all --
    // clause 10's 32 channels are enumerated directly, with no pin-selection
    // mechanics and no bus/connector concept to wire pins onto. `bus_type_id`
    // is `BUSTYPE_ANALOG_IN` (this table's only synthetic, non-ISO-22900-2
    // bus type -- see that constant's own doc comment) for all 32, since
    // there is no per-row wiring to distinguish; each row's own native id
    // (via `hw_protocol_override`) is what actually differentiates it for
    // `PassThruConnect`/`ChannelKey`/occupancy purposes. `protocol_name` is a
    // project-chosen descriptive name matching the native id's own numbering
    // (`"ANALOG_IN_1"`..`"ANALOG_IN_32"`) -- no ISO 22900-2 short name exists
    // to alias (this bus/protocol family has no ISO 22900-2 source at all,
    // per `BUSTYPE_ANALOG_IN`'s own doc comment).
    ResourceDef {
        resource_id: 0x023D,
        protocol_name: "ANALOG_IN_1",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_1),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x023E,
        protocol_name: "ANALOG_IN_2",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_2),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x023F,
        protocol_name: "ANALOG_IN_3",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_3),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0240,
        protocol_name: "ANALOG_IN_4",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_4),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0241,
        protocol_name: "ANALOG_IN_5",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_5),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0242,
        protocol_name: "ANALOG_IN_6",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_6),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0243,
        protocol_name: "ANALOG_IN_7",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_7),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0244,
        protocol_name: "ANALOG_IN_8",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_8),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0245,
        protocol_name: "ANALOG_IN_9",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_9),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0246,
        protocol_name: "ANALOG_IN_10",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_10),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0247,
        protocol_name: "ANALOG_IN_11",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_11),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0248,
        protocol_name: "ANALOG_IN_12",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_12),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0249,
        protocol_name: "ANALOG_IN_13",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_13),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x024A,
        protocol_name: "ANALOG_IN_14",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_14),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x024B,
        protocol_name: "ANALOG_IN_15",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_15),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x024C,
        protocol_name: "ANALOG_IN_16",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_16),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x024D,
        protocol_name: "ANALOG_IN_17",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_17),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x024E,
        protocol_name: "ANALOG_IN_18",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_18),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x024F,
        protocol_name: "ANALOG_IN_19",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_19),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0250,
        protocol_name: "ANALOG_IN_20",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_20),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0251,
        protocol_name: "ANALOG_IN_21",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_21),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0252,
        protocol_name: "ANALOG_IN_22",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_22),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0253,
        protocol_name: "ANALOG_IN_23",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_23),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0254,
        protocol_name: "ANALOG_IN_24",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_24),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0255,
        protocol_name: "ANALOG_IN_25",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_25),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0256,
        protocol_name: "ANALOG_IN_26",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_26),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0257,
        protocol_name: "ANALOG_IN_27",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_27),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0258,
        protocol_name: "ANALOG_IN_28",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_28),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x0259,
        protocol_name: "ANALOG_IN_29",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_29),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x025A,
        protocol_name: "ANALOG_IN_30",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_30),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x025B,
        protocol_name: "ANALOG_IN_31",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_31),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x025C,
        protocol_name: "ANALOG_IN_32",
        config_name: None,
        protocol: ChannelProtocol::ANALOG_IN,
        hw_protocol_override: Some(j2534_0404::PROTOCOL_ANALOG_IN_32),
        bus_type_id: BUSTYPE_ANALOG_IN,
        bus_type_name: "ANALOG_IN",
        dlc_pins: &[],
    },
    // SAE J2534-2 clause 16 (SAE J1939 Protocol, ADR-179/Phase 5): two
    // bare-protocol rows, not one like UART Echo Byte/Honda DIAG-H/SAE J1708
    // above -- unlike every other standalone protocol, ISO 22900-2 already
    // names two distinct J1939-based application stacks as separate
    // protocol-name presets (`comparam_defaults.rs`'s already-present
    // `iso_obd_on_sae_j1939_73()`/`sae_j1939_73_on_sae_j1939_21()`, both
    // seeded from the shared `j1939_can_common()` helper but differing in
    // `PARAM_MESSAGE_PRIORITY`), the same "one shared ChannelProtocol, two
    // distinct resource-table presets sharing one underlying channel" shape
    // `ISO_15765_2`/`ISO_OBD_on_ISO_15765_4` (resources 0x0206/0x0209)
    // already use, not the "N distinct native ids, N rows" shape the SCI/
    // Analog Input blocks use. `protocol` is `ChannelProtocol::J1939_PS`
    // directly for both rows, and `hw_protocol_override` is `None` -- the
    // `_PS` id IS the protocol identity (see `protocol.rs`'s own doc comment
    // on the constant). `bus_type_name` reuses `comparam_defaults.rs`'s
    // existing but previously-unwired `sae_j1939_11_dwcan()` bustype
    // default, mirroring J1708's own precedent of reusing an
    // already-present-but-unwired preset. `dlc_pins: &[]` -- unlike J1708's
    // own editorial-convenience pin default, clause 16.3.2.1 keeps the
    // physical layer pin-unassigned until an explicit
    // `SET_CONFIG(CONFIG_J1962_PINS)` with no fallback default at all (ADR-179
    // Decision 2); `names::resolve_pin_selection`'s own J1939 arm enforces
    // this by always requiring explicit `dlc_pin_data`, never falling back
    // to a row default the way its J1708/Honda-DIAG-H/UART-Echo-Byte arms
    // do.
    ResourceDef {
        resource_id: 0x025D,
        protocol_name: "ISO_OBD_on_SAE_J1939_73",
        config_name: None,
        protocol: ChannelProtocol::J1939_PS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1939,
        bus_type_name: "SAE_J1939_11_DWCAN",
        dlc_pins: &[],
    },
    ResourceDef {
        resource_id: 0x025E,
        protocol_name: "SAE_J1939_73_on_SAE_J1939_21",
        config_name: None,
        protocol: ChannelProtocol::J1939_PS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_SAE_J1939,
        bus_type_name: "SAE_J1939_11_DWCAN",
        dlc_pins: &[],
    },
    // SAE J2534-2 clause 19 (TP2.0 Protocol, ADR-188/Phase 7 Stage 7a): a
    // single row -- unlike SAE J1939's two ISO-OBD-derived presets above,
    // TP2.0 has no ISO 22900-2 preset at all to give it multiple named
    // application-layer identities, mirroring UART Echo Byte's/Honda
    // DIAG-H's own single-row shape.
    ResourceDef {
        resource_id: 0x025F,
        protocol_name: "SAE_J2819_TP2_0",
        config_name: None,
        protocol: ChannelProtocol::TP2_0_PS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_TP2_0,
        bus_type_name: "TP2_0_DWCAN",
        dlc_pins: PINS_TP2_0,
    },
    // SAE J2534-2 clause 11 (GM UART Protocol, SAE J2740, ADR-189/Phase 8): a
    // single row -- like UART Echo Byte/Honda DIAG-H, GM UART has no ISO
    // 22900-2 preset at all to give it multiple named application-layer
    // identities.
    ResourceDef {
        resource_id: 0x0260,
        protocol_name: "GM_UART",
        config_name: None,
        protocol: ChannelProtocol::GM_UART_PS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_GM_UART,
        bus_type_name: "GM_UART_UART",
        dlc_pins: PINS_GM_UART,
    },
    // SAE J2534-2 clause 24 (Ethernet_NDIS, ADR-194/Phase 16): a single row
    // -- like UART Echo Byte/Honda DIAG-H/GM UART, clause 24 has no ISO
    // 22900-2 preset at all to give it multiple named application-layer
    // identities. `hw_protocol_override` is `None` -- the raw
    // `PROTOCOL_ETHERNET_NDIS` id IS the `ChannelProtocol::ETHERNET_NDIS`
    // identity directly (`protocol.rs`'s own doc comment on the constant).
    // No `_PS`/`_CHx` mechanics: `dlc_pins` here is descriptive only (see
    // `PINS_ETHERNET_NDIS`'s own doc comment) -- pin selection is by
    // connect flag, not `J1962_PINS`-style resolution.
    ResourceDef {
        resource_id: 0x0261,
        protocol_name: "ETHERNET_NDIS",
        config_name: None,
        protocol: ChannelProtocol::ETHERNET_NDIS,
        hw_protocol_override: None,
        bus_type_id: BUSTYPE_IEEE_802_3,
        bus_type_name: "IEEE_802_3",
        dlc_pins: PINS_ETHERNET_NDIS,
    },
];

/// Returns the static resource table, in resource-ID order.
pub(super) fn resource_table() -> &'static [ResourceDef] {
    &RESOURCE_TABLE
}

/// Looks up a single row by its opaque resource ID.
pub(super) fn find_by_resource_id(resource_id: u32) -> Option<&'static ResourceDef> {
    resource_table()
        .iter()
        .find(|row| row.resource_id == resource_id)
}

/// SAE J2534-2 clause 6.3.1 Table 1's `_PS` (Pin Selection) protocol ID
/// variants (ADR-156 Decision 2), keyed by the *native J2534 hardware
/// protocol id* a resolved resource would otherwise connect with (i.e.
/// `ResourceDef::hw_protocol_override.unwrap_or_else(||
/// protocol.j2534_protocol_id())`) -- not the service-level
/// `ChannelProtocol` -- so a service-level variant riding one of these
/// physical channels (e.g. `ISO_14229_3_ON_ISO_15765_2`, whose
/// `j2534_protocol_id()` is `ISO15765`) resolves to the same `_PS` id as
/// the base protocol itself.
///
/// Table 1 lists seven entries: `J1850VPW_PS`, `J1850PWM_PS`, `ISO9141_PS`,
/// `ISO14230_PS`, `CAN_PS`, `ISO15765_PS`, and `J2610_PS` -- the last
/// consolidates all four native SCI hardware ids
/// (`SCI_A_ENGINE`/`_A_TRANS`/`_B_ENGINE`/`_B_TRANS`) into one `J2610_PS`
/// id per the spec's own note under Table 1 (the SAE J2610 Chrysler SCI
/// protocols share one new ProtocolID). `None` for
/// every other hardware id -- every other J2534-2 `_PS` family (SW_CAN,
/// SW_ISO15765, GM_UART, UART_ECHO_BYTE, HONDA_DIAGH, J1939, J1708, TP2.0,
/// FT_CAN, FT_ISO15765, FD_CAN, FD_ISO15765, ...) is out of scope for this
/// phase (ADR-156 Consequences, "Accepted residual").
pub(super) fn ps_protocol_id(hw_protocol_id: u32) -> Option<u32> {
    match hw_protocol_id {
        j2534_0404::J1850VPW => Some(j2534_0404::PROTOCOL_J1850VPW_PS),
        j2534_0404::J1850PWM => Some(j2534_0404::PROTOCOL_J1850PWM_PS),
        j2534_0404::ISO9141 => Some(j2534_0404::PROTOCOL_ISO9141_PS),
        j2534_0404::ISO14230 => Some(j2534_0404::PROTOCOL_ISO14230_PS),
        j2534_0404::CAN => Some(j2534_0404::PROTOCOL_CAN_PS),
        j2534_0404::ISO15765 => Some(j2534_0404::PROTOCOL_ISO15765_PS),
        j2534_0404::SCI_A_ENGINE
        | j2534_0404::SCI_A_TRANS
        | j2534_0404::SCI_B_ENGINE
        | j2534_0404::SCI_B_TRANS => Some(j2534_0404::PROTOCOL_J2610_PS),
        _ => None,
    }
}

/// Exact inverse of [`ps_protocol_id`] (ADR-157 Decision): maps a `_PS`
/// hardware protocol id back to its base J2534-1 id, identity for every
/// other value (including every already-base id). `PROTOCOL_J2610_PS`
/// collapses back to one representative SCI id (`SCI_A_ENGINE`, the first
/// arm `ps_protocol_id` lists among the four it consolidates) rather than
/// round-tripping the exact SCI variant -- a deliberate, narrow
/// simplification documented here and at every Plane B call site that needs
/// SCI-variant fidelity instead (`LogicalLinkState::base_hw_protocol_id`).
///
/// ADR-156 Decision 3 addendum (Phase 2b): also consults
/// [`chx_base_protocol_id`] for a `_CHx` hardware id, discarding the channel
/// index (Plane B callers only need the base family, not which index). This
/// is how every pre-existing ADR-157 Plane B call site (and any future one)
/// handles a `_CHx` link correctly for free, with no new call-site sweep --
/// the same reasoning that motivated funneling `_PS` through this one
/// function in the first place.
pub(super) fn base_protocol_id(hw_protocol_id: u32) -> u32 {
    match hw_protocol_id {
        j2534_0404::PROTOCOL_J1850VPW_PS => j2534_0404::J1850VPW,
        j2534_0404::PROTOCOL_J1850PWM_PS => j2534_0404::J1850PWM,
        j2534_0404::PROTOCOL_ISO9141_PS => j2534_0404::ISO9141,
        j2534_0404::PROTOCOL_ISO14230_PS => j2534_0404::ISO14230,
        j2534_0404::PROTOCOL_CAN_PS => j2534_0404::CAN,
        j2534_0404::PROTOCOL_ISO15765_PS => j2534_0404::ISO15765,
        j2534_0404::PROTOCOL_J2610_PS => j2534_0404::SCI_A_ENGINE,
        // ADR-158/Phase 3a: SAE J2534-2 clause 21 CAN FD's one `_PS`-only
        // hardware id -- every Plane B site (ComParam support, filter-type
        // checks, protocol-family gates, `GetResourceStatus`, etc.) treats
        // an FD_CAN_PS link as CAN-family for behavior purposes, the same
        // reasoning ADR-157 already established for every other `_PS`/`_CHx`
        // id.
        j2534_0404::PROTOCOL_FD_CAN_PS => j2534_0404::CAN,
        // ADR-159/Phase 3b: SAE J2534-2 clause 22 ISO15765-on-CAN-FD's own
        // `_PS`-only hardware id -- every Plane B site treats an
        // FD_ISO15765_PS link as ISO15765-family for behavior purposes, the
        // same reasoning ADR-158 already established for FD_CAN_PS.
        j2534_0404::PROTOCOL_FD_ISO15765_PS => j2534_0404::ISO15765,
        // ADR-164/Phase 4: SAE J2534-2 clause 9 Single Wire CAN's two
        // `_PS`-only hardware ids -- same reasoning as the FD arms just
        // above (a resource row's `hw_protocol_override` stores the
        // qualified id directly, since clause 9 has no unqualified base id
        // either), so every Plane B site treats an SW link as its base
        // CAN/ISO15765 family for free.
        j2534_0404::PROTOCOL_SW_CAN_PS => j2534_0404::CAN,
        j2534_0404::PROTOCOL_SW_ISO15765_PS => j2534_0404::ISO15765,
        // ADR-168/Phase 6: SAE J2534-2 clause 20 Fault-Tolerant CAN's two
        // `_PS`-only hardware ids -- same reasoning as the SW arms just
        // above (a resource row's `hw_protocol_override` stores the
        // qualified id directly, since clause 20 has no unqualified base id
        // either), so every Plane B site treats an FT link as its base
        // CAN/ISO15765 family for free.
        j2534_0404::PROTOCOL_FT_CAN_PS => j2534_0404::CAN,
        j2534_0404::PROTOCOL_FT_ISO15765_PS => j2534_0404::ISO15765,
        // ADR-211 Decision item 2: recurse the extracted base back through
        // `base_protocol_id` itself rather than returning it raw. This is a
        // strict no-op for every one of the 13 families closed by ADR-206
        // through ADR-210 (each `_PS` id `chx_block_base` is keyed on is
        // already a self-referential fixed point of `base_protocol_id`, so
        // applying it twice changes nothing) but is REQUIRED for a
        // CAN-collapse family such as Fault-Tolerant CAN (`FT_CAN_PS`,
        // ADR-211): `chx_block_base` keys `FT_CAN_CH1..128` off
        // `PROTOCOL_FT_CAN_PS`, not the true base `CAN`, so without this
        // recursion `base_protocol_id(FT_CAN_CH3)` would incorrectly return
        // `PROTOCOL_FT_CAN_PS` instead of `CAN`.
        other => chx_base_protocol_id(other)
            .map(|(base, _index)| base_protocol_id(base))
            .unwrap_or(other),
    }
}

/// SAE J2534-2 clause 21 CAN FD (ADR-158/Phase 3a) and clause 22
/// ISO15765-on-CAN-FD (ADR-159/Phase 3b): the `_PS`-only FD protocol ids,
/// keyed by base hardware protocol id -- neither has an unqualified base id
/// of its own (Tables 89/96 list only `_PS`/`_CHx` variants, unlike every
/// family [`ps_protocol_id`] covers), so a D-PDU client never explicitly
/// requests either (`_PS` or `_CHx`, `names.rs`); the adapter infers FD mode
/// from staged Working ComParams at `ConnectComLogicalLink` time
/// (`rpc_link::J2534Service::apply_fd_mode`) and substitutes through this
/// function. `CAN -> Some(PROTOCOL_FD_CAN_PS)`, `ISO15765 ->
/// Some(PROTOCOL_FD_ISO15765_PS)`; `None` for every other base id. This
/// function stays `_PS`-only -- it is the family's qualifying key into
/// [`chx_block_base`], the same role every other collapse family's own
/// `_PS`-keyed function plays; [`fd_protocol_id_for_link`] composes it with
/// [`chx_protocol_id`] for the `_CHx` case (ADR-213).
pub(super) fn fd_protocol_id(base_hw_protocol_id: u32) -> Option<u32> {
    match base_hw_protocol_id {
        j2534_0404::CAN => Some(j2534_0404::PROTOCOL_FD_CAN_PS),
        j2534_0404::ISO15765 => Some(j2534_0404::PROTOCOL_FD_ISO15765_PS),
        _ => None,
    }
}

/// ADR-213: composes [`fd_protocol_id`] with [`chx_protocol_id`] when a link
/// carries a `_CHx` Additional-Channel index, so `apply_fd_mode` has one call
/// for both the `_PS` and `_CHx` promotion cases. `None` under the same
/// conditions [`fd_protocol_id`] itself returns `None`.
pub(super) fn fd_protocol_id_for_link(
    base_hw_protocol_id: u32,
    channel_index: Option<u32>,
) -> Option<u32> {
    let ps_id = fd_protocol_id(base_hw_protocol_id)?;
    match channel_index {
        Some(index) => chx_protocol_id(ps_id, index),
        None => Some(ps_id),
    }
}

/// `true` for [`fd_protocol_id`]'s two in-scope `_PS` outputs
/// (`PROTOCOL_FD_CAN_PS`/`PROTOCOL_FD_ISO15765_PS`, ADR-158/ADR-159), or any
/// id in either family's own clause-7 `_CH1..128` Additional Channels range
/// (`PROTOCOL_FD_CAN_CH1..128`/`PROTOCOL_FD_ISO15765_CH1..128`, ADR-213/Round
/// 3) -- `false` for every other id, including a base id or a `_PS`/`_CHx` id
/// from a different family.
///
/// Unlike [`is_sw_protocol_id`]/[`is_ft_protocol_id`], which each deliberately
/// stay a strict `_PS`-only two-value check with a *separate* family-wide
/// `is_sw_family_protocol_id`/`is_ft_family_protocol_id` predicate added
/// alongside for `_CHx`-inclusive call sites, this predicate is **widened in
/// place** rather than split (ADR-213 Decision item 2): CAN FD has no
/// `names::resolve_pin_selection` arm of its own to keep the narrow form for
/// (unlike SW-CAN/FT-CAN), and every one of this predicate's call sites
/// (ComParam-support gating, RX/TX flag handling, `CoptUpdateparam`'s
/// ComParam-consistency guard, the connect-time data-phase-rate/pin-synthesis
/// snapshot) needs to treat an FD `_CHx` link exactly like an FD `_PS` link.
/// Widening in place, rather than adding a second `is_fd_family_protocol_id`,
/// avoids a class of silent-miss bug this series has hit before (a call site
/// left keyed on the narrow form after a family gains `_CHx` support) at
/// every one of those call sites at once.
pub(super) fn is_fd_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_FD_CAN_PS
        || hw_protocol_id == j2534_0404::PROTOCOL_FD_ISO15765_PS
        || (j2534_0404::PROTOCOL_FD_CAN_CH1..=j2534_0404::PROTOCOL_FD_CAN_CH128)
            .contains(&hw_protocol_id)
        || (j2534_0404::PROTOCOL_FD_ISO15765_CH1..=j2534_0404::PROTOCOL_FD_ISO15765_CH128)
            .contains(&hw_protocol_id)
}

/// `true` for SAE J2534-2 clause 9's two Single Wire CAN (SWCAN/GMLAN,
/// ADR-164/Phase 4) `_PS` hardware protocol ids (`PROTOCOL_SW_CAN_PS`/
/// `PROTOCOL_SW_ISO15765_PS`) -- `false` for every other id, including a
/// base id or a `_PS`/`_CHx` id from a different family. Parallel to
/// [`is_fd_protocol_id`]: like clause 21/22 CAN FD, clause 9 defines no
/// unqualified base SWCAN id at all, so a resource-table row's own
/// `hw_protocol_override` stores one of these two ids directly (clause
/// 9.2.1's pin-unassigned-until-`SET_CONFIG` model) rather than a native
/// base id the way every other `hw_protocol_override` in this table does.
/// This predicate is how [`base_protocol_id`]'s new SW arms, the connect-time
/// FD-mode reject guard (`rpc_link::J2534Service::apply_fd_mode`), and every
/// other downstream Plane B call site recognize that shape.
pub(super) fn is_sw_protocol_id(hw_protocol_id: u32) -> bool {
    matches!(
        hw_protocol_id,
        j2534_0404::PROTOCOL_SW_CAN_PS | j2534_0404::PROTOCOL_SW_ISO15765_PS
    )
}

/// `true` for either of `PROTOCOL_SW_CAN_PS`/`PROTOCOL_SW_ISO15765_PS`, or
/// any id in either family's own clause-7 `_CH1..128` Additional Channels
/// range (`PROTOCOL_SW_CAN_CAN_CH1..128`/`PROTOCOL_SW_CAN_ISO15765_CH1..128`
/// -- note both block constant names are prefixed `SW_CAN_` regardless of
/// which `_PS` id they extend, a confirmed vendor-header naming
/// inconsistency unlike [`is_ft_family_protocol_id`]'s own two blocks;
/// ADR-212 Context item 2) -- `false` for every other id. Unlike
/// [`is_sw_protocol_id`], which deliberately stays a strict two-value check
/// for `names::resolve_pin_selection`'s own arm-gate purpose and the mock's
/// `pins_assigned` gate, this range-inclusive predicate exists for the
/// handful of call sites that must treat a `_CHx` link identically to its
/// `_PS` sibling for family-wide behavioral checks unrelated to pin
/// selection: [`connect_discovery_check`]'s Discovery-gating arms,
/// [`chx_device_info_supported_parameter`]'s guard arms,
/// [`bustype_default_name_for_hw_protocol_id`], `rpc_link.rs`'s
/// `apply_fd_mode` SW-vs-FD-mode rejection guards and its
/// `GetResourceStatus`/`GetConflictingResources` query-candidate-matching
/// sites, `comparam_id.rs`'s `CP_ChangeSpeed*` translation block, and
/// `events.rs`'s `SW_CAN_HS_RX`/`SW_CAN_NS_RX` RxStatus withholding gate
/// (ADR-212). Mirrors [`is_ft_family_protocol_id`]'s shape.
pub(super) fn is_sw_family_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_SW_CAN_PS
        || hw_protocol_id == j2534_0404::PROTOCOL_SW_ISO15765_PS
        || (j2534_0404::PROTOCOL_SW_CAN_CAN_CH1..=j2534_0404::PROTOCOL_SW_CAN_CAN_CH128)
            .contains(&hw_protocol_id)
        || (j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1..=j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH128)
            .contains(&hw_protocol_id)
}

/// `true` for SAE J2534-2 clause 20's two Fault-Tolerant CAN (ISO 11898-3,
/// ADR-168/Phase 6) `_PS` hardware protocol ids (`PROTOCOL_FT_CAN_PS`/
/// `PROTOCOL_FT_ISO15765_PS`) -- `false` for every other id, including a
/// base id or a `_PS`/`_CHx` id from a different family. Parallel to
/// [`is_sw_protocol_id`]: like clause 9 SWCAN, clause 20 defines no
/// unqualified base FTCAN id at all, so a resource-table row's own
/// `hw_protocol_override` stores one of these two ids directly (clause
/// 20.2.1's pin-unassigned-until-`SET_CONFIG` model) rather than a native
/// base id the way every other `hw_protocol_override` in this table does.
/// This predicate is how [`base_protocol_id`]'s new FT arms, and every other
/// downstream Plane B call site, recognize that shape.
pub(super) fn is_ft_protocol_id(hw_protocol_id: u32) -> bool {
    matches!(
        hw_protocol_id,
        j2534_0404::PROTOCOL_FT_CAN_PS | j2534_0404::PROTOCOL_FT_ISO15765_PS
    )
}

/// `true` for either of `PROTOCOL_FT_CAN_PS`/`PROTOCOL_FT_ISO15765_PS`, or
/// any id in either family's own clause-7 `_CH1..128` Additional Channels
/// range (`PROTOCOL_FT_CAN_CH1..128`/`PROTOCOL_FT_ISO15765_CH1..128` --
/// vendor header names this family's two `_CHx` blocks consistently with
/// their own `_PS` ids, confirmed against `j2534_v0404.h`, no naming
/// inconsistency here) -- `false` for every other id. Unlike
/// [`is_ft_protocol_id`], which deliberately stays a strict two-value check
/// for `names::resolve_pin_selection`'s own arm-gate purpose and the mock's
/// `pins_assigned` gate, this range-inclusive predicate exists for the
/// handful of call sites that must treat a `_CHx` link identically to its
/// `_PS` sibling for family-wide behavioral checks unrelated to pin
/// selection: [`connect_discovery_check`]'s Discovery-gating arms,
/// [`bustype_default_name_for_hw_protocol_id`], `rpc_link.rs`'s
/// `apply_fd_mode` FT-vs-FD-mode rejection guards, and its
/// `GetResourceStatus`/`GetConflictingResources` query-candidate-matching
/// sites (ADR-211). Mirrors [`is_j1708_family_protocol_id`]'s shape, doubled
/// for this family's two `_PS`/block pairs instead of one (similar in shape
/// to [`is_tp2_0_family_protocol_id`], also doubled).
pub(super) fn is_ft_family_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_FT_CAN_PS
        || hw_protocol_id == j2534_0404::PROTOCOL_FT_ISO15765_PS
        || (j2534_0404::PROTOCOL_FT_CAN_CH1..=j2534_0404::PROTOCOL_FT_CAN_CH128)
            .contains(&hw_protocol_id)
        || (j2534_0404::PROTOCOL_FT_ISO15765_CH1..=j2534_0404::PROTOCOL_FT_ISO15765_CH128)
            .contains(&hw_protocol_id)
}

/// `true` for SAE J2534-2 clause 12's one UART Echo Byte Protocol
/// (ADR-170/Phase 9) `_PS` hardware protocol id (`PROTOCOL_UART_ECHO_BYTE_PS`)
/// -- `false` for every other id. Unlike [`is_sw_protocol_id`]/
/// [`is_ft_protocol_id`], this is not "the CAN/ISO15765-family analog" --
/// clause 12 defines no CAN-family relationship at all, so there is no
/// `base_protocol_id` arm collapsing this id onto a different family (its
/// own `ChannelProtocol::UART_ECHO_BYTE_PS` already self-identifies via
/// `j2534_protocol_id()`, so `base_protocol_id` already returns this id
/// unchanged via its identity fallback -- no new arm needed there). This
/// predicate exists purely for `names::resolve_pin_selection`'s own
/// dedicated arm, mirroring the SW/FT arms' shape: like SW/FT, clause 12.2.2
/// identifies no default pin at all when this id is named directly outside
/// the resource table (row 0x023A's own pin 7 is a resolution convenience,
/// not a spec default -- ADR-170 Decision 2), so a directly-named connect
/// with no `dlc_pin_data` and no matching row must still be rejected the
/// same way SW/FT's own raw-id-without-pins case is. This deliberately stays
/// a single-`_PS`-value check even though UART Echo Byte is now one of the
/// eighteen in-scope `_CHx` families (ADR-207) -- callers that must treat a
/// `_CHx` link the same as its `_PS` sibling (Discovery gating, bustype-name
/// fallback, Repeat Messaging exclusion) use
/// [`is_uart_echo_byte_family_protocol_id`] instead; this narrower predicate
/// is for `names.rs`'s own arm-gate purpose only, where widening it would be
/// wrong (see that function's own doc comment).
pub(super) fn is_uart_echo_byte_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
}

/// `true` for `PROTOCOL_UART_ECHO_BYTE_PS` itself, or any id in the
/// clause-7 `_CH1..128` Additional Channels range (`PROTOCOL_ECHO_BYTE_CH1`
/// through `PROTOCOL_ECHO_BYTE_CH128` -- note the vendor header drops the
/// `UART_` prefix for the `_CHx` block, a naming inconsistency confirmed
/// against `j2534_v0404.h` and re-exported as-is; see `j2534-0404/src/lib.rs`)
/// -- `false` for every other id. Unlike [`is_uart_echo_byte_protocol_id`],
/// which deliberately stays a single-`_PS`-value check for
/// `names::resolve_pin_selection`'s own arm-gate purpose, this range-inclusive
/// predicate exists for the handful of call sites that must treat a `_CHx`
/// link identically to its `_PS` sibling for family-wide behavioral checks
/// unrelated to pin selection: [`connect_discovery_check`]'s Stage-1
/// Discovery fail-fast gate, [`bustype_default_name_for_hw_protocol_id`]'s
/// fallback naming, and `rpc_misc.rs`'s clause-12.3.3.1 Repeat Messaging
/// rejection (both the start and the query/stop sibling). Mirrors
/// [`is_gm_uart_protocol_id`]'s exact shape. Added in ADR-207 Decision item
/// 10 after an `edge-case-hunter` review found `connect_discovery_check`'s
/// bare exact-match arm let a UART Echo Byte `_CHx` connect silently skip
/// the Discovery-based fail-fast rejection its `_PS` sibling still enforces,
/// and a follow-up sweep found three more raw-`hw_protocol_id` call sites
/// with the same gap.
pub(super) fn is_uart_echo_byte_family_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        || (j2534_0404::PROTOCOL_ECHO_BYTE_CH1..=j2534_0404::PROTOCOL_ECHO_BYTE_CH128)
            .contains(&hw_protocol_id)
}

/// `true` for SAE J2534-2 clause 13's one Honda DIAG-H Protocol
/// (ADR-174/Phase 10) `_PS` hardware protocol id (`PROTOCOL_HONDA_DIAGH_PS`)
/// -- `false` for every other id. Parallel to
/// [`is_uart_echo_byte_protocol_id`]: clause 13 defines no CAN-family
/// relationship at all, so there is no `base_protocol_id` arm collapsing this
/// id onto a different family (its own `ChannelProtocol::HONDA_DIAGH_PS`
/// already self-identifies via `j2534_protocol_id()`, so `base_protocol_id`
/// already returns this id unchanged via its identity fallback -- no new arm
/// needed there). This predicate exists purely for
/// `names::resolve_pin_selection`'s own dedicated arm: clause 13.2.3
/// identifies no default pin at all when this id is named directly outside
/// the resource table (row 0x023B's own pin 14 is a resolution convenience,
/// not a spec default -- ADR-174 Decision 2), so a directly-named connect
/// with no `dlc_pin_data` and no matching row must still be rejected the
/// same way UART Echo Byte's own raw-id-without-pins case is. This
/// deliberately stays a single-`_PS`-value check even though Honda DIAG-H is
/// now one of the eighteen in-scope `_CHx` families (ADR-208) -- callers that
/// must treat a `_CHx` link the same as its `_PS` sibling (Discovery gating,
/// bustype-name fallback, the `comparam_id.rs` P1/P3/P4 native-timing
/// translation) use [`is_honda_diagh_family_protocol_id`] instead; this
/// narrower predicate is for `names.rs`'s own arm-gate purpose only, and for
/// `peer_closed_set_extra_pins`/`names.rs`'s own
/// `row_needs_dynamic_pin_selection` (both resource-table-row-keyed, never a
/// live `_CHx` id), where widening it would be wrong.
pub(super) fn is_honda_diagh_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_HONDA_DIAGH_PS
}

/// `true` for `PROTOCOL_HONDA_DIAGH_PS` itself, or any id in the clause-7
/// `_CH1..128` Additional Channels range (`PROTOCOL_HONDA_DIAGH_CH1` through
/// `PROTOCOL_HONDA_DIAGH_CH128` -- unlike UART Echo Byte's own `_CHx` block,
/// the vendor header names this family's `_CHx` block consistently with its
/// `_PS` id, confirmed against `j2534_v0404.h`) -- `false` for every other
/// id. Unlike [`is_honda_diagh_protocol_id`], which deliberately stays a
/// single-`_PS`-value check for `names::resolve_pin_selection`'s own
/// arm-gate purpose (and for `peer_closed_set_extra_pins`/`names.rs`'s own
/// `row_needs_dynamic_pin_selection`, both resource-table-row-keyed), this
/// range-inclusive predicate exists for the handful of call sites that must
/// treat a `_CHx` link identically to its `_PS` sibling for family-wide
/// behavioral checks unrelated to pin selection:
/// [`connect_discovery_check`]'s Stage-1 Discovery fail-fast gate,
/// [`bustype_default_name_for_hw_protocol_id`]'s fallback naming, and
/// `comparam_id.rs`'s `CP_P1Max`/`CP_P3Min`/`CP_P4Min` native-timing
/// translation block (three call sites, ADR-208). Mirrors
/// [`is_gm_uart_protocol_id`]'s/[`is_uart_echo_byte_family_protocol_id`]'s
/// exact shape.
pub(super) fn is_honda_diagh_family_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_HONDA_DIAGH_PS
        || (j2534_0404::PROTOCOL_HONDA_DIAGH_CH1..=j2534_0404::PROTOCOL_HONDA_DIAGH_CH128)
            .contains(&hw_protocol_id)
}

/// `true` for SAE J2534-2 clause 17's one SAE J1708 Protocol (ADR-175/Phase
/// 11) `_PS` hardware protocol id (`PROTOCOL_J1708_PS`) -- `false` for every
/// other id. Parallel to [`is_uart_echo_byte_protocol_id`]/
/// [`is_honda_diagh_protocol_id`]: clause 17 defines no CAN-family
/// relationship at all, so there is no `base_protocol_id` arm collapsing this
/// id onto a different family (its own `ChannelProtocol::J1708_PS` already
/// self-identifies via `j2534_protocol_id()`, so `base_protocol_id` already
/// returns this id unchanged via its identity fallback -- no new arm needed
/// there). This predicate exists purely for `names::resolve_pin_selection`'s
/// own dedicated arm: clause 17.3.2.1 identifies no default pin at all when
/// this id is named directly outside the resource table (row 0x023C's own
/// pins 3/11 are a resolution convenience, not a spec default -- ADR-175
/// Decision 2), so a directly-named connect with no `dlc_pin_data` and no
/// matching row must still be rejected the same way UART Echo Byte's/Honda
/// DIAG-H's own raw-id-without-pins case is. This deliberately stays a
/// single-`_PS`-value check even though SAE J1708 is now one of the fifteen
/// in-scope `_CHx` families (ADR-209) -- callers that must treat a `_CHx`
/// link the same as its `_PS` sibling (Discovery gating, bustype-name
/// fallback, `rpc_primitive.rs`'s/`rpc_misc.rs`'s own message-priority
/// TX-flags gates) use [`is_j1708_family_protocol_id`] instead; this
/// narrower predicate is for `names.rs`'s own arm-gate purpose only, and for
/// `names.rs`'s own `row_needs_dynamic_pin_selection` (resource-table-row-
/// keyed, never a live `_CHx` id), where widening it would be wrong.
pub(super) fn is_j1708_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_J1708_PS
}

/// `true` for `PROTOCOL_J1708_PS` itself, or any id in the clause-7
/// `_CH1..128` Additional Channels range (`PROTOCOL_J1708_CH1` through
/// `PROTOCOL_J1708_CH128` -- like Honda DIAG-H's own `_CHx` block, the vendor
/// header names this family's `_CHx` block consistently with its `_PS` id,
/// confirmed against `j2534_v0404.h`) -- `false` for every other id. Unlike
/// [`is_j1708_protocol_id`], which deliberately stays a single-`_PS`-value
/// check for `names::resolve_pin_selection`'s own arm-gate purpose (and for
/// `names.rs`'s own `row_needs_dynamic_pin_selection`,
/// resource-table-row-keyed), this range-inclusive predicate exists for the
/// handful of call sites that must treat a `_CHx` link identically to its
/// `_PS` sibling for family-wide behavioral checks unrelated to pin
/// selection: [`connect_discovery_check`]'s Discovery-gating arm,
/// [`bustype_default_name_for_hw_protocol_id`], `rpc_primitive.rs`'s
/// `apply_resolved_tx_flags` (its `msg_priority_tx_flags()` OR gate), and
/// `rpc_misc.rs`'s `ioctl_start_repeat_message` TxFlags composition's own
/// local mirror of that gate (four call sites, ADR-209). Mirrors
/// [`is_gm_uart_protocol_id`]'s/[`is_honda_diagh_family_protocol_id`]'s exact
/// shape.
pub(super) fn is_j1708_family_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_J1708_PS
        || (j2534_0404::PROTOCOL_J1708_CH1..=j2534_0404::PROTOCOL_J1708_CH128)
            .contains(&hw_protocol_id)
}

/// `true` for SAE J2534-2 clause 16's SAE J1939 Protocol (ADR-179/Phase 5)
/// hardware protocol ids: `PROTOCOL_J1939_PS` itself, or any id in the
/// clause-16 `_CH1..128` Additional Channels range (`PROTOCOL_J1939_CH1`
/// through `PROTOCOL_J1939_CH128`) -- `false` for every other id. Unlike
/// [`is_j1708_protocol_id`] (a single-value check even though J1708 IS now
/// also in scope for Additional Channels, ADR-209 -- see
/// [`is_j1708_family_protocol_id`] for that family-wide range instead), this
/// predicate itself also recognizes the `_CHx` range directly. J1939 IS now
/// one of the eighteen in-scope `_CHx` families [`chx_base_protocol_id`]
/// covers (`J1939_CHx` Additional Channels, ADR-206) -- but this function's
/// own range check predates that wiring and was deliberately written to
/// recognize the `_CH1..128` range up front (its doc comment, prior to
/// ADR-206, explained this as anticipating a future direct/legacy creation
/// reaching a `_CHx` id before `chx_base_protocol_id` resolved one), so
/// callers needing this protocol's own ComParam-allowlist/translation
/// gating (`comparam_support::is_j1939_param`,
/// `comparam_id::to_j2534_config_id`) already recognize a `_CHx` id
/// correctly, with no change needed here for ADR-206. UART Echo Byte, Honda
/// DIAG-H, SAE J1708, and TP2.0 are also now among the eighteen in-scope
/// `_CHx` families (`ECHO_BYTE_CHx`/`HONDA_DIAGH_CHx`/`J1708_CHx`/`TP2_0_CHx`
/// Additional Channels, ADR-207/ADR-208/ADR-209/ADR-210), and
/// [`is_uart_echo_byte_protocol_id`]/[`is_honda_diagh_protocol_id`]/
/// [`is_j1708_protocol_id`]/[`is_tp2_0_protocol_id`] each deliberately stay a
/// single-value check unlike this predicate -- none of the four protocols'
/// own ComParam-allowlist gating is equivalent to this function's own `_CHx`
/// recognition (Honda DIAG-H's `comparam_id.rs` P1/P3/P4 translation block
/// DOES need the same `_CHx`-aware treatment, but via a separate, dedicated
/// [`is_honda_diagh_family_protocol_id`] predicate instead of widening this
/// protocol's own single-value arm-gate check), so all four were left as the
/// plain exact-`_PS`-match shape they already had for `names.rs`'s arm-gate
/// purpose. Each DOES have its own family-wide callers, though (Discovery
/// gating, bustype-name fallback, and -- Honda DIAG-H, SAE J1708, and TP2.0
/// only -- their own message-priority/native-timing/broadcast TX-flags or
/// translation call sites; UART Echo Byte also has a Repeat Messaging
/// exclusion, TP2.0 has a conditional Repeat Messaging rejection of its own
/// shape, neither Honda DIAG-H nor SAE J1708 does) --
/// [`is_uart_echo_byte_family_protocol_id`]/[`is_honda_diagh_family_protocol_id`]/
/// [`is_j1708_family_protocol_id`]/[`is_tp2_0_family_protocol_id`] cover
/// those instead of widening any of the four protocols' own arm-gate
/// predicates.
/// Clause 16 defines no CAN-family
/// relationship at all, so there is no explicit `base_protocol_id` match arm
/// for either shape -- its own `ChannelProtocol::J1939_PS` already
/// self-identifies via `j2534_protocol_id()`, so `base_protocol_id` already
/// returns `PROTOCOL_J1939_PS` unchanged via its identity fallback for the
/// `_PS` id, and (since ADR-206 extended [`chx_base_protocol_id`] to
/// resolve `PROTOCOL_J1939_CH1..128`) `base_protocol_id`'s existing
/// `other => chx_base_protocol_id(other).map(...).unwrap_or(other)`
/// fallback now resolves a `_CHx` id to `PROTOCOL_J1939_PS` the same way,
/// still with no new arm needed there.
pub(super) fn is_j1939_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_J1939_PS
        || (j2534_0404::PROTOCOL_J1939_CH1..=j2534_0404::PROTOCOL_J1939_CH128)
            .contains(&hw_protocol_id)
}

/// `true` for SAE J2534-2 clause 19's one TP2.0 Protocol (ADR-188/Phase 7
/// Stage 7a) `_PS` hardware protocol id (`PROTOCOL_TP2_0_PS`) -- `false` for
/// every other id, including any `_CHx` id in the clause-7 Additional
/// Channels range. Unlike [`is_j1939_protocol_id`], this deliberately stays a
/// single-`_PS`-value check even though TP2.0 is now one of the fifteen
/// in-scope `_CHx` families (ADR-210) -- callers that must treat a `_CHx`
/// link the same as its `_PS` sibling (Discovery gating, bustype-name
/// fallback, `comparam_id.rs`'s/`rpc_primitive.rs`'s/`rpc_misc.rs`'s/
/// `rpc_link.rs`'s/`events.rs`'s/`events_rx_routing.rs`'s own TP2.0-specific
/// checks) use [`is_tp2_0_family_protocol_id`] instead; this narrower
/// predicate is for `names.rs`'s own arm-gate purpose only, and for
/// `names.rs`'s own `row_needs_dynamic_pin_selection` (resource-table-row-
/// keyed, never a live `_CHx` id), where widening it would be wrong.
/// No CAN-family relationship at all, so there is no `base_protocol_id` arm
/// collapsing this id onto a different family (its own
/// `ChannelProtocol::TP2_0_PS` already self-identifies via
/// `j2534_protocol_id()`, so `base_protocol_id` already returns this id
/// unchanged via its identity fallback -- no new arm needed there).
pub(super) fn is_tp2_0_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_TP2_0_PS
}

/// `true` for `PROTOCOL_TP2_0_PS` itself, or any id in the clause-7
/// `_CH1..128` Additional Channels range (`PROTOCOL_TP2_0_CH1` through
/// `PROTOCOL_TP2_0_CH128` -- like Honda DIAG-H's/SAE J1708's own `_CHx`
/// blocks, the vendor header names this family's `_CHx` block consistently
/// with its `_PS` id, confirmed against `j2534_v0404.h`) -- `false` for every
/// other id. Unlike [`is_tp2_0_protocol_id`], which deliberately stays a
/// single-`_PS`-value check for `names::resolve_pin_selection`'s own arm-gate
/// purpose (and for `names.rs`'s own `row_needs_dynamic_pin_selection`,
/// resource-table-row-keyed), this range-inclusive predicate exists for the
/// twenty call sites that must treat a `_CHx` link identically to its `_PS`
/// sibling for family-wide behavioral checks unrelated to pin selection
/// (ADR-210): [`connect_discovery_check`]'s Discovery-gating arm,
/// [`bustype_default_name_for_hw_protocol_id`], `comparam_id.rs`'s
/// `PARAM_TP20_BROADCAST_INTERVAL` translation block, `rpc_primitive.rs`'s
/// five sites (`apply_resolved_tx_flags`'s and `resolve_send_recv_tx`'s own
/// `tp20_is_broadcast` computations, and three
/// `validate_tp20_broadcast_address_range` call-site gates),
/// `rpc_misc.rs`'s conditional Repeat Messaging rejection, `rpc_link.rs`'s
/// four `DestroyComLogicalLink`/`DisconnectComLogicalLink`
/// teardown/disarm gates, `events.rs`'s six sites, and
/// `events_rx_routing.rs`'s `unique_resp_ids` construction. Mirrors
/// [`is_j1708_family_protocol_id`]'s exact shape.
pub(super) fn is_tp2_0_family_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_TP2_0_PS
        || (j2534_0404::PROTOCOL_TP2_0_CH1..=j2534_0404::PROTOCOL_TP2_0_CH128)
            .contains(&hw_protocol_id)
}

/// `true` for SAE J2534-2 clause 11's GM UART Protocol (SAE J2740,
/// ADR-189/Phase 8) hardware protocol ids: `PROTOCOL_GM_UART_PS` itself, or
/// any id in the clause-7 `_CH1..128` Additional Channels range
/// (`PROTOCOL_GM_UART_CH1` through `PROTOCOL_GM_UART_CH128`) -- `false` for
/// every other id. Mirrors [`is_j1939_protocol_id`]'s range-inclusive shape
/// (not [`is_j1708_protocol_id`]'s/[`is_tp2_0_protocol_id`]'s single-value
/// arm-gate shape -- both of those families also have their own separate
/// family-wide range predicate, [`is_j1708_family_protocol_id`]/
/// [`is_tp2_0_family_protocol_id`], for their own non-arm-gate call sites).
/// No CAN-family relationship at
/// all, so there is no `base_protocol_id` arm collapsing this id onto a
/// different family (its own `ChannelProtocol::GM_UART_PS` already
/// self-identifies via `j2534_protocol_id()`, so `base_protocol_id` already
/// returns `PROTOCOL_GM_UART_PS` unchanged via its identity fallback -- no
/// new arm needed there, mirroring `is_j1939_protocol_id`'s own doc comment
/// reasoning).
pub(super) fn is_gm_uart_protocol_id(hw_protocol_id: u32) -> bool {
    hw_protocol_id == j2534_0404::PROTOCOL_GM_UART_PS
        || (j2534_0404::PROTOCOL_GM_UART_CH1..=j2534_0404::PROTOCOL_GM_UART_CH128)
            .contains(&hw_protocol_id)
}

/// Maps a standalone hardware protocol id -- one reached via a no-
/// resource-table-row raw-id `CreateComLogicalLink` (a directly-named `_PS`
/// id with no matching row, `resource_row: None` in
/// `rpc_link::J2534Service::rpc_create_com_logical_link`) -- back to the
/// `comparam_defaults::bustype_default_params` name string a matching
/// resource-table row for the same protocol family would itself carry.
///
/// This is the general form of the Honda DIAG-H/SAE J1708 fallback arms
/// `rpc_create_com_logical_link` used to have (Honda DIAG-H: ADR-174, Codex
/// review PR #63 round 2; SAE J1708: ADR-175, an edge-case-hunter finding on
/// the Phase 11 PR), extended to the seven more protocol families that share
/// the identical gap (SWCAN, FT-CAN, UART Echo Byte, SAE J1939, TP2.0, GM
/// UART, and -- ADR-194/Phase 16 -- Ethernet_NDIS): a directly-named raw id
/// has no resource-table row to supply its own `bus_type_name`, so the
/// caller-supplied `RscData::bus_type_name` -- unvalidated against the
/// resolved protocol -- would otherwise decide the Working ComParamSet's
/// bustype defaults, silently losing the protocol's own mandatory baud rate
/// when that string is absent or names a different bus type entirely. `None`
/// for every id outside these nine families (including a base/generic
/// CAN-family id -- extending
/// this to the legacy base `_PS`/`_CHx` raw-id route is a real but separate
/// gap, out of scope here since `DATA_RATE` is genuinely client-configurable
/// on that route and callers historically supply `bus_type_name` for it).
pub(super) fn bustype_default_name_for_hw_protocol_id(hw_protocol_id: u32) -> Option<&'static str> {
    if is_sw_family_protocol_id(hw_protocol_id) {
        Some("SAE_J2411_SWCAN")
    } else if is_ft_family_protocol_id(hw_protocol_id) {
        Some("ISO_11898_3_DWFTCAN")
    } else if is_uart_echo_byte_family_protocol_id(hw_protocol_id) {
        Some("UART_ECHO_BYTE_UART")
    } else if is_honda_diagh_family_protocol_id(hw_protocol_id) {
        Some("HONDA_DIAGH_UART")
    } else if is_j1708_family_protocol_id(hw_protocol_id) {
        Some("SAE_J1708_UART")
    } else if is_j1939_protocol_id(hw_protocol_id) {
        Some("SAE_J1939_11_DWCAN")
    } else if is_tp2_0_family_protocol_id(hw_protocol_id) {
        Some("TP2_0_DWCAN")
    } else if is_gm_uart_protocol_id(hw_protocol_id) {
        Some("GM_UART_UART")
    } else if hw_protocol_id == j2534_0404::PROTOCOL_ETHERNET_NDIS {
        Some("IEEE_802_3")
    } else {
        None
    }
}

/// `true` for any of SAE J2534-2 clause 10's 32 native, independent,
/// read-only Analog Input protocol ids (`PROTOCOL_ANALOG_IN_1`..
/// `PROTOCOL_ANALOG_IN_32`, ADR-177/Phase 15) -- `false` for every other id,
/// including `ChannelProtocol::ANALOG_IN`'s own shared identity value
/// (0x0170, not a native id at all). The canonical protocol-identity check
/// used throughout the connect flow (`rpc_link.rs`'s `analog_sample_rate`
/// exclusivity/requirement) and the write/filter rejection sites
/// (`rpc_primitive.rs`/`rpc_misc.rs`) -- both a resource-table connect
/// (whose `hw_protocol_override` is always one of these 32 ids) and a
/// legacy direct creation naming a `PROTOCOL_ANALOG_IN_x` id raw (which
/// self-identifies via `ChannelProtocol::j2534_protocol_id()`'s catch-all
/// pass-through) resolve to a native id in this range, so callers pass the
/// resolved native `hw_protocol_id`, not `protocol.value()` directly.
pub(super) fn is_analog_in_protocol_id(hw_protocol_id: u32) -> bool {
    (j2534_0404::PROTOCOL_ANALOG_IN_1..=j2534_0404::PROTOCOL_ANALOG_IN_32).contains(&hw_protocol_id)
}

/// SAE J2534-2 clause 6's `0x0000PPSS` `CONFIG_J1962_PINS` packing (mirrors
/// `names::J2534Service::compute_pin_select`'s primary/secondary split,
/// duplicated here for the same "this table already owns the pin data"
/// reason [`default_dlc_pins_for_hw_protocol`] documents) of a hardware
/// protocol's own DEFAULT DLC pins. Needed by the CAN FD connect-time
/// substitution (ADR-158/Phase 3a): an FD_CAN_PS channel has no
/// "default, pins-preassigned" connect the way Classic CAN's own default
/// connect does (clause 21 has no base id at all, only `_PS`/`_CHx`
/// variants) -- clause 6.3.3.2's default DLC pins still have to be packed
/// and sent via `SET_CONFIG(CONFIG_J1962_PINS)` for an unqualified FD
/// connect, exactly as a genuine Pin Selection request would. `None` for a
/// hardware id with no default pins at all
/// ([`default_dlc_pins_for_hw_protocol`] returns `None`), or whose default
/// pins don't split into exactly one primary + one secondary pin (should be
/// unreachable for every hardware id this is actually called with in this
/// phase -- CAN's own default pins are exactly `PIN_HI`/`PIN_LOW`).
pub(super) fn default_pin_select_for_base(base_hw_protocol_id: u32) -> Option<u32> {
    let pins = default_dlc_pins_for_hw_protocol(base_hw_protocol_id)?;
    let mut primary = None;
    let mut secondary = None;
    for &(pin_number, pin_type) in pins {
        match pin_type {
            PIN_HI | PIN_K | PIN_TX | PIN_PLUS => primary = Some(pin_number),
            PIN_LOW | PIN_L | PIN_RX | PIN_MINUS => secondary = Some(pin_number),
            _ => {}
        }
    }
    match (primary, secondary) {
        (Some(pp), Some(ss)) => Some(((pp & 0xFF) << 8) | (ss & 0xFF)),
        _ => None,
    }
}

/// The forward arithmetic block base ("_CH1" hardware id) for each of SAE
/// J2534-2 clause 7's eighteen in-scope Additional Channels protocol
/// families (ADR-156 Decision 3/Phase 2b; extended to GM UART by ADR-189
/// Decision 2/Phase 8, to SAE J1939 by ADR-206, to UART Echo Byte by
/// ADR-207, to Honda DIAG-H by ADR-208, to SAE J1708 by ADR-209, to
/// TP2.0 by ADR-210, to Fault-Tolerant CAN by ADR-211, to Single Wire
/// CAN by ADR-212, and to CAN FD/ISO15765-on-CAN-FD by ADR-213) -- the same
/// eighteen [`ps_protocol_id`]-or-`_PS`-only
/// families [`base_protocol_id`] already covers. `None` for every other base
/// hardware id (the one remaining out-of-scope `_CHx` family, Analog Inputs
/// -- ADR-156 Consequences' "Accepted residual", confirmed structurally
/// out-of-scope rather than merely not-yet-covered by ADR-213's own Context).
/// All four native SAE J2610 SCI
/// hardware ids map onto the single `PROTOCOL_J2610_CH1` block, mirroring
/// [`ps_protocol_id`]'s own SCI collapse for `_PS`. Unlike the first seven
/// entries (keyed by a native, unqualified base hardware id), GM UART, SAE
/// J1939, UART Echo Byte, Honda DIAG-H, SAE J1708, TP2.0, Fault-Tolerant
/// CAN/Fault-Tolerant ISO15765, Single Wire CAN/Single Wire ISO15765, and CAN
/// FD/ISO15765-on-CAN-FD
/// are each keyed
/// by their own `_PS`-only id
/// (`PROTOCOL_GM_UART_PS`/`PROTOCOL_J1939_PS`/`PROTOCOL_UART_ECHO_BYTE_PS`/
/// `PROTOCOL_HONDA_DIAGH_PS`/`PROTOCOL_J1708_PS`/`PROTOCOL_TP2_0_PS`/
/// `PROTOCOL_FT_CAN_PS`/`PROTOCOL_FT_ISO15765_PS`/
/// `PROTOCOL_SW_CAN_PS`/`PROTOCOL_SW_ISO15765_PS`/
/// `PROTOCOL_FD_CAN_PS`/`PROTOCOL_FD_ISO15765_PS`) --
/// clause 11/clause 16/clause 12/clause 13/clause 17/clause 19 define no
/// unqualified base id at all for any of the first six (ADR-189 Decision 1,
/// ADR-179 Decision 1, ADR-170, ADR-174, ADR-175, ADR-188), so those `_PS`
/// ids ARE the "base" this table's other entries would otherwise use.
/// Fault-Tolerant CAN/ISO15765, Single Wire CAN/ISO15765, and CAN
/// FD/ISO15765-on-CAN-FD are different:
/// clause 20/clause 9/clause 21-22 DO define an
/// unqualified base (`CAN`/`ISO15765`, via [`base_protocol_id`]'s own FT/SW/
/// FD arms) -- they are keyed here by their `_PS` id anyway, matching every
/// closed family's own convention, precisely because [`base_protocol_id`]'s
/// recursive fallback (ADR-211 Decision item 2) makes that safe: without it,
/// keying a CAN-collapse family's block by its `_PS` id here would make
/// `base_protocol_id` of a `_CHx` id in that block resolve to the `_PS` id
/// instead of the true `CAN`/`ISO15765` base. Single Wire CAN also has its
/// own confirmed vendor-header naming quirk (ADR-212 Context item 2): both
/// of its `_CHx` block constant names are prefixed `SW_CAN_` regardless of
/// which `_PS` id they extend (`PROTOCOL_SW_CAN_CAN_CH1` for
/// `PROTOCOL_SW_CAN_PS`, `PROTOCOL_SW_CAN_ISO15765_CH1` for
/// `PROTOCOL_SW_ISO15765_PS`) -- purely a naming fact on the value side, the
/// key is still each `_PS` id as usual. CAN FD/ISO15765-on-CAN-FD has no such
/// quirk -- like Fault-Tolerant CAN, its `_CHx` block constant names are
/// consistent with their own `_PS` id (ADR-213 Decision item 2).
fn chx_block_base(base_hw_protocol_id: u32) -> Option<u32> {
    match base_hw_protocol_id {
        j2534_0404::J1850VPW => Some(j2534_0404::PROTOCOL_J1850VPW_CH1),
        j2534_0404::J1850PWM => Some(j2534_0404::PROTOCOL_J1850PWM_CH1),
        j2534_0404::ISO9141 => Some(j2534_0404::PROTOCOL_ISO9141_CH1),
        j2534_0404::ISO14230 => Some(j2534_0404::PROTOCOL_ISO14230_CH1),
        j2534_0404::CAN => Some(j2534_0404::PROTOCOL_CAN_CH1),
        j2534_0404::ISO15765 => Some(j2534_0404::PROTOCOL_ISO15765_CH1),
        j2534_0404::SCI_A_ENGINE
        | j2534_0404::SCI_A_TRANS
        | j2534_0404::SCI_B_ENGINE
        | j2534_0404::SCI_B_TRANS => Some(j2534_0404::PROTOCOL_J2610_CH1),
        j2534_0404::PROTOCOL_GM_UART_PS => Some(j2534_0404::PROTOCOL_GM_UART_CH1),
        j2534_0404::PROTOCOL_J1939_PS => Some(j2534_0404::PROTOCOL_J1939_CH1),
        j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS => Some(j2534_0404::PROTOCOL_ECHO_BYTE_CH1),
        j2534_0404::PROTOCOL_HONDA_DIAGH_PS => Some(j2534_0404::PROTOCOL_HONDA_DIAGH_CH1),
        j2534_0404::PROTOCOL_J1708_PS => Some(j2534_0404::PROTOCOL_J1708_CH1),
        j2534_0404::PROTOCOL_TP2_0_PS => Some(j2534_0404::PROTOCOL_TP2_0_CH1),
        // ADR-211/Phase 6: SAE J2534-2 clause 20 Fault-Tolerant CAN's two
        // `_PS`-only hardware ids. See this function's own doc comment for
        // why keying by `_PS` here (rather than the true `CAN`/`ISO15765`
        // base these two families DO have, unlike the six entries just
        // above) is safe.
        j2534_0404::PROTOCOL_FT_CAN_PS => Some(j2534_0404::PROTOCOL_FT_CAN_CH1),
        j2534_0404::PROTOCOL_FT_ISO15765_PS => Some(j2534_0404::PROTOCOL_FT_ISO15765_CH1),
        // ADR-212/Round 2: SAE J2534-2 clause 9 Single Wire CAN's two
        // `_PS`-only hardware ids. See this function's own doc comment for
        // why keying by `_PS` here is safe, and for the vendor-header
        // naming quirk on the value side (both block constants are
        // prefixed `SW_CAN_` regardless of which `_PS` id they extend).
        j2534_0404::PROTOCOL_SW_CAN_PS => Some(j2534_0404::PROTOCOL_SW_CAN_CAN_CH1),
        j2534_0404::PROTOCOL_SW_ISO15765_PS => Some(j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1),
        // ADR-213/Round 3: SAE J2534-2 clause 21 CAN FD / clause 22
        // ISO15765-on-CAN-FD's two `_PS`-only hardware ids -- the third and
        // final CAN-collapse family in this series. See this function's own
        // doc comment for why keying by `_PS` here (rather than the true
        // `CAN`/`ISO15765` base this family also has) is safe.
        j2534_0404::PROTOCOL_FD_CAN_PS => Some(j2534_0404::PROTOCOL_FD_CAN_CH1),
        j2534_0404::PROTOCOL_FD_ISO15765_PS => Some(j2534_0404::PROTOCOL_FD_ISO15765_CH1),
        _ => None,
    }
}

/// SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 3/Phase 2b)
/// forward mapping: `chx_base(base_hw_protocol_id) + channel_index - 1`, for
/// all eighteen in-scope protocol families [`chx_block_base`] covers.
/// `None` when `channel_index` is outside `1..=128`, or when
/// `base_hw_protocol_id` has no in-scope `_CHx` block (including a `_PS` or
/// already-`_CHx` id -- this function's input is always a *base* hardware
/// id, mirroring [`ps_protocol_id`]'s own contract).
pub(super) fn chx_protocol_id(base_hw_protocol_id: u32, channel_index: u32) -> Option<u32> {
    if !(1..=128).contains(&channel_index) {
        return None;
    }
    chx_block_base(base_hw_protocol_id).map(|block_base| block_base + channel_index - 1)
}

/// Exact inverse of [`chx_protocol_id`] (ADR-156 Decision 3/Phase 2b): a
/// range-membership test, not an enumerable match like [`ps_protocol_id`]'s
/// -- each of the eighteen in-scope families occupies a contiguous 128-value
/// block (nineteen total blocks, ADR-213 Consequences), so membership is
/// decided by range rather than by a per-value
/// lookup table. Returns `Some((base_hw_protocol_id, channel_index))` when
/// `hw_protocol_id` falls inside one of those nineteen blocks
/// (`channel_index` in `1..=128`), `None` otherwise. As of ADR-213 every
/// block in SAE J2534-2 clause 7's full `_CHx` vocabulary region
/// (`PROTOCOL_CAN_CH1..PROTOCOL_FD_ISO15765_CH128`, [`is_chx_protocol_id`])
/// is now in-scope here -- there is no longer a remaining out-of-scope
/// `_CHx` block for a caller to route to a clean rejection; Analog Inputs
/// (clause 10, ADR-156 Consequences' "Accepted residual") is structurally
/// out of `_CHx` scope entirely and its native ids sit outside this region
/// to begin with, so it was never one of these blocks. Mirrors
/// [`base_protocol_id`]'s single-representative SCI collapse: any id in the
/// `PROTOCOL_J2610_CH1..PROTOCOL_J2610_CH128` block resolves to the
/// `SCI_A_ENGINE` representative base id, never the exact SCI variant (no
/// `_CHx` id encodes which SCI wiring it means -- clause 7's vendor-connector
/// mapping is opaque, unlike `_PS`'s DLC-pin-derived variant). GM UART's, SAE
/// J1939's, UART Echo Byte's, Honda DIAG-H's, SAE J1708's, TP2.0's,
/// Fault-Tolerant CAN's/Fault-Tolerant ISO15765's, Single Wire
/// CAN's/Single Wire ISO15765's, and CAN FD's/ISO15765-on-CAN-FD's own
/// entries (ADR-189/Phase 8, ADR-206, ADR-207, ADR-208, ADR-209, ADR-210,
/// ADR-211, ADR-212, ADR-213) resolve to
/// `PROTOCOL_GM_UART_PS`/`PROTOCOL_J1939_PS`/`PROTOCOL_UART_ECHO_BYTE_PS`/
/// `PROTOCOL_HONDA_DIAGH_PS`/`PROTOCOL_J1708_PS`/`PROTOCOL_TP2_0_PS`/
/// `PROTOCOL_FT_CAN_PS`/`PROTOCOL_FT_ISO15765_PS`/
/// `PROTOCOL_SW_CAN_PS`/`PROTOCOL_SW_ISO15765_PS`/
/// `PROTOCOL_FD_CAN_PS`/`PROTOCOL_FD_ISO15765_PS`
/// respectively -- each its own `_PS`-only id, not the true native
/// `CAN`/`ISO15765` base Fault-Tolerant CAN/ISO15765, Single Wire
/// CAN/ISO15765, and CAN FD/ISO15765-on-CAN-FD actually have (see
/// [`chx_block_base`]'s own doc comment for why this is safe: every caller
/// of this function that wants the true base composes through
/// [`base_protocol_id`], which recurses once more, ADR-211 Decision item 2).
pub(super) fn chx_base_protocol_id(hw_protocol_id: u32) -> Option<(u32, u32)> {
    const BLOCKS: [(u32, u32); 19] = [
        (j2534_0404::PROTOCOL_J1850VPW_CH1, j2534_0404::J1850VPW),
        (j2534_0404::PROTOCOL_J1850PWM_CH1, j2534_0404::J1850PWM),
        (j2534_0404::PROTOCOL_ISO9141_CH1, j2534_0404::ISO9141),
        (j2534_0404::PROTOCOL_ISO14230_CH1, j2534_0404::ISO14230),
        (j2534_0404::PROTOCOL_CAN_CH1, j2534_0404::CAN),
        (j2534_0404::PROTOCOL_ISO15765_CH1, j2534_0404::ISO15765),
        (j2534_0404::PROTOCOL_J2610_CH1, j2534_0404::SCI_A_ENGINE),
        (
            j2534_0404::PROTOCOL_GM_UART_CH1,
            j2534_0404::PROTOCOL_GM_UART_PS,
        ),
        (
            j2534_0404::PROTOCOL_J1939_CH1,
            j2534_0404::PROTOCOL_J1939_PS,
        ),
        (
            j2534_0404::PROTOCOL_ECHO_BYTE_CH1,
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
        ),
        (
            j2534_0404::PROTOCOL_HONDA_DIAGH_CH1,
            j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
        ),
        (
            j2534_0404::PROTOCOL_J1708_CH1,
            j2534_0404::PROTOCOL_J1708_PS,
        ),
        (
            j2534_0404::PROTOCOL_TP2_0_CH1,
            j2534_0404::PROTOCOL_TP2_0_PS,
        ),
        (
            j2534_0404::PROTOCOL_FT_CAN_CH1,
            j2534_0404::PROTOCOL_FT_CAN_PS,
        ),
        (
            j2534_0404::PROTOCOL_FT_ISO15765_CH1,
            j2534_0404::PROTOCOL_FT_ISO15765_PS,
        ),
        // ADR-212/Round 2: Single Wire CAN. Block constant names are both
        // prefixed `SW_CAN_` (see this function's own doc comment and
        // `chx_block_base`'s), but the key on the value side is still each
        // `_PS` id as usual.
        (
            j2534_0404::PROTOCOL_SW_CAN_CAN_CH1,
            j2534_0404::PROTOCOL_SW_CAN_PS,
        ),
        (
            j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1,
            j2534_0404::PROTOCOL_SW_ISO15765_PS,
        ),
        // ADR-213/Round 3: CAN FD / ISO15765-on-CAN-FD, the third and final
        // CAN-collapse family in this series. No naming quirk on the value
        // side here (unlike Single Wire CAN just above) -- the vendor header
        // names this family's two `_CHx` blocks consistently with their own
        // `_PS` ids.
        (
            j2534_0404::PROTOCOL_FD_CAN_CH1,
            j2534_0404::PROTOCOL_FD_CAN_PS,
        ),
        (
            j2534_0404::PROTOCOL_FD_ISO15765_CH1,
            j2534_0404::PROTOCOL_FD_ISO15765_PS,
        ),
    ];
    for &(block_base, base) in &BLOCKS {
        if hw_protocol_id >= block_base && hw_protocol_id < block_base + 128 {
            return Some((base, hw_protocol_id - block_base + 1));
        }
    }
    None
}

/// SAE J2534-2 clause 8 (Mixed Format Frames on a CAN Network, ADR-160/Phase
/// 3c): the raw-CAN-family protocol id paired with an ISO15765-family hardware
/// protocol id, per clause 8.1's association rule (`CAN`↔`ISO15765`,
/// `CAN_PS`↔`ISO15765_PS`, `CAN_CHx`↔`ISO15765_CHx`) -- the id a `PASS_FILTER`/
/// `BLOCK_FILTER` on a `CAN_MIXED_FORMAT`-enabled channel must use (clause
/// 8.2.2.4), the OPPOSITE of the literal-connect-id rule every other filter
/// type follows. `hw_protocol_id` must already satisfy
/// `base_protocol_id(hw_protocol_id) == j2534_0404::ISO15765` and
/// `!is_fd_protocol_id(hw_protocol_id)` (native-mixed's own connect-time gate)
/// -- undefined qualifier families (e.g. `SW_ISO15765_PS`) fall back to plain
/// `CAN`, which should be unreachable given that gate.
pub(super) fn mixed_format_can_protocol_id(hw_protocol_id: u32) -> u32 {
    match hw_protocol_id {
        j2534_0404::PROTOCOL_ISO15765_PS => j2534_0404::PROTOCOL_CAN_PS,
        other => match chx_base_protocol_id(other) {
            Some((j2534_0404::ISO15765, index)) => {
                chx_protocol_id(j2534_0404::CAN, index).unwrap_or(j2534_0404::CAN)
            }
            _ => j2534_0404::CAN,
        },
    }
}

/// `true` when `hw_protocol_id` falls anywhere in SAE J2534-2 clause 7's
/// full `_CHx` vocabulary region (clause 24/26, `PROTOCOL_CAN_CH1` through
/// `PROTOCOL_FD_ISO15765_CH128`) -- all 18 protocol families (19 total native
/// `_CHx` blocks, three of the eighteen families each splitting into two
/// blocks -- FT-CAN, SW-CAN, and CAN FD, ADR-213 Consequences), not just the
/// eighteen [`chx_base_protocol_id`] resolves. Needed for the clause-5 J2534-2 opt-in
/// gate (ADR-156 Decision 4): a caller naming ANY `_CHx`-vocabulary id --
/// even one from an unimplemented family -- must hit that gate before
/// resolution, then get a clean rejection for an out-of-scope family rather
/// than silently falling through as an unrecognized raw id to the native
/// layer.
pub(super) fn is_chx_protocol_id(hw_protocol_id: u32) -> bool {
    (j2534_0404::PROTOCOL_CAN_CH1..=j2534_0404::PROTOCOL_FD_ISO15765_CH128)
        .contains(&hw_protocol_id)
}

/// Returns `true` when `hw_protocol_id` is one of the four native SAE J2610
/// SCI hardware ids [`ps_protocol_id`] collapses onto the single
/// `PROTOCOL_J2610_PS` id (ADR-156 Decision 2's Table 1 consolidation note).
/// Needed alongside [`base_protocol_id`]'s single-representative collapse
/// wherever a caller queries the raw, unqualified `PROTOCOL_J2610_PS` id
/// itself and must match a connected link using ANY of the four variants,
/// not just the one `base_protocol_id` happens to pick (`GetResourceStatus`'s
/// `ResourceId` route, Codex review, PR #28).
pub(super) fn is_sci_hw_protocol_id(hw_protocol_id: u32) -> bool {
    matches!(
        hw_protocol_id,
        j2534_0404::SCI_A_ENGINE
            | j2534_0404::SCI_A_TRANS
            | j2534_0404::SCI_B_ENGINE
            | j2534_0404::SCI_B_TRANS
    )
}

/// Returns `true` when `hw_protocol_id` is one of the seven raw SAE J2534-2
/// clause 6 Pin Selection (`_PS`) hardware protocol ids -- a direct, finite
/// match over the same ids [`ps_protocol_id`] produces, not an inference from
/// [`base_protocol_id`]'s normalization outcome (edge-case-hunter, PR #29,
/// post-approval pass). `resolve_pin_selection`'s `already_ps` used to derive
/// this as "normalization changed the id, and it isn't `_CHx`"
/// (`base_protocol_id(hw) != hw && !is_chx_protocol_id(hw)`), which already
/// needed that `_CHx` exclusion term once, when the normalization funnel's
/// domain grew in this same PR to also cover `_CHx` -- a third id family
/// added to the funnel in the future would silently miscompute it again.
/// This predicate is what any consumer needing "was clause-6 `_PS`
/// vocabulary used" should call instead: `resolve_pin_selection`'s
/// canonicalized `Ok(None)` outcome deliberately erases that signal (the
/// whole point of unifying `ChannelKey`/lock scope with an ordinary base
/// connect, PR #28's `9db653f`), so it can never be recovered from the
/// resolution *outcome* -- only from the raw id itself.
pub(super) fn is_ps_protocol_id(hw_protocol_id: u32) -> bool {
    matches!(
        hw_protocol_id,
        j2534_0404::PROTOCOL_J1850VPW_PS
            | j2534_0404::PROTOCOL_J1850PWM_PS
            | j2534_0404::PROTOCOL_ISO9141_PS
            | j2534_0404::PROTOCOL_ISO14230_PS
            | j2534_0404::PROTOCOL_CAN_PS
            | j2534_0404::PROTOCOL_ISO15765_PS
            | j2534_0404::PROTOCOL_J2610_PS
    )
}

/// SAE J2534-2 clause 7 (Additional Channels, ADR-156 Decision 4/Phase 2b
/// design review correction) capacity discovery: the `DEVICE_INFO_<PROTOCOL>
/// _SUPPORTED` parameter (clause 25.3.2.2 Table 111) whose packed value's
/// `QQ` byte (bits 16-23, Codex review PR #29 -- not bits 8-15, which is
/// `RR`, the unrelated `_PS` channel count) carries the count of available
/// `_CHx` ids for `j2534_proto_id`'s family, for seventeen of the
/// eighteen in-scope families [`chx_protocol_id`] covers (ADR-211
/// correction: was "six of the eight" before this function's own family
/// count and [`chx_protocol_id`]'s both grew well past that stale figure
/// across several later phases -- the seven ADR-156-original families
/// plus, as of ADR-211, Fault-Tolerant CAN's two flags; as of ADR-212,
/// Single Wire CAN's two flags; as of ADR-213, CAN FD/ISO15765-on-CAN-FD's
/// two flags; and, as a mechanical extension of that same
/// established pattern, SAE J1939, UART Echo Byte, Honda DIAG-H, SAE J1708,
/// and TP2.0). `None` for every other id --
/// GM UART remains a deliberately accepted residual (ADR-189 Decision 5).
/// All four native SAE J2610 SCI hardware ids share the single consolidated
/// `DEVICE_INFO_J2610_SUPPORTED` parameter, mirroring every other `_CHx`/
/// `_PS` SCI collapse in this module.
///
/// Two-tier contract (ADR-211, Codex review correction, PR #124): checked
/// directly against the raw, pre-normalization `j2534_proto_id`, NOT
/// `base_protocol_id(j2534_proto_id)` -- the FT-CAN, Single Wire CAN, and CAN
/// FD guard arms below
/// mirror [`connect_discovery_check`]'s own FT-CAN/SW-CAN/CAN-FD arms and exist for
/// the
/// same reason its doc comment already explains (all three are
/// CAN-collapse families; a base-keyed lookup would consult
/// `DEVICE_INFO_CAN_SUPPORTED`/`DEVICE_INFO_ISO15765_SUPPORTED` instead of
/// the family-specific flag that actually governs its `_CHx` count). Every
/// other
/// family is normalized through [`base_protocol_id`] first, so a raw `_CHx`
/// id from one of those families (e.g. `PROTOCOL_CAN_CH1`) still resolves to
/// its own base correctly.
pub(super) fn chx_device_info_supported_parameter(j2534_proto_id: u32) -> Option<u32> {
    match j2534_proto_id {
        id if id == j2534_0404::PROTOCOL_FT_CAN_PS
            || (j2534_0404::PROTOCOL_FT_CAN_CH1..=j2534_0404::PROTOCOL_FT_CAN_CH128)
                .contains(&id) =>
        {
            Some(j2534_0404::DEVICE_INFO_FT_CAN_SUPPORTED)
        }
        id if id == j2534_0404::PROTOCOL_FT_ISO15765_PS
            || (j2534_0404::PROTOCOL_FT_ISO15765_CH1..=j2534_0404::PROTOCOL_FT_ISO15765_CH128)
                .contains(&id) =>
        {
            Some(j2534_0404::DEVICE_INFO_FT_ISO15765_SUPPORTED)
        }
        // ADR-212/Round 2 (SAE J2534-2 clause 9 Single Wire CAN).
        id if id == j2534_0404::PROTOCOL_SW_CAN_PS
            || (j2534_0404::PROTOCOL_SW_CAN_CAN_CH1..=j2534_0404::PROTOCOL_SW_CAN_CAN_CH128)
                .contains(&id) =>
        {
            Some(j2534_0404::DEVICE_INFO_SW_CAN_SUPPORTED)
        }
        id if id == j2534_0404::PROTOCOL_SW_ISO15765_PS
            || (j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1
                ..=j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH128)
                .contains(&id) =>
        {
            Some(j2534_0404::DEVICE_INFO_SW_ISO15765_SUPPORTED)
        }
        // Mechanical extension of ADR-211's/ADR-212's established pattern,
        // closing the `_CHx` channel-count-cap enforcement gap
        // the Prioritized Backlog recorded for these
        // five families (SAE J1939, UART Echo Byte, Honda DIAG-H, SAE J1708,
        // TP2.0). GM UART is deliberately excluded here -- its own gap is a
        // separate, already-accepted residual (ADR-189 Decision 5), not
        // something this fix touches. Each arm below uses the family-wide
        // predicate (covers both the `_PS` id and the full `_CH1..128`
        // range) rather than a bare `_PS` exact-match, so a raw `_CHx` id is
        // matched directly here too -- a bare `_PS` check would silently
        // miss every `_CHx` id and defeat the point of this fix.
        id if is_j1939_protocol_id(id) => Some(j2534_0404::DEVICE_INFO_J1939_SUPPORTED),
        id if is_uart_echo_byte_family_protocol_id(id) => {
            Some(j2534_0404::DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED)
        }
        id if is_honda_diagh_family_protocol_id(id) => {
            Some(j2534_0404::DEVICE_INFO_HONDA_DIAGH_SUPPORTED)
        }
        id if is_j1708_family_protocol_id(id) => Some(j2534_0404::DEVICE_INFO_J1708_SUPPORTED),
        id if is_tp2_0_family_protocol_id(id) => Some(j2534_0404::DEVICE_INFO_TP2_0_SUPPORTED),
        // ADR-213/Round 3 (SAE J2534-2 clause 21 CAN FD / clause 22
        // ISO15765-on-CAN-FD): the third and final CAN-collapse family in
        // this series, mirroring the FT-CAN/SW-CAN arms above (two-tier
        // contract: checked directly against the raw `j2534_proto_id`, not
        // `base_protocol_id(j2534_proto_id)`, for the same reason those do).
        id if id == j2534_0404::PROTOCOL_FD_CAN_PS
            || (j2534_0404::PROTOCOL_FD_CAN_CH1..=j2534_0404::PROTOCOL_FD_CAN_CH128)
                .contains(&id) =>
        {
            Some(j2534_0404::DEVICE_INFO_FD_CAN_SUPPORTED)
        }
        id if id == j2534_0404::PROTOCOL_FD_ISO15765_PS
            || (j2534_0404::PROTOCOL_FD_ISO15765_CH1..=j2534_0404::PROTOCOL_FD_ISO15765_CH128)
                .contains(&id) =>
        {
            Some(j2534_0404::DEVICE_INFO_FD_ISO15765_SUPPORTED)
        }
        _ => match base_protocol_id(j2534_proto_id) {
            j2534_0404::J1850VPW => Some(j2534_0404::DEVICE_INFO_J1850VPW_SUPPORTED),
            j2534_0404::J1850PWM => Some(j2534_0404::DEVICE_INFO_J1850PWM_SUPPORTED),
            j2534_0404::ISO9141 => Some(j2534_0404::DEVICE_INFO_ISO9141_SUPPORTED),
            j2534_0404::ISO14230 => Some(j2534_0404::DEVICE_INFO_ISO14230_SUPPORTED),
            j2534_0404::CAN => Some(j2534_0404::DEVICE_INFO_CAN_SUPPORTED),
            j2534_0404::ISO15765 => Some(j2534_0404::DEVICE_INFO_ISO15765_SUPPORTED),
            j2534_0404::SCI_A_ENGINE
            | j2534_0404::SCI_A_TRANS
            | j2534_0404::SCI_B_ENGINE
            | j2534_0404::SCI_B_TRANS => Some(j2534_0404::DEVICE_INFO_J2610_SUPPORTED),
            _ => None,
        },
    }
}

/// ADR-185 Stage 1 connect-path Discovery-cache capability mapping: the
/// `DeviceFlag` check `J2534Service::enforce_discovery_capability` should
/// run before a brand-new physical channel connect, for the six J2534-2
/// protocol families that had zero Discovery-based fail-fast before this
/// (SWCAN/clause 9, FT-CAN/clause 20, UART Echo Byte/clause 12, Honda
/// DIAG-H/clause 13, J1708/clause 17, Analog Inputs/clause 10). `None` for
/// every other id -- the native `PassThruConnect` error remains the sole
/// authority for those, exactly as before this ADR.
///
/// Keyed on the raw, POST-SUBSTITUTION `j2534_proto_id`, not
/// `base_protocol_id(j2534_proto_id)`: [`base_protocol_id`] (immediately
/// above) already normalizes `PROTOCOL_SW_CAN_PS`/`PROTOCOL_FT_CAN_PS` (and
/// their ISO15765 counterparts) down to `CAN`/`ISO15765` for every other
/// Plane B purpose, and a base-keyed lookup here would check
/// `DEVICE_INFO_CAN_SUPPORTED` instead of `DEVICE_INFO_SW_CAN_SUPPORTED`/
/// `DEVICE_INFO_FT_CAN_SUPPORTED` -- never distinguishing a CAN-capable-but-
/// not-SWCAN/FT-CAN device from a genuinely SWCAN/FT-CAN-capable one.
pub(super) fn connect_discovery_check(j2534_proto_id: u32) -> Option<DiscoveryCheck> {
    match j2534_proto_id {
        // ADR-212/Round 2 (SAE J2534-2 clause 9 Single Wire CAN): unlike the
        // original bare exact-match arm this replaces, these two cover both
        // `SW_CAN_PS`/`SW_ISO15765_PS` AND their own `_CHx` ranges -- SWCAN
        // is a Stage-1 DeviceFlag family with in-scope `_CHx` Additional
        // Channels, so a `_CHx`-qualified connect gets the same fail-fast
        // Discovery gate the `_PS` connect does. Two guard arms (rather than
        // one `is_sw_family_protocol_id(id)` arm) since this family has two
        // distinct `_PS`/block pairs, each needing its own `DeviceFlag`
        // parameter -- mirrors FT-CAN's own two-arm shape just below.
        id if id == j2534_0404::PROTOCOL_SW_CAN_PS
            || (j2534_0404::PROTOCOL_SW_CAN_CAN_CH1..=j2534_0404::PROTOCOL_SW_CAN_CAN_CH128)
                .contains(&id) =>
        {
            Some(DiscoveryCheck::DeviceFlag {
                parameter: j2534_0404::DEVICE_INFO_SW_CAN_SUPPORTED,
                input_value: 0,
            })
        }
        id if id == j2534_0404::PROTOCOL_SW_ISO15765_PS
            || (j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1
                ..=j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH128)
                .contains(&id) =>
        {
            Some(DiscoveryCheck::DeviceFlag {
                parameter: j2534_0404::DEVICE_INFO_SW_ISO15765_SUPPORTED,
                input_value: 0,
            })
        }
        // ADR-211 (SAE J2534-2 clause 20 Fault-Tolerant CAN): unlike the
        // original single-`_PS`-id arms this replaces, these two cover both
        // `FT_CAN_PS`/`FT_ISO15765_PS` AND their own `_CHx` ranges --
        // Fault-Tolerant CAN is now a Stage-1 DeviceFlag family with in-scope
        // `_CHx` Additional Channels, so a `_CHx`-qualified connect gets the
        // same fail-fast Discovery gate the `_PS` connect does. Two guard
        // arms (rather than one `is_ft_family_protocol_id(id)` arm) since
        // this family has two distinct `_PS`/block pairs, each needing its
        // own `DeviceFlag` parameter -- mirrors SWCAN's own two-arm shape
        // just above, both generalized to also match each family's `_CHx`
        // block (ADR-212 Round 2 widened SWCAN's own arms to match).
        id if id == j2534_0404::PROTOCOL_FT_CAN_PS
            || (j2534_0404::PROTOCOL_FT_CAN_CH1..=j2534_0404::PROTOCOL_FT_CAN_CH128)
                .contains(&id) =>
        {
            Some(DiscoveryCheck::DeviceFlag {
                parameter: j2534_0404::DEVICE_INFO_FT_CAN_SUPPORTED,
                input_value: 0,
            })
        }
        id if id == j2534_0404::PROTOCOL_FT_ISO15765_PS
            || (j2534_0404::PROTOCOL_FT_ISO15765_CH1..=j2534_0404::PROTOCOL_FT_ISO15765_CH128)
                .contains(&id) =>
        {
            Some(DiscoveryCheck::DeviceFlag {
                parameter: j2534_0404::DEVICE_INFO_FT_ISO15765_SUPPORTED,
                input_value: 0,
            })
        }
        // ADR-207/Phase 9 (SAE J2534-2 clause 12 UART Echo Byte): unlike
        // every arm above (each a single `_PS` id), this covers both
        // `UART_ECHO_BYTE_PS` and the `ECHO_BYTE_CHx` range -- UART Echo
        // Byte is also a Stage-1 DeviceFlag family with in-scope `_CHx`
        // Additional Channels (ADR-207 Decision item 10), so a
        // `_CHx`-qualified connect gets the same fail-fast Discovery gate
        // the `_PS` connect does. Mirrors GM UART's own arm below, added
        // after an `edge-case-hunter` review found the original bare
        // exact-match arm let a `_CHx` connect silently skip this gate.
        id if is_uart_echo_byte_family_protocol_id(id) => Some(DiscoveryCheck::DeviceFlag {
            parameter: j2534_0404::DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED,
            input_value: 0,
        }),
        // ADR-208 Decision item 6 (SAE J2534-2 clause 13 Honda DIAG-H):
        // unlike a single `_PS`-id arm, this covers both
        // `HONDA_DIAGH_PS` and the `HONDA_DIAGH_CHx` range -- Honda DIAG-H
        // is also a Stage-1 DeviceFlag family with in-scope `_CHx`
        // Additional Channels (ADR-208), so a `_CHx`-qualified connect gets
        // the same fail-fast Discovery gate the `_PS` connect does. Mirrors
        // GM UART's/UART Echo Byte's own arms above, found by up-front
        // investigation before implementation (ADR-208 Context) rather than
        // a later edge-case-hunter round the way UART Echo Byte's arm was.
        id if is_honda_diagh_family_protocol_id(id) => Some(DiscoveryCheck::DeviceFlag {
            parameter: j2534_0404::DEVICE_INFO_HONDA_DIAGH_SUPPORTED,
            input_value: 0,
        }),
        // ADR-209 Decision item 6 (SAE J2534-2 clause 17 SAE J1708): unlike
        // a single `_PS`-id arm, this covers both `J1708_PS` and the
        // `J1708_CHx` range -- SAE J1708 is also a Stage-1 DeviceFlag family
        // with in-scope `_CHx` Additional Channels (ADR-209), so a
        // `_CHx`-qualified connect gets the same fail-fast Discovery gate
        // the `_PS` connect does. Mirrors GM UART's/UART Echo Byte's/Honda
        // DIAG-H's own arms above, found by up-front investigation before
        // implementation (ADR-209 Context) rather than a later
        // edge-case-hunter round the way UART Echo Byte's arm was.
        id if is_j1708_family_protocol_id(id) => Some(DiscoveryCheck::DeviceFlag {
            parameter: j2534_0404::DEVICE_INFO_J1708_SUPPORTED,
            input_value: 0,
        }),
        // ADR-213/Round 3 (SAE J2534-2 clause 21 CAN FD / clause 22
        // ISO15765-on-CAN-FD): the third and final CAN-collapse family in
        // this series, mirroring FT-CAN's/SW-CAN's own two-arm shape above
        // (two distinct `_PS`/block pairs, each needing its own `DeviceFlag`
        // parameter). Fulfills ADR-159 Decision item 7's own Discovery
        // deferral for the `_CHx`-capacity dimension specifically -- clause-5
        // opt-in enforcement at `apply_fd_mode` itself remains unchanged,
        // still out of this ADR's scope, matching every other family's own
        // precedent of leaving the pname-opt-in check where it already is.
        id if id == j2534_0404::PROTOCOL_FD_CAN_PS
            || (j2534_0404::PROTOCOL_FD_CAN_CH1..=j2534_0404::PROTOCOL_FD_CAN_CH128)
                .contains(&id) =>
        {
            Some(DiscoveryCheck::DeviceFlag {
                parameter: j2534_0404::DEVICE_INFO_FD_CAN_SUPPORTED,
                input_value: 0,
            })
        }
        id if id == j2534_0404::PROTOCOL_FD_ISO15765_PS
            || (j2534_0404::PROTOCOL_FD_ISO15765_CH1..=j2534_0404::PROTOCOL_FD_ISO15765_CH128)
                .contains(&id) =>
        {
            Some(DiscoveryCheck::DeviceFlag {
                parameter: j2534_0404::DEVICE_INFO_FD_ISO15765_SUPPORTED,
                input_value: 0,
            })
        }
        id if is_analog_in_protocol_id(id) => Some(DiscoveryCheck::DeviceFlag {
            parameter: j2534_0404::DEVICE_INFO_ANALOG_IN_SUPPORTED,
            input_value: 0,
        }),
        // ADR-188/Phase 7 Stage 7a (SAE J2534-2 clause 19 TP2.0): the flat
        // `DEVICE_INFO_TP2_0_SUPPORTED` bit only -- `_SIMULTANEOUS`/
        // `_PS_J1962` stay deferred, the same residual every ADR-185 Stage-1
        // family above already accepts. ADR-210 Decision item 7 widens this
        // arm from a bare `_PS` exact-match to `is_tp2_0_family_protocol_id`
        // -- TP2.0 is also now a Stage-1 DeviceFlag family with in-scope
        // `_CHx` Additional Channels, so a `_CHx`-qualified connect gets the
        // same fail-fast Discovery gate the `_PS` connect does. Mirrors GM
        // UART's/UART Echo Byte's/Honda DIAG-H's/SAE J1708's own arms above.
        id if is_tp2_0_family_protocol_id(id) => Some(DiscoveryCheck::DeviceFlag {
            parameter: j2534_0404::DEVICE_INFO_TP2_0_SUPPORTED,
            input_value: 0,
        }),
        // ADR-189/Phase 8 (SAE J2534-2 clause 11 GM UART): unlike every arm
        // above (each a single `_PS` id), this covers both `GM_UART_PS` and
        // the `GM_UART_CHx` range -- GM UART is also the first Stage-1
        // DeviceFlag family with in-scope `_CHx` Additional Channels
        // (ADR-189 Decision 2), so a `_CHx`-qualified connect gets the same
        // fail-fast Discovery gate the `_PS` connect does. The flat
        // `DEVICE_INFO_GM_UART_SUPPORTED` bit only -- `_SIMULTANEOUS`/
        // `_PS_J1962` stay deferred, the same residual every ADR-185 Stage-1
        // family above already accepts.
        id if is_gm_uart_protocol_id(id) => Some(DiscoveryCheck::DeviceFlag {
            parameter: j2534_0404::DEVICE_INFO_GM_UART_SUPPORTED,
            input_value: 0,
        }),
        // ADR-194/Phase 16 (SAE J2534-2 clause 24 Ethernet_NDIS): a
        // ninth Stage-1-wired family (the original six -- SWCAN, FT-CAN,
        // UART Echo Byte, Honda DIAG-H, J1708, Analog Inputs -- plus
        // TP2.0 and GM UART, each added in a later phase), the same flat
        // `DeviceFlag` shape every family above uses.
        j2534_0404::PROTOCOL_ETHERNET_NDIS => Some(DiscoveryCheck::DeviceFlag {
            parameter: j2534_0404::DEVICE_INFO_ETHERNET_NDIS_SUPPORTED,
            input_value: 0,
        }),
        _ => None,
    }
}

/// Canonical default DLC pins for a bare hardware protocol id with no
/// matching `resources` table row in hand (e.g. `ChannelProtocol::CAN`
/// resolved via `names::map_protocol_name`'s legacy alias path, which never
/// touches this table at all) -- the same per-protocol defaults the
/// `PINS_*` constants above already encode per resource row, needed so
/// ADR-156 Decision 2's Pin Selection resolution has *some* default pin
/// set to diff the caller's `dlc_pin_data` against even when resolution
/// didn't go through a table row. Covers exactly the same hardware ids
/// [`ps_protocol_id`] does (by construction, so the two are never out of
/// sync); `None` for everything else.
pub(super) fn default_dlc_pins_for_hw_protocol(
    hw_protocol_id: u32,
) -> Option<&'static [(u32, u32)]> {
    match hw_protocol_id {
        j2534_0404::J1850VPW => Some(PINS_SAE_J1850_VPW),
        j2534_0404::J1850PWM => Some(PINS_SAE_J1850_PWM),
        j2534_0404::ISO9141 => Some(PINS_ISO_9141_2_UART),
        j2534_0404::ISO14230 => Some(PINS_ISO_14230_1_UART),
        j2534_0404::CAN => Some(PINS_ISO_11898_2_DWCAN),
        j2534_0404::ISO15765 => Some(PINS_ISO_11898_2_DWCAN),
        j2534_0404::SCI_A_ENGINE => Some(PINS_SCI_A_ENGINE),
        j2534_0404::SCI_A_TRANS => Some(PINS_SCI_A_TRANS),
        j2534_0404::SCI_B_ENGINE => Some(PINS_SCI_B_ENGINE),
        j2534_0404::SCI_B_TRANS => Some(PINS_SCI_B_TRANS),
        _ => None,
    }
}

/// Whether a secondary (two-pin) DLC pin selection is required, optional, or
/// disallowed for a given base hardware protocol id, for the general SAE
/// J2534-2 clause 6 Pin Selection fallback path (ADR-201; closes the P2
/// backlog item design-advisor left open in ADR-168's Seventh
/// correction/Accepted residual, which fixed only the FT-CAN arm's own
/// closed two-pair check and explicitly deferred this general path pending
/// a real per-bus predicate rather than a uniform pin-count rule).
///
/// Covers exactly the same ten hardware ids [`ps_protocol_id`] does; `None`
/// for everything else (a domain-invariant break if `ps_protocol_id` returns
/// `Some` for an id this function doesn't also cover -- callers must treat
/// that combination as an internal error, never a silent skip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SecondaryPinRequirement {
    /// A one-pin (primary only) selection must be rejected -- the protocol's
    /// physical layer has no valid one-pin shape. `CAN`/`ISO15765` (standard
    /// differential CAN HI/LOW -- single-wire CAN is a structurally distinct
    /// hardware id family handled by the SW clause-9 arm elsewhere in
    /// `names::resolve_pin_selection`, so plain `CAN_PS`/`ISO15765_PS` has no
    /// legitimate 1-pin form), `J1850PWM` (Ford SCP, differential
    /// PLUS/MINUS per `PINS_SAE_J1850_PWM`), and all four SCI ids (SAE J2610
    /// needs both Tx and Rx for a bidirectional diagnostic link; no
    /// one-pin SCI shape is defined).
    Required,
    /// Either a one-pin or a two-pin selection is valid. `ISO9141`/
    /// `ISO14230` (K-line): the L-line secondary is genuinely optional for a
    /// real, common K-only diagnostic configuration.
    Optional,
    /// A two-pin selection, or any secondary-typed pin, must be rejected.
    /// `J1850VPW` (GM/Chrysler Class 2) is genuinely single-wire --
    /// `PINS_SAE_J1850_VPW` has only a primary entry, no secondary pin
    /// exists for this bus at all, unlike `J1850PWM`.
    NeverPresent,
}

pub(super) fn secondary_pin_requirement(hw_protocol_id: u32) -> Option<SecondaryPinRequirement> {
    match hw_protocol_id {
        j2534_0404::CAN
        | j2534_0404::ISO15765
        | j2534_0404::J1850PWM
        | j2534_0404::SCI_A_ENGINE
        | j2534_0404::SCI_A_TRANS
        | j2534_0404::SCI_B_ENGINE
        | j2534_0404::SCI_B_TRANS => Some(SecondaryPinRequirement::Required),
        j2534_0404::ISO9141 | j2534_0404::ISO14230 => Some(SecondaryPinRequirement::Optional),
        j2534_0404::J1850VPW => Some(SecondaryPinRequirement::NeverPresent),
        _ => None,
    }
}

/// Returns the first (lowest, i.e. table-order) resource ID whose row's
/// `ChannelProtocol` exactly matches `protocol`, or `None` when no row
/// carries it at all (a legacy-only extended protocol not present in the
/// table). Several rows can share one `ChannelProtocol` (alias rows, e.g.
/// 0x0205/0x0209, or the four `SAE_J2610_on_SAE_J2610_SCI` configuration
/// rows); table order is the tie-break for reporting a single resource ID
/// for that protocol (`GetResourceStatus`/`GetConflictingResources` response
/// echo, ADR-069). Note this ignores `hw_protocol_override` -- callers that
/// need a specific hardware configuration must resolve through
/// `names::find_table_row_by_name`'s pin-aware selection instead.
///
/// ADR-164/Phase 4 verification: this function's own table-order tie-break
/// is *by design* incapable of distinguishing an SW row (e.g. 0x0226) from
/// its dual-wire sibling (0x0201) when both share one `ChannelProtocol` --
/// it takes no `hw_protocol_override`/hardware-id input at all. This is only
/// safe because its two call sites (`rpc_link::rpc_get_resource_status`,
/// `names::J2534Service::resolve_object_id`'s `ObjtResource` arm) both use
/// it exclusively as a fallback for a `map_protocol_name`-only legacy alias
/// query (no direct table-name/id match), where "the lowest resource_id
/// sharing this protocol" is already the pre-existing, documented answer
/// with no live-link state involved. The genuine live-link SW-vs-dual-wire
/// attribution hazard this function cannot resolve lives in
/// `rpc_get_resource_status`'s `active_candidate`/`matches_status_hw_id`
/// matching instead, which checks a connected link's *raw*
/// `hw_protocol_id` (not just its normalized `base_hw_protocol_id()`) for
/// exactly this reason -- see that function's own comments.
pub(super) fn find_resource_id_for_protocol(protocol: ChannelProtocol) -> Option<u32> {
    resource_table()
        .iter()
        .find(|row| row.protocol == protocol)
        .map(|row| row.resource_id)
}

/// ISO 22900-2 §9.4.26 conflict predicate (ADR-106): `a` and `b` (distinct
/// rows) conflict iff they share at least one physical DLC pin *number*
/// (`dlc_pins`'s `.0`, ignoring the logical pin type `.1` -- the same
/// physical pin can carry a different logical pin type across
/// configurations, e.g. pin 6 is `PIN_HI` on the CAN rows but `PIN_TX` on
/// `SCI_A_ENGINE`, and that is still a real wiring conflict) *or* they sit on
/// the same physical controller (identical `bus_type_id`, used here as the
/// controller-group proxy -- Annex G.1.4's own example flags two routes as
/// conflicting solely because they share one CAN controller, even with
/// disjoint pins; the `SAE_J2610_UART` rows are the concrete case in this
/// table, four alternate SCI wirings of one shared transceiver, e.g. pins
/// (9, 15) on `SCI_B_TRANS` share no pin number with `SCI_A_ENGINE`'s (6, 7)
/// but both still occupy the one physical controller) -- and are not the
/// same electrical configuration on that controller (identical `bus_type_id`
/// *and* `dlc_pins`) -- ISO 22900-2 explicitly allows several CLLs to share
/// one physical channel this way (e.g. the ten `ISO_11898_2_DWCAN`-family
/// rows 0x0201-0x020A, which all share `PINS_ISO_11898_2_DWCAN`, or the two
/// `SAE_J2610_UART` naming schemes for one SCI config, e.g. 0x0220 vs
/// 0x0224), so that case must not be reported as a conflict.
///
/// This is a static, table-only computation: no live connection state is
/// consulted, matching §9.4.26.2's "before any `ComLogicalLink` exists"
/// resource-table query semantics. See `rpc_link::rpc_get_conflicting_resources`.
///
/// SAE J2534-2 clause 24 Table 103 (ADR-194/Phase 16, Codex review finding
/// on PR #102): the `ETHERNET_NDIS` row's `dlc_pins` (`PINS_ETHERNET_NDIS`)
/// carries only Option 1's Tx pins (3/11), since `dlc_pins` also feeds
/// `names::resolve_pin_selection`'s exact-length defaults-match check --
/// widening it to include Option 2's alternate Tx pins (1/9) there would
/// turn a caller's currently-accepted no-op `dlc_pin_data` request (Option
/// 1's exact 5 pins) into a spurious "no _PS variant" rejection. But THIS
/// function answers before any `ComLogicalLink` -- and therefore before any
/// `CP_NdisPinOption` -- exists, so the conservative and correct static
/// answer must account for either option a future connect could still
/// select: `ethernet_ndis_alternate_pin_overlap` below checks Option 2's
/// pins (1/9) directly, without touching `dlc_pins` itself. Without this,
/// e.g. resource `0x0260` GM_UART's pin 9 (`PINS_GM_UART`) would never be
/// reported as conflicting with an `ETHERNET_NDIS` connection that ends up
/// staging `CP_NdisPinOption = 2`, even though the two would contend for
/// the same physical DLC pin.
pub(super) fn rows_conflict(a: &ResourceDef, b: &ResourceDef) -> bool {
    let same_bus = a.bus_type_id == b.bus_type_id;
    let same_config = same_bus && a.dlc_pins == b.dlc_pins;
    let pin_overlap = a
        .dlc_pins
        .iter()
        .any(|&(pin_a, _)| b.dlc_pins.iter().any(|&(pin_b, _)| pin_a == pin_b))
        || ethernet_ndis_alternate_pin_overlap(a, b);
    a.resource_id != b.resource_id && !same_config && (pin_overlap || same_bus)
}

/// SAE J2534-2 clause 24 Table 103's Option 2 alternate Tx pins (1 = Tx(+),
/// 9 = Tx(-) -- the same logical roles `PINS_ETHERNET_NDIS` types Option
/// 1's pins 3/11 as, `PIN_PLUS`/`PIN_MINUS`). See `rows_conflict`'s own doc
/// comment for why this lives here rather than in `PINS_ETHERNET_NDIS`
/// itself.
const ETHERNET_NDIS_ALTERNATE_TX_PINS: [u32; 2] = [1, 9];

/// Codex review finding, PR #102 round 7: a peer row's own dynamically
/// selectable pins (via `names.rs`'s `resolve_pin_selection`), not just its
/// static `dlc_pins` default, can also collide with Ethernet_NDIS's Option 2
/// alternate Tx pins (`ETHERNET_NDIS_ALTERNATE_TX_PINS`, 1/9). Concrete case:
/// Honda DIAG-H's row (0x023B) defaults to pin 14 only, but its own
/// `resolve_pin_selection` arm accepts pin 1 too (clause 13.2.4's two
/// documented pins), so a real Honda DIAG-H connection configured on pin 1
/// would contend with an Option-2 Ethernet_NDIS connection's Tx(+) -- not
/// caught by comparing only static `dlc_pins` on both sides.
///
/// A round-8 finding raised the same question for FT-CAN's own alternate
/// pin-pair (clause 20.2.1's (3,11), reversed onto Ethernet_NDIS's Option 1
/// BASE pins 3/11): investigation (`edge-case-hunter`, PR #102 close-out)
/// found this does not correspond to a live gap against the current
/// resource table. FT-CAN's row (0x0230, and every other FT-CAN-family row)
/// defaults to `dlc_pins` pins 1/9 -- identical to
/// `ETHERNET_NDIS_ALTERNATE_TX_PINS` -- so `rows_conflict`'s ordinary,
/// unconditional `dlc_pins`-overlap check (above) already reports FT-CAN as
/// conflicting with Ethernet_NDIS for every row pair, independent of this
/// function entirely; adding FT-CAN's `(3, 11)` extras here changed no
/// observable `rows_conflict` result (confirmed by enumerating all 97 table
/// rows with and without it) and was reverted. No protocol in the current
/// closed set has default pins disjoint from `ETHERNET_NDIS_ALTERNATE_TX_PINS`
/// while also having extras that overlap Ethernet_NDIS's Option 1 base pins
/// -- see the backlog entry for the
/// general, protocol-pairwise version of this limitation this function does
/// not attempt to solve.
///
/// Returns the closed set of EXTRA pins (beyond the row's own `dlc_pins`)
/// `resolve_pin_selection` accepts for the standalone-row protocols whose
/// own arm there validates against a specific, bounded pin set and whose
/// extras can actually affect this check's outcome: Honda DIAG-H (clause
/// 13.2.4: pin 1 or 14 -- row default is 14, so 1 is the only extra) and GM
/// UART (clause 11.2.2: pin 1 or 9 -- row default is 9, so 1 is the only
/// extra). Deliberately excludes FT-CAN (see above -- its extras never
/// change the outcome given its own default pins) and UART Echo
/// Byte/J1708/J1939: their own `resolve_pin_selection` arms accept ANY
/// caller-supplied pin with no closed-set validation at all (see those
/// arms' own comments in `names.rs`), so there is no bounded extra-pin set
/// to enumerate for them -- a real, pre-existing gap in `rows_conflict`'s
/// conflict model this function does not attempt to close, since it applies
/// to every pair of dynamic-pin protocols symmetrically, not only against
/// Ethernet_NDIS (the backlog).
fn peer_closed_set_extra_pins(row: &ResourceDef) -> &'static [u32] {
    let hw_id = row
        .hw_protocol_override
        .unwrap_or_else(|| row.protocol.j2534_protocol_id());
    if is_honda_diagh_protocol_id(hw_id) || is_gm_uart_protocol_id(hw_id) {
        &[1]
    } else {
        &[]
    }
}

fn ethernet_ndis_alternate_pin_overlap(a: &ResourceDef, b: &ResourceDef) -> bool {
    let is_ethernet_ndis = |row: &ResourceDef| row.protocol == ChannelProtocol::ETHERNET_NDIS;
    let check = |peer: &ResourceDef| {
        peer.dlc_pins
            .iter()
            .any(|&(pin, _)| ETHERNET_NDIS_ALTERNATE_TX_PINS.contains(&pin))
            || peer_closed_set_extra_pins(peer)
                .iter()
                .any(|pin| ETHERNET_NDIS_ALTERNATE_TX_PINS.contains(pin))
    };
    if is_ethernet_ndis(a) {
        check(b)
    } else if is_ethernet_ndis(b) {
        check(a)
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_97_rows_with_unique_ascending_resource_ids() {
        let table = resource_table();
        assert_eq!(table.len(), 97);
        for pair in table.windows(2) {
            assert!(
                pair[0].resource_id < pair[1].resource_id,
                "resource IDs should be strictly ascending: {:#06x} >= {:#06x}",
                pair[0].resource_id,
                pair[1].resource_id
            );
        }
        assert_eq!(table.first().unwrap().resource_id, 0x0201);
        assert_eq!(table.last().unwrap().resource_id, 0x0261);
    }

    /// ADR-185 Stage 1, `edge-case-hunter` finding: `j2534-0404-mock`
    /// advertises `DEVICE_INFO_SW_CAN_SUPPORTED`/`_SW_ISO15765_SUPPORTED`/
    /// `_FT_CAN_SUPPORTED`/`_FT_ISO15765_SUPPORTED` (and the other three
    /// Stage 1 flags) as uniformly supported, so an end-to-end connect test
    /// cannot distinguish `connect_discovery_check` naming the CORRECT
    /// `DEVICE_INFO_*` parameter for a given `j2534_proto_id` from an
    /// accidentally-swapped one (e.g. `PROTOCOL_SW_CAN_PS` mapped to
    /// `DEVICE_INFO_SW_ISO15765_SUPPORTED`) -- every combination would still
    /// report `supported: true` and the connect would still succeed. This
    /// pins the mapping directly, parameter-by-parameter, independent of
    /// mock behavior.
    #[test]
    fn connect_discovery_check_maps_each_stage_1_protocol_id_to_its_own_device_info_parameter() {
        let cases = [
            (
                j2534_0404::PROTOCOL_SW_CAN_PS,
                j2534_0404::DEVICE_INFO_SW_CAN_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_SW_ISO15765_PS,
                j2534_0404::DEVICE_INFO_SW_ISO15765_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_FT_CAN_PS,
                j2534_0404::DEVICE_INFO_FT_CAN_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_FT_ISO15765_PS,
                j2534_0404::DEVICE_INFO_FT_ISO15765_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
                j2534_0404::DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
                j2534_0404::DEVICE_INFO_HONDA_DIAGH_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_J1708_PS,
                j2534_0404::DEVICE_INFO_J1708_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_ANALOG_IN_1,
                j2534_0404::DEVICE_INFO_ANALOG_IN_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_TP2_0_PS,
                j2534_0404::DEVICE_INFO_TP2_0_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_ANALOG_IN_32,
                j2534_0404::DEVICE_INFO_ANALOG_IN_SUPPORTED,
            ),
            (
                j2534_0404::PROTOCOL_ETHERNET_NDIS,
                j2534_0404::DEVICE_INFO_ETHERNET_NDIS_SUPPORTED,
            ),
        ];
        for (proto_id, expected_parameter) in cases {
            let check = connect_discovery_check(proto_id).unwrap_or_else(|| {
                panic!("protocol id {proto_id:#010x} should have a Stage 1 Discovery check")
            });
            match check {
                DiscoveryCheck::DeviceFlag {
                    parameter,
                    input_value,
                } => {
                    assert_eq!(
                        parameter, expected_parameter,
                        "protocol id {proto_id:#010x} mapped to the wrong DEVICE_INFO_* \
                         parameter"
                    );
                    assert_eq!(input_value, 0, "no Stage 1 row is per-pin");
                }
                _ => panic!("expected DeviceFlag for {proto_id:#010x}"),
            }
        }
        // No row exists for a plain, unrelated base protocol -- the native
        // connect error remains the sole authority for those.
        assert!(connect_discovery_check(j2534_0404::CAN).is_none());
    }

    #[test]
    fn find_by_resource_id_round_trips_every_row() {
        for row in resource_table() {
            let found = find_by_resource_id(row.resource_id).expect("row should be found");
            assert_eq!(found.resource_id, row.resource_id);
        }
        assert!(find_by_resource_id(0xFFFF).is_none());
    }

    #[test]
    fn sci_config_rows_share_protocol_name_but_have_distinct_config_names_and_protocols() {
        let sci_rows: Vec<&ResourceDef> = resource_table()
            .iter()
            .filter(|row| row.protocol_name.eq_ignore_ascii_case("SAE_J2610_SCI"))
            .collect();
        assert_eq!(sci_rows.len(), 4);
        let mut config_names: Vec<&str> = sci_rows.iter().filter_map(|r| r.config_name).collect();
        config_names.sort_unstable();
        assert_eq!(
            config_names,
            vec!["SCI_A_ENGINE", "SCI_A_TRANS", "SCI_B_ENGINE", "SCI_B_TRANS"]
        );
        // Bare config rows never carry a hardware override -- their own
        // `ChannelProtocol` already names the exact SCI variant.
        assert!(
            sci_rows
                .iter()
                .all(|row| row.hw_protocol_override.is_none())
        );
    }

    /// Spec correction: `SAE_J2610_on_SAE_J2610_SCI` expands to four rows
    /// (0x021E-0x0221), one per SCI configuration, mirroring `SAE_J2610_SCI`.
    /// All four share one `ChannelProtocol` (0x0160) but must have distinct
    /// `hw_protocol_override`s and pin sets, and none may set `config_name`
    /// (that stays exclusive to the bare `SAE_J2610_SCI` rows).
    #[test]
    fn sae_j2610_on_rows_expand_to_four_configs_with_distinct_hw_overrides() {
        let rows: Vec<&ResourceDef> = resource_table()
            .iter()
            .filter(|row| {
                row.protocol_name
                    .eq_ignore_ascii_case("SAE_J2610_on_SAE_J2610_SCI")
            })
            .collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter().map(|r| r.resource_id).collect::<Vec<_>>(),
            vec![0x021E, 0x021F, 0x0220, 0x0221]
        );
        assert!(rows.iter().all(|row| row.config_name.is_none()));
        assert!(
            rows.iter()
                .all(|row| row.protocol == ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI)
        );

        let mut overrides: Vec<u32> = rows
            .iter()
            .map(|row| row.hw_protocol_override.expect("every row should override"))
            .collect();
        overrides.sort_unstable();
        let mut expected = vec![
            j2534_0404::SCI_A_ENGINE,
            j2534_0404::SCI_A_TRANS,
            j2534_0404::SCI_B_ENGINE,
            j2534_0404::SCI_B_TRANS,
        ];
        expected.sort_unstable();
        assert_eq!(overrides, expected);

        let mut pins: Vec<&[(u32, u32)]> = rows.iter().map(|row| row.dlc_pins).collect();
        pins.sort_unstable();
        pins.dedup();
        assert_eq!(
            pins.len(),
            4,
            "each configuration should have distinct pins"
        );
    }

    #[test]
    fn find_resource_id_for_protocol_picks_first_table_order_match_among_aliases() {
        // 0x0205 (ISO_15031_5_on_ISO_15765_4) and 0x0209 (ISO_OBD_on_ISO_15765_4,
        // an alias row) share ChannelProtocol::ISO_15031_5_ON_ISO_15765_4; the
        // lower resource ID (table order) wins.
        assert_eq!(
            find_resource_id_for_protocol(ChannelProtocol::ISO_15031_5_ON_ISO_15765_4),
            Some(0x0205)
        );
        // ISO15765 is unique to resource 0x0206.
        assert_eq!(
            find_resource_id_for_protocol(ChannelProtocol::ISO15765),
            Some(0x0206)
        );
        // A legacy-only extended protocol with no table row at all.
        assert_eq!(
            find_resource_id_for_protocol(ChannelProtocol::ISO_11783_12_ON_ISO_11783_5),
            None
        );
        // The four SAE_J2610_on_SAE_J2610_SCI rows share one ChannelProtocol;
        // the first in table order (0x021E, SCI_A_ENGINE) wins.
        assert_eq!(
            find_resource_id_for_protocol(ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI),
            Some(0x021E)
        );
    }

    /// ADR-106: two rows sharing `bus_type_id` *and* `dlc_pins` are the same
    /// electrical configuration on one controller -- ISO 22900-2 explicitly
    /// allows several CLLs to share it, so `rows_conflict` must say `false`
    /// even though every pin number overlaps. 0x0201/0x0202 are two of the
    /// ten `ISO_11898_2_DWCAN`-family rows, all wired identically.
    #[test]
    fn rows_conflict_is_false_for_identical_electrical_configuration() {
        let a = find_by_resource_id(0x0201).unwrap();
        let b = find_by_resource_id(0x0202).unwrap();
        assert_eq!(a.bus_type_id, b.bus_type_id);
        assert_eq!(a.dlc_pins, b.dlc_pins);
        assert!(!rows_conflict(a, b));
        assert!(!rows_conflict(b, a));
    }

    /// ADR-106: a K-line row on the ISO_14230_1_UART-only bus (0x020B,
    /// bus 0x0302) and the combined ISO_9141_2_UART_and_ISO_14230_1_UART bus
    /// row (0x0212, bus 0x0304) share DLC pins 7/15 but are different
    /// bus_type_ids -- a real physical conflict, not spec-legal sharing.
    #[test]
    fn rows_conflict_is_true_for_cross_bus_type_pin_overlap() {
        let a = find_by_resource_id(0x020B).unwrap();
        let b = find_by_resource_id(0x0212).unwrap();
        assert_ne!(a.bus_type_id, b.bus_type_id);
        assert!(rows_conflict(a, b));
        assert!(rows_conflict(b, a));
    }

    /// ADR-106: a CAN row (pins 6/14) and a SAE_J1850_VPW row (pin 2) share
    /// no DLC pin number and sit on different buses -- no conflict.
    #[test]
    fn rows_conflict_is_false_for_disjoint_pins_on_different_buses() {
        let a = find_by_resource_id(0x0201).unwrap();
        let b = find_by_resource_id(0x0218).unwrap();
        assert!(!rows_conflict(a, b));
        assert!(!rows_conflict(b, a));
    }

    /// A row never conflicts with itself.
    #[test]
    fn rows_conflict_is_false_for_the_same_row() {
        let a = find_by_resource_id(0x0201).unwrap();
        assert!(!rows_conflict(a, a));
    }

    /// ADR-106 (`same_bus` addition): 0x0220 (`SCI_B_ENGINE` via
    /// `SAE_J2610_on_SAE_J2610_SCI`, pins (12,7)) and 0x0221 (`SCI_B_TRANS`,
    /// pins (9,15)) share no DLC pin number but are both alternate wirings of
    /// the one shared `SAE_J2610_UART` SCI controller, so they must conflict.
    #[test]
    fn rows_conflict_is_true_for_same_controller_disjoint_pins_sci_hw_override_rows() {
        let a = find_by_resource_id(0x0220).unwrap();
        let b = find_by_resource_id(0x0221).unwrap();
        assert_eq!(a.bus_type_id, b.bus_type_id);
        assert!(
            !a.dlc_pins
                .iter()
                .any(|&(pa, _)| b.dlc_pins.iter().any(|&(pb, _)| pa == pb))
        );
        assert!(rows_conflict(a, b));
        assert!(rows_conflict(b, a));
    }

    /// Same as above, but for the bare `SAE_J2610_SCI` (`config_name`-bearing)
    /// naming scheme: 0x0224 (`SCI_B_ENGINE`) and 0x0225 (`SCI_B_TRANS`).
    #[test]
    fn rows_conflict_is_true_for_same_controller_disjoint_pins_sci_config_name_rows() {
        let a = find_by_resource_id(0x0224).unwrap();
        let b = find_by_resource_id(0x0225).unwrap();
        assert_eq!(a.bus_type_id, b.bus_type_id);
        assert!(rows_conflict(a, b));
        assert!(rows_conflict(b, a));
    }

    /// Cross-naming-scheme case: 0x021E (`SCI_A_ENGINE` via
    /// `SAE_J2610_on_SAE_J2610_SCI`, pins (6,7)) and 0x0221 (`SCI_B_TRANS`,
    /// pins (9,15)) also share no pin but must conflict, since `same_bus`
    /// applies regardless of which SCI naming scheme a row uses.
    #[test]
    fn rows_conflict_is_true_for_same_controller_across_sci_naming_schemes() {
        let a = find_by_resource_id(0x021E).unwrap();
        let b = find_by_resource_id(0x0221).unwrap();
        assert_eq!(a.bus_type_id, b.bus_type_id);
        assert!(rows_conflict(a, b));
        assert!(rows_conflict(b, a));
    }

    /// ADR-106: 0x0220 (`SCI_B_ENGINE` via `hw_protocol_override`) and 0x0224
    /// (`SCI_B_ENGINE` via `config_name`) are the same electrical
    /// configuration under two naming schemes -- identical `bus_type_id` and
    /// `dlc_pins` -- so `same_config` must still exempt this pair even after
    /// the `same_bus` addition.
    #[test]
    fn rows_conflict_is_false_for_same_sci_config_across_naming_schemes() {
        let a = find_by_resource_id(0x0220).unwrap();
        let b = find_by_resource_id(0x0224).unwrap();
        assert_eq!(a.bus_type_id, b.bus_type_id);
        assert_eq!(a.dlc_pins, b.dlc_pins);
        assert!(!rows_conflict(a, b));
        assert!(!rows_conflict(b, a));
    }

    /// SAE J2534-2 clause 24 Table 103 (ADR-194/Phase 16, Codex review
    /// finding on PR #102): `ETHERNET_NDIS` (0x0261)'s `dlc_pins` types only
    /// Option 1's Tx pins (3/11) -- pin 9, which `GM_UART` (0x0260)
    /// occupies exclusively, is Option 2's alternate Tx(-) pin, invisible to
    /// a plain `dlc_pins` overlap check. `rows_conflict` must still report
    /// this pair as conflicting, since a real connection could stage
    /// `CP_NdisPinOption = 2` and genuinely contend for pin 9.
    #[test]
    fn rows_conflict_is_true_for_ethernet_ndis_option_2_alternate_pin_overlap() {
        let ndis = find_by_resource_id(0x0261).unwrap();
        let gm_uart = find_by_resource_id(0x0260).unwrap();
        assert!(
            !ndis
                .dlc_pins
                .iter()
                .any(|&(pin, _)| gm_uart.dlc_pins.iter().any(|&(other, _)| pin == other)),
            "this test should exercise the alternate-pin path, not an ordinary dlc_pins overlap"
        );
        assert!(rows_conflict(ndis, gm_uart));
        assert!(rows_conflict(gm_uart, ndis));
    }

    /// Codex review finding, PR #102 round 7: `HONDA_DIAGH` (0x023B)
    /// defaults to pin 14 only in its own `dlc_pins`, but its
    /// `resolve_pin_selection` arm (`names.rs`) also accepts pin 1 (clause
    /// 13.2.4's two documented pins) -- a real Honda DIAG-H connection
    /// configured on pin 1 would contend with an Option-2 Ethernet_NDIS
    /// connection's Tx(+), which a plain `dlc_pins` overlap (or even the
    /// Option-2-alternate-pin check alone, without also consulting the
    /// peer's own dynamically selectable pins) would miss entirely.
    #[test]
    fn rows_conflict_is_true_for_ethernet_ndis_against_honda_diagh_dynamic_alternate_pin() {
        let ndis = find_by_resource_id(0x0261).unwrap();
        let honda_diagh = find_by_resource_id(0x023B).unwrap();
        assert!(
            !ndis
                .dlc_pins
                .iter()
                .any(|&(pin, _)| honda_diagh.dlc_pins.iter().any(|&(other, _)| pin == other)),
            "this test should exercise the peer-dynamic-pin path, not an ordinary dlc_pins overlap"
        );
        assert!(rows_conflict(ndis, honda_diagh));
        assert!(rows_conflict(honda_diagh, ndis));
    }

    /// A round-8 review finding asked whether FT-CAN's own alternate pin-pair
    /// (clause 20.2.1's (3,11), reversed onto Ethernet_NDIS's Option 1 base
    /// pins) needed the same peer-dynamic-pin treatment as the Honda DIAG-H
    /// case above. Investigation (`edge-case-hunter`, PR #102 close-out)
    /// found `rows_conflict(ndis, ftcan)` was already `true` before any
    /// round-7/8 change, for an unrelated reason: `ISO_11898_RAW_FTCAN`'s
    /// (0x0230) own `dlc_pins` DEFAULT to pins 1/9, which is exactly
    /// `ETHERNET_NDIS_ALTERNATE_TX_PINS` -- the ordinary, unconditional
    /// `dlc_pins`-overlap branch in `ethernet_ndis_alternate_pin_overlap`
    /// already catches this pair without needing `peer_closed_set_extra_pins`
    /// to know about FT-CAN's alternate (3,11) pair at all. This regression
    /// test pins that real, already-correct behavior and asserts the actual
    /// mechanism it exercises, rather than the peer-dynamic-pin path a
    /// since-reverted round-8 change mistakenly believed was needed here.
    #[test]
    fn rows_conflict_is_true_for_ethernet_ndis_against_ftcan_via_default_pin_overlap() {
        let ndis = find_by_resource_id(0x0261).unwrap();
        let ftcan = find_by_resource_id(0x0230).unwrap();
        assert!(
            ftcan
                .dlc_pins
                .iter()
                .any(|&(pin, _)| ETHERNET_NDIS_ALTERNATE_TX_PINS.contains(&pin)),
            "this test should exercise the alternate-pin dlc_pins-overlap path -- \
             FT-CAN's default pins are expected to equal ETHERNET_NDIS_ALTERNATE_TX_PINS"
        );
        assert_ne!(
            ndis.bus_type_id, ftcan.bus_type_id,
            "this test should exercise the pin-overlap path, not the same_bus shortcut"
        );
        assert!(rows_conflict(ndis, ftcan));
        assert!(rows_conflict(ftcan, ndis));
    }

    /// A row with no pin in common with either of `ETHERNET_NDIS`'s two Tx
    /// pin options (3/11 or 1/9), its Rx pins (12/13), or its Activation
    /// Line (8) must not be reported as conflicting just because the
    /// alternate-pin check exists. 0x0201 (`ISO_11898_2_DWCAN`, pins 6/14)
    /// is disjoint from all of these on a different bus.
    #[test]
    fn rows_conflict_is_false_for_ethernet_ndis_against_a_fully_disjoint_row() {
        let ndis = find_by_resource_id(0x0261).unwrap();
        let can = find_by_resource_id(0x0201).unwrap();
        assert!(!rows_conflict(ndis, can));
        assert!(!rows_conflict(can, ndis));
    }

    /// Systematic sweep (ADR-069): every resource row's `bus_type_name` must
    /// resolve via `bustype_default_params`, and every row's `protocol_name`
    /// must either resolve via `protocol_default_params` or be explicitly
    /// allowlisted in `comparam_defaults::PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS`
    /// (native-`ChannelProtocol` resources whose "protocol name" is actually
    /// an ISO 22900-2 BUSTYPE name, already covered by the bus-type default
    /// alone). This is a regression gate: a resource row added later with no
    /// matching `comparam_defaults.rs` entry -- and not a deliberate
    /// allowlist addition -- fails this test instead of silently producing
    /// an incomplete Working set at `CreateComLogicalLink` time (the bug
    /// three separate PR-review passes each caught one row at a time before
    /// this test existed).
    #[test]
    fn every_resource_row_has_protocol_and_bustype_defaults_or_is_allowlisted() {
        use super::super::comparam_defaults;

        let mut missing_protocol_defaults = Vec::new();
        let mut missing_bustype_defaults = Vec::new();

        for row in resource_table() {
            if comparam_defaults::bustype_default_params(row.bus_type_name).is_none() {
                missing_bustype_defaults.push(row.bus_type_name);
            }

            let has_protocol_defaults =
                comparam_defaults::protocol_default_params(row.protocol_name).is_some();
            let allowlisted = comparam_defaults::PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS
                .iter()
                .any(|name| name.eq_ignore_ascii_case(row.protocol_name));
            if !has_protocol_defaults && !allowlisted {
                missing_protocol_defaults.push(row.protocol_name);
            }
        }

        assert!(
            missing_bustype_defaults.is_empty(),
            "every resource row's bus_type_name must resolve via bustype_default_params; \
             missing: {missing_bustype_defaults:?}"
        );
        assert!(
            missing_protocol_defaults.is_empty(),
            "every resource row's protocol_name must resolve via protocol_default_params \
             or be in comparam_defaults::PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS; missing: \
             {missing_protocol_defaults:?}"
        );

        // The allowlist itself must not contain a name that (a) doesn't
        // actually appear in the table, or (b) *does* resolve via
        // protocol_default_params (i.e. an allowlist entry that is no
        // longer needed and should be removed instead of kept around).
        for &allowlisted_name in comparam_defaults::PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS {
            assert!(
                resource_table()
                    .iter()
                    .any(|row| row.protocol_name.eq_ignore_ascii_case(allowlisted_name)),
                "allowlisted name {allowlisted_name:?} does not match any resource row"
            );
            assert!(
                comparam_defaults::protocol_default_params(allowlisted_name).is_none(),
                "allowlisted name {allowlisted_name:?} now resolves via protocol_default_params; \
                 remove it from the allowlist"
            );
        }
    }

    /// Regression guard for `names.rs::find_bustype_id_by_name`'s stated
    /// assumption: every row sharing one `bus_type_name` (case-insensitive)
    /// also shares one `bus_type_id`. That helper does a plain first-match
    /// lookup with no ambiguity handling (unlike `find_table_row_by_name`'s
    /// `protocol_name` resolution), on the strength of this invariant holding
    /// by construction today -- nothing else enforces it against a future
    /// table edit, e.g. a typo'd/duplicate `bus_type_name` added to a new row
    /// with the wrong `bus_type_id` would silently make
    /// `GetObjectId(OBJT_BUSTYPE, ...)` return a wrong answer with no error.
    #[test]
    fn every_bus_type_name_maps_to_exactly_one_bus_type_id() {
        use std::collections::HashMap;

        let mut first_seen: HashMap<String, u32> = HashMap::new();
        let mut mismatches = Vec::new();

        for row in resource_table() {
            let key = row.bus_type_name.to_ascii_lowercase();
            match first_seen.get(&key) {
                None => {
                    first_seen.insert(key, row.bus_type_id);
                }
                Some(&expected_id) if expected_id != row.bus_type_id => {
                    mismatches.push(format!(
                        "resource_id {:#06x}: bus_type_name {:?} maps to bus_type_id \
                         {:#06x}, but an earlier row with the same bus_type_name maps to \
                         {:#06x}",
                        row.resource_id, row.bus_type_name, row.bus_type_id, expected_id
                    ));
                }
                Some(_) => {}
            }
        }

        assert!(
            mismatches.is_empty(),
            "every row sharing one bus_type_name must share one bus_type_id; violations: \
             {mismatches:?}"
        );
    }

    /// Spec correction (ADR-070): the combined bus is `SAE_J1850` (0x0307,
    /// renamed from `SAE_J1850_VPW_and_SAE_J1850_PWM`) and now carries three
    /// rows -- 0x021A moved onto it from the VPW-only bus, joining 0x021C/
    /// 0x021D which were already there. The VPW-only bus (0x0306) keeps its
    /// other three rows unchanged.
    #[test]
    fn sae_j1850_bus_rename_moves_0x021a_and_keeps_vpw_bus_rows() {
        for id in [0x021A, 0x021C, 0x021D] {
            let row = find_by_resource_id(id).unwrap();
            assert_eq!(row.bus_type_id, BUSTYPE_SAE_J1850, "row {id:#06x}");
            assert_eq!(row.bus_type_name, "SAE_J1850", "row {id:#06x}");
            assert_eq!(row.dlc_pins, PINS_SAE_J1850, "row {id:#06x}");
        }

        let vpw_rows: Vec<u32> = resource_table()
            .iter()
            .filter(|r| r.bus_type_id == BUSTYPE_SAE_J1850_VPW)
            .map(|r| r.resource_id)
            .collect();
        assert_eq!(vpw_rows, vec![0x0218, 0x0219, 0x021B]);

        let sae_j1850_rows: Vec<u32> = resource_table()
            .iter()
            .filter(|r| r.bus_type_id == BUSTYPE_SAE_J1850)
            .map(|r| r.resource_id)
            .collect();
        assert_eq!(sae_j1850_rows, vec![0x021A, 0x021C, 0x021D]);
    }

    /// Pin-type spec correction: J1850 pin 2 is always `PLUS`, and the PWM/
    /// SAE_J1850 buses' pin 10 is always `MINUS` -- across all three buses
    /// that use them (VPW-only, PWM-only, and the auto-detecting SAE_J1850
    /// bus).
    #[test]
    fn j1850_pin_types_are_plus_minus_across_all_three_buses() {
        for row in resource_table() {
            for &(pin_number, pin_type) in row.dlc_pins {
                if pin_number == 2 {
                    assert_eq!(pin_type, PIN_PLUS, "row {:#06x} pin 2", row.resource_id);
                }
                if pin_number == 10 {
                    assert_eq!(pin_type, PIN_MINUS, "row {:#06x} pin 10", row.resource_id);
                }
            }
        }
    }

    /// ADR-156 Decision 2: every one of clause 6.3.1 Table 1's seven base
    /// hardware protocols maps to its `_PS` variant, and the four native SCI
    /// ids all consolidate onto the single `J2610_PS` id per the spec's own
    /// Table 1 note.
    #[test]
    fn ps_protocol_id_covers_table_1s_seven_entries() {
        assert_eq!(
            ps_protocol_id(j2534_0404::J1850VPW),
            Some(j2534_0404_sys::bindings::PROTOCOL_J1850VPW_PS)
        );
        assert_eq!(
            ps_protocol_id(j2534_0404::J1850PWM),
            Some(j2534_0404_sys::bindings::PROTOCOL_J1850PWM_PS)
        );
        assert_eq!(
            ps_protocol_id(j2534_0404::ISO9141),
            Some(j2534_0404_sys::bindings::PROTOCOL_ISO9141_PS)
        );
        assert_eq!(
            ps_protocol_id(j2534_0404::ISO14230),
            Some(j2534_0404_sys::bindings::PROTOCOL_ISO14230_PS)
        );
        assert_eq!(
            ps_protocol_id(j2534_0404::CAN),
            Some(j2534_0404_sys::bindings::PROTOCOL_CAN_PS)
        );
        assert_eq!(
            ps_protocol_id(j2534_0404::ISO15765),
            Some(j2534_0404_sys::bindings::PROTOCOL_ISO15765_PS)
        );
        for sci in [
            j2534_0404::SCI_A_ENGINE,
            j2534_0404::SCI_A_TRANS,
            j2534_0404::SCI_B_ENGINE,
            j2534_0404::SCI_B_TRANS,
        ] {
            assert_eq!(
                ps_protocol_id(sci),
                Some(j2534_0404_sys::bindings::PROTOCOL_J2610_PS),
                "hw id {sci}"
            );
        }
    }

    /// ADR-157: `base_protocol_id` is the exact inverse of `ps_protocol_id`
    /// for every base id it maps -- a future one-sided edit to either
    /// function (e.g. a new `_PS` family added to one but not the other)
    /// breaks this round trip.
    #[test]
    fn base_protocol_id_round_trips_every_ps_protocol_id_mapping() {
        for base in [
            j2534_0404::J1850VPW,
            j2534_0404::J1850PWM,
            j2534_0404::ISO9141,
            j2534_0404::ISO14230,
            j2534_0404::CAN,
            j2534_0404::ISO15765,
        ] {
            let ps = ps_protocol_id(base).expect("base id should map to a _PS id");
            assert_eq!(base_protocol_id(ps), base, "base id {base}");
        }
        // The SCI family's four native ids all consolidate onto one
        // representative id (SCI_A_ENGINE) rather than round-tripping the
        // exact variant -- deliberate, see `base_protocol_id`'s doc comment.
        assert_eq!(
            base_protocol_id(j2534_0404::PROTOCOL_J2610_PS),
            j2534_0404::SCI_A_ENGINE
        );
        // Identity for every non-`_PS` value, including every already-base
        // id.
        for base in [j2534_0404::CAN, j2534_0404::ISO15765, j2534_0404::J1850VPW] {
            assert_eq!(base_protocol_id(base), base);
        }
        assert_eq!(base_protocol_id(0xFFFF_FFFF), 0xFFFF_FFFF);
    }

    /// edge-case-hunter, PR #29 (post-approval pass): `is_ps_protocol_id`
    /// must stay in lockstep with `ps_protocol_id` -- true for every id
    /// `ps_protocol_id` produces, false for every base id and for every
    /// `_CHx` id (the two id families it must never be confused with).
    #[test]
    fn is_ps_protocol_id_matches_every_ps_protocol_id_mapping() {
        for base in [
            j2534_0404::J1850VPW,
            j2534_0404::J1850PWM,
            j2534_0404::ISO9141,
            j2534_0404::ISO14230,
            j2534_0404::CAN,
            j2534_0404::ISO15765,
            j2534_0404::SCI_A_ENGINE,
            j2534_0404::SCI_A_TRANS,
            j2534_0404::SCI_B_ENGINE,
            j2534_0404::SCI_B_TRANS,
        ] {
            let ps = ps_protocol_id(base).expect("base id should map to a _PS id");
            assert!(is_ps_protocol_id(ps), "_PS id for base {base}");
            assert!(!is_ps_protocol_id(base), "base id {base} is not itself _PS");
        }
        assert!(!is_ps_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 + 4));
        assert!(!is_ps_protocol_id(0xFFFF_FFFF));
    }

    /// ADR-156 Decision 3/Phase 2b, mirroring `base_protocol_id_round_trips_
    /// every_ps_protocol_id_mapping` above: `chx_base_protocol_id` is the
    /// exact inverse of `chx_protocol_id` for every in-scope base id, across
    /// representative indices (first, an interior, and last), and
    /// `base_protocol_id` (the ADR-157 funnel every Plane B site uses)
    /// recognizes every `_CHx` id transparently too.
    #[test]
    fn chx_protocol_id_round_trips_every_in_scope_base_protocol_id_mapping() {
        for base in [
            j2534_0404::J1850VPW,
            j2534_0404::J1850PWM,
            j2534_0404::ISO9141,
            j2534_0404::ISO14230,
            j2534_0404::CAN,
            j2534_0404::ISO15765,
        ] {
            for index in [1, 64, 128] {
                let chx = chx_protocol_id(base, index).unwrap_or_else(|| {
                    panic!("base id {base} index {index} should map to a _CHx id")
                });
                assert_eq!(
                    chx_base_protocol_id(chx),
                    Some((base, index)),
                    "base id {base} index {index}"
                );
                assert_eq!(
                    base_protocol_id(chx),
                    base,
                    "base_protocol_id should normalize _CHx id for base {base} index {index}"
                );
            }
        }

        // The SCI family's four native ids all consolidate onto one
        // representative id (SCI_A_ENGINE) rather than round-tripping the
        // exact variant -- the same accepted-residual collapse
        // `ps_protocol_id`/`base_protocol_id` already apply to `_PS`.
        for index in [1, 64, 128] {
            let chx = chx_protocol_id(j2534_0404::SCI_A_ENGINE, index)
                .expect("SCI_A_ENGINE index should map to a _CHx id");
            assert_eq!(
                chx_base_protocol_id(chx),
                Some((j2534_0404::SCI_A_ENGINE, index))
            );
            assert_eq!(base_protocol_id(chx), j2534_0404::SCI_A_ENGINE);
        }
        // Every other native SCI hardware id maps into the same
        // representative block, since `chx_protocol_id`'s input is always a
        // base id and `chx_block_base` collapses all four to one block.
        for sci in [
            j2534_0404::SCI_A_TRANS,
            j2534_0404::SCI_B_ENGINE,
            j2534_0404::SCI_B_TRANS,
        ] {
            assert_eq!(
                chx_protocol_id(sci, 1),
                chx_protocol_id(j2534_0404::SCI_A_ENGINE, 1),
                "hw id {sci}"
            );
        }

        // GM UART (ADR-189/Phase 8): keyed by its own `_PS`-only id rather
        // than a native base id (clause 11 defines no unqualified base id),
        // otherwise the same round-trip shape as the six native-base
        // families above.
        for index in [1, 64, 128] {
            let chx = chx_protocol_id(j2534_0404::PROTOCOL_GM_UART_PS, index)
                .unwrap_or_else(|| panic!("GM_UART_PS index {index} should map to a _CHx id"));
            assert_eq!(
                chx_base_protocol_id(chx),
                Some((j2534_0404::PROTOCOL_GM_UART_PS, index)),
                "GM_UART_PS index {index}"
            );
            assert_eq!(
                base_protocol_id(chx),
                j2534_0404::PROTOCOL_GM_UART_PS,
                "base_protocol_id should normalize a GM UART _CHx id for index {index}"
            );
        }

        // SAE J1939 (ADR-206): keyed by its own `_PS`-only id rather than a
        // native base id (clause 16 defines no unqualified base id either),
        // the same round-trip shape as GM UART's own block just above.
        for index in [1, 64, 128] {
            let chx = chx_protocol_id(j2534_0404::PROTOCOL_J1939_PS, index)
                .unwrap_or_else(|| panic!("J1939_PS index {index} should map to a _CHx id"));
            assert_eq!(
                chx_base_protocol_id(chx),
                Some((j2534_0404::PROTOCOL_J1939_PS, index)),
                "J1939_PS index {index}"
            );
            assert_eq!(
                base_protocol_id(chx),
                j2534_0404::PROTOCOL_J1939_PS,
                "base_protocol_id should normalize a J1939 _CHx id for index {index}"
            );
        }

        // UART Echo Byte (ADR-207): keyed by its own `_PS`-only id rather
        // than a native base id (clause 12 defines no unqualified base id
        // either), the same round-trip shape as GM UART's/SAE J1939's own
        // blocks just above.
        for index in [1, 64, 128] {
            let chx = chx_protocol_id(j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS, index)
                .unwrap_or_else(|| {
                    panic!("UART_ECHO_BYTE_PS index {index} should map to a _CHx id")
                });
            assert_eq!(
                chx_base_protocol_id(chx),
                Some((j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS, index)),
                "UART_ECHO_BYTE_PS index {index}"
            );
            assert_eq!(
                base_protocol_id(chx),
                j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
                "base_protocol_id should normalize a UART Echo Byte _CHx id for index {index}"
            );
        }

        // Out-of-range index: rejected regardless of protocol.
        assert_eq!(chx_protocol_id(j2534_0404::CAN, 0), None);
        assert_eq!(chx_protocol_id(j2534_0404::CAN, 129), None);

        // Out-of-scope base protocol (no in-scope _CHx block at all): rejected.
        // Uses Analog Inputs -- ADR-213's own Context confirms Analog Inputs
        // (clause 10) is the one remaining family structurally out of `_CHx`
        // scope (it enumerates its 32 channels directly, with no base/`_CHx`
        // relationship for the mechanism to model at all), now that CAN FD/
        // ISO15765-on-CAN-FD were brought into scope by ADR-213 itself; this
        // test's own prior example (`PROTOCOL_FD_CAN_PS`) was moved into
        // scope by that same round, so it was swapped here.
        assert_eq!(
            chx_protocol_id(j2534_0404_sys::bindings::PROTOCOL_ANALOG_IN_1, 1),
            None
        );

        // `chx_base_protocol_id` rejects a raw id outside every in-scope
        // block, including a family with no `_CHx` block at all
        // (`is_chx_protocol_id` alone recognizes the *vocabulary region* --
        // see its own test -- Analog Inputs' native ids aren't even inside
        // that region, confirming it never gets a block to begin with). Uses
        // an Analog Inputs id -- confirmed still out of scope by ADR-213's
        // own Context (CAN FD's own `_CHx` block, this test's prior example,
        // was moved into scope by that same round).
        assert_eq!(
            chx_base_protocol_id(j2534_0404_sys::bindings::PROTOCOL_ANALOG_IN_1),
            None
        );
        assert_eq!(chx_base_protocol_id(0xFFFF_FFFF), None);
    }

    /// `is_chx_protocol_id` recognizes the full clause-24/26 `_CHx` region --
    /// all 18 protocol families' blocks (19 total native blocks, three of the
    /// eighteen families -- FT-CAN, SW-CAN, CAN FD -- each splitting into two
    /// blocks, ADR-213 Consequences), not just the in-scope ones -- so the
    /// clause-5 opt-in gate is hit before resolution even for an
    /// out-of-scope family.
    #[test]
    fn is_chx_protocol_id_covers_the_full_region_not_just_in_scope_families() {
        assert!(is_chx_protocol_id(j2534_0404::PROTOCOL_CAN_CH1));
        assert!(is_chx_protocol_id(
            j2534_0404_sys::bindings::PROTOCOL_FD_ISO15765_CH128
        ));
        // An in-scope family's interior/last id.
        assert!(is_chx_protocol_id(
            chx_protocol_id(j2534_0404::CAN, 64).unwrap()
        ));
        // CAN FD/ISO15765-on-CAN-FD are now in-scope families as of ADR-213
        // (all 18 protocol families are), so these ids are inside the full
        // region for the same reason every other in-scope family's ids are
        // -- membership here isn't a proxy for "implemented" one way or the
        // other, it's just the vocabulary-region boundary check.
        assert!(is_chx_protocol_id(
            j2534_0404_sys::bindings::PROTOCOL_FD_CAN_CH1
        ));
        assert!(is_chx_protocol_id(
            j2534_0404_sys::bindings::PROTOCOL_FD_ISO15765_CH1
        ));
        // Outside the region entirely.
        assert!(!is_chx_protocol_id(j2534_0404::CAN));
        assert!(!is_chx_protocol_id(j2534_0404::PROTOCOL_CAN_PS));
        assert!(!is_chx_protocol_id(j2534_0404::PROTOCOL_CAN_CH1 - 1));
        assert!(!is_chx_protocol_id(
            j2534_0404_sys::bindings::PROTOCOL_FD_ISO15765_CH128 + 1
        ));
        assert!(!is_chx_protocol_id(0xFFFF_FFFF));
    }

    /// SAE J2534-2 clause 8.1 (ADR-160/Phase 3c): `mixed_format_can_protocol_id`
    /// maps an ISO15765-family hardware id to its paired raw-CAN id, across
    /// the unqualified base id, the `_PS` variant, and a representative `_CHx`
    /// variant.
    #[test]
    fn mixed_format_can_protocol_id_maps_iso15765_family_to_paired_can_id() {
        assert_eq!(
            mixed_format_can_protocol_id(j2534_0404::ISO15765),
            j2534_0404::CAN
        );
        assert_eq!(
            mixed_format_can_protocol_id(j2534_0404::PROTOCOL_ISO15765_PS),
            j2534_0404::PROTOCOL_CAN_PS
        );
        let iso15765_ch5 = chx_protocol_id(j2534_0404::ISO15765, 5)
            .expect("ISO15765 index 5 should map to a _CHx id");
        let can_ch5 =
            chx_protocol_id(j2534_0404::CAN, 5).expect("CAN index 5 should map to a _CHx id");
        assert_eq!(mixed_format_can_protocol_id(iso15765_ch5), can_ch5);
    }

    /// Out-of-scope protocols (ADR-156 Consequences' "Accepted residual" --
    /// e.g. SW_CAN, J1939, which have no plain/native J2534-1 hardware id at
    /// all, only `_PS`/`_CHx` variants) have no `_PS` mapping in this phase;
    /// an unrecognized/legacy-extended id is likewise `None`.
    #[test]
    fn ps_protocol_id_returns_none_outside_table_1s_scope() {
        // A `_PS` id itself is not a valid *input* (inputs are native/base
        // hardware ids) -- passing one through confirms it isn't spuriously
        // matched by the match arms above.
        assert_eq!(
            ps_protocol_id(j2534_0404_sys::bindings::PROTOCOL_SW_CAN_PS),
            None
        );
        assert_eq!(
            ps_protocol_id(j2534_0404_sys::bindings::PROTOCOL_J1939_PS),
            None
        );
        assert_eq!(ps_protocol_id(0xFFFF_FFFF), None);
    }

    /// `default_dlc_pins_for_hw_protocol` covers exactly the same hardware
    /// ids `ps_protocol_id` does, and its answers match the resource table's
    /// own default rows for the same protocol (e.g. `find_by_resource_id
    /// (0x0201)` -- `ISO_11898_RAW`, the CAN row with no override).
    #[test]
    fn default_dlc_pins_for_hw_protocol_matches_table_defaults() {
        assert_eq!(
            default_dlc_pins_for_hw_protocol(j2534_0404::CAN),
            Some(find_by_resource_id(0x0201).unwrap().dlc_pins)
        );
        assert_eq!(
            default_dlc_pins_for_hw_protocol(j2534_0404::ISO15765),
            Some(PINS_ISO_11898_2_DWCAN)
        );
        assert_eq!(
            default_dlc_pins_for_hw_protocol(j2534_0404::J1850VPW),
            Some(PINS_SAE_J1850_VPW)
        );
        assert_eq!(
            default_dlc_pins_for_hw_protocol(j2534_0404_sys::bindings::PROTOCOL_SW_CAN_PS),
            None
        );
    }

    /// ADR-201: `secondary_pin_requirement` covers exactly the same ten
    /// hardware ids `ps_protocol_id` does (`None` elsewhere), and its answer
    /// is internally consistent with `default_dlc_pins_for_hw_protocol`'s
    /// own default-pins table length for the same id --
    /// `NeverPresent` <-> a 1-pin default table, `Required`/`Optional` <-> a
    /// 2-pin default table.
    #[test]
    fn secondary_pin_requirement_matches_ps_protocol_id_scope_and_default_pins_length() {
        for hw_protocol_id in [
            j2534_0404::J1850VPW,
            j2534_0404::J1850PWM,
            j2534_0404::ISO9141,
            j2534_0404::ISO14230,
            j2534_0404::CAN,
            j2534_0404::ISO15765,
            j2534_0404::SCI_A_ENGINE,
            j2534_0404::SCI_A_TRANS,
            j2534_0404::SCI_B_ENGINE,
            j2534_0404::SCI_B_TRANS,
        ] {
            let requirement = secondary_pin_requirement(hw_protocol_id)
                .unwrap_or_else(|| panic!("hw id {hw_protocol_id} should have a requirement"));
            assert!(
                ps_protocol_id(hw_protocol_id).is_some(),
                "hw id {hw_protocol_id}: secondary_pin_requirement covers it but ps_protocol_id \
                 does not"
            );
            let default_len = default_dlc_pins_for_hw_protocol(hw_protocol_id)
                .unwrap_or_else(|| panic!("hw id {hw_protocol_id} should have default pins"))
                .len();
            match requirement {
                SecondaryPinRequirement::NeverPresent => {
                    assert_eq!(
                        default_len, 1,
                        "hw id {hw_protocol_id}: NeverPresent should pair with a 1-pin default"
                    );
                }
                SecondaryPinRequirement::Required | SecondaryPinRequirement::Optional => {
                    assert_eq!(
                        default_len, 2,
                        "hw id {hw_protocol_id}: Required/Optional should pair with a 2-pin \
                         default"
                    );
                }
            }
        }

        // `ps_protocol_id`'s own out-of-scope ids must be `None` here too.
        assert_eq!(
            secondary_pin_requirement(j2534_0404_sys::bindings::PROTOCOL_SW_CAN_PS),
            None
        );
        assert_eq!(secondary_pin_requirement(0xFFFF_FFFF), None);
    }

    // ── ADR-158/Phase 3a: CAN FD (`fd_protocol_id`/`is_fd_protocol_id`) ─────

    /// `fd_protocol_id`/`is_fd_protocol_id`/`base_protocol_id`'s FD arms
    /// round-trip for both CAN and ISO15765 (ADR-158/ADR-159), mirroring
    /// `base_protocol_id_round_trips_every_ps_protocol_id_mapping`'s style;
    /// `fd_protocol_id` is `None` for every other base id.
    #[test]
    fn fd_protocol_id_round_trips_for_can_and_iso15765_and_is_none_elsewhere() {
        let fd_can = fd_protocol_id(j2534_0404::CAN).expect("CAN should map to a CAN FD _PS id");
        assert_eq!(fd_can, j2534_0404::PROTOCOL_FD_CAN_PS);
        assert!(is_fd_protocol_id(fd_can));
        assert_eq!(base_protocol_id(fd_can), j2534_0404::CAN);

        let fd_iso15765 = fd_protocol_id(j2534_0404::ISO15765)
            .expect("ISO15765 should map to an ISO15765 FD _PS id");
        assert_eq!(fd_iso15765, j2534_0404::PROTOCOL_FD_ISO15765_PS);
        assert!(is_fd_protocol_id(fd_iso15765));
        assert_eq!(base_protocol_id(fd_iso15765), j2534_0404::ISO15765);

        for base in [
            j2534_0404::J1850VPW,
            j2534_0404::J1850PWM,
            j2534_0404::ISO9141,
            j2534_0404::ISO14230,
            j2534_0404::SCI_A_ENGINE,
        ] {
            assert_eq!(fd_protocol_id(base), None, "base id {base}");
        }
    }

    /// `is_fd_protocol_id` is `true` for both `_PS` ids and both families'
    /// full `_CH1..128` ranges (ADR-213 Decision item 2's in-place widening),
    /// and `false` for every base/`_PS`/`_CHx` id outside those -- mirrors
    /// `is_ps_protocol_id_matches_every_ps_protocol_id_mapping`'s
    /// lockstep-with-neighboring-families check.
    #[test]
    fn is_fd_protocol_id_false_for_every_other_family() {
        assert!(!is_fd_protocol_id(j2534_0404::CAN));
        assert!(!is_fd_protocol_id(j2534_0404::ISO15765));
        assert!(!is_fd_protocol_id(j2534_0404::PROTOCOL_CAN_PS));
        assert!(!is_fd_protocol_id(j2534_0404::PROTOCOL_ISO15765_PS));
        assert!(!is_fd_protocol_id(j2534_0404::PROTOCOL_CAN_CH1));
        assert!(!is_fd_protocol_id(0xFFFF_FFFF));

        // ADR-213: the widened predicate's positive `_CHx` cases.
        assert!(is_fd_protocol_id(j2534_0404::PROTOCOL_FD_CAN_PS));
        assert!(is_fd_protocol_id(j2534_0404::PROTOCOL_FD_ISO15765_PS));
        assert!(is_fd_protocol_id(
            j2534_0404_sys::bindings::PROTOCOL_FD_CAN_CH1
        ));
        assert!(is_fd_protocol_id(
            j2534_0404_sys::bindings::PROTOCOL_FD_CAN_CH128
        ));
        assert!(is_fd_protocol_id(
            j2534_0404_sys::bindings::PROTOCOL_FD_ISO15765_CH1
        ));
        assert!(is_fd_protocol_id(
            j2534_0404_sys::bindings::PROTOCOL_FD_ISO15765_CH128
        ));
        // A neighboring family's own `_CHx` id must still be false.
        assert!(!is_fd_protocol_id(
            j2534_0404_sys::bindings::PROTOCOL_FT_CAN_CH1
        ));
    }

    /// `default_pin_select_for_base` packs CAN's own default pins (6/HI,
    /// 14/LOW) into the same `0x0000PPSS` format
    /// `names::J2534Service::compute_pin_select` produces -- `0x060E`, the
    /// exact value cited by this module's own doc comments elsewhere
    /// (`ResourceDef` module doc, `SAE_J2610_on_SAE_J2610_SCI` history).
    /// `None` for a hardware id with no default pins at all.
    #[test]
    fn default_pin_select_for_base_packs_can_defaults() {
        assert_eq!(
            default_pin_select_for_base(j2534_0404::CAN),
            Some(0x0000_060E)
        );
        assert_eq!(
            default_pin_select_for_base(j2534_0404_sys::bindings::PROTOCOL_SW_CAN_PS),
            None
        );
    }

    // ── ADR-168/Phase 6: Fault-Tolerant CAN (`is_ft_protocol_id`) ───────────

    /// `is_ft_protocol_id` is `true` only for its two own ids (mirrors
    /// `is_fd_protocol_id_false_for_every_other_family`'s
    /// lockstep-with-neighboring-families check), and `base_protocol_id`'s
    /// new FT arms round-trip both to their CAN/ISO15765 base family, the
    /// same reasoning `base_protocol_id_round_trips_every_ps_protocol_id_mapping`
    /// pins for the seven Table 1 families.
    #[test]
    fn is_ft_protocol_id_true_only_for_its_two_own_ids_and_base_protocol_id_round_trips() {
        assert!(is_ft_protocol_id(j2534_0404::PROTOCOL_FT_CAN_PS));
        assert!(is_ft_protocol_id(j2534_0404::PROTOCOL_FT_ISO15765_PS));
        assert!(!is_ft_protocol_id(j2534_0404::CAN));
        assert!(!is_ft_protocol_id(j2534_0404::ISO15765));
        assert!(!is_ft_protocol_id(j2534_0404::PROTOCOL_CAN_PS));
        assert!(!is_ft_protocol_id(j2534_0404::PROTOCOL_SW_CAN_PS));
        assert!(!is_ft_protocol_id(j2534_0404::PROTOCOL_SW_ISO15765_PS));
        assert!(!is_ft_protocol_id(j2534_0404::PROTOCOL_CAN_CH1));
        assert!(!is_ft_protocol_id(0xFFFF_FFFF));

        assert_eq!(
            base_protocol_id(j2534_0404::PROTOCOL_FT_CAN_PS),
            j2534_0404::CAN
        );
        assert_eq!(
            base_protocol_id(j2534_0404::PROTOCOL_FT_ISO15765_PS),
            j2534_0404::ISO15765
        );
    }

    // ── ADR-211: SAE J2534-2 clause 7 Additional Channels for Fault-Tolerant
    // CAN (`is_ft_family_protocol_id`) ───────────────────────────────────────

    /// `is_ft_family_protocol_id` is `true` for either `_PS` id AND the full
    /// `_CH1..128` range of EACH of the two families (mirroring
    /// `is_j1708_family_protocol_id`'s/`is_tp2_0_family_protocol_id`'s shape,
    /// doubled since Fault-Tolerant CAN has two `_PS`/block pairs instead of
    /// one), while the narrower `is_ft_protocol_id` stays a strict
    /// two-value, `_PS`-only check -- the two predicates must disagree on a
    /// `_CHx` id, since that's the entire reason the family-wide predicate
    /// exists (ADR-211's own investigation found `connect_discovery_check`/
    /// `bustype_default_name_for_hw_protocol_id`/`rpc_link.rs`'s
    /// `apply_fd_mode`/`GetResourceStatus`/`GetConflictingResources` sites
    /// all needing this widening).
    #[test]
    fn is_ft_family_protocol_id_true_for_either_ps_and_the_full_chx_ranges() {
        assert!(is_ft_family_protocol_id(j2534_0404::PROTOCOL_FT_CAN_PS));
        assert!(is_ft_family_protocol_id(
            j2534_0404::PROTOCOL_FT_ISO15765_PS
        ));
        assert!(is_ft_family_protocol_id(j2534_0404::PROTOCOL_FT_CAN_CH1));
        assert!(is_ft_family_protocol_id(j2534_0404::PROTOCOL_FT_CAN_CH128));
        assert!(is_ft_family_protocol_id(
            j2534_0404::PROTOCOL_FT_CAN_CH1 + 64 // an interior _CHx id
        ));
        assert!(is_ft_family_protocol_id(
            j2534_0404::PROTOCOL_FT_ISO15765_CH1
        ));
        assert!(is_ft_family_protocol_id(
            j2534_0404::PROTOCOL_FT_ISO15765_CH128
        ));
        assert!(is_ft_family_protocol_id(
            j2534_0404::PROTOCOL_FT_ISO15765_CH1 + 64 // an interior _CHx id
        ));
        assert!(!is_ft_family_protocol_id(
            j2534_0404::PROTOCOL_FT_CAN_CH1 - 1
        ));
        // NOTE: `PROTOCOL_FT_CAN_CH128 + 1` is NOT a valid "just past the
        // block" probe here -- the vendor header places the two FT `_CHx`
        // blocks back-to-back (`FT_CAN_CH128` at 0x94FF, `FT_ISO15765_CH1` at
        // 0x9500), so that value IS `PROTOCOL_FT_ISO15765_CH1`, correctly
        // recognized by this same predicate's other range. Use a value past
        // the ISO15765 block's own end instead.
        assert!(!is_ft_family_protocol_id(
            j2534_0404::PROTOCOL_FT_ISO15765_CH128 + 1
        ));
        assert!(!is_ft_family_protocol_id(j2534_0404::CAN));
        assert!(!is_ft_family_protocol_id(j2534_0404::ISO15765));
        assert!(!is_ft_family_protocol_id(j2534_0404::PROTOCOL_SW_CAN_PS));
        assert!(!is_ft_family_protocol_id(0xFFFF_FFFF));

        // The narrower, arm-gate-only predicate must NOT recognize a `_CHx`
        // id -- that's the exact gap `is_ft_family_protocol_id` was added to
        // close for its own (different) call sites.
        assert!(is_ft_protocol_id(j2534_0404::PROTOCOL_FT_CAN_PS));
        assert!(!is_ft_protocol_id(j2534_0404::PROTOCOL_FT_CAN_CH1));
    }

    /// Regression test for ADR-211: `chx_base_protocol_id`/`chx_block_base`
    /// key Fault-Tolerant CAN's two blocks by their own `_PS` id, not the
    /// true `CAN`/`ISO15765` base -- so without the `base_protocol_id`
    /// recursion fix (ADR-211 Decision item 2),
    /// `base_protocol_id(chx_protocol_id(PROTOCOL_FT_CAN_PS, N).unwrap())`
    /// would incorrectly equal `PROTOCOL_FT_CAN_PS` rather than `CAN`. This is
    /// a dedicated test, not an extension of
    /// `chx_protocol_id_round_trips_every_in_scope_base_protocol_id_mapping`
    /// above, since that test's own GM UART/SAE J1939/UART Echo Byte blocks
    /// assert the OPPOSITE shape (`base_protocol_id(chx) == their own _PS`
    /// id, since those three families have no true native base at all) --
    /// mixing the two assertion shapes into one test would be confusing.
    #[test]
    fn base_protocol_id_recurses_through_a_ft_chx_id_to_the_true_can_iso15765_base() {
        for index in [1, 64, 128] {
            let chx = chx_protocol_id(j2534_0404::PROTOCOL_FT_CAN_PS, index)
                .unwrap_or_else(|| panic!("FT_CAN_PS index {index} should map to a _CHx id"));
            assert_eq!(
                chx_base_protocol_id(chx),
                Some((j2534_0404::PROTOCOL_FT_CAN_PS, index)),
                "FT_CAN_PS index {index}: chx_base_protocol_id should resolve to the _PS id, \
                 not the true base -- base_protocol_id is what recurses further"
            );
            assert_eq!(
                base_protocol_id(chx),
                j2534_0404::CAN,
                "base_protocol_id should recurse a FT_CAN _CHx id all the way to CAN, not stop \
                 at PROTOCOL_FT_CAN_PS, for index {index}"
            );

            let chx_iso15765 = chx_protocol_id(j2534_0404::PROTOCOL_FT_ISO15765_PS, index)
                .unwrap_or_else(|| panic!("FT_ISO15765_PS index {index} should map to a _CHx id"));
            assert_eq!(
                chx_base_protocol_id(chx_iso15765),
                Some((j2534_0404::PROTOCOL_FT_ISO15765_PS, index)),
                "FT_ISO15765_PS index {index}"
            );
            assert_eq!(
                base_protocol_id(chx_iso15765),
                j2534_0404::ISO15765,
                "base_protocol_id should recurse a FT_ISO15765 _CHx id all the way to ISO15765, \
                 not stop at PROTOCOL_FT_ISO15765_PS, for index {index}"
            );
        }
    }

    /// Regression test (Codex review correction, PR #124, ADR-211): before
    /// this fix, `chx_device_info_supported_parameter` was keyed on the
    /// fully-normalized `base_protocol_id`, so once `base_protocol_id`'s own
    /// recursion fix (the test just above) correctly widened to collapse a
    /// Fault-Tolerant CAN `_PS`/`_CHx` id all the way to plain `CAN`/
    /// `ISO15765`, this function started incorrectly consulting
    /// `DEVICE_INFO_CAN_SUPPORTED`/`DEVICE_INFO_ISO15765_SUPPORTED` instead
    /// of the FT-specific flag that actually governs FT-CAN's own `_CHx`
    /// capacity -- exactly the class of bug `connect_discovery_check`'s own
    /// doc comment already documents. Mirrors that function's sibling
    /// regression test above (`connect_discovery_check_covers_ft_can_chx_the
    /// _same_as_its_ps_sibling`) but for this function's own contract.
    #[test]
    fn chx_device_info_supported_parameter_keys_ft_can_by_the_ft_flag_not_the_generic_can_flag() {
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_FT_CAN_PS),
            Some(j2534_0404::DEVICE_INFO_FT_CAN_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_FT_CAN_CH1),
            Some(j2534_0404::DEVICE_INFO_FT_CAN_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_FT_ISO15765_PS),
            Some(j2534_0404::DEVICE_INFO_FT_ISO15765_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_FT_ISO15765_CH1),
            Some(j2534_0404::DEVICE_INFO_FT_ISO15765_SUPPORTED)
        );
        // The 7 ADR-156-original families must be unaffected, including when
        // passed their own raw `_CHx` id directly (not just their bare base).
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::CAN),
            Some(j2534_0404::DEVICE_INFO_CAN_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_CAN_CH1),
            Some(j2534_0404::DEVICE_INFO_CAN_SUPPORTED)
        );
        // Out-of-scope family (GM UART _PS, ADR-189 Decision 5's accepted
        // residual): still None.
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_GM_UART_PS),
            None
        );
    }

    /// Regression test for ADR-211: before the fix, `connect_discovery_check`
    /// matched Fault-Tolerant CAN via two bare exact-match arms on
    /// `PROTOCOL_FT_CAN_PS`/`PROTOCOL_FT_ISO15765_PS` alone, so a `_CHx`
    /// Additional Channels connect would have silently returned `None` --
    /// skipping the ADR-185 Stage-1 Discovery fail-fast gate the `_PS` route
    /// still enforces. Mirrors
    /// `connect_discovery_check_covers_tp2_0_chx_the_same_as_its_ps_sibling`'s
    /// own assertion shape, but covers both families' own `_PS`/`_CHx` sets
    /// separately since each needs its own distinct `DeviceFlag` parameter.
    #[test]
    fn connect_discovery_check_covers_ft_can_chx_the_same_as_its_ps_sibling() {
        for proto_id in [
            j2534_0404::PROTOCOL_FT_CAN_PS,
            j2534_0404::PROTOCOL_FT_CAN_CH1,
            j2534_0404::PROTOCOL_FT_CAN_CH128,
        ] {
            let check = connect_discovery_check(proto_id).unwrap_or_else(|| {
                panic!(
                    "protocol id {proto_id:#010x} should have a Stage 1 Discovery check \
                     (ADR-211)"
                )
            });
            match check {
                DiscoveryCheck::DeviceFlag {
                    parameter,
                    input_value,
                } => {
                    assert_eq!(
                        parameter,
                        j2534_0404::DEVICE_INFO_FT_CAN_SUPPORTED,
                        "protocol id {proto_id:#010x} mapped to the wrong DEVICE_INFO_* \
                         parameter"
                    );
                    assert_eq!(input_value, 0, "no Stage 1 row is per-pin");
                }
                _ => panic!("expected DeviceFlag for {proto_id:#010x}"),
            }
        }

        for proto_id in [
            j2534_0404::PROTOCOL_FT_ISO15765_PS,
            j2534_0404::PROTOCOL_FT_ISO15765_CH1,
            j2534_0404::PROTOCOL_FT_ISO15765_CH128,
        ] {
            let check = connect_discovery_check(proto_id).unwrap_or_else(|| {
                panic!(
                    "protocol id {proto_id:#010x} should have a Stage 1 Discovery check \
                     (ADR-211)"
                )
            });
            match check {
                DiscoveryCheck::DeviceFlag {
                    parameter,
                    input_value,
                } => {
                    assert_eq!(
                        parameter,
                        j2534_0404::DEVICE_INFO_FT_ISO15765_SUPPORTED,
                        "protocol id {proto_id:#010x} mapped to the wrong DEVICE_INFO_* \
                         parameter"
                    );
                    assert_eq!(input_value, 0, "no Stage 1 row is per-pin");
                }
                _ => panic!("expected DeviceFlag for {proto_id:#010x}"),
            }
        }
    }

    // ── ADR-212/Round 2: Single Wire CAN (`is_sw_family_protocol_id`) ──────

    /// Mirrors `is_ft_family_protocol_id_true_for_either_ps_and_the_full_chx_
    /// ranges`, but for Single Wire CAN's own two `_PS`/block pairs (ADR-212).
    #[test]
    fn is_sw_family_protocol_id_true_for_either_ps_and_the_full_chx_ranges() {
        assert!(is_sw_family_protocol_id(j2534_0404::PROTOCOL_SW_CAN_PS));
        assert!(is_sw_family_protocol_id(
            j2534_0404::PROTOCOL_SW_ISO15765_PS
        ));
        assert!(is_sw_family_protocol_id(
            j2534_0404::PROTOCOL_SW_CAN_CAN_CH1
        ));
        assert!(is_sw_family_protocol_id(
            j2534_0404::PROTOCOL_SW_CAN_CAN_CH128
        ));
        assert!(is_sw_family_protocol_id(
            j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1
        ));
        assert!(is_sw_family_protocol_id(
            j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH128
        ));
        // An interior id from each range.
        assert!(is_sw_family_protocol_id(
            j2534_0404::PROTOCOL_SW_CAN_CAN_CH1 + 10
        ));
        assert!(is_sw_family_protocol_id(
            j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1 + 10
        ));

        assert!(!is_sw_family_protocol_id(
            j2534_0404::PROTOCOL_SW_CAN_CAN_CH1 - 1
        ));
        assert!(!is_sw_family_protocol_id(
            j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH128 + 1
        ));
        assert!(!is_sw_family_protocol_id(j2534_0404::CAN));
        assert!(!is_sw_family_protocol_id(j2534_0404::ISO15765));
        assert!(!is_sw_family_protocol_id(j2534_0404::PROTOCOL_FT_CAN_PS));
        assert!(!is_sw_family_protocol_id(0xFFFF_FFFF));

        // The narrower, arm-gate-only predicate must NOT recognize a `_CHx`
        // id -- that's the exact gap `is_sw_family_protocol_id` was added to
        // close for its own (different) call sites.
        assert!(is_sw_protocol_id(j2534_0404::PROTOCOL_SW_CAN_PS));
        assert!(!is_sw_protocol_id(j2534_0404::PROTOCOL_SW_CAN_CAN_CH1));
    }

    /// Mirrors `base_protocol_id_recurses_through_a_ft_chx_id_to_the_true_can_
    /// iso15765_base`, but for Single Wire CAN: `chx_base_protocol_id`/
    /// `chx_block_base` key SW-CAN's two blocks by their own `_PS` id, not
    /// the true `CAN`/`ISO15765` base, so without `base_protocol_id`'s own
    /// recursion (ADR-211 Decision item 2, inherited unchanged by this round)
    /// `base_protocol_id(chx_protocol_id(PROTOCOL_SW_CAN_PS, N).unwrap())`
    /// would incorrectly equal `PROTOCOL_SW_CAN_PS` rather than `CAN`.
    #[test]
    fn base_protocol_id_recurses_through_a_sw_chx_id_to_the_true_can_iso15765_base() {
        for index in [1, 64, 128] {
            let chx = chx_protocol_id(j2534_0404::PROTOCOL_SW_CAN_PS, index)
                .unwrap_or_else(|| panic!("SW_CAN_PS index {index} should map to a _CHx id"));
            assert_eq!(
                chx_base_protocol_id(chx),
                Some((j2534_0404::PROTOCOL_SW_CAN_PS, index)),
                "SW_CAN_PS index {index}: chx_base_protocol_id should resolve to the _PS id, \
                 not the true base -- base_protocol_id is what recurses further"
            );
            assert_eq!(
                base_protocol_id(chx),
                j2534_0404::CAN,
                "base_protocol_id should recurse a SW_CAN _CHx id all the way to CAN, not stop \
                 at PROTOCOL_SW_CAN_PS, for index {index}"
            );

            let chx_iso15765 = chx_protocol_id(j2534_0404::PROTOCOL_SW_ISO15765_PS, index)
                .unwrap_or_else(|| panic!("SW_ISO15765_PS index {index} should map to a _CHx id"));
            assert_eq!(
                chx_base_protocol_id(chx_iso15765),
                Some((j2534_0404::PROTOCOL_SW_ISO15765_PS, index)),
                "SW_ISO15765_PS index {index}"
            );
            assert_eq!(
                base_protocol_id(chx_iso15765),
                j2534_0404::ISO15765,
                "base_protocol_id should recurse a SW_ISO15765 _CHx id all the way to ISO15765, \
                 not stop at PROTOCOL_SW_ISO15765_PS, for index {index}"
            );
        }
    }

    /// Mirrors `chx_device_info_supported_parameter_keys_ft_can_by_the_ft_
    /// flag_not_the_generic_can_flag`, but for Single Wire CAN (ADR-212).
    #[test]
    fn chx_device_info_supported_parameter_keys_sw_can_by_the_sw_flag_not_the_generic_can_flag() {
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_SW_CAN_PS),
            Some(j2534_0404::DEVICE_INFO_SW_CAN_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_SW_CAN_CAN_CH1),
            Some(j2534_0404::DEVICE_INFO_SW_CAN_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_SW_ISO15765_PS),
            Some(j2534_0404::DEVICE_INFO_SW_ISO15765_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1),
            Some(j2534_0404::DEVICE_INFO_SW_ISO15765_SUPPORTED)
        );
        // The 7 ADR-156-original families must be unaffected.
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::CAN),
            Some(j2534_0404::DEVICE_INFO_CAN_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_CAN_CH1),
            Some(j2534_0404::DEVICE_INFO_CAN_SUPPORTED)
        );
        // Out-of-scope family (GM UART _PS, ADR-189 Decision 5's accepted
        // residual): still None.
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_GM_UART_PS),
            None
        );
    }

    /// Regression test for the mechanical extension of ADR-211's/ADR-212's
    /// established pattern: before this fix, `chx_device_info_supported_
    /// parameter` had no arm at all for these five families, so
    /// `check_chx_capacity` silently no-opped (never enforced the SAE
    /// J2534-2 clause 7 `_CHx` channel-count cap) for any of them. Asserts
    /// both the `_PS` id and the `_CH1` id resolve to the correct
    /// `DEVICE_INFO_*_SUPPORTED` flag for each family, and that GM UART's
    /// own `_PS` id still maps to `None` -- pinning down that its exclusion
    /// (ADR-189 Decision 5) is deliberate, not a future accidental gap.
    #[test]
    fn chx_device_info_supported_parameter_covers_the_five_previously_uncovered_chx_families() {
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_J1939_PS),
            Some(j2534_0404::DEVICE_INFO_J1939_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_J1939_CH1),
            Some(j2534_0404::DEVICE_INFO_J1939_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS),
            Some(j2534_0404::DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_ECHO_BYTE_CH1),
            Some(j2534_0404::DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_HONDA_DIAGH_PS),
            Some(j2534_0404::DEVICE_INFO_HONDA_DIAGH_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_HONDA_DIAGH_CH1),
            Some(j2534_0404::DEVICE_INFO_HONDA_DIAGH_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_J1708_PS),
            Some(j2534_0404::DEVICE_INFO_J1708_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_J1708_CH1),
            Some(j2534_0404::DEVICE_INFO_J1708_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_TP2_0_PS),
            Some(j2534_0404::DEVICE_INFO_TP2_0_SUPPORTED)
        );
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_TP2_0_CH1),
            Some(j2534_0404::DEVICE_INFO_TP2_0_SUPPORTED)
        );
        // GM UART stays a deliberately accepted residual (ADR-189 Decision
        // 5), not something this fix touches.
        assert_eq!(
            chx_device_info_supported_parameter(j2534_0404::PROTOCOL_GM_UART_PS),
            None
        );
    }

    /// Mirrors `connect_discovery_check_covers_ft_can_chx_the_same_as_its_ps_
    /// sibling`, but for Single Wire CAN (ADR-212): before this round,
    /// `connect_discovery_check` matched SWCAN via a single bare exact-match
    /// arm, so a `_CHx` Additional Channels connect would have silently
    /// returned `None` -- skipping the ADR-185 Stage-1 Discovery fail-fast
    /// gate the `_PS` route still enforces.
    #[test]
    fn connect_discovery_check_covers_sw_can_chx_the_same_as_its_ps_sibling() {
        for proto_id in [
            j2534_0404::PROTOCOL_SW_CAN_PS,
            j2534_0404::PROTOCOL_SW_CAN_CAN_CH1,
            j2534_0404::PROTOCOL_SW_CAN_CAN_CH128,
        ] {
            let check = connect_discovery_check(proto_id).unwrap_or_else(|| {
                panic!(
                    "protocol id {proto_id:#010x} should have a Stage 1 Discovery check \
                     (ADR-212)"
                )
            });
            match check {
                DiscoveryCheck::DeviceFlag {
                    parameter,
                    input_value,
                } => {
                    assert_eq!(
                        parameter,
                        j2534_0404::DEVICE_INFO_SW_CAN_SUPPORTED,
                        "protocol id {proto_id:#010x} mapped to the wrong DEVICE_INFO_* \
                         parameter"
                    );
                    assert_eq!(input_value, 0, "no Stage 1 row is per-pin");
                }
                _ => panic!("expected DeviceFlag for {proto_id:#010x}"),
            }
        }

        for proto_id in [
            j2534_0404::PROTOCOL_SW_ISO15765_PS,
            j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH1,
            j2534_0404::PROTOCOL_SW_CAN_ISO15765_CH128,
        ] {
            let check = connect_discovery_check(proto_id).unwrap_or_else(|| {
                panic!(
                    "protocol id {proto_id:#010x} should have a Stage 1 Discovery check \
                     (ADR-212)"
                )
            });
            match check {
                DiscoveryCheck::DeviceFlag {
                    parameter,
                    input_value,
                } => {
                    assert_eq!(
                        parameter,
                        j2534_0404::DEVICE_INFO_SW_ISO15765_SUPPORTED,
                        "protocol id {proto_id:#010x} mapped to the wrong DEVICE_INFO_* \
                         parameter"
                    );
                    assert_eq!(input_value, 0, "no Stage 1 row is per-pin");
                }
                _ => panic!("expected DeviceFlag for {proto_id:#010x}"),
            }
        }
    }

    // ── ADR-174/Phase 10: Honda DIAG-H (`is_honda_diagh_protocol_id`) ───────

    /// `is_honda_diagh_protocol_id` is `true` only for its own id, and
    /// `base_protocol_id` leaves it unchanged (identity fallback, no
    /// dedicated arm needed -- mirrors `is_uart_echo_byte_protocol_id`'s own
    /// shape, which has no dedicated round-trip test of its own since it too
    /// relies purely on the identity fallback).
    #[test]
    fn is_honda_diagh_protocol_id_true_only_for_its_own_id_and_base_protocol_id_is_identity() {
        assert!(is_honda_diagh_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_PS
        ));
        assert!(!is_honda_diagh_protocol_id(
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        ));
        assert!(!is_honda_diagh_protocol_id(j2534_0404::CAN));
        assert!(!is_honda_diagh_protocol_id(j2534_0404::ISO15765));
        assert!(!is_honda_diagh_protocol_id(j2534_0404::PROTOCOL_CAN_PS));
        assert!(!is_honda_diagh_protocol_id(j2534_0404::PROTOCOL_CAN_CH1));
        assert!(!is_honda_diagh_protocol_id(0xFFFF_FFFF));

        assert_eq!(
            base_protocol_id(j2534_0404::PROTOCOL_HONDA_DIAGH_PS),
            j2534_0404::PROTOCOL_HONDA_DIAGH_PS
        );
    }

    // ── ADR-175/Phase 11: SAE J1708 (`is_j1708_protocol_id`) ────────────────

    /// `is_j1708_protocol_id` is `true` only for its own id, and
    /// `base_protocol_id` leaves it unchanged (identity fallback, no
    /// dedicated arm needed -- mirrors `is_honda_diagh_protocol_id`'s own
    /// shape).
    #[test]
    fn is_j1708_protocol_id_true_only_for_its_own_id_and_base_protocol_id_is_identity() {
        assert!(is_j1708_protocol_id(j2534_0404::PROTOCOL_J1708_PS));
        assert!(!is_j1708_protocol_id(j2534_0404::PROTOCOL_HONDA_DIAGH_PS));
        assert!(!is_j1708_protocol_id(
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        ));
        assert!(!is_j1708_protocol_id(j2534_0404::CAN));
        assert!(!is_j1708_protocol_id(j2534_0404::ISO15765));
        assert!(!is_j1708_protocol_id(j2534_0404::PROTOCOL_CAN_PS));
        assert!(!is_j1708_protocol_id(j2534_0404::PROTOCOL_CAN_CH1));
        assert!(!is_j1708_protocol_id(0xFFFF_FFFF));

        assert_eq!(
            base_protocol_id(j2534_0404::PROTOCOL_J1708_PS),
            j2534_0404::PROTOCOL_J1708_PS
        );
    }

    // ── ADR-179/Phase 5: SAE J1939 (`is_j1939_protocol_id`) ─────────────────

    /// `is_j1939_protocol_id` is `true` for its own `_PS` id AND the full
    /// `_CH1..128` range (unlike every prior standalone protocol's own
    /// `is_*_protocol_id`, which checks only a single value -- see the
    /// function's own doc comment for why), and `base_protocol_id` leaves
    /// both shapes unchanged (identity fallback, no dedicated arm needed --
    /// mirrors `is_j1708_protocol_id`'s own shape for the `_PS` id).
    #[test]
    fn is_j1939_protocol_id_true_for_ps_and_the_full_chx_range_and_base_protocol_id_is_identity() {
        assert!(is_j1939_protocol_id(j2534_0404::PROTOCOL_J1939_PS));
        assert!(is_j1939_protocol_id(j2534_0404::PROTOCOL_J1939_CH1));
        assert!(is_j1939_protocol_id(j2534_0404::PROTOCOL_J1939_CH128));
        assert!(is_j1939_protocol_id(
            j2534_0404::PROTOCOL_J1939_CH1 + 64 // an interior _CHx id
        ));
        assert!(!is_j1939_protocol_id(j2534_0404::PROTOCOL_J1939_CH1 - 1));
        assert!(!is_j1939_protocol_id(j2534_0404::PROTOCOL_J1939_CH128 + 1));
        assert!(!is_j1939_protocol_id(j2534_0404::PROTOCOL_J1708_PS));
        assert!(!is_j1939_protocol_id(j2534_0404::PROTOCOL_HONDA_DIAGH_PS));
        assert!(!is_j1939_protocol_id(
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        ));
        assert!(!is_j1939_protocol_id(j2534_0404::CAN));
        assert!(!is_j1939_protocol_id(j2534_0404::ISO15765));
        assert!(!is_j1939_protocol_id(j2534_0404::PROTOCOL_CAN_PS));
        assert!(!is_j1939_protocol_id(j2534_0404::PROTOCOL_CAN_CH1));
        assert!(!is_j1939_protocol_id(0xFFFF_FFFF));

        assert_eq!(
            base_protocol_id(j2534_0404::PROTOCOL_J1939_PS),
            j2534_0404::PROTOCOL_J1939_PS
        );
    }

    // ── ADR-207 Decision item 10: `is_uart_echo_byte_family_protocol_id` ────

    /// `is_uart_echo_byte_family_protocol_id` is `true` for `_PS` AND the
    /// full `ECHO_BYTE_CH1..128` range (mirroring `is_gm_uart_protocol_id`'s
    /// shape), while the narrower `is_uart_echo_byte_protocol_id` stays
    /// `_PS`-only -- the two predicates must disagree on a `_CHx` id, since
    /// that's the entire reason the family-wide predicate exists (an
    /// `edge-case-hunter` review found `connect_discovery_check` silently
    /// skipping its Discovery gate for a `_CHx` connect because it was
    /// keyed on the narrower predicate's exact-match shape).
    #[test]
    fn is_uart_echo_byte_family_protocol_id_true_for_ps_and_the_full_chx_range() {
        assert!(is_uart_echo_byte_family_protocol_id(
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        ));
        assert!(is_uart_echo_byte_family_protocol_id(
            j2534_0404::PROTOCOL_ECHO_BYTE_CH1
        ));
        assert!(is_uart_echo_byte_family_protocol_id(
            j2534_0404::PROTOCOL_ECHO_BYTE_CH128
        ));
        assert!(is_uart_echo_byte_family_protocol_id(
            j2534_0404::PROTOCOL_ECHO_BYTE_CH1 + 64 // an interior _CHx id
        ));
        assert!(!is_uart_echo_byte_family_protocol_id(
            j2534_0404::PROTOCOL_ECHO_BYTE_CH1 - 1
        ));
        assert!(!is_uart_echo_byte_family_protocol_id(
            j2534_0404::PROTOCOL_ECHO_BYTE_CH128 + 1
        ));
        assert!(!is_uart_echo_byte_family_protocol_id(
            j2534_0404::PROTOCOL_J1708_PS
        ));
        assert!(!is_uart_echo_byte_family_protocol_id(j2534_0404::CAN));
        assert!(!is_uart_echo_byte_family_protocol_id(0xFFFF_FFFF));

        // The narrower, arm-gate-only predicate must NOT recognize a `_CHx`
        // id -- that's the exact gap `is_uart_echo_byte_family_protocol_id`
        // was added to close for its own (different) call sites.
        assert!(is_uart_echo_byte_protocol_id(
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        ));
        assert!(!is_uart_echo_byte_protocol_id(
            j2534_0404::PROTOCOL_ECHO_BYTE_CH1
        ));
    }

    /// Regression test for the `edge-case-hunter` finding fixed in ADR-207
    /// Decision item 10: before the fix, `connect_discovery_check` matched
    /// UART Echo Byte via a bare exact-match arm on `PROTOCOL_UART_ECHO_BYTE_PS`
    /// alone, so a `_CHx` Additional Channels connect (`PROTOCOL_ECHO_BYTE_CH1`
    /// etc.) silently returned `None` -- skipping the ADR-185 Stage-1
    /// Discovery fail-fast gate the `_PS` route still enforces. Mirrors
    /// `connect_discovery_check_maps_each_stage_1_protocol_id_to_its_own_device_info_parameter`'s
    /// own assertion shape for the `_PS` case, extended to also cover the
    /// `_CHx` range this fix widened the arm to accept.
    #[test]
    fn connect_discovery_check_covers_uart_echo_byte_chx_the_same_as_its_ps_sibling() {
        for proto_id in [
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
            j2534_0404::PROTOCOL_ECHO_BYTE_CH1,
            j2534_0404::PROTOCOL_ECHO_BYTE_CH128,
        ] {
            let check = connect_discovery_check(proto_id).unwrap_or_else(|| {
                panic!(
                    "protocol id {proto_id:#010x} should have a Stage 1 Discovery check \
                     (ADR-207 Decision item 10)"
                )
            });
            match check {
                DiscoveryCheck::DeviceFlag {
                    parameter,
                    input_value,
                } => {
                    assert_eq!(
                        parameter,
                        j2534_0404::DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED,
                        "protocol id {proto_id:#010x} mapped to the wrong DEVICE_INFO_* \
                         parameter"
                    );
                    assert_eq!(input_value, 0, "no Stage 1 row is per-pin");
                }
                _ => panic!("expected DeviceFlag for {proto_id:#010x}"),
            }
        }
    }

    // ── ADR-208 Decision item 6: `is_honda_diagh_family_protocol_id` ────────

    /// `is_honda_diagh_family_protocol_id` is `true` for `_PS` AND the full
    /// `HONDA_DIAGH_CH1..128` range (mirroring `is_gm_uart_protocol_id`'s/
    /// `is_uart_echo_byte_family_protocol_id`'s shape), while the narrower
    /// `is_honda_diagh_protocol_id` stays `_PS`-only -- the two predicates
    /// must disagree on a `_CHx` id, since that's the entire reason the
    /// family-wide predicate exists (ADR-208's up-front sweep found
    /// `connect_discovery_check`/`bustype_default_name_for_hw_protocol_id`/
    /// `comparam_id.rs`'s P1/P3/P4 translation block all silently skipping a
    /// `_CHx` connect because they were keyed on the narrower predicate's
    /// exact-match shape).
    #[test]
    fn is_honda_diagh_family_protocol_id_true_for_ps_and_the_full_chx_range() {
        assert!(is_honda_diagh_family_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_PS
        ));
        assert!(is_honda_diagh_family_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_CH1
        ));
        assert!(is_honda_diagh_family_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_CH128
        ));
        assert!(is_honda_diagh_family_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_CH1 + 64 // an interior _CHx id
        ));
        assert!(!is_honda_diagh_family_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_CH1 - 1
        ));
        assert!(!is_honda_diagh_family_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_CH128 + 1
        ));
        assert!(!is_honda_diagh_family_protocol_id(
            j2534_0404::PROTOCOL_J1708_PS
        ));
        assert!(!is_honda_diagh_family_protocol_id(
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        ));
        assert!(!is_honda_diagh_family_protocol_id(j2534_0404::CAN));
        assert!(!is_honda_diagh_family_protocol_id(0xFFFF_FFFF));

        // The narrower, arm-gate-only predicate must NOT recognize a `_CHx`
        // id -- that's the exact gap `is_honda_diagh_family_protocol_id`
        // was added to close for its own (different) call sites.
        assert!(is_honda_diagh_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_PS
        ));
        assert!(!is_honda_diagh_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_CH1
        ));
    }

    /// Regression test for ADR-208 Decision item 6: before the fix,
    /// `connect_discovery_check` matched Honda DIAG-H via a bare exact-match
    /// arm on `PROTOCOL_HONDA_DIAGH_PS` alone, so a `_CHx` Additional
    /// Channels connect (`PROTOCOL_HONDA_DIAGH_CH1` etc.) would have silently
    /// returned `None` -- skipping the ADR-185 Stage-1 Discovery fail-fast
    /// gate the `_PS` route still enforces. Mirrors
    /// `connect_discovery_check_covers_uart_echo_byte_chx_the_same_as_its_ps_sibling`'s
    /// own assertion shape.
    #[test]
    fn connect_discovery_check_covers_honda_diagh_chx_the_same_as_its_ps_sibling() {
        for proto_id in [
            j2534_0404::PROTOCOL_HONDA_DIAGH_PS,
            j2534_0404::PROTOCOL_HONDA_DIAGH_CH1,
            j2534_0404::PROTOCOL_HONDA_DIAGH_CH128,
        ] {
            let check = connect_discovery_check(proto_id).unwrap_or_else(|| {
                panic!(
                    "protocol id {proto_id:#010x} should have a Stage 1 Discovery check \
                     (ADR-208 Decision item 6)"
                )
            });
            match check {
                DiscoveryCheck::DeviceFlag {
                    parameter,
                    input_value,
                } => {
                    assert_eq!(
                        parameter,
                        j2534_0404::DEVICE_INFO_HONDA_DIAGH_SUPPORTED,
                        "protocol id {proto_id:#010x} mapped to the wrong DEVICE_INFO_* \
                         parameter"
                    );
                    assert_eq!(input_value, 0, "no Stage 1 row is per-pin");
                }
                _ => panic!("expected DeviceFlag for {proto_id:#010x}"),
            }
        }
    }

    // ── ADR-209 Decision item 6: `is_j1708_family_protocol_id` ──────────────

    /// `is_j1708_family_protocol_id` is `true` for `_PS` AND the full
    /// `J1708_CH1..128` range (mirroring `is_gm_uart_protocol_id`'s/
    /// `is_honda_diagh_family_protocol_id`'s shape), while the narrower
    /// `is_j1708_protocol_id` stays `_PS`-only -- the two predicates must
    /// disagree on a `_CHx` id, since that's the entire reason the
    /// family-wide predicate exists (ADR-209's up-front sweep found
    /// `connect_discovery_check`/`bustype_default_name_for_hw_protocol_id`/
    /// `rpc_primitive.rs`'s/`rpc_misc.rs`'s message-priority TX-flags gates
    /// all silently skipping a `_CHx` connect because they were keyed on the
    /// narrower predicate's exact-match shape).
    #[test]
    fn is_j1708_family_protocol_id_true_for_ps_and_the_full_chx_range() {
        assert!(is_j1708_family_protocol_id(j2534_0404::PROTOCOL_J1708_PS));
        assert!(is_j1708_family_protocol_id(j2534_0404::PROTOCOL_J1708_CH1));
        assert!(is_j1708_family_protocol_id(
            j2534_0404::PROTOCOL_J1708_CH128
        ));
        assert!(is_j1708_family_protocol_id(
            j2534_0404::PROTOCOL_J1708_CH1 + 64 // an interior _CHx id
        ));
        assert!(!is_j1708_family_protocol_id(
            j2534_0404::PROTOCOL_J1708_CH1 - 1
        ));
        assert!(!is_j1708_family_protocol_id(
            j2534_0404::PROTOCOL_J1708_CH128 + 1
        ));
        assert!(!is_j1708_family_protocol_id(
            j2534_0404::PROTOCOL_HONDA_DIAGH_PS
        ));
        assert!(!is_j1708_family_protocol_id(
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        ));
        assert!(!is_j1708_family_protocol_id(j2534_0404::CAN));
        assert!(!is_j1708_family_protocol_id(0xFFFF_FFFF));

        // The narrower, arm-gate-only predicate must NOT recognize a `_CHx`
        // id -- that's the exact gap `is_j1708_family_protocol_id` was added
        // to close for its own (different) call sites.
        assert!(is_j1708_protocol_id(j2534_0404::PROTOCOL_J1708_PS));
        assert!(!is_j1708_protocol_id(j2534_0404::PROTOCOL_J1708_CH1));
    }

    /// Regression test for ADR-209 Decision item 6: before the fix,
    /// `connect_discovery_check` matched SAE J1708 via a bare exact-match
    /// arm on `PROTOCOL_J1708_PS` alone, so a `_CHx` Additional Channels
    /// connect (`PROTOCOL_J1708_CH1` etc.) would have silently returned
    /// `None` -- skipping the ADR-185 Stage-1 Discovery fail-fast gate the
    /// `_PS` route still enforces. Mirrors
    /// `connect_discovery_check_covers_honda_diagh_chx_the_same_as_its_ps_sibling`'s
    /// own assertion shape.
    #[test]
    fn connect_discovery_check_covers_j1708_chx_the_same_as_its_ps_sibling() {
        for proto_id in [
            j2534_0404::PROTOCOL_J1708_PS,
            j2534_0404::PROTOCOL_J1708_CH1,
            j2534_0404::PROTOCOL_J1708_CH128,
        ] {
            let check = connect_discovery_check(proto_id).unwrap_or_else(|| {
                panic!(
                    "protocol id {proto_id:#010x} should have a Stage 1 Discovery check \
                     (ADR-209 Decision item 6)"
                )
            });
            match check {
                DiscoveryCheck::DeviceFlag {
                    parameter,
                    input_value,
                } => {
                    assert_eq!(
                        parameter,
                        j2534_0404::DEVICE_INFO_J1708_SUPPORTED,
                        "protocol id {proto_id:#010x} mapped to the wrong DEVICE_INFO_* \
                         parameter"
                    );
                    assert_eq!(input_value, 0, "no Stage 1 row is per-pin");
                }
                _ => panic!("expected DeviceFlag for {proto_id:#010x}"),
            }
        }
    }

    // ── ADR-210 Decision item 6: `is_tp2_0_family_protocol_id` ──────────────

    /// `is_tp2_0_family_protocol_id` is `true` for `_PS` AND the full
    /// `_CH1..128` range, while the narrower `is_tp2_0_protocol_id` stays
    /// `_PS`-only -- the two predicates must disagree on a `_CHx` id, since
    /// that's the entire reason the family-wide predicate exists (ADR-210's
    /// up-front sweep found `connect_discovery_check`/
    /// `bustype_default_name_for_hw_protocol_id`/`comparam_id.rs`'s/
    /// `rpc_primitive.rs`'s/`rpc_misc.rs`'s/`rpc_link.rs`'s/`events.rs`'s/
    /// `events_rx_routing.rs`'s own checks all silently skipping or
    /// misrouting a `_CHx` connect because they were keyed on the narrower
    /// predicate's exact-match shape).
    #[test]
    fn is_tp2_0_family_protocol_id_true_for_ps_and_the_full_chx_range() {
        assert!(is_tp2_0_family_protocol_id(j2534_0404::PROTOCOL_TP2_0_PS));
        assert!(is_tp2_0_family_protocol_id(j2534_0404::PROTOCOL_TP2_0_CH1));
        assert!(is_tp2_0_family_protocol_id(
            j2534_0404::PROTOCOL_TP2_0_CH128
        ));
        assert!(is_tp2_0_family_protocol_id(
            j2534_0404::PROTOCOL_TP2_0_CH1 + 64 // an interior _CHx id
        ));
        assert!(!is_tp2_0_family_protocol_id(
            j2534_0404::PROTOCOL_TP2_0_CH1 - 1
        ));
        assert!(!is_tp2_0_family_protocol_id(
            j2534_0404::PROTOCOL_TP2_0_CH128 + 1
        ));
        assert!(!is_tp2_0_family_protocol_id(j2534_0404::PROTOCOL_J1708_PS));
        assert!(!is_tp2_0_family_protocol_id(
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
        ));
        assert!(!is_tp2_0_family_protocol_id(j2534_0404::CAN));
        assert!(!is_tp2_0_family_protocol_id(0xFFFF_FFFF));

        // The narrower, arm-gate-only predicate must NOT recognize a `_CHx`
        // id -- that's the exact gap `is_tp2_0_family_protocol_id` was added
        // to close for its own (different) call sites.
        assert!(is_tp2_0_protocol_id(j2534_0404::PROTOCOL_TP2_0_PS));
        assert!(!is_tp2_0_protocol_id(j2534_0404::PROTOCOL_TP2_0_CH1));
    }

    /// Regression test for ADR-210 Decision item 7: before the fix,
    /// `connect_discovery_check` matched TP2.0 via a bare exact-match arm on
    /// `PROTOCOL_TP2_0_PS` alone, so a `_CHx` Additional Channels connect
    /// (`PROTOCOL_TP2_0_CH1` etc.) would have silently returned `None` --
    /// skipping the ADR-185 Stage-1 Discovery fail-fast gate the `_PS` route
    /// still enforces. Mirrors
    /// `connect_discovery_check_covers_j1708_chx_the_same_as_its_ps_sibling`'s
    /// own assertion shape.
    #[test]
    fn connect_discovery_check_covers_tp2_0_chx_the_same_as_its_ps_sibling() {
        for proto_id in [
            j2534_0404::PROTOCOL_TP2_0_PS,
            j2534_0404::PROTOCOL_TP2_0_CH1,
            j2534_0404::PROTOCOL_TP2_0_CH128,
        ] {
            let check = connect_discovery_check(proto_id).unwrap_or_else(|| {
                panic!(
                    "protocol id {proto_id:#010x} should have a Stage 1 Discovery check \
                     (ADR-210 Decision item 7)"
                )
            });
            match check {
                DiscoveryCheck::DeviceFlag {
                    parameter,
                    input_value,
                } => {
                    assert_eq!(
                        parameter,
                        j2534_0404::DEVICE_INFO_TP2_0_SUPPORTED,
                        "protocol id {proto_id:#010x} mapped to the wrong DEVICE_INFO_* \
                         parameter"
                    );
                    assert_eq!(input_value, 0, "no Stage 1 row is per-pin");
                }
                _ => panic!("expected DeviceFlag for {proto_id:#010x}"),
            }
        }
    }

    // ── `bustype_default_name_for_hw_protocol_id` (generalizes the Honda
    // DIAG-H/SAE J1708 `rpc_create_com_logical_link` fallback arms to SWCAN,
    // FT-CAN, UART Echo Byte, and SAE J1939) ────────────────────────────────

    /// A representative hardware protocol id per in-scope family -- including
    /// BOTH sub-variants SWCAN/FT-CAN each have (raw-CAN vs. ISO15765) --
    /// resolves to the expected bustype default name, and that name is
    /// itself a real, resolvable `comparam_defaults::bustype_default_params`
    /// entry, not just a plausible-looking string.
    #[test]
    fn bustype_default_name_for_hw_protocol_id_covers_every_in_scope_family() {
        use super::super::comparam_defaults;

        let cases = [
            (j2534_0404::PROTOCOL_SW_CAN_PS, "SAE_J2411_SWCAN"),
            (j2534_0404::PROTOCOL_SW_ISO15765_PS, "SAE_J2411_SWCAN"),
            (j2534_0404::PROTOCOL_FT_CAN_PS, "ISO_11898_3_DWFTCAN"),
            (j2534_0404::PROTOCOL_FT_ISO15765_PS, "ISO_11898_3_DWFTCAN"),
            // ADR-211: a `_CHx` id must resolve identically to its `_PS`
            // sibling now that `is_ft_family_protocol_id` (not the
            // arm-gate-only `is_ft_protocol_id`) backs this branch.
            (j2534_0404::PROTOCOL_FT_CAN_CH1, "ISO_11898_3_DWFTCAN"),
            (j2534_0404::PROTOCOL_FT_ISO15765_CH1, "ISO_11898_3_DWFTCAN"),
            (
                j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
                "UART_ECHO_BYTE_UART",
            ),
            // ADR-207 Decision item 10: a `_CHx` id must resolve identically
            // to its `_PS` sibling now that `is_uart_echo_byte_family_protocol_id`
            // (not the arm-gate-only `is_uart_echo_byte_protocol_id`) backs
            // this branch.
            (j2534_0404::PROTOCOL_ECHO_BYTE_CH1, "UART_ECHO_BYTE_UART"),
            (j2534_0404::PROTOCOL_HONDA_DIAGH_PS, "HONDA_DIAGH_UART"),
            // ADR-208 Decision item 6: a `_CHx` id must resolve identically
            // to its `_PS` sibling now that `is_honda_diagh_family_protocol_id`
            // (not the arm-gate-only `is_honda_diagh_protocol_id`) backs this
            // branch.
            (j2534_0404::PROTOCOL_HONDA_DIAGH_CH1, "HONDA_DIAGH_UART"),
            (j2534_0404::PROTOCOL_J1708_PS, "SAE_J1708_UART"),
            // ADR-209 Decision item 6: a `_CHx` id must resolve identically
            // to its `_PS` sibling now that `is_j1708_family_protocol_id`
            // (not the arm-gate-only `is_j1708_protocol_id`) backs this
            // branch.
            (j2534_0404::PROTOCOL_J1708_CH1, "SAE_J1708_UART"),
            (j2534_0404::PROTOCOL_J1939_PS, "SAE_J1939_11_DWCAN"),
            (j2534_0404::PROTOCOL_TP2_0_PS, "TP2_0_DWCAN"),
            // ADR-210 Decision item 7: a `_CHx` id must resolve identically
            // to its `_PS` sibling now that `is_tp2_0_family_protocol_id`
            // (not the arm-gate-only `is_tp2_0_protocol_id`) backs this
            // branch.
            (j2534_0404::PROTOCOL_TP2_0_CH1, "TP2_0_DWCAN"),
            (j2534_0404::PROTOCOL_GM_UART_PS, "GM_UART_UART"),
            (j2534_0404::PROTOCOL_ETHERNET_NDIS, "IEEE_802_3"),
        ];

        for (hw_protocol_id, expected_name) in cases {
            let name = bustype_default_name_for_hw_protocol_id(hw_protocol_id);
            assert_eq!(
                name,
                Some(expected_name),
                "unexpected bustype default name for hw_protocol_id {hw_protocol_id:#010x}"
            );
            assert!(
                comparam_defaults::bustype_default_params(name.unwrap()).is_some(),
                "{expected_name:?} does not resolve via bustype_default_params"
            );
        }

        // Out of scope: a base/generic id, or an id from an unrelated family.
        assert_eq!(
            bustype_default_name_for_hw_protocol_id(j2534_0404::CAN),
            None
        );
        assert_eq!(
            bustype_default_name_for_hw_protocol_id(j2534_0404::ISO15765),
            None
        );
        assert_eq!(
            bustype_default_name_for_hw_protocol_id(j2534_0404::PROTOCOL_CAN_PS),
            None
        );
        assert_eq!(bustype_default_name_for_hw_protocol_id(0xFFFF_FFFF), None);
    }

    /// Table-parity guard: whenever a resource-table row's own resolved
    /// hardware protocol id (`hw_protocol_override.unwrap_or(...)`) is one of
    /// `bustype_default_name_for_hw_protocol_id`'s nine in-scope families, the
    /// helper's independently-computed name must match that row's own
    /// `bus_type_name` (case-insensitively, since `bustype_default_params`
    /// lowercases its input) -- catching a future resource-table row silently
    /// drifting from what this helper would compute for the same hardware id.
    #[test]
    fn bustype_default_name_for_hw_protocol_id_agrees_with_every_matching_table_row() {
        for row in resource_table() {
            let hw_protocol_id = row
                .hw_protocol_override
                .unwrap_or_else(|| row.protocol.j2534_protocol_id());
            if let Some(name) = bustype_default_name_for_hw_protocol_id(hw_protocol_id) {
                assert!(
                    name.eq_ignore_ascii_case(row.bus_type_name),
                    "resource_id {:#06x}: bustype_default_name_for_hw_protocol_id returned \
                     {name:?} but the row's own bus_type_name is {:?}",
                    row.resource_id,
                    row.bus_type_name
                );
            }
        }
    }
}
