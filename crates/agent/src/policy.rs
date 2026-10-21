//! Which requests the minimal runner may send (ADR-235 item 8, ADR-247, ADR-342).
//!
//! Until job authorization, approval and the execution preconditions exist (design 5.5, 5.6,
//! 6, 8.9), a job may only read: a request that changes the ECU's state, session or memory is
//! refused before anything is sent. The check runs on the whole program before the link opens,
//! again once the link is open and the VCI is known, and in the host for every request it
//! sends, so a refused request never reaches the bus.
//!
//! The one exception is a debug build talking to a simulated VCI, `sim-vci` (ADR-247) or
//! `iso22900-mock` (ADR-331): there a job may send any request, so the write paths can be
//! exercised without hardware. The
//! [`Permission::Simulator`] variant does not exist in a release build, so no release code can
//! name it, let alone grant it.

use diag_ir::declaration::EcuVariant;
use diag_ir::{Op, Program};

use crate::host::HostError;

/// What a job may send, decided per link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Only the read-only services of the link's application layer
    /// ([`ApplicationLayer::read_only_services`], ADR-235 item 8, ADR-342).
    ReadOnly,
    /// Any request the host implements, on a simulated VCI of a debug build (ADR-247, ADR-331).
    /// The `FlashTransfer` instruction is covered too (ADR-250). The `SecurityAccess`
    /// instruction is still refused: a standalone agent has no key source (ADR-259).
    #[cfg(debug_assertions)]
    Simulator,
}

/// An application layer the agent has a read-only list for (ADR-309 item 4, ADR-342). Adding one
/// is an agent change, since the match in [`ApplicationLayer::read_only_services`] must name its
/// list; the program never says which of its services are read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationLayer {
    Uds,
}

impl ApplicationLayer {
    /// The layer a protocol declaration names (`Protocol::application_layer`, ADR-321 item 4),
    /// or `None` when the agent has no list for it.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "UDS" => Some(Self::Uds),
            _ => None,
        }
    }

    /// The service identifiers a read-only job may send on this layer.
    pub fn read_only_services(self) -> &'static [u8] {
        match self {
            Self::Uds => UDS_READ_ONLY_SERVICES,
        }
    }

    /// The layer a job's requests are judged by before its link opens: the one every protocol
    /// of its variant names (ADR-309 item 3). `None`, so that a read-only job may send nothing,
    /// when the variant declares no protocol, protocols naming different layers, or a layer
    /// without a list here. A job without a document runs on a link its caller built, which is
    /// UDS on CAN (`LinkConfig::validate`).
    pub fn of_job(variant: Option<&EcuVariant<'_>>) -> Option<Self> {
        let Some(variant) = variant else {
            return Some(Self::Uds);
        };
        let mut layers = variant
            .protocols()
            .into_iter()
            .flatten()
            .map(|protocol| protocol.application_layer().and_then(Self::named));
        let first = layers.next()??;
        layers.all(|layer| layer == Some(first)).then_some(first)
    }
}

/// UDS's read-only services: ReadDTCInformation and ReadDataByIdentifier, which change nothing
/// on the ECU. TesterPresent is left out: it keeps the ECU's current session alive, which a job
/// may only do once it owns that session.
pub const UDS_READ_ONLY_SERVICES: &[u8] = &[0x19, 0x22];

/// The most a job may get in this build, before the VCI behind the worker is known: the
/// simulator permission in a debug build, read-only in a release build (ADR-247). The
/// pre-open check of a program uses it, so a release build refuses a write before anything
/// opens.
#[cfg(debug_assertions)]
pub fn build_ceiling() -> Permission {
    Permission::Simulator
}

/// See the debug build's `build_ceiling`.
#[cfg(not(debug_assertions))]
pub fn build_ceiling() -> Permission {
    Permission::ReadOnly
}

/// What `j2534-0404-service` puts in `VersionData::vendor_name` and `pdu_api_sw_name`.
#[cfg(debug_assertions)]
const J2534_SERVICE_VENDOR_NAME: &str = "j2534-0404";
#[cfg(debug_assertions)]
const J2534_SERVICE_PDU_API_SW_NAME: &str = "J2534";

