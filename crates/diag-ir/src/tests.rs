use std::collections::VecDeque;

use super::*;

#[derive(Debug, thiserror::Error)]
#[error("mock host failure")]
struct MockError;

/// Records every call. Answers come from queues; an empty queue is a host error, `None` in the
/// pending queues means "not answered yet".
#[derive(Default)]
struct MockHost {
    calls: Vec<String>,
    responses: VecDeque<Result<Vec<u8>, MockError>>,
    keys: VecDeque<Option<Vec<u8>>>,
    hmi: VecDeque<Option<Vec<u8>>>,
    /// Answers to `record_input`; when empty, `responses` is used.
    records: VecDeque<Option<Vec<u8>>>,
    /// Answers to `wait`; an empty queue means the time has passed.
    timers: VecDeque<bool>,
    fail_next: bool,
    /// Panic if any host method is called.
    forbidden: bool,
    inquiries: Vec<u64>,
    logs: Vec<(u8, String)>,
}

impl MockHost {
    fn fail(&mut self) -> Result<(), MockError> {
        assert!(!self.forbidden, "the host must not be called");
        if std::mem::take(&mut self.fail_next) {
            Err(MockError)
        } else {
            Ok(())
        }
    }

    fn response(&mut self) -> Result<Vec<u8>, MockError> {
        self.fail()?;
        self.responses.pop_front().unwrap_or(Err(MockError))
    }
}

impl DiagHost for MockHost {
    type Error = MockError;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, MockError> {
        self.calls
            .push(format!("service {service:#x} {payload:02x?}"));
        self.response()
    }
    fn read_dtc(&mut self, mask: u8) -> Result<Vec<u8>, MockError> {
        self.calls.push(format!("read_dtc {mask:#x}"));
        self.response()
    }
    fn routine_control(
        &mut self,
        routine: u16,
        sub: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, MockError> {
        self.calls
            .push(format!("routine {routine:#x} {sub} {payload:02x?}"));
        self.response()
    }
    fn security_access(
        &mut self,
        inquiry: u64,
        level: u8,
        seed: &[u8],
    ) -> Result<Option<Vec<u8>>, MockError> {
        self.calls.push(format!("security {level} {seed:02x?}"));
        self.inquiries.push(inquiry);
        self.fail()?;
        Ok(self.keys.pop_front().flatten())
    }
    fn flash_transfer(&mut self, block: u32, data: &[u8]) -> Result<(), MockError> {
        self.calls.push(format!("flash {block} {data:02x?}"));
        self.fail()
    }
    fn wait(&mut self, inquiry: u64, millis: u32) -> Result<bool, MockError> {
        self.calls.push(format!("wait {millis}"));
        self.inquiries.push(inquiry);
        self.fail()?;
        Ok(self.timers.pop_front().unwrap_or(true))
    }
    fn hmi_request(&mut self, inquiry: u64, form: &[u8]) -> Result<Option<Vec<u8>>, MockError> {
        self.calls.push(format!("hmi {form:02x?}"));
        self.inquiries.push(inquiry);
        self.fail()?;
        Ok(self.hmi.pop_front().flatten())
    }
    fn record_input(
        &mut self,
        inquiry: u64,
        template: &[u8],
    ) -> Result<Option<Vec<u8>>, MockError> {
        self.calls.push(format!("record {template:02x?}"));
        self.inquiries.push(inquiry);
        if !self.records.is_empty() {
            self.fail()?;
            return Ok(self.records.pop_front().flatten());
        }
        self.response().map(Some)
    }
    fn monitor_capture(&mut self, back_millis: u32) -> Result<(), MockError> {
        self.calls.push(format!("capture {back_millis}"));
        self.fail()
    }
    fn log(&mut self, level: u8, message: &str) {
        assert!(!self.forbidden, "the host must not be called");
        self.logs.push((level, message.to_owned()));
    }
}

fn prog(code: Vec<Op>, constants: Vec<Vec<u8>>) -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code,
        constants,
        sections: Vec::new(),
        source_map: Vec::new(),
    }
}

/// Steps until the program finishes, failing on anything else.
fn run(vm: &mut Vm, program: &Program, host: &mut MockHost) {
    for _ in 0..10_000 {
        match vm.step(program, host).expect("step should succeed") {
            StepOutcome::Continue => {}
            StepOutcome::Finished => return,
            StepOutcome::Waiting(on) => panic!("unexpected wait on {on:?}"),
        }
    }
    panic!("program did not finish");
}

/// Runs `code` with an empty host and returns the final stack.
fn eval(code: Vec<Op>) -> Vec<Value> {
    let program = prog(code, Vec::new());
    let mut vm = Vm::new(&program);
    run(&mut vm, &program, &mut MockHost::default());
    vm.state.stack
}

/// Steps once, expects a VM error and checks that the state did not change.
fn expect_vm_error(code: Vec<Op>, constants: Vec<Vec<u8>>, steps_before: usize) -> VmError {
    let program = prog(code, constants);
    let mut vm = Vm::new(&program);
    let mut host = MockHost::default();
    for _ in 0..steps_before {
        vm.step(&program, &mut host).unwrap();
    }
    let before = postcard::to_allocvec(&vm.state).unwrap();
    let error = match vm.step(&program, &mut host) {
        Err(StepError::Vm(e)) => e,
        other => panic!("expected a VM error, got {other:?}"),
    };
    assert_eq!(
        postcard::to_allocvec(&vm.state).unwrap(),
        before,
        "state changed on error"
    );
    error
}

fn i(v: i64) -> Op {
    Op::PushI64(v)
}

#[test]
fn integer_and_float_arithmetic() {
    assert_eq!(
        eval(vec![
            i(7),
            i(3),
            Op::Sub,
            i(4),
            Op::Mul,
            i(5),
            Op::Div,
            i(1),
            Op::Add
        ]),
        [Value::I64(4)]
    );
    assert_eq!(
        eval(vec![
            Op::PushF64(1.5),
            Op::PushF64(2.0),
            Op::Mul,
            Op::PushF64(0.0),
            Op::Div
        ]),
        [Value::F64(f64::INFINITY)]
    );
}

