//! Arbitrary bytes as a procedure and a resumed VM state (design 12.1, 8.2.5): a program and
//! state postcard decodes are checked with `Program::validate` and `Vm::check_state`, as the
//! job runner checks a journal before it acts on it, and a state that passes runs for a bounded
//! number of steps. No input may panic, overflow or exhaust memory.
#![no_main]

use std::convert::Infallible;

use diag_ir::{DiagHost, Program, StepOutcome, Vm, VmState};
use libfuzzer_sys::fuzz_target;

/// How many instructions one input may run.
const MAX_STEPS: usize = 10_000;

/// A host that answers every primitive at once, so the run goes on past it.
struct Answering;

impl DiagHost for Answering {
    type Error = Infallible;

    fn service_request(&mut self, _service: u16, payload: &[u8]) -> Result<Vec<u8>, Infallible> {
        Ok(payload.to_vec())
    }
    fn read_dtc(&mut self, mask: u8) -> Result<Vec<u8>, Infallible> {
        Ok(vec![0x59, 0x02, mask])
    }
    fn routine_control(
        &mut self,
        _routine: u16,
        sub: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, Infallible> {
        let mut response = vec![0x71, sub];
        response.extend_from_slice(payload);
        Ok(response)
    }
    fn security_access(
        &mut self,
        _inquiry: u64,
        _level: u8,
        seed: &[u8],
    ) -> Result<Option<Vec<u8>>, Infallible> {
        Ok(Some(seed.to_vec()))
    }
    fn flash_transfer(&mut self, _block: u32, _data: &[u8]) -> Result<(), Infallible> {
        Ok(())
    }
    fn wait(&mut self, _inquiry: u64, _millis: u32) -> Result<bool, Infallible> {
        Ok(true)
    }
    fn hmi_request(&mut self, _inquiry: u64, form: &[u8]) -> Result<Option<Vec<u8>>, Infallible> {
        Ok(Some(form.to_vec()))
    }
    fn record_input(
        &mut self,
        _inquiry: u64,
        template: &[u8],
    ) -> Result<Option<Vec<u8>>, Infallible> {
        Ok(Some(template.to_vec()))
    }
    fn monitor_capture(&mut self, _back_millis: u32) -> Result<(), Infallible> {
        Ok(())
    }
    fn log(&mut self, _level: u8, _message: &str) {}
}

fuzz_target!(|data: &[u8]| {
    let Ok((program, state)) = postcard::from_bytes::<(Program, VmState)>(data) else {
        return;
    };
    let _ = program.validate();
    let mut vm = Vm::resume(state);
    if vm.check_state(&program).is_err() {
        return;
    }
    let mut host = Answering;
    for _ in 0..MAX_STEPS {
        match vm.step(&program, &mut host) {
            Ok(StepOutcome::Continue) => {}
            _ => break,
        }
    }
});