/// What `iso22900-mock` reports through `iso22900-service` (ADR-331): the service passes the
/// library's version data through unchanged, so all four come from the mock.
#[cfg(debug_assertions)]
const ISO22900_MOCK_VERSION: [&str; 4] = [
    "Mock Hardware",
    "Mock Firmware",
    "Mock Vendor",
    "iso22900-mock",
];

/// The permission for the VCI a worker reports (ADR-247, ADR-331): the simulator one only for
/// the two simulated VCIs. The first is `sim-vci` through `j2534-0404-service`: its firmware and
/// DLL names (`hw_name` carries the firmware version string, `fw_name` the DLL version string),
/// and the constant `vendor_name` and `pdu_api_sw_name` that service sets. The last two matter
/// because another worker, `iso22900-service`, fills `hw_name` and `fw_name` from other sources,
/// so the meaning of those two fields holds only for the J2534 worker; they rule out an honest
/// D-PDU library, though `iso22900-service` copies a library's own names (ADR-331). The
/// second is `iso22900-mock` through `iso22900-service`, which reports exactly the mock's four
/// names; `j2534-0404-service` cannot report them, since its API name is always `J2534`.
/// Anything else, including a vendor interface, is read-only. Debug builds only: a release build
/// never asks.
#[cfg(debug_assertions)]
pub fn identify(version: &vci_service_interface::VersionData) -> Permission {
    let sim_vci = version.hw_name.starts_with("NGR-SIM ")
        && version.fw_name.starts_with("sim-vci ")
        && version.vendor_name == J2534_SERVICE_VENDOR_NAME
        && version.pdu_api_sw_name == J2534_SERVICE_PDU_API_SW_NAME;
    let iso22900_mock = [
        version.hw_name.as_str(),
        version.fw_name.as_str(),
        version.vendor_name.as_str(),
        version.pdu_api_sw_name.as_str(),
    ] == ISO22900_MOCK_VERSION;
    if sim_vci || iso22900_mock {
        Permission::Simulator
    } else {
        Permission::ReadOnly
    }
}

/// Whether a request with this service ID may be sent on a link carrying `layer`. A value that
/// is not a one-byte service ID fails as [`HostError::BadService`]. A read-only job may send
/// nothing on a layer without a list (`None`).
pub fn check_service(
    service: u16,
    layer: Option<ApplicationLayer>,
    permission: Permission,
) -> Result<(), HostError> {
    let sid = u8::try_from(service).map_err(|_| HostError::BadService(service))?;
    match permission {
        Permission::ReadOnly
            if layer.is_some_and(|layer| layer.read_only_services().contains(&sid)) =>
        {
            Ok(())
        }
        Permission::ReadOnly => Err(HostError::NotAllowed(service)),
        #[cfg(debug_assertions)]
        Permission::Simulator => Ok(()),
    }
}

/// Whether an encoded request may be sent; its first byte is the service ID.
pub fn check_request(
    request: &[u8],
    layer: Option<ApplicationLayer>,
    permission: Permission,
) -> Result<(), HostError> {
    match request.first() {
        Some(&sid) => check_service(sid.into(), layer, permission),
        None => Err(HostError::NotAllowed(0)),
    }
}