#[test]
fn arithmetic_errors_leave_the_state_unchanged() {
    assert_eq!(
        expect_vm_error(vec![i(i64::MAX), i(1), Op::Add], vec![], 2),
        VmError::Overflow
    );
    assert_eq!(
        expect_vm_error(vec![i(i64::MIN), i(-1), Op::Div], vec![], 2),
        VmError::Overflow
    );
    assert_eq!(
        expect_vm_error(vec![i(1), i(0), Op::Div], vec![], 2),
        VmError::DivisionByZero
    );
    assert_eq!(
        expect_vm_error(vec![i(1), Op::PushF64(1.0), Op::Add], vec![], 2),
        VmError::TypeMismatch
    );
    assert_eq!(
        expect_vm_error(vec![i(1), Op::Add], vec![], 1),
        VmError::StackUnderflow
    );
}

#[test]
fn bitwise_and_shifts() {
    assert_eq!(
        eval(vec![
            i(0b1100),
            i(0b1010),
            Op::BitAnd,
            i(0b0001),
            Op::BitOr,
            i(0b1111),
            Op::BitXor
        ]),
        [Value::I64(0b0110)]
    );
    // Shifts work on the bit pattern: Shr is logical, Shl drops bits without an error.
    assert_eq!(eval(vec![i(-1), i(60), Op::Shr]), [Value::I64(0xF)]);
    assert_eq!(eval(vec![i(1), i(63), Op::Shl]), [Value::I64(i64::MIN)]);
    assert_eq!(eval(vec![i(3), i(63), Op::Shl]), [Value::I64(i64::MIN)]);
    assert_eq!(
        expect_vm_error(vec![i(1), i(64), Op::Shl], vec![], 2),
        VmError::ShiftOutOfRange(64)
    );
    assert_eq!(
        expect_vm_error(vec![i(1), i(-1), Op::Shr], vec![], 2),
        VmError::ShiftOutOfRange(-1)
    );
}

#[test]
fn comparisons() {
    const T: Value = Value::Bool(true);
    const F: Value = Value::Bool(false);
    assert_eq!(eval(vec![i(2), i(2), Op::CmpEq]), [T]);
    assert_eq!(eval(vec![i(1), i(2), Op::CmpLt]), [T]);
    assert_eq!(eval(vec![i(1), i(2), Op::CmpGt]), [F]);
    assert_eq!(
        eval(vec![
            Op::PushF64(f64::NAN),
            Op::PushF64(f64::NAN),
            Op::CmpEq
        ]),
        [F]
    );
    assert_eq!(eval(vec![i(1), i(1), Op::CmpEq, Op::Not]), [F]);
    let bytes = prog(
        vec![Op::PushBytes(0), Op::PushBytes(1), Op::CmpEq],
        vec![vec![1, 2], vec![1, 2]],
    );
    let mut vm = Vm::new(&bytes);
    run(&mut vm, &bytes, &mut MockHost::default());
    assert_eq!(vm.state.stack, [T]);
    assert_eq!(
        expect_vm_error(vec![i(1), Op::PushF64(1.0), Op::CmpEq], vec![], 2),
        VmError::TypeMismatch
    );
    assert_eq!(
        expect_vm_error(
            vec![Op::PushBytes(0), Op::PushBytes(0), Op::CmpLt],
            vec![vec![]],
            2
        ),
        VmError::TypeMismatch
    );
    assert_eq!(
        expect_vm_error(vec![i(0), Op::Not], vec![], 1),
        VmError::TypeMismatch
    );
}

#[test]
fn stack_operations() {
    assert_eq!(
        eval(vec![i(1), i(2), Op::Swap, Op::Dup, Op::Pop]),
        [Value::I64(2), Value::I64(1)]
    );
}

#[test]
fn loop_with_conditional_jump() {
    // global0 = 0; while global0 < 5 { global0 += 1 }; push global0
    let code = vec![
        i(0),
        Op::StoreGlobal(0),
        Op::LoadGlobal(0), // 2
        i(5),
        Op::CmpLt,
        Op::JumpIfFalse(11),
        Op::LoadGlobal(0),
        i(1),
        Op::Add,
        Op::StoreGlobal(0),
        Op::Jump(2),
        Op::LoadGlobal(0), // 11
    ];
    assert_eq!(eval(code), [Value::I64(5)]);
    assert_eq!(
        expect_vm_error(vec![i(1), Op::JumpIfFalse(0)], vec![], 1),
        VmError::TypeMismatch
    );
    assert_eq!(
        expect_vm_error(vec![Op::Jump(2)], vec![], 0),
        VmError::BadPc(2)
    );
    // A jump to the end finishes the program.
    assert_eq!(eval(vec![Op::Jump(2), i(1)]), []);
}

#[test]
fn calls_keep_locals_per_frame() {
    // local0 = 10; call sub; push local0 + returned value. sub: local0 = 32; push local0; ret
    let code = vec![
        i(10),
        Op::StoreLocal(0),
        Op::Call(7),
        Op::LoadLocal(0),
        Op::Add,
        Op::Ret, // top-level return ends the program
        i(99),   // never reached
        i(32),   // 7: sub
        Op::StoreLocal(0),
        Op::LoadLocal(0),
        Op::Ret,
    ];
    assert_eq!(eval(code), [Value::I64(42)]);
    // A subroutine does not see its caller's locals.
    assert_eq!(
        expect_vm_error(
            vec![
                i(1),
                Op::StoreLocal(0),
                Op::Call(4),
                Op::Ret,
                Op::LoadLocal(0)
            ],
            vec![],
            3
        ),
        VmError::UndefinedVariable(0)
    );
    assert_eq!(
        expect_vm_error(vec![Op::LoadGlobal(3)], vec![], 0),
        VmError::UndefinedVariable(3)
    );
}

#[test]
fn call_depth_is_limited() {
    let program = prog(vec![Op::Call(0)], Vec::new());
    let mut vm = Vm::new(&program);
    let mut host = MockHost::default();
    for _ in 0..MAX_CALL_DEPTH {
        vm.step(&program, &mut host).unwrap();
    }
    let before = vm.state.clone();
    assert!(matches!(
        vm.step(&program, &mut host),
        Err(StepError::Vm(VmError::CallDepthExceeded))
    ));
    assert_eq!(vm.state, before);
}

