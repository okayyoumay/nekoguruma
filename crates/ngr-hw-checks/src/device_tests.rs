use super::*;

fn j2534() -> Vci {
    Vci {
        api: Api::J2534V0404,
        library: "Vendor VCI".into(),
        module: "VCI-1".into(),
        label: Some("bench".into()),
        pname: Some("Vendor VCI 1".into()),
        firmware_version: "1.2.3".into(),
        library_version: "4.5".into(),
        timing_margin_us: None,
    }
}

fn dpdu() -> Vci {
    Vci {
        api: Api::Iso22900,
        library: "Vendor D-PDU".into(),
        module: "SN-42".into(),
        label: None,
        pname: None,
        firmware_version: "2.1.0".into(),
        library_version: "3.0.7".into(),
        timing_margin_us: None,
    }
}

fn reported(name: &str, info: &str) -> ReportedModule {
    ReportedModule {
        name: name.into(),
        additional_info: info.into(),
    }
}

fn bench() -> Vec<ConfiguredModule> {
    vec![ConfiguredModule {
        label: "bench".into(),
        pname: "Vendor VCI 1".into(),
    }]
}

#[test]
fn a_j2534_module_is_the_only_configured_entry_with_the_profiles_label_and_pname() {
    let modules = [reported("bench", "Vendor VCI")];
    assert_eq!(select_module(&j2534(), &modules, &bench()), Ok(0));
}

#[test]
fn a_j2534_profile_whose_label_is_not_configured_is_refused() {
    let configured = vec![ConfiguredModule {
        label: "other".into(),
        pname: "Vendor VCI 1".into(),
    }];
    assert_eq!(
        select_module(&j2534(), &[reported("other", "Vendor VCI")], &configured),
        Err(MatchRefusal::UnknownLabel {
            label: "bench".into()
        })
    );
}

#[test]
fn a_j2534_module_must_be_the_only_one_configured() {
    let mut configured = bench();
    configured.push(ConfiguredModule {
        label: "spare".into(),
        pname: "Vendor VCI 2".into(),
    });
    let modules = [
        reported("bench", "Vendor VCI"),
        reported("spare", "Vendor VCI"),
    ];
    assert_eq!(
        select_module(&j2534(), &modules, &configured),
        Err(MatchRefusal::SeveralConfigured { count: 2 })
    );
}

#[test]
fn a_j2534_entry_naming_another_device_is_refused() {
    let configured = vec![ConfiguredModule {
        label: "bench".into(),
        pname: "Vendor VCI 9".into(),
    }];
    assert_eq!(
        select_module(&j2534(), &[reported("bench", "Vendor VCI")], &configured),
        Err(MatchRefusal::PnameDiffers {
            label: "bench".into(),
            configured: "Vendor VCI 9".into(),
            profile: "Vendor VCI 1".into(),
        })
    );
}

#[test]
fn a_j2534_module_the_worker_does_not_report_or_reports_twice_is_refused() {
    assert_eq!(
        select_module(&j2534(), &[], &bench()),
        Err(MatchRefusal::NoModule {
            module: "bench".into()
        })
    );
    let twice = [
        reported("bench", "Vendor VCI"),
        reported("bench", "Vendor VCI"),
    ];
    assert_eq!(
        select_module(&j2534(), &twice, &bench()),
        Err(MatchRefusal::SeveralModules {
            module: "bench".into(),
            count: 2
        })
    );
}

#[test]
fn a_j2534_worker_reporting_several_modules_is_refused() {
    let several = [
        reported("bench", "Vendor VCI"),
        reported("other", "Vendor VCI"),
    ];
    assert_eq!(
        select_module(&j2534(), &several, &bench()),
        Err(MatchRefusal::SeveralModules {
            module: "bench".into(),
            count: 2
        })
    );
}

#[test]
fn a_j2534_module_from_another_library_is_refused() {
    assert_eq!(
        select_module(&j2534(), &[reported("bench", "Other VCI")], &bench()),
        Err(MatchRefusal::LibraryDiffers {
            reported: "Other VCI".into(),
            profile: "Vendor VCI".into(),
        })
    );
}

#[test]
fn a_dpdu_module_is_found_by_its_whole_name_or_an_attribute_value_of_it() {
    let modules = [
        reported("VendorName='Vendor' SerialNumber='SN-41'", ""),
        reported("VendorName='Vendor' SerialNumber='SN-42' ", ""),
    ];
    assert_eq!(select_module(&dpdu(), &modules, &[]), Ok(1));
    let by_name = [reported(" SN-42 ", "")];
    assert_eq!(select_module(&dpdu(), &by_name, &[]), Ok(0));
}

#[test]
fn a_dpdu_modules_additional_information_is_not_read() {
    assert_eq!(
        select_module(&dpdu(), &[reported("Vendor VCI", "SN-42")], &[]),
        Err(MatchRefusal::NoModule {
            module: "SN-42".into()
        })
    );
    assert_eq!(
        select_module(&dpdu(), &[reported("", "SerialNumber='SN-42'")], &[]),
        Err(MatchRefusal::NoModule {
            module: "SN-42".into()
        })
    );
}