/// The first instruction of `program` the runner would refuse: its pc and why. `variant` is the
/// job's document's, which gives a `SendReceive`'s SID (ADR-322 item 1) and the application
/// layer the SIDs are judged by ([`ApplicationLayer::of_job`]); without one, every
/// `SendReceive` is refused.
pub fn check_program(
    program: &Program,
    variant: Option<&EcuVariant<'_>>,
    permission: Permission,
) -> Result<(), (u32, HostError)> {
    let layer = ApplicationLayer::of_job(variant);
    for (pc, op) in program.code.iter().enumerate() {
        // Exhaustive, so a new instruction that sends something cannot slip past.
        let refused = match op {
            // TransferData goes through `FlashTransfer` and the host's block count (ADR-250),
            // whatever the permission; refused here so it fails before anything is erased.
            Op::ServiceRequest { service: 0x36 } => Some(HostError::UseFlashTransfer),
            Op::ServiceRequest { service } => check_service(*service, layer, permission).err(),
            // The frontend lowers `diag.read_dtc` to a send-receive (ADR-309 item 4); the agent
            // builds no request for this one (ADR-342), whatever the permission.
            Op::ReadDtc { .. } => Some(HostError::Unsupported("ReadDtc")),
            // The host sends only a request that starts with the declared service's SID
            // (ADR-322 item 1), so that SID is checked as a `ServiceRequest`'s would be. A
            // download belongs at a recovery plan's boundary, which only a `ServiceRequest`
            // can be (item 2).
            Op::SendReceive { service } => {
                match variant.and_then(|v| crate::send_receive::declared_sid(v, *service)) {
                    None => Some(HostError::UndeclaredService(*service)),
                    Some(sid) => crate::send_receive::check_flash_rules(sid, false)
                        .and_then(|()| check_service(sid.into(), layer, permission))
                        .err(),
                }
            }
            // A routine can erase or actuate; none is read-only by its number alone.
            Op::RoutineControl { .. } => match permission {
                Permission::ReadOnly => Some(HostError::NotAllowed(0x31)),
                #[cfg(debug_assertions)]
                Permission::Simulator => None,
            },
            // A standalone agent has no key source, whatever the permission (ADR-259).
            Op::SecurityAccess { .. } => Some(HostError::NotAllowed(0x27)),
            // TransferData rewrites the ECU's memory: simulator only (ADR-250).
            Op::FlashTransfer { .. } => match permission {
                Permission::ReadOnly => Some(HostError::NotAllowed(0x36)),
                #[cfg(debug_assertions)]
                Permission::Simulator => None,
            },
            // Answered by the agent or the server; nothing reaches the ECU.
            Op::Wait { .. }
            | Op::WaitFromStack
            | Op::LogFromStack { .. }
            | Op::HmiRequest { .. }
            | Op::HmiRequestFromStack { .. }
            | Op::RecordInput { .. }
            | Op::MonitorCapture { .. }
            | Op::Log { .. } => None,
            Op::PushI64(_)
            | Op::PushF64(_)
            | Op::PushBytes(_)
            | Op::Pop
            | Op::Dup
            | Op::Swap
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::CmpEq
            | Op::CmpLt
            | Op::CmpGt
            | Op::Not
            | Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::Call(_)
            | Op::Ret
            | Op::LoadLocal(_)
            | Op::StoreLocal(_)
            | Op::LoadGlobal(_)
            | Op::StoreGlobal(_)
            | Op::IndexGet
            | Op::IndexSet
            | Op::Length
            | Op::Rem
            | Op::Neg
            | Op::Concat
            | Op::Slice
            | Op::IntToFloat
            | Op::FloatToInt
            | Op::ShlChecked
            // Decodes in the VM; sends nothing (ADR-316).
            | Op::DecodeField { .. }
            // Reads the artifact through the host; sends nothing (ADR-336 item 12).
            | Op::FlashBlock { .. }
            // Reads a runtime input through the host; sends nothing (ADR-338).
            | Op::ReadInput { .. }
            // Ends the procedure; sends nothing (ADR-280).
            | Op::Fail { .. } => None,
        };
        if let Some(error) = refused {
            return Err((pc as u32, error));
        }
    }
    Ok(())
}

/// Whether the program sends anything a read-only job may not (ADR-257), so it needs the
/// reprogramming slot. `variant` is as for [`check_program`].
pub fn writes(program: &Program, variant: Option<&EcuVariant<'_>>) -> bool {
    check_program(program, variant, Permission::ReadOnly).is_err()
}

#[cfg(test)]
mod tests {
    use diag_ir::IR_SCHEMA_VERSION;

    use super::*;
    use crate::send_receive::tests::{
        DOWNLOAD, NO_REQUEST, READ_DTC, RESET, SESSION, TRANSFER_DATA, TRANSFER_EXIT, WRITE_DID,
        document, document_with_protocols,
    };

    const READ_ONLY: Permission = Permission::ReadOnly;
    const UDS: Option<ApplicationLayer> = Some(ApplicationLayer::Uds);

    #[test]
    fn only_read_only_services_pass() {
        for sid in [0x19, 0x22] {
            check_service(sid, UDS, READ_ONLY).unwrap();
        }
        for sid in [
            0x10, 0x11, 0x14, 0x27, 0x28, 0x2E, 0x2F, 0x31, 0x34, 0x36, 0x37, 0x3E, 0x85,
        ] {
            assert!(
                matches!(check_service(sid, UDS, READ_ONLY), Err(HostError::NotAllowed(s)) if s == sid),
                "{sid:#x}"
            );
        }
    }