#[test]
fn byte_indexing() {
    let program = prog(
        vec![
            Op::PushBytes(0),
            i(1),
            i(0xAB),
            Op::IndexSet,
            i(1),
            Op::IndexGet,
            Op::Swap,
            i(2),
            Op::IndexGet,
        ],
        vec![vec![0, 0, 7]],
    );
    let mut vm = Vm::new(&program);
    run(&mut vm, &program, &mut MockHost::default());
    // IndexGet leaves the bytes in place and pushes the byte above them.
    assert_eq!(
        vm.state.stack,
        [
            Value::I64(0xAB),
            Value::Bytes(vec![0, 0xAB, 7]),
            Value::I64(7)
        ]
    );

    let constants = || vec![vec![0u8; 2]];
    assert_eq!(
        expect_vm_error(vec![Op::PushBytes(0), i(2), Op::IndexGet], constants(), 2),
        VmError::IndexOutOfRange(2)
    );
    assert_eq!(
        expect_vm_error(vec![Op::PushBytes(0), i(-1), Op::IndexGet], constants(), 2),
        VmError::IndexOutOfRange(-1)
    );
    assert_eq!(
        expect_vm_error(
            vec![Op::PushBytes(0), i(0), i(256), Op::IndexSet],
            constants(),
            3
        ),
        VmError::ByteOutOfRange(256)
    );
    assert_eq!(
        expect_vm_error(vec![i(0), i(0), Op::IndexGet], vec![], 2),
        VmError::TypeMismatch
    );
}

#[test]
fn diagnostic_primitives_reach_the_host() {
    let program = prog(
        vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::ReadDtc { mask: 0x08 },
            Op::PushBytes(1),
            Op::RoutineControl {
                routine: 0xFF00,
                sub: 1,
            },
            Op::PushBytes(1),
            Op::FlashTransfer { block: 3 },
            Op::Wait { millis: 50 },
            Op::RecordInput { template: 2 },
            Op::MonitorCapture { back_millis: 500 },
            Op::Log {
                level: 2,
                message: 3,
            },
        ],
        vec![vec![0xF1, 0x90], vec![0xAA], vec![7], b"done".to_vec()],
    );
    let mut host = MockHost {
        responses: VecDeque::from([
            Ok(vec![0x62, 0xF1, 0x90]),
            Ok(vec![0x59]),
            Ok(vec![0x71]),
            Ok(vec![0x01]),
        ]),
        ..MockHost::default()
    };
    let mut vm = Vm::new(&program);
    run(&mut vm, &program, &mut host);
    assert_eq!(
        host.calls,
        [
            "service 0x22 [f1, 90]",
            "read_dtc 0x8",
            "routine 0xff00 1 [aa]",
            "flash 3 [aa]",
            "wait 50",
            "record [07]",
            "capture 500",
        ]
    );
    assert_eq!(host.logs, [(2, "done".to_owned())]);
    assert_eq!(
        vm.state.stack,
        [
            Value::Bytes(vec![0x62, 0xF1, 0x90]),
            Value::Bytes(vec![0x59]),
            Value::Bytes(vec![0x71]),
            Value::Bytes(vec![0x01]),
        ]
    );
    assert_eq!(vm.state.steps, 11);
}

/// 8.2.5: after a worker crash or VCI disconnect the VM state is retained and execution resumes
/// from the diagnostic primitive, so a failed primitive must leave the state untouched.
#[test]
fn host_error_leaves_the_state_on_the_primitive() {
    let program = prog(
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        vec![vec![0xF1, 0x90]],
    );
    let mut vm = Vm::new(&program);
    let mut host = MockHost::default();
    vm.step(&program, &mut host).unwrap();
    let before = postcard::to_allocvec(&vm.state).unwrap();
    host.responses.push_back(Err(MockError));
    assert!(matches!(
        vm.step(&program, &mut host),
        Err(StepError::Host(MockError))
    ));
    assert_eq!(postcard::to_allocvec(&vm.state).unwrap(), before);

    host.responses.push_back(Ok(vec![0x62]));
    assert_eq!(vm.step(&program, &mut host).unwrap(), StepOutcome::Finished);
    assert_eq!(host.calls.len(), 2, "the primitive is sent again");
    assert_eq!(vm.state.stack, [Value::Bytes(vec![0x62])]);

    for code in [
        vec![Op::PushBytes(0), Op::FlashTransfer { block: 0 }],
        vec![Op::PushBytes(0), Op::Wait { millis: 1 }],
        vec![Op::PushBytes(0), Op::MonitorCapture { back_millis: 1 }],
    ] {
        let primitive = prog(code, vec![vec![1]]);
        let mut vm = Vm::new(&primitive);
        let mut host = MockHost::default();
        vm.step(&primitive, &mut host).unwrap();
        let before = vm.state.clone();
        host.fail_next = true;
        assert!(matches!(
            vm.step(&primitive, &mut host),
            Err(StepError::Host(_))
        ));
        assert_eq!(vm.state, before);
    }
}

#[test]
fn hmi_and_seed_key_wait_without_changing_the_state() {
    let program = prog(
        vec![
            Op::HmiRequest { form: 0 },
            Op::PushBytes(1),
            Op::SecurityAccess { level: 1 },
        ],
        vec![b"form".to_vec(), vec![0x12, 0x34]],
    );
    let mut vm = Vm::new(&program);
    let mut host = MockHost {
        hmi: VecDeque::from([None, Some(b"ok".to_vec())]),
        keys: VecDeque::from([None, None, Some(vec![0xBE, 0xEF])]),
        ..MockHost::default()
    };

    let initial = vm.state.clone();
    assert_eq!(
        vm.step(&program, &mut host).unwrap(),
        StepOutcome::Waiting(WaitingOn::Hmi)
    );
    assert_eq!(vm.state, initial);
    assert_eq!(vm.step(&program, &mut host).unwrap(), StepOutcome::Continue);
    vm.step(&program, &mut host).unwrap();

    let before = vm.state.clone();
    for _ in 0..2 {
        assert_eq!(
            vm.step(&program, &mut host).unwrap(),
            StepOutcome::Waiting(WaitingOn::SeedKey)
        );
        assert_eq!(vm.state, before);
    }
    assert_eq!(vm.step(&program, &mut host).unwrap(), StepOutcome::Finished);
    assert_eq!(
        vm.state.stack,
        [Value::Bytes(b"ok".to_vec()), Value::Bytes(vec![0xBE, 0xEF])]
    );
    assert_eq!(vm.state.steps, 3, "waiting steps are not counted");
}

