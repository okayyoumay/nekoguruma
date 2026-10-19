//! ComParam names: the one canonical form the profile check and the guard both compare by.

/// Alias to canonical name (both lowercase): every other spelling that `j2534-0404-service`
/// accepts for a ComParam (`map_comparam_name` in its `names.rs`: the raw `CONFIG_*` names such as
/// `DATA_RATE`, and the spelling variants), so a rule written for the canonical name cannot be
/// skipped by naming the same ComParam differently. `iso22900-service` has no table of its own:
/// the D-PDU library resolves names, so a name the guard does not know is refused (`ALLOWED`).
pub const ALIASES: &[(&str, &str)] = &[
    ("data_rate", "cp_baudrate"),
    ("loopback", "cp_loopback"),
    ("cp_testersourceaddress", "cp_node_address"),
    ("cp_nodeaddress", "cp_node_address"),
    ("node_address", "cp_node_address"),
    ("cp_network_line", "cp_networkline"),
    ("network_line", "cp_networkline"),
    ("p1_min", "cp_p1min"),
    ("p1_max", "cp_p1max"),
    ("p2_min", "cp_p2min"),
    ("p2_max", "cp_p2max"),
    ("p3_min", "cp_p3min"),
    ("p3_max", "cp_p3max_ecu"),
    ("p4_min", "cp_p4min"),
    ("p4_max", "cp_p4max"),
    ("w0", "cp_w0max"),
    ("w1", "cp_w1max"),
    ("w2", "cp_w2max"),
    ("w3", "cp_w3max"),
    ("w4", "cp_w4min"),
    ("w5", "cp_w5max"),
    ("tidle", "cp_tidle"),
    ("tinil", "cp_tinil"),
    ("twup", "cp_twup"),
    ("parity", "cp_parity"),
    ("cp_bit_sample_point", "cp_bitsamplepoint"),
    ("bit_sample_point", "cp_bitsamplepoint"),
    ("cp_sync_jump_width", "cp_syncjumpwidth"),
    ("sync_jump_width", "cp_syncjumpwidth"),
    ("t1_max", "cp_t1max"),
    ("t2_max", "cp_t2max"),
    ("t3_max", "cp_t3max"),
    ("t4_max", "cp_t4max"),
    ("t5_max", "cp_t5max"),
    ("iso15765_stmin", "cp_stmin"),
    ("iso15765_bs", "cp_blocksize"),
    ("iso15765_wft_max", "cp_canmaxnumwaitframes"),
    ("cp_bstx", "cp_blocksizeoverride"),
    ("cp_bs_tx", "cp_blocksizeoverride"),
    ("bs_tx", "cp_blocksizeoverride"),
    ("cp_stmintx", "cp_stminoverride"),
    ("cp_stmin_tx", "cp_stminoverride"),
    ("stmin_tx", "cp_stminoverride"),
    ("five_baud_mod", "cp_5baudmode"),
    ("data_bits", "cp_uartconfig"),
    ("cp_blocksizeecu", "cp_blocksize_ecu"),
    ("cp_maxdatalengthecu", "cp_maxdatalength_ecu"),
    ("cp_bitsamplepointecu", "cp_bitsamplepoint_ecu"),
    ("cp_samplesperbitecu", "cp_samplesperbit_ecu"),
    ("cp_syncjumpwidthecu", "cp_syncjumpwidth_ecu"),
    ("cp_kllineinit", "cp_k_l_lineinit"),
    ("cp_klinepullup", "cp_k_linepullup"),
    ("cp_terminationtypeecu", "cp_terminationtype_ecu"),
    ("cp_swcanhighvoltage", "cp_swcan_highvoltage"),
    ("cp_ecurespsourceaddr", "cp_ecurespsourceaddress"),
    ("cp_ecurespsoureaddress", "cp_ecurespsourceaddress"),
    ("cp_ecurespsoureaddr", "cp_ecurespsourceaddress"),
    ("cp_testerpresentmsg", "cp_testerpresentmessage"),
];