    #[test]
    fn values_that_are_not_service_ids_are_bad_services() {
        assert!(matches!(
            check_service(0x122, UDS, READ_ONLY),
            Err(HostError::BadService(0x122))
        ));
    }

    #[test]
    fn encoded_requests_are_checked_by_their_first_byte() {
        check_request(&[0x22, 0xF1, 0x90], UDS, READ_ONLY).unwrap();
        check_request(&[0x19, 0x02, 0x08], UDS, READ_ONLY).unwrap();
        assert!(matches!(
            check_request(&[0x31, 0x01, 0xFF, 0x00], UDS, READ_ONLY),
            Err(HostError::NotAllowed(0x31))
        ));
        assert!(matches!(
            check_request(&[0x2E, 0x22], UDS, READ_ONLY),
            Err(HostError::NotAllowed(0x2E))
        ));
        assert!(matches!(
            check_request(&[], UDS, READ_ONLY),
            Err(HostError::NotAllowed(0))
        ));
    }

    /// The read-only list is the link's application layer's (ADR-342): a layer without one
    /// lets a read-only job send nothing, not even a read.
    #[test]
    fn the_read_only_list_is_the_application_layer_s() {
        assert_eq!(ApplicationLayer::Uds.read_only_services(), [0x19, 0x22]);
        for sid in [0x19, 0x22, 0x10, 0x2E] {
            assert!(
                matches!(check_service(sid, None, READ_ONLY), Err(HostError::NotAllowed(s)) if s == sid),
                "{sid:#x}"
            );
        }
        assert!(matches!(
            check_request(&[0x22, 0xF1, 0x90], None, READ_ONLY),
            Err(HostError::NotAllowed(0x22))
        ));
        assert_eq!(ApplicationLayer::named("UDS"), UDS);
        for name in ["KWP2000", "uds", "UDS ", ""] {
            assert_eq!(ApplicationLayer::named(name), None, "{name:?}");
        }
    }

    fn protocol(layer: Option<&str>) -> diag_ir::declaration::ProtocolT {
        let mut protocol = crate::document::tests::uds_on_can();
        protocol.application_layer = layer.map(Into::into);
        protocol
    }