/// Checks the VM can make before the host call fail without calling the host, so a host answer
/// is never thrown away.
#[test]
fn vm_checks_run_before_the_host_is_called() {
    let mut code = vec![i(0); MAX_STACK];
    code.push(Op::ReadDtc { mask: 0xFF });
    let program = prog(code, Vec::new());
    let mut vm = Vm::new(&program);
    let mut host = MockHost::default();
    for _ in 0..MAX_STACK {
        vm.step(&program, &mut host).unwrap();
    }
    assert!(matches!(
        vm.step(&program, &mut host),
        Err(StepError::Vm(VmError::StackOverflow))
    ));
    assert!(host.calls.is_empty());

    for (code, constants, expected) in [
        (
            vec![i(1), Op::ServiceRequest { service: 0x22 }],
            vec![],
            VmError::TypeMismatch,
        ),
        (
            vec![i(0), Op::ServiceRequest { service: 0x22 }],
            vec![],
            VmError::TypeMismatch,
        ),
        (
            vec![i(0), Op::HmiRequest { form: 9 }],
            vec![],
            VmError::BadConstant(9),
        ),
        (
            vec![i(0), Op::RecordInput { template: 9 }],
            vec![],
            VmError::BadConstant(9),
        ),
        (
            vec![
                i(0),
                Op::Log {
                    level: 0,
                    message: 0,
                },
            ],
            vec![vec![0xFF]],
            VmError::InvalidUtf8(0),
        ),
        (
            vec![i(0), Op::FlashTransfer { block: 0 }],
            vec![],
            VmError::TypeMismatch,
        ),
    ] {
        assert_eq!(expect_vm_error(code, constants, 1), expected);
    }
    assert_eq!(
        expect_vm_error(vec![Op::PushBytes(5)], vec![], 0),
        VmError::BadConstant(5)
    );
}

#[test]
fn stack_depth_is_limited() {
    let program = prog(vec![i(0); MAX_STACK + 1], Vec::new());
    let mut vm = Vm::new(&program);
    let mut host = MockHost::default();
    for _ in 0..MAX_STACK {
        vm.step(&program, &mut host).unwrap();
    }
    assert!(matches!(
        vm.step(&program, &mut host),
        Err(StepError::Vm(VmError::StackOverflow))
    ));
}

#[test]
fn finished_program_stays_finished() {
    let program = prog(vec![i(1)], Vec::new());
    let mut vm = Vm::new(&program);
    let mut host = MockHost::default();
    assert_eq!(vm.step(&program, &mut host).unwrap(), StepOutcome::Finished);
    let after = vm.state.clone();
    assert_eq!(vm.step(&program, &mut host).unwrap(), StepOutcome::Finished);
    assert_eq!(vm.state, after);
    assert_eq!(vm.current_op(&program), Ok(None));

    vm.state.pc = 5;
    assert!(matches!(
        vm.step(&program, &mut host),
        Err(StepError::Vm(VmError::BadPc(5)))
    ));
    assert_eq!(vm.current_op(&program), Err(VmError::BadPc(5)));
}

#[test]
fn current_op_names_the_next_instruction() {
    let program = prog(vec![i(1), Op::ReadDtc { mask: 1 }], Vec::new());
    let mut vm = Vm::new(&program);
    assert_eq!(vm.current_op(&program), Ok(Some(&i(1))));
    assert!(!i(1).is_diagnostic_primitive());
    vm.step(&program, &mut MockHost::default()).unwrap();
    let op = vm.current_op(&program).unwrap().unwrap();
    assert!(op.is_diagnostic_primitive());
}

#[test]
fn schema_versions_must_match() {
    let mut newer = prog(vec![i(1)], Vec::new());
    newer.schema_version = IR_SCHEMA_VERSION + 1;
    let mut vm = Vm::new(&newer);
    assert!(matches!(
        vm.step(&newer, &mut MockHost::default()),
        Err(StepError::Vm(VmError::SchemaMismatch { .. }))
    ));

    let current = prog(vec![i(1)], Vec::new());
    let mut state = Vm::new(&current).state;
    state.schema_version = IR_SCHEMA_VERSION + 1;
    let mut vm = Vm::resume(state);
    let expected = VmError::StateSchemaMismatch {
        program: IR_SCHEMA_VERSION,
        state: IR_SCHEMA_VERSION + 1,
    };
    assert!(matches!(
        vm.step(&current, &mut MockHost::default()),
        Err(StepError::Vm(e)) if e == expected
    ));
}

/// 8.2.5: an agent crash resumes from the journaled state. Serializing the state at every
/// instruction boundary and resuming from the bytes must give the same result as an
/// uninterrupted run.
#[test]
fn state_round_trips_through_postcard_at_every_boundary() {
    let code = vec![
        i(3),
        Op::StoreGlobal(0),
        Op::PushF64(0.5),
        Op::Pop,
        Op::LoadGlobal(0), // 4: loop
        i(0),
        Op::CmpGt,
        Op::JumpIfFalse(20),
        Op::PushBytes(0),
        Op::Call(22),
        Op::Pop,
        Op::LoadGlobal(0),
        i(1),
        Op::Sub,
        Op::StoreGlobal(0),
        Op::Jump(4),
        i(0),
        i(0),
        i(0),
        i(0),
        Op::LoadGlobal(0), // 20
        Op::Ret,
        Op::StoreLocal(0), // 22: sub
        Op::LoadLocal(0),
        Op::ServiceRequest { service: 0x22 },
        Op::Ret,
    ];
    let program = prog(code, vec![vec![0xF1, 0x90]]);
    let new_host = || MockHost {
        responses: (0..3).map(|n| Ok(vec![0x62, n])).collect(),
        ..MockHost::default()
    };

    let mut straight = Vm::new(&program);
    let mut host = new_host();
    run(&mut straight, &program, &mut host);
    assert_eq!(host.calls.len(), 3);

    let mut host = new_host();
    let mut vm = Vm::new(&program);
    loop {
        let bytes = postcard::to_allocvec(&vm.state).unwrap();
        let restored: VmState = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(restored, vm.state);
        vm = Vm::resume(restored);
        if vm.step(&program, &mut host).unwrap() == StepOutcome::Finished {
            break;
        }
    }
    assert_eq!(vm.state, straight.state);
    assert_eq!(vm.state.stack, [Value::I64(0)]);
}

