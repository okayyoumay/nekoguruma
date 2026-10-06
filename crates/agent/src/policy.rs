//! Which requests the minimal runner may send (ADR-235 item 8).
//!
//! Until job authorization, approval and the execution preconditions exist (design 5.5, 5.6,
//! 6, 8.9), a job may only read: a request that changes the ECU's state, session or memory is
//! refused before anything is sent. The check runs on the whole program before the link opens
//! and again in the host for every request, so a refused request never reaches the bus.

use diag_ir::{Op, Program};

use crate::host::HostError;

/// ReadDTCInformation, ReadDataByIdentifier and TesterPresent: they change nothing on the ECU.
pub const READ_ONLY_SERVICES: &[u8] = &[0x19, 0x22, 0x3E];

/// Whether a request with this service ID may be sent.
pub fn check_service(service: u16) -> Result<(), HostError> {
    match u8::try_from(service) {
        Ok(sid) if READ_ONLY_SERVICES.contains(&sid) => Ok(()),
        _ => Err(HostError::NotAllowed(service)),
    }
}

/// The first instruction of `program` the runner would refuse: its pc and why.
pub fn check_program(program: &Program) -> Result<(), (u32, HostError)> {
    for (pc, op) in program.code.iter().enumerate() {
        let refused = match op {
            Op::ServiceRequest { service } => check_service(*service).err(),
            // A routine can erase or actuate; none is read-only by its number alone.
            Op::RoutineControl { .. } => Some(HostError::NotAllowed(0x31)),
            _ => None,
        };
        if let Some(error) = refused {
            return Err((pc as u32, error));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use diag_ir::IR_SCHEMA_VERSION;

    use super::*;

    #[test]
    fn only_read_only_services_pass() {
        for sid in [0x19, 0x22, 0x3E] {
            check_service(sid).unwrap();
        }
        for sid in [
            0x10, 0x11, 0x14, 0x27, 0x28, 0x2E, 0x2F, 0x31, 0x34, 0x36, 0x37, 0x85, 0x122,
        ] {
            assert!(
                matches!(check_service(sid), Err(HostError::NotAllowed(s)) if s == sid),
                "{sid:#x}"
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
        }
    }

    #[test]
    fn the_first_refused_instruction_is_reported() {
        check_program(&program(vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::ReadDtc { mask: 0x08 },
            Op::Jump(0),
        ]))
        .unwrap();
        let refused = check_program(&program(vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::ServiceRequest { service: 0x2E },
            Op::ServiceRequest { service: 0x11 },
        ]));
        assert!(
            matches!(refused, Err((2, HostError::NotAllowed(0x2E)))),
            "{refused:?}"
        );
        assert!(matches!(
            check_program(&program(vec![Op::RoutineControl {
                routine: 0xFF00,
                sub: 1
            }])),
            Err((0, HostError::NotAllowed(0x31)))
        ));
    }
}