#[test]
fn a_dpdu_profile_matching_no_module_or_several_is_refused() {
    assert_eq!(
        select_module(&dpdu(), &[reported("SerialNumber='SN-41'", "")], &[]),
        Err(MatchRefusal::NoModule {
            module: "SN-42".into()
        })
    );
    let both = [reported("SN-42", ""), reported("SerialNumber='SN-42'", "")];
    assert_eq!(
        select_module(&dpdu(), &both, &[]),
        Err(MatchRefusal::SeveralModules {
            module: "SN-42".into(),
            count: 2
        })
    );
}

#[test]
fn an_empty_module_or_label_is_refused_before_matching() {
    let unnamed = Vci {
        module: "  ".into(),
        ..dpdu()
    };
    assert_eq!(
        select_module(&unnamed, &[reported("", "")], &[]),
        Err(MatchRefusal::Unnamed { what: "module" })
    );
    let unlabelled = Vci {
        label: Some(" ".into()),
        ..j2534()
    };
    assert_eq!(
        select_module(&unlabelled, &[reported("", "Vendor VCI")], &bench()),
        Err(MatchRefusal::Unnamed { what: "label" })
    );
}

#[test]
fn j2534_names_are_compared_without_surrounding_spaces() {
    let padded = Vci {
        library: " Vendor VCI ".into(),
        label: Some(" bench ".into()),
        pname: Some("Vendor VCI 1 ".into()),
        ..j2534()
    };
    let configured = vec![ConfiguredModule {
        label: "bench ".into(),
        pname: " Vendor VCI 1".into(),
    }];
    let modules = [reported(" bench", "Vendor VCI ")];
    assert_eq!(select_module(&padded, &modules, &configured), Ok(0));
    assert_eq!(
        select_module(&padded, &[], &configured),
        Err(MatchRefusal::NoModule {
            module: "bench".into()
        })
    );
}

#[test]
fn a_j2534_profile_with_nothing_configured_is_refused() {
    assert_eq!(
        select_module(&j2534(), &[reported("bench", "Vendor VCI")], &[]),
        Err(MatchRefusal::UnknownLabel {
            label: "bench".into()
        })
    );
}

#[test]
fn attribute_values_are_read_from_quoted_pairs_only() {
    assert_eq!(
        attribute_values("A='x' B = 'y z' C=unquoted D='open"),
        vec!["x", "y z"]
    );
    assert!(attribute_values("").is_empty());
}

#[test]
fn a_coded_version_is_three_decimal_bytes() {
    assert!(is_coded_version("2.1.0"));
    assert!(is_coded_version("255.0.255"));
    for bad in [
        "4.5", "1.2.3.4", "256.0.0", "1..2", "a.b.c", "-1.0.0", "+1.0.0", "",
    ] {
        assert!(!is_coded_version(bad), "{bad}");
    }
}

#[test]
fn j2534_versions_are_compared_as_the_strings_the_device_reports() {
    let version = ReportedVersion {
        vendor_name: String::new(),
        hw_name: "1.2.3".into(),
        fw_name: "4.5".into(),
        ..Default::default()
    };
    assert_eq!(check_versions(&j2534(), &version), Ok(()));
    let padded = ReportedVersion {
        vendor_name: String::new(),
        hw_name: " 1.2.3 ".into(),
        ..version.clone()
    };
    assert_eq!(check_versions(&j2534(), &padded), Ok(()));
    assert_eq!(
        check_versions(
            &j2534(),
            &ReportedVersion {
                vendor_name: String::new(),
                hw_name: "1.2.4".into(),
                ..version.clone()
            }
        ),
        Err(MatchRefusal::FirmwareDiffers {
            reported: "1.2.4".into(),
            profile: "1.2.3".into()
        })
    );
    assert_eq!(
        check_versions(
            &j2534(),
            &ReportedVersion {
                fw_name: "4.6".into(),
                ..version
            }
        ),
        Err(MatchRefusal::LibraryVersionDiffers {
            reported: "4.6".into(),
            profile: "4.5".into()
        })
    );
}

#[test]
fn dpdu_versions_are_compared_as_major_minor_revision() {
    let version = ReportedVersion {
        fw_version: 0x0201_0000,
        pdu_api_sw_version: 0x0300_0700,
        ..Default::default()
    };
    assert_eq!(check_versions(&dpdu(), &version), Ok(()));
    let padded = Vci {
        firmware_version: " 2.1.0 ".into(),
        ..dpdu()
    };
    assert_eq!(check_versions(&padded, &version), Ok(()));
    assert_eq!(
        check_versions(
            &dpdu(),
            &ReportedVersion {
                fw_version: 0x0202_0000,
                ..version.clone()
            }
        ),
        Err(MatchRefusal::FirmwareDiffers {
            reported: "2.2.0".into(),
            profile: "2.1.0".into()
        })
    );
    assert_eq!(
        check_versions(
            &dpdu(),
            &ReportedVersion {
                pdu_api_sw_version: 0x0300_0800,
                ..version
            }
        ),
        Err(MatchRefusal::LibraryVersionDiffers {
            reported: "3.0.8".into(),
            profile: "3.0.7".into()
        })
    );
}

#[test]
fn a_coded_version_ignores_its_low_byte() {
    assert_eq!(coded_version(0x0102_03FF), "1.2.3");
    assert_eq!(coded_version(u32::MAX), "255.255.255");
}