/// Every host-calling primitive, set up so that the next step calls the host.
fn primitive_programs() -> Vec<(Op, Program)> {
    let constants = vec![vec![0x22], b"text".to_vec()];
    let with_bytes = |op: Op| {
        (
            op.clone(),
            prog(vec![Op::PushBytes(0), op], constants.clone()),
        )
    };
    let alone = |op: Op| (op.clone(), prog(vec![i(0), op], constants.clone()));
    vec![
        with_bytes(Op::ServiceRequest { service: 0x22 }),
        alone(Op::ReadDtc { mask: 0xFF }),
        with_bytes(Op::RoutineControl { routine: 1, sub: 1 }),
        with_bytes(Op::SecurityAccess { level: 1 }),
        with_bytes(Op::FlashTransfer { block: 0 }),
        alone(Op::Wait { millis: 10 }),
        alone(Op::HmiRequest { form: 1 }),
        alone(Op::RecordInput { template: 1 }),
        alone(Op::MonitorCapture { back_millis: 10 }),
    ]
}

/// 8.2.5 for every primitive: a host error leaves the state byte-identical and a retry
/// completes the instruction.
#[test]
fn every_primitive_is_atomic_on_host_error() {
    for (op, program) in primitive_programs() {
        let mut vm = Vm::new(&program);
        let mut host = MockHost {
            responses: VecDeque::from([Err(MockError), Ok(vec![1])]),
            keys: VecDeque::from([Some(vec![2])]),
            hmi: VecDeque::from([Some(vec![3])]),
            ..MockHost::default()
        };
        vm.step(&program, &mut host).unwrap();
        let before = postcard::to_allocvec(&vm.state).unwrap();
        // Queue-driven primitives fail on the queued error; the others on `fail_next`.
        host.fail_next = !matches!(
            op,
            Op::ServiceRequest { .. }
                | Op::ReadDtc { .. }
                | Op::RoutineControl { .. }
                | Op::RecordInput { .. }
        );
        assert!(
            matches!(vm.step(&program, &mut host), Err(StepError::Host(_))),
            "{op:?} should report the host error"
        );
        assert_eq!(
            postcard::to_allocvec(&vm.state).unwrap(),
            before,
            "{op:?} changed the state"
        );
        assert_eq!(
            vm.step(&program, &mut host).unwrap(),
            StepOutcome::Finished,
            "{op:?} should complete on retry"
        );
        assert_eq!(vm.state.steps, 2, "{op:?}");
        assert_eq!(vm.state.pc, 2, "{op:?}");
    }
}

/// With a full stack, a primitive that pushes an answer fails before the host is asked, so an
/// answer is never thrown away.
#[test]
fn full_stack_is_detected_before_the_host_is_asked() {
    for op in [
        Op::ReadDtc { mask: 0xFF },
        Op::HmiRequest { form: 0 },
        Op::RecordInput { template: 0 },
    ] {
        let mut code = vec![i(0); MAX_STACK];
        code.push(op.clone());
        let program = prog(code, vec![vec![1]]);
        let mut vm = Vm::new(&program);
        let mut host = MockHost::default();
        for _ in 0..MAX_STACK {
            vm.step(&program, &mut host).unwrap();
        }
        host.forbidden = true;
        let before = postcard::to_allocvec(&vm.state).unwrap();
        assert!(
            matches!(
                vm.step(&program, &mut host),
                Err(StepError::Vm(VmError::StackOverflow))
            ),
            "{op:?}"
        );
        assert_eq!(postcard::to_allocvec(&vm.state).unwrap(), before, "{op:?}");
    }
}

#[test]
fn primitive_classification_covers_every_instruction() {
    let primitives: Vec<Op> = primitive_programs().into_iter().map(|(op, _)| op).collect();
    let all = [
        i(0),
        Op::PushF64(0.0),
        Op::PushBytes(0),
        Op::Pop,
        Op::Dup,
        Op::Swap,
        Op::Add,
        Op::Sub,
        Op::Mul,
        Op::Div,
        Op::BitAnd,
        Op::BitOr,
        Op::BitXor,
        Op::Shl,
        Op::Shr,
        Op::CmpEq,
        Op::CmpLt,
        Op::CmpGt,
        Op::Not,
        Op::Jump(0),
        Op::JumpIfFalse(0),
        Op::Call(0),
        Op::Ret,
        Op::LoadLocal(0),
        Op::StoreLocal(0),
        Op::LoadGlobal(0),
        Op::StoreGlobal(0),
        Op::IndexGet,
        Op::IndexSet,
        Op::ServiceRequest { service: 0 },
        Op::ReadDtc { mask: 0 },
        Op::RoutineControl { routine: 0, sub: 0 },
        Op::SecurityAccess { level: 0 },
        Op::FlashTransfer { block: 0 },
        Op::Wait { millis: 0 },
        Op::HmiRequest { form: 0 },
        Op::RecordInput { template: 0 },
        Op::MonitorCapture { back_millis: 0 },
        Op::Log {
            level: 0,
            message: 0,
        },
    ];
    for (index, op) in all.iter().enumerate() {
        // The postcard encoding starts with the variant index, so this also pins the variant
        // order: reordering `Op` would change every stored program (ADR-233).
        assert_eq!(
            postcard::to_allocvec(op).unwrap()[0] as usize,
            index,
            "{op:?} moved"
        );
        let expected = matches!(op, Op::Log { .. })
            || primitives
                .iter()
                .any(|p| std::mem::discriminant(p) == std::mem::discriminant(op));
        assert_eq!(op.is_diagnostic_primitive(), expected, "{op:?}");
    }
}

