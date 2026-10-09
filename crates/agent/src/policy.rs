//! Which requests the minimal runner may send (ADR-235 item 8, ADR-247).
//!
//! Until job authorization, approval and the execution preconditions exist (design 5.5, 5.6,
//! 6, 8.9), a job may only read: a request that changes the ECU's state, session or memory is
//! refused before anything is sent. The check runs on the whole program before the link opens,
//! again once the link is open and the VCI is known, and in the host for every request it
//! sends, so a refused request never reaches the bus.
//!
//! The one exception is a debug build talking to the `sim-vci` simulator (ADR-247): there a job
//! may send any request, so the write paths can be exercised without hardware. The
//! [`Permission::Simulator`] variant does not exist in a release build, so no release code can
//! name it, let alone grant it.

use diag_ir::{Op, Program};

use crate::host::HostError;

/// What a job may send, decided per link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Only [`READ_ONLY_SERVICES`] (ADR-235 item 8).
    ReadOnly,
    /// Any request the host implements, on the simulated VCI of a debug build (ADR-247).
    /// The `FlashTransfer` instruction is covered too (ADR-250). The `SecurityAccess`
    /// instruction is still refused: a standalone agent has no key source (ADR-259).
    #[cfg(debug_assertions)]
    Simulator,
}

/// ReadDTCInformation and ReadDataByIdentifier: they change nothing on the ECU. TesterPresent
/// is left out: it keeps the ECU's current session alive, which a job may only do once it owns
/// that session.
pub const READ_ONLY_SERVICES: &[u8] = &[0x19, 0x22];

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

/// The permission for the VCI a worker reports (ADR-247): the simulator one only when the
/// firmware and DLL names are those `sim-vci` reports through `j2534-0404-service` (`hw_name`
/// carries the firmware version string, `fw_name` the DLL version string) and that service
/// itself answered (it sets `vendor_name` and `pdu_api_sw_name` to constants). The last two
/// matter because another worker, `iso22900-service`, fills `hw_name` and `fw_name` from other
/// sources, so the meaning of those two fields holds only for the J2534 worker. Anything else,
/// including a vendor interface, is read-only. Debug builds only: a release build never asks.
#[cfg(debug_assertions)]
pub fn identify(version: &vci_service_interface::VersionData) -> Permission {
    if version.hw_name.starts_with("NGR-SIM ")
        && version.fw_name.starts_with("sim-vci ")
        && version.vendor_name == J2534_SERVICE_VENDOR_NAME
        && version.pdu_api_sw_name == J2534_SERVICE_PDU_API_SW_NAME
    {
        Permission::Simulator
    } else {
        Permission::ReadOnly
    }
}

/// Whether a request with this service ID may be sent. A value that is not a UDS service ID
/// fails as [`HostError::BadService`].
pub fn check_service(service: u16, permission: Permission) -> Result<(), HostError> {
    let sid = u8::try_from(service).map_err(|_| HostError::BadService(service))?;
    match permission {
        Permission::ReadOnly if READ_ONLY_SERVICES.contains(&sid) => Ok(()),
        Permission::ReadOnly => Err(HostError::NotAllowed(service)),
        #[cfg(debug_assertions)]
        Permission::Simulator => Ok(()),
    }
}

/// Whether an encoded request may be sent; its first byte is the service ID.
pub fn check_request(request: &[u8], permission: Permission) -> Result<(), HostError> {
    match request.first() {
        Some(&sid) => check_service(sid.into(), permission),
        None => Err(HostError::NotAllowed(0)),
    }
}

/// The first instruction of `program` the runner would refuse: its pc and why.
pub fn check_program(program: &Program, permission: Permission) -> Result<(), (u32, HostError)> {
    for (pc, op) in program.code.iter().enumerate() {
        // Exhaustive, so a new instruction that sends something cannot slip past.
        let refused = match op {
            // TransferData goes through `FlashTransfer` and the host's block count (ADR-250),
            // whatever the permission; refused here so it fails before anything is erased.
            Op::ServiceRequest { service: 0x36 } => Some(HostError::UseFlashTransfer),
            Op::ServiceRequest { service } => check_service(*service, permission).err(),
            // Always ReadDTCInformation.
            Op::ReadDtc { .. } => None,
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
            | Op::HmiRequest { .. }
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
            | Op::IndexSet => None,
        };
        if let Some(error) = refused {
            return Err((pc as u32, error));
        }
    }
    Ok(())
}

