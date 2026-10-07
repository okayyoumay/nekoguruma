//! Which requests the minimal runner may send (ADR-235 item 8).
//!
//! Until job authorization, approval and the execution preconditions exist (design 5.5, 5.6,
//! 6, 8.9), a job may only read: a request that changes the ECU's state, session or memory is
//! refused before anything is sent. The check runs on the whole program before the link opens
//! and again in the host for every request it sends, so a refused request never reaches the
//! bus.

use diag_ir::{Op, Program};

use crate::host::HostError;

/// ReadDTCInformation and ReadDataByIdentifier: they change nothing on the ECU. TesterPresent
/// is left out: it keeps the ECU's current session alive, which a job may only do once it owns
/// that session.
pub const READ_ONLY_SERVICES: &[u8] = &[0x19, 0x22];

/// Whether a request with this service ID may be sent. A value that is not a UDS service ID
/// fails as [`HostError::BadService`].
pub fn check_service(service: u16) -> Result<(), HostError> {
    let sid = u8::try_from(service).map_err(|_| HostError::BadService(service))?;
    if READ_ONLY_SERVICES.contains(&sid) {
        Ok(())
    } else {
        Err(HostError::NotAllowed(service))
    }
}

/// Whether an encoded request may be sent; its first byte is the service ID.
pub fn check_request(request: &[u8]) -> Result<(), HostError> {
    match request.first() {
        Some(&sid) => check_service(sid.into()),
        None => Err(HostError::NotAllowed(0)),
    }
}

/// The first instruction of `program` the runner would refuse: its pc and why.
pub fn check_program(program: &Program) -> Result<(), (u32, HostError)> {
    for (pc, op) in program.code.iter().enumerate() {
        // Exhaustive, so a new instruction that sends something cannot slip past.
        let refused = match op {
            Op::ServiceRequest { service } => check_service(*service).err(),
            // Always ReadDTCInformation.
            Op::ReadDtc { .. } => None,
            // A routine can erase or actuate; none is read-only by its number alone.
            Op::RoutineControl { .. } => Some(HostError::NotAllowed(0x31)),
            Op::SecurityAccess { .. } => Some(HostError::NotAllowed(0x27)),
            Op::FlashTransfer { .. } => Some(HostError::NotAllowed(0x36)),
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

#[cfg(test)]
mod tests {
    use diag_ir::IR_SCHEMA_VERSION;

    use super::*;

    #[test]
    fn only_read_only_services_pass() {
        for sid in [0x19, 0x22] {
            check_service(sid).unwrap();
        }
        for sid in [
            0x10, 0x11, 0x14, 0x27, 0x28, 0x2E, 0x2F, 0x31, 0x34, 0x36, 0x37, 0x3E, 0x85,
        ] {
            assert!(
                matches!(check_service(sid), Err(HostError::NotAllowed(s)) if s == sid),
                "{sid:#x}"
            );
        }
    }

    #[test]
    fn values_that_are_not_service_ids_are_bad_services() {
        assert!(matches!(
            check_service(0x122),
            Err(HostError::BadService(0x122))
        ));
    }

    #[test]
    fn encoded_requests_are_checked_by_their_first_byte() {
        check_request(&[0x22, 0xF1, 0x90]).unwrap();
        check_request(&[0x19, 0x02, 0x08]).unwrap();
        assert!(matches!(
            check_request(&[0x31, 0x01, 0xFF, 0x00]),
            Err(HostError::NotAllowed(0x31))
        ));
        assert!(matches!(
            check_request(&[0x2E, 0x22]),
            Err(HostError::NotAllowed(0x2E))
        ));
        assert!(matches!(check_request(&[]), Err(HostError::NotAllowed(0))));
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
            let refused = check_program(&program(vec![Op::PushBytes(0), op]));
            assert!(
                matches!(refused, Err((1, HostError::NotAllowed(s))) if s == sid),
                "{refused:?}"
            );
        }
    }
}