#[test]
fn more_arithmetic_and_comparison_edges() {
    assert_eq!(
        expect_vm_error(vec![i(i64::MAX), i(2), Op::Mul], vec![], 2),
        VmError::Overflow
    );
    assert_eq!(
        expect_vm_error(vec![i(i64::MIN), i(1), Op::Sub], vec![], 2),
        VmError::Overflow
    );
    // Integer division truncates toward zero.
    assert_eq!(eval(vec![i(-7), i(2), Op::Div]), [Value::I64(-3)]);
    assert_eq!(eval(vec![i(-1), i(63), Op::Shr]), [Value::I64(1)]);
    assert_eq!(eval(vec![i(5), i(0), Op::Shl]), [Value::I64(5)]);
    let t = || Value::Bool(true);
    let f = || Value::Bool(false);
    assert_eq!(
        eval(vec![Op::PushF64(1.0), Op::PushF64(2.0), Op::CmpLt]),
        [t()]
    );
    assert_eq!(
        eval(vec![Op::PushF64(1.0), Op::PushF64(2.0), Op::CmpGt]),
        [f()]
    );
    assert_eq!(
        eval(vec![Op::PushF64(2.0), Op::PushF64(1.0), Op::CmpGt]),
        [t()]
    );
    assert_eq!(eval(vec![i(2), i(1), Op::CmpLt]), [f()]);
    // NaN is unordered.
    for op in [Op::CmpLt, Op::CmpGt] {
        assert_eq!(
            eval(vec![Op::PushF64(f64::NAN), Op::PushF64(1.0), op]),
            [f()]
        );
    }
}

#[test]
fn branch_targets_are_checked_and_may_end_the_program() {
    assert_eq!(
        expect_vm_error(vec![Op::Call(2)], vec![], 0),
        VmError::BadPc(2)
    );
    let program = prog(
        vec![i(1), i(1), Op::CmpEq, Op::Not, Op::JumpIfFalse(9)],
        vec![],
    );
    let mut vm = Vm::new(&program);
    for _ in 0..4 {
        vm.step(&program, &mut MockHost::default()).unwrap();
    }
    let before = vm.state.clone();
    assert!(matches!(
        vm.step(&program, &mut MockHost::default()),
        Err(StepError::Vm(VmError::BadPc(9)))
    ));
    assert_eq!(vm.state, before);

    // A false condition jumping to the end, and a call to the end, finish the program.
    assert_eq!(
        eval(vec![i(1), i(2), Op::CmpEq, Op::JumpIfFalse(5), i(9)]),
        []
    );
    let program = prog(vec![Op::Call(1)], vec![]);
    let mut vm = Vm::new(&program);
    assert_eq!(
        vm.step(&program, &mut MockHost::default()).unwrap(),
        StepOutcome::Finished
    );
}

/// A resumed state is not trusted: a frame returning outside the program fails without
/// changing the state, and an exhausted step counter stops the VM.
#[test]
fn resumed_state_is_checked() {
    let program = prog(vec![Op::Ret], vec![]);
    let mut state = Vm::new(&program).state;
    state.call_stack.push(Frame {
        return_pc: 999,
        locals: Vec::new(),
    });
    let mut vm = Vm::resume(state.clone());
    assert!(matches!(
        vm.step(&program, &mut MockHost::default()),
        Err(StepError::Vm(VmError::BadReturnPc(999)))
    ));
    assert_eq!(vm.state, state);

    // Limits and return positions are checked before any instruction, host-calling ones
    // included, and the host is never asked.
    let primitive = prog(
        vec![
            Op::MonitorCapture { back_millis: 1 },
            Op::Wait { millis: 1 },
        ],
        vec![],
    );
    let base = Vm::new(&primitive).state;
    let frame = |return_pc| Frame {
        return_pc,
        locals: Vec::new(),
    };
    let oversized_stack = VmState {
        stack: vec![Value::I64(0); MAX_STACK + 1],
        ..base.clone()
    };
    let deep_calls = VmState {
        call_stack: (0..=MAX_CALL_DEPTH).map(|_| frame(1)).collect(),
        pc: 1,
        ..base.clone()
    };
    let bad_return = VmState {
        call_stack: vec![frame(1), frame(3)],
        ..base.clone()
    };
    // The damaged frame is not the top one, so `Ret` would not reach it for a while.
    let bad_lower_return = VmState {
        call_stack: vec![frame(3), frame(1)],
        ..base.clone()
    };
    // A finished state that breaks an invariant is refused, not reported finished.
    let finished_oversized = VmState {
        stack: vec![Value::I64(0); MAX_STACK + 1],
        pc: 2,
        ..base.clone()
    };
    for (state, expected) in [
        (oversized_stack, VmError::StackOverflow),
        (deep_calls, VmError::CallDepthExceeded),
        (bad_return, VmError::BadReturnPc(3)),
        (bad_lower_return, VmError::BadReturnPc(3)),
        (finished_oversized, VmError::StackOverflow),
    ] {
        let before = postcard::to_allocvec(&state).unwrap();
        let mut vm = Vm::resume(state);
        // The runner sees the refusal before it journals anything for the instruction.
        assert_eq!(vm.check_state(&primitive), Err(expected.clone()));
        assert_eq!(vm.current_op(&primitive), Err(expected.clone()));
        let mut host = MockHost {
            forbidden: true,
            ..MockHost::default()
        };
        assert!(
            matches!(vm.step(&primitive, &mut host), Err(StepError::Vm(ref e)) if *e == expected),
            "{expected:?}"
        );
        assert_eq!(
            postcard::to_allocvec(&vm.state).unwrap(),
            before,
            "{expected:?}"
        );
    }
    // The limits themselves are allowed.
    let mut vm = Vm::resume(VmState {
        stack: vec![Value::I64(0); MAX_STACK],
        call_stack: (0..MAX_CALL_DEPTH).map(|_| frame(2)).collect(),
        ..base
    });
    vm.step(&primitive, &mut MockHost::default()).unwrap();

    let mut state = Vm::new(&program).state;
    state.steps = u64::MAX;
    let mut vm = Vm::resume(state.clone());
    assert!(matches!(
        vm.step(&program, &mut MockHost::default()),
        Err(StepError::Vm(VmError::StepLimit))
    ));
    assert_eq!(vm.state, state);
}

