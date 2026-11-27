//! A job's result as JSON: what `ngr-agent run` prints and what the intake enqueues in the upload
//! ledger (design 5.2, 5.3, ADR-397, ADR-511 item 8).

use diag_ir::{Value, VmState};

/// Why a result cannot be written as JSON.
#[derive(Debug, thiserror::Error)]
pub enum ResultJsonError {
    /// `serde_json` would print an infinite or NaN value as `null`, which no longer reads back as
    /// the state.
    #[error("the final state holds a non-finite float, which JSON cannot represent")]
    NonFinite,
    #[error("cannot print the result: {0}")]
    Encode(#[source] serde_json::Error),
}

/// The final state of a run as JSON text.
pub fn vm_state_json(state: &VmState) -> Result<String, ResultJsonError> {
    if holds_non_finite_float(state) {
        return Err(ResultJsonError::NonFinite);
    }
    serde_json::to_string(state).map_err(ResultJsonError::Encode)
}

/// Whether any value in `state` is an infinite or NaN `F64`.
fn holds_non_finite_float(state: &VmState) -> bool {
    let frames = state.call_stack.iter().flat_map(|frame| &frame.locals);
    state
        .stack
        .iter()
        .chain(
            state
                .locals
                .iter()
                .chain(&state.globals)
                .chain(frames)
                .flatten(),
        )
        .any(|value| matches!(value, Value::F64(number) if !number.is_finite()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_non_finite_floats_anywhere_in_the_state() {
        let finite = VmState {
            schema_version: 0,
            pc: 0,
            stack: vec![Value::F64(1.5), Value::Bytes(vec![1])],
            locals: vec![None, Some(Value::F64(-0.0))],
            globals: vec![Some(Value::I64(1))],
            call_stack: vec![diag_ir::Frame {
                return_pc: 0,
                locals: vec![Some(Value::F64(f64::MAX))],
            }],
            steps: 0,
            target: 0,
        };
        vm_state_json(&finite).unwrap();

        let mut stack = finite.clone();
        stack.stack.push(Value::F64(f64::INFINITY));
        let mut global = finite.clone();
        global.globals.push(Some(Value::F64(f64::NAN)));
        let mut frame = finite;
        frame.call_stack[0]
            .locals
            .push(Some(Value::F64(f64::NEG_INFINITY)));
        for state in [stack, global, frame] {
            assert!(
                matches!(vm_state_json(&state), Err(ResultJsonError::NonFinite)),
                "{state:?}"
            );
        }
    }
}
