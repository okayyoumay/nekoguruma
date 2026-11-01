use super::*;

fn protocol<'a>(params: &[(&'a str, i64)]) -> DeclaredProtocol<'a> {
    DeclaredProtocol {
        short_name: Some("ISO15765"),
        application_layer: Some("UDS"),
        baud_rate: 500_000,
        tx_id: 0x7E0,
        rx_id: 0x7E8,
        functional_id: 0x7DF,
        extended_addressing: false,
        params: params
            .iter()
            .map(|&(name, value)| DeclaredComParam {
                short_name: Some(name),
                value,
                unit_id: 0,
            })
            .collect(),
    }
}

fn refusals(protocol: &DeclaredProtocol<'_>) -> Vec<LinkRefusal> {
    check_protocol(protocol).expect_err("refused")
}

fn why(refusals: &[LinkRefusal]) -> Vec<&ComParamRefusal> {
    refusals
        .iter()
        .filter_map(|r| match r {
            LinkRefusal::ComParam { why, .. } => Some(why),
            _ => None,
        })
        .collect()
}

#[test]
fn a_protocol_of_the_example_ecu_is_accepted() {
    let link = check_protocol(&protocol(&[("CP_BlockSize", 0), ("cp_stmin", 0)])).expect("ok");
    assert_eq!(
        (link.baud_rate, link.tx_id, link.rx_id),
        (500_000, 0x7E0, 0x7E8)
    );
    assert_eq!(
        link.params,
        [("CP_BlockSize".to_owned(), 0), ("cp_stmin".to_owned(), 0)]
    );
    assert_eq!(link.p2_max_us, None);
}

#[test]
fn exactly_one_protocol_is_the_variants_link() {
    assert_eq!(only_protocol::<u8>(&[]), Err(LinkRefusal::NoProtocol));
    assert_eq!(only_protocol(&[1, 2]), Err(LinkRefusal::SeveralProtocols));
    assert_eq!(only_protocol(&[7]), Ok(&7));
}

#[test]
fn only_uds_on_can_by_its_two_names_is_accepted() {
    let mut named = protocol(&[]);
    named.short_name = Some(PROTOCOL_UDS_ON_CAN);
    check_protocol(&named).expect("the standard name");
    for (name, layer) in [
        (Some("CAN"), Some("UDS")),
        (Some("ISO_15765_3_on_ISO_15765_2"), Some("UDS")),
        (Some("ISO15765"), Some("KWP2000")),
        (None, Some("UDS")),
        (Some("ISO15765"), None),
    ] {
        let mut other = protocol(&[]);
        other.short_name = name;
        other.application_layer = layer;
        assert!(
            matches!(refusals(&other)[..], [LinkRefusal::NotUdsOnCan { .. }]),
            "{name:?} {layer:?}"
        );
    }
}

#[test]
fn extended_addressing_is_refused() {
    let mut extended = protocol(&[]);
    extended.extended_addressing = true;
    assert_eq!(refusals(&extended), [LinkRefusal::ExtendedAddressing]);
}