/// The inquiry passed to the host stays the same while a wait is polled and changes when the
/// same instruction is reached again.
#[test]
fn waits_carry_a_stable_inquiry() {
    // Two passes over one HmiRequest, then a Wait that is polled twice.
    let code = vec![
        i(0),
        Op::StoreGlobal(0),
        Op::HmiRequest { form: 0 }, // 2
        Op::Pop,
        Op::LoadGlobal(0),
        i(1),
        Op::Add,
        Op::Dup,
        Op::StoreGlobal(0),
        i(2),
        Op::CmpLt,
        Op::JumpIfFalse(13),
        Op::Jump(2),
        Op::Wait { millis: 100 }, // 13
    ];
    let program = prog(code, vec![b"form".to_vec()]);
    let mut vm = Vm::new(&program);
    let mut host = MockHost {
        hmi: VecDeque::from([None, Some(vec![1]), None, None, Some(vec![2])]),
        timers: VecDeque::from([false, false, true]),
        ..MockHost::default()
    };
    let mut waits = Vec::new();
    loop {
        match vm.step(&program, &mut host).unwrap() {
            StepOutcome::Continue => {}
            StepOutcome::Finished => break,
            StepOutcome::Waiting(on) => waits.push(on),
        }
    }
    use WaitingOn::{Hmi, Timer};
    assert_eq!(waits, [Hmi, Hmi, Hmi, Timer, Timer]);
    let i = &host.inquiries;
    assert_eq!(i.len(), 8);
    assert!(i[0] == i[1] && i[2] == i[3] && i[3] == i[4] && i[5] == i[6] && i[6] == i[7]);
    assert!(i[0] < i[2] && i[2] < i[5], "{i:?}");
}

/// Values that are easy to get wrong in serialization survive a round trip bit for bit.
#[test]
fn unusual_values_round_trip() {
    let state = VmState {
        schema_version: IR_SCHEMA_VERSION,
        pc: 3,
        stack: vec![
            Value::F64(f64::NAN),
            Value::F64(-0.0),
            Value::I64(i64::MIN),
            Value::I64(i64::MAX),
            Value::Bool(false),
            Value::Bytes(Vec::new()),
        ],
        locals: vec![None, Some(Value::I64(1))],
        globals: vec![Some(Value::Bytes(vec![0xFF; 3])), None],
        call_stack: vec![Frame {
            return_pc: 2,
            locals: vec![Some(Value::F64(f64::INFINITY)), None],
        }],
        steps: u64::MAX - 1,
        checkpoint: Some(Checkpoint {
            pc: 1,
            section: 0,
            vin: Some("VIN".into()),
            artifact_digest: None,
            at: "2026-10-06T00:00:00Z".into(),
        }),
        resume_count: 2,
    };
    let bytes = postcard::to_allocvec(&state).unwrap();
    let restored: VmState = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(postcard::to_allocvec(&restored).unwrap(), bytes);
    let Value::F64(nan) = restored.stack[0] else {
        panic!("not a float")
    };
    assert_eq!(nan.to_bits(), f64::NAN.to_bits());
    let Value::F64(zero) = restored.stack[1] else {
        panic!("not a float")
    };
    assert!(zero.is_sign_negative());
}

/// Record template input is a kind of HMI request (4.3.1): it waits without blocking, keeps its
/// inquiry while the operator has not answered, and gets a new one when reached again.
#[test]
fn record_input_waits_for_the_operator() {
    // Two passes over one RecordInput, each answered on the second poll.
    let code = vec![
        i(0),
        Op::StoreGlobal(0),
        Op::RecordInput { template: 0 }, // 2
        Op::Pop,
        Op::LoadGlobal(0),
        i(1),
        Op::Add,
        Op::Dup,
        Op::StoreGlobal(0),
        i(2),
        Op::CmpLt,
        Op::JumpIfFalse(13),
        Op::Jump(2),
    ];
    let program = prog(code, vec![b"tpl".to_vec()]);
    let mut vm = Vm::new(&program);
    let mut host = MockHost {
        records: VecDeque::from([None, Some(b"a".to_vec()), None, Some(b"b".to_vec())]),
        ..MockHost::default()
    };
    let mut waits = 0;
    loop {
        let before = postcard::to_allocvec(&vm.state).unwrap();
        match vm.step(&program, &mut host).unwrap() {
            StepOutcome::Continue => {}
            StepOutcome::Finished => break,
            StepOutcome::Waiting(on) => {
                assert_eq!(on, WaitingOn::Hmi);
                assert_eq!(postcard::to_allocvec(&vm.state).unwrap(), before);
                waits += 1;
            }
        }
    }
    assert_eq!(waits, 2);
    let q = &host.inquiries;
    assert_eq!(q.len(), 4);
    assert!(
        q[0] == q[1] && q[2] == q[3] && q[0] != 0 && q[0] < q[2],
        "{q:?}"
    );
    assert!(host.records.is_empty());
}