/// Whether the program sends anything a read-only job may not (ADR-257), so it needs the
/// reprogramming slot.
pub fn writes(program: &Program) -> bool {
    check_program(program, Permission::ReadOnly).is_err()
}

#[cfg(test)]
mod tests {
    use diag_ir::IR_SCHEMA_VERSION;

    use super::*;

    const READ_ONLY: Permission = Permission::ReadOnly;

    #[test]
    fn only_read_only_services_pass() {
        for sid in [0x19, 0x22] {
            check_service(sid, READ_ONLY).unwrap();
        }
        for sid in [
            0x10, 0x11, 0x14, 0x27, 0x28, 0x2E, 0x2F, 0x31, 0x34, 0x36, 0x37, 0x3E, 0x85,
        ] {
            assert!(
                matches!(check_service(sid, READ_ONLY), Err(HostError::NotAllowed(s)) if s == sid),
                "{sid:#x}"
            );
        }
    }

    #[test]
    fn values_that_are_not_service_ids_are_bad_services() {
        assert!(matches!(
            check_service(0x122, READ_ONLY),
            Err(HostError::BadService(0x122))
        ));
    }

    #[test]
    fn encoded_requests_are_checked_by_their_first_byte() {
        check_request(&[0x22, 0xF1, 0x90], READ_ONLY).unwrap();
        check_request(&[0x19, 0x02, 0x08], READ_ONLY).unwrap();
        assert!(matches!(
            check_request(&[0x31, 0x01, 0xFF, 0x00], READ_ONLY),
            Err(HostError::NotAllowed(0x31))
        ));
        assert!(matches!(
            check_request(&[0x2E, 0x22], READ_ONLY),
            Err(HostError::NotAllowed(0x2E))
        ));
        assert!(matches!(
            check_request(&[], READ_ONLY),
            Err(HostError::NotAllowed(0))
        ));
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
                Op::ReadDtc { mask: 0x08 },
                Op::Jump(0),
            ]),
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
            let refused = check_program(&program(vec![Op::PushBytes(0), op]), READ_ONLY);
            assert!(
                matches!(refused, Err((1, HostError::NotAllowed(s))) if s == sid),
                "{refused:?}"
            );
        }
        let refused = check_program(
            &program(vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x36 }]),
            READ_ONLY,
        );
        assert!(
            matches!(refused, Err((1, HostError::UseFlashTransfer))),
            "{refused:?}"
        );
    }

    #[test]
    fn a_program_writes_when_a_read_only_job_may_not_send_it() {
        assert!(!writes(&program(vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::ReadDtc { mask: 0x08 },
        ])));
        assert!(!writes(&program(Vec::new())));
        for op in [
            Op::ServiceRequest { service: 0x2E },
            Op::RoutineControl {
                routine: 0xFF00,
                sub: 1,
            },
            Op::FlashTransfer { block: 0 },
        ] {
            assert!(writes(&program(vec![Op::PushBytes(0), op])));
        }
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
                check_service(sid, SIMULATOR).unwrap_or_else(|e| panic!("{sid:#x}: {e}"));
            }
            for sid in [0x2E, 0x31, 0x34] {
                check_request(&[sid, 0x01], SIMULATOR).unwrap();
            }
        }

        #[test]
        fn values_that_are_not_service_ids_are_still_bad_services() {
            assert!(matches!(
                check_service(0x122, SIMULATOR),
                Err(HostError::BadService(0x122))
            ));
            assert!(matches!(
                check_request(&[], SIMULATOR),
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
                SIMULATOR,
            )
            .unwrap();
            let refused = check_program(
                &program(vec![Op::PushBytes(0), Op::SecurityAccess { level: 1 }]),
                SIMULATOR,
            );
            assert!(
                matches!(refused, Err((1, HostError::NotAllowed(0x27)))),
                "{refused:?}"
            );
            // TransferData only through FlashTransfer, refused before the link opens.
            let refused = check_program(
                &program(vec![
                    Op::ServiceRequest { service: 0x34 },
                    Op::PushBytes(0),
                    Op::ServiceRequest { service: 0x36 },
                ]),
                SIMULATOR,
            );
            assert!(
                matches!(refused, Err((2, HostError::UseFlashTransfer))),
                "{refused:?}"
            );
            let refused = check_program(
                &program(vec![Op::ServiceRequest { service: 0x122 }]),
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
    }
}