#[test]
fn an_identifier_above_eleven_bits_is_refused_whichever_it_is() {
    for (which, change) in [
        (
            Identifier::Tx,
            (|p: &mut DeclaredProtocol<'_>| p.tx_id = 0x800) as fn(&mut _),
        ),
        (Identifier::Rx, |p| p.rx_id = 0x800),
        (Identifier::Functional, |p| p.functional_id = 0x800),
    ] {
        let mut wide = protocol(&[]);
        change(&mut wide);
        assert_eq!(
            refusals(&wide),
            [LinkRefusal::WideIdentifier { which, id: 0x800 }]
        );
    }
    let mut edge = protocol(&[]);
    (edge.tx_id, edge.rx_id, edge.functional_id) = (0x7FF, 0x7FF, 0x7FF);
    check_protocol(&edge).expect("11 bits fit");
}

#[test]
fn a_zero_baud_rate_is_refused() {
    let mut zero = protocol(&[]);
    zero.baud_rate = 0;
    assert_eq!(refusals(&zero), [LinkRefusal::ZeroBaudRate]);
}

#[test]
fn a_comparam_declared_twice_is_refused_ignoring_case() {
    let found = refusals(&protocol(&[
        ("CP_StMin", 0),
        ("cp_stmin", 1),
        ("CP_StMin", 2),
    ]));
    assert_eq!(
        found,
        [
            LinkRefusal::ComParam {
                at: 1,
                name: "cp_stmin".into(),
                why: ComParamRefusal::DeclaredTwice { earlier: 0 }
            },
            LinkRefusal::ComParam {
                at: 2,
                name: "CP_StMin".into(),
                why: ComParamRefusal::DeclaredTwice { earlier: 0 }
            },
        ]
    );
}

#[test]
fn a_comparam_with_a_unit_is_refused() {
    let mut with_unit = protocol(&[("CP_StMin", 0)]);
    with_unit.params[0].unit_id = 2;
    assert_eq!(why(&refusals(&with_unit)), [&ComParamRefusal::HasUnit]);
    assert!(refusals(&with_unit)[0].to_string().contains("unit"));
}

#[test]
fn a_value_outside_32_bits_unsigned_is_refused_and_the_edge_accepted() {
    for value in [-1, 4_294_967_296, i64::MIN, i64::MAX] {
        assert_eq!(
            why(&refusals(&protocol(&[("CP_StMin", value)]))),
            [&ComParamRefusal::NotUnsigned32(value)]
        );
    }
    let link = check_protocol(&protocol(&[("CP_StMin", 4_294_967_295)])).expect("fits");
    assert_eq!(link.params, [("CP_StMin".to_owned(), u32::MAX)]);
}

#[test]
fn a_timing_is_taken_apart_from_the_further_comparams_in_any_case() {
    let link = check_protocol(&protocol(&[
        ("cp_p2max", 50_000),
        ("CP_P2Star", 1),
        ("CP_RC78COMPLETIONTIMEOUT", 4_294_967_000),
        ("CP_Cr", 7),
    ]))
    .expect("ok");
    assert_eq!(link.p2_max_us, Some(50_000));
    assert_eq!(link.p2_star_us, Some(1));
    assert_eq!(link.rc78_completion_us, Some(MAX_TIMING_US));
    assert_eq!(link.params, [("CP_Cr".to_owned(), 7)]);
    assert_eq!(timing_of("Cp_P2star"), Some(Timing::P2Star));
    assert_eq!(timing_of("CP_StMin"), None);
}

#[test]
fn a_timing_of_zero_or_above_the_range_is_refused() {
    for name in TIMING_COMPARAMS {
        for value in [0, 4_294_967_001, 4_294_967_295] {
            assert_eq!(
                why(&refusals(&protocol(&[(name, value)]))),
                [&ComParamRefusal::TimingOutOfRange(value)],
                "{name} {value}"
            );
        }
    }
}

#[test]
fn every_name_the_link_sets_or_that_outlasts_the_host_is_refused() {
    for name in SET_BY_LINK {
        if timing_of(name).is_some() {
            continue;
        }
        for spelled in [
            name.to_owned(),
            name.to_ascii_lowercase(),
            name.to_ascii_uppercase(),
        ] {
            assert_eq!(
                why(&refusals(&protocol(&[(&spelled, 0)]))),
                [&ComParamRefusal::SetByLink],
                "{spelled}"
            );
        }
    }
    for name in OUTLAST_THE_HOST {
        assert_eq!(
            why(&refusals(&protocol(&[(name, 0)]))),
            [&ComParamRefusal::OutlastsHost],
            "{name}"
        );
        assert_eq!(
            further_comparam(&name.to_lowercase()),
            Err(ComParamRefusal::OutlastsHost)
        );
    }
}

#[test]
fn a_name_without_the_standard_prefix_is_refused_without_a_panic() {
    for name in [
        "DATA_RATE",
        "cp",
        " CP_X",
        "",
        "CP\u{e9}x",
        "\u{e9}CP_x",
        "C",
    ] {
        assert_eq!(
            further_comparam(name),
            Err(ComParamRefusal::NotStandardName),
            "{name:?}"
        );
    }
    assert_eq!(further_comparam("cp_blocksize"), Ok(()));
}

#[test]
fn a_protocol_lists_all_its_refusals_protocol_level_first() {
    let mut bad = protocol(&[("DATA_RATE", 0), ("CP_StMin", -1)]);
    bad.baud_rate = 0;
    bad.extended_addressing = true;
    let found = refusals(&bad);
    assert!(matches!(
        found[..],
        [
            LinkRefusal::ExtendedAddressing,
            LinkRefusal::ZeroBaudRate,
            LinkRefusal::ComParam { at: 0, .. },
            LinkRefusal::ComParam { at: 1, .. },
        ]
    ));
}

#[test]
fn the_refusal_texts_name_the_comparam() {
    let found = refusals(&protocol(&[("CP_Baudrate", 1)]));
    assert_eq!(
        found[0].to_string(),
        "ComParam CP_Baudrate is one the link sets itself"
    );
}

#[test]
fn a_wide_identifier_is_named_readably() {
    let mut wide = protocol(&[]);
    wide.tx_id = 0x800;
    let text = refusals(&wide)[0].to_string();
    assert!(text.contains("transmit identifier 0x800"), "{text}");
    assert_eq!(wide_identifier(Identifier::Rx, 0x7FF), None);
    assert!(wide_identifier(Identifier::Rx, 0x800).is_some());
}