/// ADR-233: for a diagnostic primitive, `current_op` succeeds only if `step` reaches the host,
/// so a runner never journals an intent for a request that is not sent. Every invalid setup is
/// refused by both, with the same error and without a host call; every valid one reaches the
/// host exactly once.
#[test]
fn current_op_and_step_agree_on_primitives() {
    let full = || vec![i(0); MAX_STACK];
    let with = |mut setup: Vec<Op>, op: Op| {
        setup.push(op);
        setup
    };
    let invalid: Vec<(Vec<Op>, Vec<Vec<u8>>, VmError)> = vec![
        (
            vec![Op::ServiceRequest { service: 1 }],
            vec![],
            VmError::StackUnderflow,
        ),
        (
            with(vec![i(1)], Op::ServiceRequest { service: 1 }),
            vec![],
            VmError::TypeMismatch,
        ),
        (
            with(vec![i(1)], Op::RoutineControl { routine: 1, sub: 1 }),
            vec![],
            VmError::TypeMismatch,
        ),
        (
            with(vec![i(1)], Op::SecurityAccess { level: 1 }),
            vec![],
            VmError::TypeMismatch,
        ),
        (
            with(vec![i(1)], Op::FlashTransfer { block: 0 }),
            vec![],
            VmError::TypeMismatch,
        ),
        (
            vec![Op::FlashTransfer { block: 0 }],
            vec![],
            VmError::StackUnderflow,
        ),
        (
            with(full(), Op::ReadDtc { mask: 1 }),
            vec![],
            VmError::StackOverflow,
        ),
        (
            vec![Op::HmiRequest { form: 0 }],
            vec![],
            VmError::BadConstant(0),
        ),
        (
            with(full(), Op::HmiRequest { form: 0 }),
            vec![vec![]],
            VmError::StackOverflow,
        ),
        (
            vec![Op::RecordInput { template: 3 }],
            vec![],
            VmError::BadConstant(3),
        ),
        (
            with(full(), Op::RecordInput { template: 0 }),
            vec![vec![]],
            VmError::StackOverflow,
        ),
        (
            vec![Op::Log {
                level: 0,
                message: 0,
            }],
            vec![vec![0xFF]],
            VmError::InvalidUtf8(0),
        ),
        (
            vec![Op::Log {
                level: 0,
                message: 1,
            }],
            vec![],
            VmError::BadConstant(1),
        ),
    ];
    for (code, constants, expected) in invalid {
        let setup_steps = code.len() - 1;
        let program = prog(code, constants);
        let mut vm = Vm::new(&program);
        let mut host = MockHost::default();
        for _ in 0..setup_steps {
            vm.step(&program, &mut host).unwrap();
        }
        host.forbidden = true;
        let before = postcard::to_allocvec(&vm.state).unwrap();
        assert_eq!(vm.current_op(&program), Err(expected.clone()));
        assert!(
            matches!(vm.step(&program, &mut host), Err(StepError::Vm(ref e)) if *e == expected),
            "{expected:?}"
        );
        assert_eq!(
            postcard::to_allocvec(&vm.state).unwrap(),
            before,
            "{expected:?}"
        );
    }

    let mut valid: Vec<(Op, Program)> = primitive_programs();
    valid.push((
        Op::Log {
            level: 1,
            message: 1,
        },
        prog(
            vec![
                i(0),
                Op::Log {
                    level: 1,
                    message: 1,
                },
            ],
            vec![vec![], b"ok".to_vec()],
        ),
    ));
    for (op, program) in valid {
        let mut vm = Vm::new(&program);
        let mut host = MockHost {
            responses: VecDeque::from([Ok(vec![1])]),
            keys: VecDeque::from([Some(vec![2])]),
            hmi: VecDeque::from([Some(vec![3])]),
            ..MockHost::default()
        };
        vm.step(&program, &mut host).unwrap();
        assert_eq!(vm.current_op(&program), Ok(Some(&op)));
        vm.step(&program, &mut host).unwrap();
        assert_eq!(
            host.calls.len() + host.logs.len(),
            1,
            "{op:?} should reach the host once"
        );
    }
}

/// A check that is too strict would be shared by `current_op` and `step`, so they would agree
/// on refusing a valid call. Every primitive, right at the stack limit it can run at, must still
/// be accepted and reach the host exactly once.
#[test]
fn primitives_run_at_the_stack_limit() {
    let cases = [
        (Op::ServiceRequest { service: 1 }, MAX_STACK),
        (Op::RoutineControl { routine: 1, sub: 1 }, MAX_STACK),
        (Op::SecurityAccess { level: 1 }, MAX_STACK),
        (Op::FlashTransfer { block: 0 }, MAX_STACK),
        (
            Op::Log {
                level: 0,
                message: 0,
            },
            MAX_STACK,
        ),
        (Op::Wait { millis: 1 }, MAX_STACK),
        (Op::MonitorCapture { back_millis: 1 }, MAX_STACK),
        (Op::ReadDtc { mask: 1 }, MAX_STACK - 1),
        (Op::HmiRequest { form: 0 }, MAX_STACK - 1),
        (Op::RecordInput { template: 0 }, MAX_STACK - 1),
    ];
    // The list covers every diagnostic primitive once.
    let kinds: std::collections::HashSet<_> = cases
        .iter()
        .map(|(op, _)| std::mem::discriminant(op))
        .collect();
    assert_eq!(kinds.len(), cases.len());
    assert!(cases.iter().all(|(op, _)| op.is_diagnostic_primitive()));
    assert_eq!(
        cases.len(),
        primitive_programs().len() + 1,
        "every primitive plus Log"
    );

    for (op, depth) in cases {
        let mut stack = vec![Value::I64(0); depth];
        if let Some(top) = stack.last_mut() {
            *top = Value::Bytes(vec![0x41]);
        }
        let program = prog(vec![op.clone()], vec![b"ok".to_vec()]);
        let mut vm = Vm::resume(VmState {
            stack,
            ..Vm::new(&program).state
        });
        let mut host = MockHost {
            responses: VecDeque::from([Ok(vec![1])]),
            keys: VecDeque::from([Some(vec![2])]),
            hmi: VecDeque::from([Some(vec![3])]),
            records: VecDeque::from([Some(vec![4])]),
            ..MockHost::default()
        };
        assert_eq!(vm.current_op(&program), Ok(Some(&op)), "{op:?}");
        assert_eq!(
            vm.step(&program, &mut host).unwrap(),
            StepOutcome::Finished,
            "{op:?}"
        );
        assert_eq!(host.calls.len() + host.logs.len(), 1, "{op:?}");
        assert!(vm.state.stack.len() <= MAX_STACK, "{op:?}");
    }
}
