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