    /// Before the link opens, a job's requests are judged by the layer every protocol of its
    /// variant names; anything else leaves no list, so a read-only job may send nothing.
    #[test]
    fn a_job_s_layer_is_the_one_its_variant_s_protocols_name() {
        assert_eq!(ApplicationLayer::of_job(None), UDS);
        let layer = |layers: &[Option<&str>]| {
            let document = document_with_protocols(
                layers.iter().map(|layer| protocol(*layer)).collect(),
                None,
            );
            ApplicationLayer::of_job(Some(&document.variant()))
        };
        assert_eq!(layer(&[Some("UDS")]), UDS);
        assert_eq!(layer(&[Some("UDS"), Some("UDS")]), UDS);
        for layers in [
            &[][..],
            &[Some("KWP2000")],
            &[None],
            &[Some("UDS"), Some("KWP2000")],
            &[Some("UDS"), None],
        ] {
            assert_eq!(layer(layers), None, "{layers:?}");
        }
        // A read on a variant whose layer has no list is refused, as a send-receive and as a
        // request; on UDS the same program passes.
        let reads = program(vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::PushBytes(0),
            Op::SendReceive { service: READ_DTC },
        ]);
        let document = document_with_protocols(vec![protocol(Some("UDS"))], None);
        check_program(&reads, Some(&document.variant()), READ_ONLY).unwrap();
        let document = document_with_protocols(vec![protocol(Some("KWP2000"))], None);
        let refused = check_program(&reads, Some(&document.variant()), READ_ONLY);
        assert!(
            matches!(refused, Err((1, HostError::NotAllowed(0x22)))),
            "{refused:?}"
        );
        let refused = check_program(
            &send_receive(READ_DTC),
            Some(&document.variant()),
            READ_ONLY,
        );
        assert!(
            matches!(refused, Err((1, HostError::NotAllowed(0x19)))),
            "{refused:?}"
        );
        // A release build's ceiling is read-only, so the same holds for it.
        #[cfg(not(debug_assertions))]
        assert!(matches!(
            check_program(&reads, Some(&document.variant()), build_ceiling()),
            Err((1, HostError::NotAllowed(0x22)))
        ));
    }

    /// The agent builds no request for `ReadDtc` (ADR-342): the frontend lowers `diag.read_dtc`
    /// to a send-receive, so the instruction is refused before anything opens, whatever the
    /// permission.
    #[test]
    fn read_dtc_is_refused_whatever_the_permission() {
        let read_dtc = program(vec![Op::PushI64(1), Op::ReadDtc { mask: 0x08 }]);
        for permission in [READ_ONLY, build_ceiling()] {
            let refused = check_program(&read_dtc, None, permission);
            assert!(
                matches!(refused, Err((1, HostError::Unsupported("ReadDtc")))),
                "{refused:?}"
            );
        }
    }

    fn program(code: Vec<Op>) -> Program {
        Program {
            schema_version: IR_SCHEMA_VERSION,
            code,
            constants: Vec::new(),
            sections: Vec::new(),
            source_map: Vec::new(),
            identity: Default::default(),
            preconditions: Default::default(),
            flash: Vec::new(),
        }
    }

    #[test]
    fn the_first_refused_instruction_is_reported() {
        check_program(
            &program(vec![
                Op::PushBytes(0),
                Op::ServiceRequest { service: 0x22 },
                Op::ServiceRequest { service: 0x19 },
                Op::Jump(0),
            ]),
            None,
            READ_ONLY,
        )
        .unwrap();
        let refused = check_program(
            &program(vec![
                Op::PushBytes(0),
                Op::ServiceRequest { service: 0x22 },
                Op::ServiceRequest { service: 0x2E },
                Op::ServiceRequest { service: 0x11 },
            ]),
            None,
            READ_ONLY,
        );
        assert!(
            matches!(refused, Err((2, HostError::NotAllowed(0x2E)))),
            "{refused:?}"
        );
        for (op, sid) in [
            (
                Op::RoutineControl {
                    routine: 0xFF00,
                    sub: 1,
                },
                0x31,
            ),
            (Op::SecurityAccess { level: 1 }, 0x27),
            (Op::FlashTransfer { block: 0 }, 0x36),
        ] {
            let refused = check_program(&program(vec![Op::PushBytes(0), op]), None, READ_ONLY);
            assert!(
                matches!(refused, Err((1, HostError::NotAllowed(s))) if s == sid),
                "{refused:?}"
            );
        }
        let refused = check_program(
            &program(vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x36 }]),
            None,
            READ_ONLY,
        );
        assert!(
            matches!(refused, Err((1, HostError::UseFlashTransfer))),
            "{refused:?}"
        );
    }

    fn send_receive(service: u32) -> Program {
        program(vec![Op::PushBytes(0), Op::SendReceive { service }])
    }

    /// The release agent's read-only rule (ADR-235 item 8) holds for a send-receive: its
    /// declared SID is checked as a `ServiceRequest`'s (ADR-322 item 1). A release build runs
    /// this test with its own ceiling, which is read-only.
    #[test]
    fn a_send_receive_is_checked_by_its_declared_service() {
        let document = document();
        let variant = Some(document.variant());
        check_program(&send_receive(READ_DTC), variant.as_ref(), READ_ONLY).unwrap();
        check_program(&send_receive(READ_DTC), variant.as_ref(), build_ceiling()).unwrap();
        for service in [WRITE_DID, SESSION, RESET] {
            let refused = check_program(&send_receive(service), variant.as_ref(), READ_ONLY);
            assert!(
                matches!(refused, Err((1, HostError::NotAllowed(_)))),
                "{refused:?}"
            );
        }
        #[cfg(not(debug_assertions))]
        assert!(matches!(
            check_program(&send_receive(WRITE_DID), variant.as_ref(), build_ceiling()),
            Err((1, HostError::NotAllowed(0x2E)))
        ));
        // Undeclared, or no document at all.
        for (service, variant) in [
            (NO_REQUEST, variant.as_ref()),
            (99, variant.as_ref()),
            (READ_DTC, None),
        ] {
            let refused = check_program(&send_receive(service), variant, READ_ONLY);
            assert!(
                matches!(refused, Err((1, HostError::UndeclaredService(s))) if s == service),
                "{refused:?}"
            );
        }
        // A download belongs at a plan's boundary (ADR-322 item 2), whatever the permission.
        for (service, sid) in [(DOWNLOAD, 0x34), (TRANSFER_EXIT, 0x37)] {
            let refused = check_program(&send_receive(service), variant.as_ref(), READ_ONLY);
            assert!(
                matches!(refused, Err((1, HostError::DownloadBySendReceive(s))) if s == sid),
                "{refused:?}"
            );
        }
        let refused = check_program(&send_receive(TRANSFER_DATA), variant.as_ref(), READ_ONLY);
        assert!(
            matches!(refused, Err((1, HostError::UseFlashTransfer))),
            "{refused:?}"
        );
    }

    #[test]
    fn a_program_writes_when_a_read_only_job_may_not_send_it() {
        assert!(!writes(
            &program(vec![
                Op::PushBytes(0),
                Op::ServiceRequest { service: 0x22 },
                Op::ServiceRequest { service: 0x19 },
            ]),
            None
        ));
        assert!(!writes(&program(Vec::new()), None));
        for op in [
            Op::ServiceRequest { service: 0x2E },
            Op::RoutineControl {
                routine: 0xFF00,
                sub: 1,
            },
            Op::FlashTransfer { block: 0 },
        ] {
            assert!(writes(&program(vec![Op::PushBytes(0), op]), None));
        }
        // A send-receive writes by its declared service.
        let document = document();
        let variant = Some(document.variant());
        assert!(!writes(&send_receive(READ_DTC), variant.as_ref()));
        assert!(writes(&send_receive(WRITE_DID), variant.as_ref()));
        assert!(writes(&send_receive(READ_DTC), None));
    }

    #[cfg(debug_assertions)]
    mod simulator {
        use vci_service_interface::VersionData;

        use super::*;

        const SIMULATOR: Permission = Permission::Simulator;

        #[test]
        fn any_service_id_passes() {
            for sid in [
                0x10, 0x11, 0x14, 0x19, 0x22, 0x27, 0x28, 0x2E, 0x2F, 0x31, 0x34, 0x36, 0x37, 0x3E,
                0x85, 0xFF,
            ] {
                // Whatever the layer: the simulator permission has no list.
                check_service(sid, None, SIMULATOR).unwrap_or_else(|e| panic!("{sid:#x}: {e}"));
            }
            for sid in [0x2E, 0x31, 0x34] {
                check_request(&[sid, 0x01], None, SIMULATOR).unwrap();
            }
        }

        #[test]
        fn values_that_are_not_service_ids_are_still_bad_services() {
            assert!(matches!(
                check_service(0x122, None, SIMULATOR),
                Err(HostError::BadService(0x122))
            ));
            assert!(matches!(
                check_request(&[], None, SIMULATOR),
                Err(HostError::NotAllowed(0))
            ));
        }

        #[test]
        fn routines_and_flash_transfer_pass_but_security_access_does_not() {
            check_program(
                &program(vec![
                    Op::PushBytes(0),
                    Op::ServiceRequest { service: 0x10 },
                    Op::ServiceRequest { service: 0x34 },
                    Op::RoutineControl {
                        routine: 0xFF01,
                        sub: 1,
                    },
                    Op::PushBytes(0),
                    Op::FlashTransfer { block: 0 },
                ]),
                None,
                SIMULATOR,
            )
            .unwrap();
            let refused = check_program(
                &program(vec![Op::PushBytes(0), Op::SecurityAccess { level: 1 }]),
                None,
                SIMULATOR,
            );
            assert!(
                matches!(refused, Err((1, HostError::NotAllowed(0x27)))),
                "{refused:?}"
            );
            // A send-receive of any declared service but a download.
            let document = document();
            let variant = Some(document.variant());
            for service in [READ_DTC, WRITE_DID, SESSION, RESET] {
                check_program(&send_receive(service), variant.as_ref(), SIMULATOR)
                    .unwrap_or_else(|e| panic!("{service}: {e:?}"));
            }
            let refused = check_program(&send_receive(DOWNLOAD), variant.as_ref(), SIMULATOR);
            assert!(
                matches!(refused, Err((1, HostError::DownloadBySendReceive(0x34)))),
                "{refused:?}"
            );
            // TransferData only through FlashTransfer, refused before the link opens.
            let refused = check_program(
                &program(vec![
                    Op::ServiceRequest { service: 0x34 },
                    Op::PushBytes(0),
                    Op::ServiceRequest { service: 0x36 },
                ]),
                None,
                SIMULATOR,
            );
            assert!(
                matches!(refused, Err((2, HostError::UseFlashTransfer))),
                "{refused:?}"
            );
            let refused = check_program(
                &program(vec![Op::ServiceRequest { service: 0x122 }]),
                None,
                SIMULATOR,
            );
            assert!(
                matches!(refused, Err((0, HostError::BadService(0x122)))),
                "{refused:?}"
            );
        }

        #[test]
        fn a_debug_build_may_reach_the_simulator_permission() {
            assert_eq!(build_ceiling(), Permission::Simulator);
        }

        fn version(hw_name: &str, fw_name: &str) -> VersionData {
            version_from(hw_name, fw_name, "j2534-0404", "J2534")
        }

        fn version_from(
            hw_name: &str,
            fw_name: &str,
            vendor_name: &str,
            pdu_api_sw_name: &str,
        ) -> VersionData {
            VersionData {
                hw_name: hw_name.to_owned(),
                fw_name: fw_name.to_owned(),
                vendor_name: vendor_name.to_owned(),
                pdu_api_sw_name: pdu_api_sw_name.to_owned(),
                ..VersionData::default()
            }
        }

        #[test]
        fn only_the_simulator_names_identify_the_simulator() {
            assert_eq!(
                identify(&version("NGR-SIM 1.0", "sim-vci 0.1.0")),
                Permission::Simulator
            );
            // The right version strings from a worker other than j2534-0404-service.
            for (vendor, api) in [
                ("", ""),
                ("j2534-0404", ""),
                ("", "J2534"),
                ("iso22900", "J2534"),
                ("j2534-0404", "ISO 22900-2"),
                ("J2534-0404", "J2534"),
                ("j2534-0404 ", "J2534"),
                ("j2534-0404", "j2534"),
            ] {
                assert_eq!(
                    identify(&version_from("NGR-SIM 1.0", "sim-vci 0.1.0", vendor, api)),
                    Permission::ReadOnly,
                    "{vendor:?} / {api:?}"
                );
            }
            for (hw, fw) in [
                ("NGR-SIM 1.0", ""),
                ("", "sim-vci 0.1.0"),
                ("NGR-SIM 1.0", "1.0.0"),
                ("1.0.0", "sim-vci 0.1.0"),
                ("", ""),
                ("NGR-SIM", "sim-vci"),
                ("NGR-SIM-ECU", "sim-vci 0.1.0"),
                ("NGR-SIM 1.0", "sim-vci"),
                ("Vendor Interface 2", "Vendor DLL 3.1"),
                (" NGR-SIM 1.0", "sim-vci 0.1.0"),
            ] {
                assert_eq!(
                    identify(&version(hw, fw)),
                    Permission::ReadOnly,
                    "{hw:?} / {fw:?}"
                );
            }
        }

        #[test]
        fn only_the_mock_names_identify_the_d_pdu_simulator() {
            let [hw, fw, vendor, api] = ISO22900_MOCK_VERSION;
            assert_eq!(
                identify(&version_from(hw, fw, vendor, api)),
                Permission::Simulator
            );
            // Each name changed in turn, and the mock's API name from the J2534 worker.
            for names in [
                ["", fw, vendor, api],
                [hw, "", vendor, api],
                [hw, fw, "", api],
                [hw, fw, vendor, ""],
                ["mock hardware", fw, vendor, api],
                [hw, fw, vendor, "iso22900-mock "],
                [hw, fw, vendor, "ISO22900-MOCK"],
                [hw, fw, "Real Vendor", api],
                [hw, fw, "j2534-0404", api],
                [hw, fw, vendor, "J2534"],
            ] {
                let [hw, fw, vendor, api] = names;
                assert_eq!(
                    identify(&version_from(hw, fw, vendor, api)),
                    Permission::ReadOnly,
                    "{names:?}"
                );
            }
        }
    }
}