/// The canonical (lowercase) name of a ComParam name: ASCII case folded, aliases resolved.
pub fn canonical(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == lower)
        .map_or(lower, |(_, canonical)| (*canonical).to_owned())
}

/// Protocol and timing ComParams that pass without a rule against the profile, and the only ones
/// a profile may pin in `[link.comparams]` (canonical, lowercase): what the agent's link sets (`CP_P2Max`, `CP_P2Star`, the 0x78 response-pending
/// handling) and the UDS-on-CAN timing, flow control and tester-present ComParams a read-only
/// check may set, by the names of ISO 22900-2 (2022) Annex B (B.3 on the timing, error handling
/// and tester-present classes, and the transport layer of ISO 15765-2 (2024) clause 10). The
/// ComParams with a rule (identifiers, formats, addresses, filler, address modes, the
/// tester-present message, the baud rate) are not here, and neither is anything else: a name
/// that is on no list is refused.
pub const ALLOWED: &[&str] = &[
    // Session timing.
    "cp_p2max",
    "cp_p2star",
    "cp_p2max_ecu",
    "cp_p2star_ecu",
    "cp_p3phys",
    "cp_p3func",
    "cp_cyclicresptimeout",
    // ISO 15765-2 frame timing and flow control.
    "cp_ar",
    "cp_ar_ecu",
    "cp_as",
    "cp_as_ecu",
    "cp_br",
    "cp_br_ecu",
    "cp_bs",
    "cp_bs_ecu",
    "cp_cr",
    "cp_cr_ecu",
    "cp_cs",
    "cp_cs_ecu",
    "cp_blocksize",
    "cp_blocksize_ecu",
    "cp_blocksizeoverride",
    "cp_stmin",
    "cp_stmin_ecu",
    "cp_stminoverride",
    "cp_canmaxnumwaitframes",
    // Response pending handling.
    "cp_rc78handling",
    "cp_rc78completiontimeout",
    "cp_rcbyteoffset",
    // Tester present timing and handling (the message and address mode have rules).
    "cp_testerpresenttime",
    "cp_testerpresenthandling",
    "cp_testerpresentreqrsp",
    "cp_testerpresentexpposresp",
    "cp_testerpresentexpnegresp",
    "cp_testerpresentsendtype",
];

/// The ComParams of [`ALLOWED`] whose value is not a number (byte fields): a `SetComParam` may
/// set them, but a profile cannot pin one with an integer.
pub const NON_NUMERIC: &[&str] = &["cp_testerpresentexpposresp", "cp_testerpresentexpnegresp"];

/// Whether a profile's `[link.comparams]` may pin this canonical name: a numeric ComParam on the
/// allow-list, which the guard compares and requires.
pub fn profile_may_pin(canonical: &str) -> bool {
    ALLOWED.contains(&canonical) && !NON_NUMERIC.contains(&canonical)
}

/// The profile key that gives a ComParam the guard derives from the typed fields, by canonical
/// name.
pub fn typed_field(canonical: &str) -> Option<&'static str> {
    Some(match canonical {
        "cp_baudrate" => "`baud_rate`",
        "cp_canphysreqid" => "`request_id`",
        "cp_canrespusdtid" | "cp_canrespuudtid" => "`response_id`",
        "cp_canfuncreqid" => "`functional_id`",
        "cp_canphysreqformat"
        | "cp_canfuncreqformat"
        | "cp_canrespusdtformat"
        | "cp_canrespuudtformat" => "`id_bits` and `addressing`",
        "cp_canphysreqextaddr"
        | "cp_canfuncreqextaddr"
        | "cp_canrespusdtextaddr"
        | "cp_canrespuudtextaddr" => "`address`",
        "cp_canfillerbyte" | "cp_canfillerbytehandling" => "`padding`",
        "cp_requestaddrmode" | "cp_testerpresentaddrmode" => "`functional_id`",
        _ => return None,
    })
}
