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
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
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

// ---------------------------------------------------------------- Restart declaration (ADR-245)

/// The encodings changed in schema 2; a program or state of version 1 must be refused.
#[test]
fn schema_version_one_is_refused() {
    let mut old = prog(vec![i(1)], Vec::new());
    old.schema_version = 1;
    assert!(matches!(
        Vm::new(&old).check_state(&old),
        Err(VmError::SchemaMismatch { program: 1, .. })
    ));
    let current = prog(vec![i(1)], Vec::new());
    let mut state = Vm::new(&current).state;
    state.schema_version = 1;
    assert!(matches!(
        Vm::resume(state).check_state(&current),
        Err(VmError::StateSchemaMismatch { state: 1, .. })
    ));
}

const SVC: Source = Source::EcuService {
    service_id: 1,
    field_id: 1,
};

fn precondition(default: Option<Source>, programming: Option<Source>) -> Precondition {
    Precondition {
        satisfied: Satisfied { lower: 0, upper: 0 },
        default_session: default,
        programming_session: programming,
    }
}

fn plan(flash_session: u32, stage: u32, entry: u32) -> FlashRecovery {
    FlashRecovery {
        flash_session,
        stage,
        max_resumes: 3,
        recovery_required: RecoveryRequired::Never,
        boundaries: RecoveryBoundaries {
            entry_pc: entry,
            erase_pc: entry + 1,
            transfer_exit_pc: entry + 3,
            post_transfer_end_pc: entry + 5,
        },
        timing: RecoveryTiming {
            session_timeout_millis: 5000,
            teardown_margin_millis: 500,
            ecu_startup_millis: 2000,
            confirmation_window_millis: 10_000,
        },
        version_read_retries: 2,
        no_application: Some(NoApplication::Nrc(0x22)),
    }
}

/// Ten instructions; the erase request at 1 (and 6, where a second plan would start) and the
/// transfer exit at 3, no jumps.
fn recovery_code() -> Vec<Op> {
    let mut code = vec![Op::Pop; 10];
    for pc in [1, 6] {
        code[pc] = Op::RoutineControl { routine: 1, sub: 1 };
    }
    code[2] = Op::ServiceRequest { service: 0x34 };
    code[3] = Op::ServiceRequest { service: 0x37 };
    code
}

fn recovery_program() -> Program {
    let mut program = prog(recovery_code(), Vec::new());
    program.identity = IdentitySources {
        vin: Some(SVC),
        hardware_part_number: Some(SVC),
        software_version: Some(SVC),
    };
    program.flash.push(plan(1, 1, 0));
    program
}

fn mutate_plan(f: impl FnOnce(&mut FlashRecovery)) -> Program {
    let mut program = recovery_program();
    f(&mut program.flash[0]);
    program
}

#[test]
fn a_program_without_a_declaration_is_valid() {
    assert_eq!(prog(vec![Op::Pop; 4], Vec::new()).validate(), Ok(()));
    assert_eq!(recovery_program().validate(), Ok(()));
}

/// The done-when case: a plan that allows a restart needs every declared precondition mapped
/// for both sessions; a plan that does not allow one needs none of that.
#[test]
fn a_restartable_plan_needs_every_precondition_mapped_for_both_sessions() {
    let mut program = recovery_program();
    program.preconditions.engine = Some(precondition(Some(SVC), None));
    assert_eq!(
        program.validate(),
        Err(ProgramError::UnmappedPrecondition {
            kind: PreconditionKind::Engine,
            session: SessionKind::Programming,
        })
    );
    program.preconditions.engine = Some(precondition(Some(SVC), Some(SVC)));
    assert_eq!(program.validate(), Ok(()));

    // Intervention from the erase on: no restart is allowed, so the mapping is not asked for.
    let mut no_restart = recovery_program();
    no_restart.preconditions.engine = Some(precondition(Some(SVC), None));
    no_restart.flash[0].recovery_required = RecoveryRequired::FromPc(1);
    assert!(!no_restart.flash[0].allows_restart());
    assert_eq!(no_restart.validate(), Ok(()));

    // From the transfer exit on: restart before it is allowed, so it is refused again.
    no_restart.flash[0].recovery_required = RecoveryRequired::FromPc(3);
    assert!(no_restart.flash[0].allows_restart());
    assert!(matches!(
        no_restart.validate(),
        Err(ProgramError::UnmappedPrecondition { .. })
    ));
}

#[test]
fn boundaries_must_be_in_order_inside_the_code() {
    for edit in [
        (|b: &mut RecoveryBoundaries| b.entry_pc = 2) as fn(&mut RecoveryBoundaries),
        |b| b.erase_pc = b.transfer_exit_pc,
        |b| b.transfer_exit_pc = b.post_transfer_end_pc,
        |b| b.post_transfer_end_pc = 11,
    ] {
        let program = mutate_plan(|p| edit(&mut p.boundaries));
        assert_eq!(
            program.validate(),
            Err(ProgramError::BoundaryOutOfOrder { flash_session: 1 })
        );
    }
}

#[test]
fn erase_and_transfer_exit_must_be_the_right_requests() {
    for (pc, op) in [
        (1, Op::Pop),
        (1, Op::ServiceRequest { service: 0x31 }),
        (3, Op::Pop),
        (3, Op::RoutineControl { routine: 1, sub: 1 }),
        (3, Op::ServiceRequest { service: 0x36 }),
    ] {
        let mut program = recovery_program();
        program.code[pc as usize] = op;
        assert_eq!(
            program.validate(),
            Err(ProgramError::WrongBoundaryInstruction {
                flash_session: 1,
                pc
            })
        );
    }
    // The download request starts a plan that does not erase.
    let mut program = recovery_program();
    program.code[1] = Op::ServiceRequest { service: 0x34 };
    program.code[2] = Op::Pop;
    assert_eq!(program.validate(), Ok(()));
}

#[test]
fn a_plan_needs_a_request_download() {
    // A RoutineControl erase with no RequestDownload before the exit.
    let mut program = recovery_program();
    program.code[2] = Op::Pop;
    assert_eq!(
        program.validate(),
        Err(ProgramError::MissingRequestDownload { flash_session: 1 })
    );
    program.code[2] = Op::ServiceRequest { service: 0x34 };
    assert_eq!(program.validate(), Ok(()));
    // An erase that is itself the RequestDownload is enough.
    program.code[1] = Op::ServiceRequest { service: 0x34 };
    program.code[2] = Op::Pop;
    assert_eq!(program.validate(), Ok(()));
}

#[test]
fn download_instructions_belong_to_a_transfer_range() {
    for op in [
        Op::ServiceRequest { service: 0x34 },
        Op::ServiceRequest { service: 0x36 },
        Op::ServiceRequest { service: 0x37 },
        Op::FlashTransfer { block: 0 },
    ] {
        // No plan at all.
        let program = prog(vec![op.clone()], Vec::new());
        assert_eq!(
            program.validate(),
            Err(ProgramError::DownloadOutsidePlan { pc: 0 })
        );
        // After the plan's transfer exit.
        let mut program = recovery_program();
        program.code[4] = op.clone();
        assert_eq!(
            program.validate(),
            Err(ProgramError::DownloadOutsidePlan { pc: 4 })
        );
        // Between the RequestDownload and the transfer exit (moved to 4), where only the
        // declared exit may be a RequestTransferExit and no second RequestDownload may sit.
        let mut program = recovery_program();
        program.code[4] = Op::ServiceRequest { service: 0x37 };
        program.code[3] = op.clone();
        program.flash[0].boundaries.transfer_exit_pc = 4;
        program.flash[0].boundaries.post_transfer_end_pc = 5;
        let undeclared = matches!(
            op,
            Op::ServiceRequest {
                service: 0x34 | 0x37
            }
        );
        if undeclared {
            assert_eq!(
                program.validate(),
                Err(ProgramError::UndeclaredTransferBoundary {
                    flash_session: 1,
                    pc: 3
                })
            );
        } else {
            assert_eq!(program.validate(), Ok(()));
        }
    }
}

/// The journal's markers guard only the declared boundaries, so a plan holds one
/// RequestDownload and one RequestTransferExit.
#[test]
fn a_plan_holds_one_request_download_and_one_transfer_exit() {
    // A second RequestDownload after the one that follows the erase.
    let mut program = recovery_program();
    program.code[2] = Op::ServiceRequest { service: 0x34 };
    assert_eq!(program.validate(), Ok(()));
    program.code[1] = Op::ServiceRequest { service: 0x34 };
    assert_eq!(
        program.validate(),
        Err(ProgramError::UndeclaredTransferBoundary {
            flash_session: 1,
            pc: 2
        })
    );
    // A RequestTransferExit before the declared one.
    let mut program = recovery_program();
    program.code[2] = Op::ServiceRequest { service: 0x37 };
    assert_eq!(
        program.validate(),
        Err(ProgramError::UndeclaredTransferBoundary {
            flash_session: 1,
            pc: 2
        })
    );
}

/// A reversed or out-of-code section would read as empty in the overlap checks.
#[test]
fn sections_must_lie_in_the_code_in_order() {
    for (start_pc, end_pc) in [(3, 2), (0, 11)] {
        let mut program = recovery_program();
        program.sections.push(Section {
            start_pc,
            end_pc,
            interruptible: Interruptible::RecoveryRequired,
            idempotency: Idempotency::Safe,
            expected_millis: 0,
        });
        assert_eq!(
            program.validate(),
            Err(ProgramError::InvalidSection { section: 0 }),
            "{start_pc}..{end_pc}"
        );
    }
}

#[test]
fn an_empty_section_is_invalid() {
    let mut program = recovery_program();
    program.sections.push(Section {
        interruptible: Interruptible::Yes,
        ..recovery_section(2, 2)
    });
    assert_eq!(
        program.validate(),
        Err(ProgramError::InvalidSection { section: 0 })
    );
}

/// Every declared precondition is checked before the procedure starts, so it needs a
/// default-session source even when no plan allows a restart.
#[test]
fn a_declared_precondition_needs_a_default_session_source() {
    let mut program = prog(vec![Op::Pop], Vec::new());
    program.preconditions.engine = Some(precondition(None, Some(SVC)));
    assert_eq!(
        program.validate(),
        Err(ProgramError::UnmappedPrecondition {
            kind: PreconditionKind::Engine,
            session: SessionKind::Default
        })
    );
    program.preconditions.engine = Some(precondition(Some(SVC), None));
    assert_eq!(program.validate(), Ok(()));
}

/// The instructions a second plan at entry 5 needs: RoutineControl erase at 6 (already there),
/// RequestDownload at 7, transfer exit at 8.
fn second_plan_ops(program: &mut Program) {
    program.code[7] = Op::ServiceRequest { service: 0x34 };
    program.code[8] = Op::ServiceRequest { service: 0x37 };
}

#[test]
fn plans_must_be_distinct_and_apart() {
    let mut program = recovery_program();
    second_plan_ops(&mut program);
    program.flash.push(plan(1, 2, 5));
    assert_eq!(
        program.validate(),
        Err(ProgramError::DuplicateFlashSession(1))
    );

    let mut program = recovery_program();
    second_plan_ops(&mut program);
    program.flash.push(plan(2, 1, 5));
    assert_eq!(program.validate(), Err(ProgramError::DuplicateStage(1)));

    let mut program = recovery_program();
    program.code[5] = Op::RoutineControl { routine: 1, sub: 1 };
    program.code[6] = Op::ServiceRequest { service: 0x34 };
    program.code[7] = Op::ServiceRequest { service: 0x37 };
    program.flash.push(plan(2, 2, 4));
    assert_eq!(
        program.validate(),
        Err(ProgramError::OverlappingFlashRecoveries { a: 1, b: 2 })
    );

    // Adjacent ranges do not overlap.
    let mut program = recovery_program();
    second_plan_ops(&mut program);
    program.flash.push(plan(2, 2, 5));
    assert_eq!(program.validate(), Ok(()));
}

#[test]
fn the_recovery_position_must_lie_in_the_plan() {
    // Before the entry, at the exclusive end and past it.
    for pc in [0, 5, 6] {
        let mut program = recovery_program();
        if pc == 0 {
            program.flash[0].boundaries.entry_pc = 1;
        }
        program.flash[0].recovery_required = RecoveryRequired::FromPc(pc);
        assert_eq!(
            program.validate(),
            Err(ProgramError::RecoveryRequiredOutOfRange {
                flash_session: 1,
                pc
            })
        );
    }
    for pc in [0, 4] {
        let mut program = recovery_program();
        program.flash[0].recovery_required = RecoveryRequired::FromPc(pc);
        assert_eq!(program.validate(), Ok(()));
    }
}

fn recovery_section(start_pc: u32, end_pc: u32) -> Section {
    Section {
        start_pc,
        end_pc,
        interruptible: Interruptible::RecoveryRequired,
        idempotency: Idempotency::Unsafe,
        expected_millis: 0,
    }
}

#[test]
fn a_recovery_section_must_not_lie_in_the_restartable_range() {
    // Never: the whole plan is restartable.
    let mut program = recovery_program();
    program.sections.push(recovery_section(4, 5));
    assert_eq!(
        program.validate(),
        Err(ProgramError::ContradictoryInterruptibility {
            flash_session: 1,
            section: 0
        })
    );
    // FromPc(3): a section from 3 on is consistent, one before it is not.
    let mut program = recovery_program();
    program.flash[0].recovery_required = RecoveryRequired::FromPc(3);
    program.sections.push(recovery_section(3, 5));
    assert_eq!(program.validate(), Ok(()));
    program.sections.push(recovery_section(2, 4));
    assert_eq!(
        program.validate(),
        Err(ProgramError::ContradictoryInterruptibility {
            flash_session: 1,
            section: 1
        })
    );
    // Other interruptibility and sections outside the plan are fine.
    let mut program = recovery_program();
    program.sections.push(Section {
        interruptible: Interruptible::No,
        idempotency: Idempotency::Safe,
        ..recovery_section(1, 5)
    });
    program.sections.push(recovery_section(7, 9));
    assert_eq!(program.validate(), Ok(()));
}

/// A plan at 0..10 (erase 1, transfer exit 3, end 5 for `recovery_program`); `edit` places the
/// instructions under test.
fn flow_program(edit: impl FnOnce(&mut Vec<Op>)) -> Program {
    let mut program = recovery_program();
    program.code.resize(12, Op::Pop);
    edit(&mut program.code);
    program
}

fn flow_error(program: &Program, pc: u32) -> Option<ProgramError> {
    match program.validate() {
        Err(
            e @ (ProgramError::JumpOutOfRecovery { pc: p, .. }
            | ProgramError::ControlFlowIntoPlan { pc: p, .. }
            | ProgramError::CallOrReturnInPlan { pc: p, .. }
            | ProgramError::BackwardJumpAcrossRecovery { pc: p, .. }),
        ) if p == pc => Some(e),
        _ => None,
    }
}

#[test]
fn a_plan_contains_no_call_or_return() {
    for op in [Op::Call(9), Op::Ret] {
        let program = flow_program(|c| c[4] = op.clone());
        assert_eq!(
            program.validate(),
            Err(ProgramError::CallOrReturnInPlan {
                flash_session: 1,
                pc: 4
            })
        );
        // Outside the plan they are fine.
        let program = flow_program(|c| c[7] = op.clone());
        assert_eq!(program.validate(), Ok(()));
    }
}

#[test]
fn a_plan_is_entered_only_at_its_entry() {
    // From outside, a jump into the middle is refused; the entry and the end are fine.
    for target in [1, 3, 4] {
        for op in [Op::Jump(target), Op::JumpIfFalse(target)] {
            let program = flow_program(|c| c[7] = op.clone());
            assert_eq!(
                program.validate(),
                Err(ProgramError::ControlFlowIntoPlan {
                    flash_session: 1,
                    pc: 7
                })
            );
        }
    }
    for target in [0, 5, 6, 11] {
        let program = flow_program(|c| c[7] = Op::Jump(target));
        assert_eq!(program.validate(), Ok(()));
    }
    // A call into the plan is refused, entry included: the plan is not a subroutine.
    for target in [0, 2, 4] {
        let program = flow_program(|c| c[7] = Op::Call(target));
        assert_eq!(
            program.validate(),
            Err(ProgramError::ControlFlowIntoPlan {
                flash_session: 1,
                pc: 7
            })
        );
    }
    let program = flow_program(|c| c[7] = Op::Call(9));
    assert_eq!(program.validate(), Ok(()));
}

#[test]
fn a_jump_inside_a_plan_stays_in_it() {
    // Leaving forward by one is fine, further or backwards is not (from the last stage).
    let program = flow_program(|c| c[4] = Op::Jump(5));
    assert_eq!(program.validate(), Ok(()));
    let program = flow_program(|c| c[4] = Op::Jump(6));
    assert!(flow_error(&program, 4).is_some());
}

#[test]
fn a_jump_before_the_erase_cannot_skip_it() {
    // The replayed steps may loop among themselves and may reach the erase.
    let program = flow_program(|c| c[0] = Op::Jump(1));
    assert_eq!(program.validate(), Ok(()));
    // With more room before the erase.
    let mut program = recovery_program();
    program.code = vec![Op::Pop; 12];
    program.code[3] = Op::RoutineControl { routine: 1, sub: 1 };
    program.code[4] = Op::ServiceRequest { service: 0x34 };
    program.code[5] = Op::ServiceRequest { service: 0x37 };
    program.flash[0].boundaries = RecoveryBoundaries {
        entry_pc: 0,
        erase_pc: 3,
        transfer_exit_pc: 5,
        post_transfer_end_pc: 7,
    };
    program.code[1] = Op::Jump(0);
    program.code[0] = Op::JumpIfFalse(3);
    assert_eq!(program.validate(), Ok(()));
    program.code[1] = Op::Jump(4);
    assert!(flow_error(&program, 1).is_some());
    program.code[1] = Op::Jump(7);
    assert!(flow_error(&program, 1).is_some());
}

#[test]
fn a_jump_in_the_transfer_stays_between_erase_and_exit() {
    // erase 1, transfer exit 3: pc 2 is in the transfer.
    for target in [1, 2, 3] {
        let program = flow_program(|c| {
            c[1] = Op::ServiceRequest { service: 0x34 };
            c[2] = Op::Jump(target);
        });
        assert_eq!(program.validate(), Ok(()), "target {target}");
    }
    for target in [0, 4, 5] {
        let program = flow_program(|c| {
            c[1] = Op::ServiceRequest { service: 0x34 };
            c[2] = Op::JumpIfFalse(target);
        });
        assert_eq!(
            flow_error(&program, 2),
            Some(ProgramError::JumpOutOfRecovery {
                flash_session: 1,
                pc: 2
            }),
            "target {target}"
        );
    }
}

#[test]
fn a_jump_after_the_exit_redoes_the_transfer_or_goes_on() {
    // Exit at 3, end 5: pc 4 is after the exit.
    for target in [1, 5] {
        let program = flow_program(|c| c[4] = Op::Jump(target));
        assert_eq!(program.validate(), Ok(()), "target {target}");
    }
    for target in [0, 2, 3] {
        let program = flow_program(|c| c[4] = Op::Jump(target));
        assert!(flow_error(&program, 4).is_some(), "target {target}");
    }
}

/// A call from at or after the recovery point back before it would run the plan again before
/// the point, so it is refused like a backward jump.
#[test]
fn a_backward_call_does_not_cross_the_recovery_point() {
    let mut program = flow_program(|c| c[7] = Op::Call(0));
    program.flash[0].recovery_required = RecoveryRequired::FromPc(4);
    assert_eq!(
        program.validate(),
        Err(ProgramError::BackwardJumpAcrossRecovery {
            flash_session: 1,
            pc: 7
        })
    );
    // Without a recovery point the same call is only refused for entering the plan.
    let program = flow_program(|c| c[7] = Op::Call(0));
    assert_eq!(
        program.validate(),
        Err(ProgramError::ControlFlowIntoPlan {
            flash_session: 1,
            pc: 7
        })
    );
}

/// Between a routine-control erase and the RequestDownload after it, a jump may not land
/// past the RequestDownload: RequestTransferExit would follow an erase with no download.
#[test]
fn a_jump_after_the_erase_cannot_skip_the_request_download() {
    let program_with = |jump: u32| {
        let mut program = flow_program(|c| {
            c[2] = Op::Jump(jump);
            c[3] = Op::ServiceRequest { service: 0x34 };
            c[4] = Op::ServiceRequest { service: 0x37 };
        });
        program.flash[0].boundaries.transfer_exit_pc = 4;
        program
    };
    assert_eq!(
        program_with(4).validate(),
        Err(ProgramError::JumpOutOfRecovery {
            flash_session: 1,
            pc: 2
        })
    );
    assert_eq!(program_with(3).validate(), Ok(()));
    assert_eq!(program_with(1).validate(), Ok(()));
}

/// After a routine-control erase, no block may be sent before the RequestDownload.
#[test]
fn transfer_data_must_follow_the_request_download() {
    for op in [
        Op::ServiceRequest { service: 0x36 },
        Op::FlashTransfer { block: 0 },
    ] {
        let mut program = flow_program(|c| {
            c[2] = op.clone();
            c[3] = Op::ServiceRequest { service: 0x34 };
            c[4] = Op::ServiceRequest { service: 0x37 };
        });
        program.flash[0].boundaries.transfer_exit_pc = 4;
        assert_eq!(
            program.validate(),
            Err(ProgramError::TransferBeforeRequestDownload {
                flash_session: 1,
                pc: 2
            }),
            "{op:?}"
        );
        // After the RequestDownload it is the transfer itself.
        let mut program = flow_program(|c| {
            c[2] = Op::ServiceRequest { service: 0x34 };
            c[3] = op.clone();
            c[4] = Op::ServiceRequest { service: 0x37 };
        });
        program.flash[0].boundaries.transfer_exit_pc = 4;
        assert_eq!(program.validate(), Ok(()), "{op:?}");
    }
}

/// A plan runs only at the top level: a subroutine that can reach it, by jumping or falling
/// through, would return to its call site after the plan ran.
#[test]
fn a_subroutine_cannot_reach_a_plan() {
    // The subroutine at 6 jumps to the plan's entry.
    let program = flow_program(|c| {
        c[6] = Op::Jump(0);
        c[9] = Op::Call(6);
    });
    assert_eq!(
        program.validate(),
        Err(ProgramError::PlanInSubroutine {
            flash_session: 1,
            pc: 9
        })
    );
    // A subroutine after the plan that returns without reaching it is fine.
    let program = flow_program(|c| {
        c[7] = Op::Ret;
        c[9] = Op::Call(6);
    });
    assert_eq!(program.validate(), Ok(()));
    // A subroutine before the plan that falls through into it.
    let mut program = flow_program(|_| {});
    program.code.insert(0, Op::Pop);
    let b = &mut program.flash[0].boundaries;
    b.entry_pc += 1;
    b.erase_pc += 1;
    b.transfer_exit_pc += 1;
    b.post_transfer_end_pc += 1;
    program.code[10] = Op::Call(0);
    assert_eq!(
        program.validate(),
        Err(ProgramError::PlanInSubroutine {
            flash_session: 1,
            pc: 10
        })
    );
}

#[test]
fn a_backward_jump_does_not_cross_the_recovery_point() {
    // FromPc(4): the jump at 4 back to the erase crosses it; a jump at 2 does not.
    let mut program = flow_program(|c| c[4] = Op::Jump(1));
    program.flash[0].recovery_required = RecoveryRequired::FromPc(4);
    assert_eq!(
        program.validate(),
        Err(ProgramError::BackwardJumpAcrossRecovery {
            flash_session: 1,
            pc: 4
        })
    );
    let mut program = flow_program(|c| {
        c[1] = Op::ServiceRequest { service: 0x34 };
        c[2] = Op::Jump(1);
    });
    program.flash[0].recovery_required = RecoveryRequired::FromPc(4);
    assert_eq!(program.validate(), Ok(()));
    // A jump at the point itself that stays at or after it is fine.
    let mut program = flow_program(|c| c[4] = Op::Jump(5));
    program.flash[0].recovery_required = RecoveryRequired::FromPc(4);
    assert_eq!(program.validate(), Ok(()));
    // A loop outside the plan that contains the whole plan crosses the point too.
    let mut program = flow_program(|c| c[7] = Op::Jump(0));
    program.flash[0].recovery_required = RecoveryRequired::FromPc(4);
    assert_eq!(
        program.validate(),
        Err(ProgramError::BackwardJumpAcrossRecovery {
            flash_session: 1,
            pc: 7
        })
    );
}

#[test]
fn sources_are_checked() {
    let mut program = recovery_program();
    program.preconditions.voltage_mv = Some(Precondition {
        satisfied: Satisfied { lower: 2, upper: 1 },
        ..precondition(Some(SVC), Some(SVC))
    });
    assert_eq!(
        program.validate(),
        Err(ProgramError::EmptyRange(PreconditionKind::Voltage))
    );

    let mut program = recovery_program();
    program.preconditions.ignition = Some(precondition(
        Some(Source::RuntimeInput(RuntimeInput::EngineRunning)),
        Some(SVC),
    ));
    assert_eq!(
        program.validate(),
        Err(ProgramError::InputDoesNotReport {
            kind: PreconditionKind::Ignition,
            input: RuntimeInput::EngineRunning
        })
    );
    program.preconditions.ignition = Some(precondition(
        Some(Source::RuntimeInput(RuntimeInput::IgnitionOn)),
        Some(SVC),
    ));
    assert_eq!(program.validate(), Ok(()));

    let mut program = recovery_program();
    program.identity.vin = Some(Source::RuntimeInput(RuntimeInput::IgnitionOn));
    assert_eq!(
        program.validate(),
        Err(ProgramError::IdentityFromRuntimeInput(IdentityKind::Vin))
    );

    for (service_id, field_id) in [(0, 1), (1, 0)] {
        let zero = Source::EcuService {
            service_id,
            field_id,
        };
        let mut program = recovery_program();
        program.identity.software_version = Some(zero);
        assert_eq!(
            program.validate(),
            Err(ProgramError::ZeroId {
                owner: SourceOwner::Identity(IdentityKind::SoftwareVersion)
            })
        );
        let mut program = recovery_program();
        program.preconditions.vehicle_speed = Some(precondition(Some(SVC), Some(zero)));
        assert_eq!(
            program.validate(),
            Err(ProgramError::ZeroId {
                owner: SourceOwner::Precondition {
                    kind: PreconditionKind::VehicleSpeed,
                    session: SessionKind::Programming
                }
            })
        );
    }
}

#[test]
fn a_restartable_plan_needs_the_identity_timeout_and_resume_limit() {
    for kind in [
        IdentityKind::Vin,
        IdentityKind::HardwarePartNumber,
        IdentityKind::SoftwareVersion,
    ] {
        let mut program = recovery_program();
        match kind {
            IdentityKind::Vin => program.identity.vin = None,
            IdentityKind::HardwarePartNumber => program.identity.hardware_part_number = None,
            IdentityKind::SoftwareVersion => program.identity.software_version = None,
        }
        assert_eq!(
            program.validate(),
            Err(ProgramError::MissingIdentitySource(kind))
        );
        // No restart allowed: not required.
        program.flash[0].recovery_required = RecoveryRequired::FromPc(1);
        assert_eq!(program.validate(), Ok(()));
    }

    let program = mutate_plan(|p| p.timing.session_timeout_millis = 0);
    assert_eq!(
        program.validate(),
        Err(ProgramError::MissingSessionTimeout { flash_session: 1 })
    );
    let program = mutate_plan(|p| p.max_resumes = 0);
    assert_eq!(
        program.validate(),
        Err(ProgramError::ZeroResumeLimit { flash_session: 1 })
    );
    // No application declared stays legal.
    let program = mutate_plan(|p| p.no_application = None);
    assert_eq!(program.validate(), Ok(()));
}

#[test]
fn runtime_inputs_report_their_own_precondition_only() {
    let pairs = [
        (
            RuntimeInput::SupplyVoltageMillivolts,
            PreconditionKind::Voltage,
        ),
        (
            RuntimeInput::ExternalSupplyConnected,
            PreconditionKind::ExternalSupply,
        ),
        (RuntimeInput::IgnitionOn, PreconditionKind::Ignition),
        (RuntimeInput::EngineRunning, PreconditionKind::Engine),
        (
            RuntimeInput::VehicleSpeedKmh,
            PreconditionKind::VehicleSpeed,
        ),
    ];
    for (a, kind_a) in pairs {
        for (_, kind_b) in pairs {
            assert_eq!(a.reports(kind_b), kind_a == kind_b, "{a:?} {kind_b:?}");
        }
    }
}

#[test]
fn a_full_declaration_round_trips_through_postcard() {
    let mut program = recovery_program();
    program.preconditions = Preconditions {
        voltage_mv: Some(Precondition {
            satisfied: Satisfied {
                lower: 12_000,
                upper: 15_000,
            },
            default_session: Some(Source::RuntimeInput(RuntimeInput::SupplyVoltageMillivolts)),
            programming_session: Some(SVC),
        }),
        engine: Some(precondition(Some(SVC), Some(SVC))),
        ..Preconditions::default()
    };
    program.flash[0].recovery_required = RecoveryRequired::FromPc(4);
    program.sections.push(recovery_section(4, 5));
    assert_eq!(program.validate(), Ok(()));
    let bytes = postcard::to_allocvec(&program).unwrap();
    let restored: Program = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(restored.identity, program.identity);
    assert_eq!(restored.preconditions, program.preconditions);
    assert_eq!(restored.flash, program.flash);
    assert_eq!(restored.validate(), Ok(()));
    assert_eq!(postcard::to_allocvec(&restored).unwrap(), bytes);
}

#[test]
fn a_restartable_plan_does_not_replay_an_unsafe_section() {
    let unsafe_section = Section {
        interruptible: Interruptible::Yes,
        ..recovery_section(0, 1)
    };
    let mut program = recovery_program();
    program.sections.push(unsafe_section.clone());
    assert_eq!(
        program.validate(),
        Err(ProgramError::UnsafeSectionInReplay {
            flash_session: 1,
            section: 0
        })
    );
    // A safe section is fine; an unsafe one after the erase is redone with the transfer.
    program.sections[0].idempotency = Idempotency::Safe;
    assert_eq!(program.validate(), Ok(()));
    program.sections.push(Section {
        interruptible: Interruptible::Yes,
        ..recovery_section(3, 4)
    });
    assert_eq!(
        program.validate(),
        Err(ProgramError::UnsafeSectionInReplay {
            flash_session: 1,
            section: 1
        })
    );
    // From the recovery point on, nothing is replayed: an unsafe section there is fine.
    program.flash[0].recovery_required = RecoveryRequired::FromPc(3);
    assert_eq!(program.validate(), Ok(()));
    // The plan does not allow a restart, so nothing is replayed.
    let mut program = recovery_program();
    program.sections.push(unsafe_section);
    program.flash[0].recovery_required = RecoveryRequired::FromPc(1);
    assert_eq!(program.validate(), Ok(()));
}

#[test]
fn an_empty_restartable_range_contradicts_nothing() {
    let mut program = recovery_program();
    program.flash[0].recovery_required = RecoveryRequired::FromPc(0);
    program.sections.push(recovery_section(0, 5));
    assert_eq!(program.validate(), Ok(()));
    // One step later the section overlaps the restartable range.
    program.flash[0].recovery_required = RecoveryRequired::FromPc(1);
    assert!(matches!(
        program.validate(),
        Err(ProgramError::ContradictoryInterruptibility { .. })
    ));
}

#[test]
fn the_no_application_response_code_must_be_usable() {
    for nrc in [0x00, 0x78] {
        let program = mutate_plan(|p| p.no_application = Some(NoApplication::Nrc(nrc)));
        assert_eq!(
            program.validate(),
            Err(ProgramError::InvalidNoApplicationNrc {
                flash_session: 1,
                nrc
            })
        );
    }
    let program = mutate_plan(|p| p.no_application = Some(NoApplication::Nrc(0x7F)));
    assert_eq!(program.validate(), Ok(()));
}

#[test]
fn flag_preconditions_range_over_zero_and_one() {
    for (lower, upper, ok) in [
        (0, 0, true),
        (1, 1, true),
        (0, 1, true),
        (-1, 0, false),
        (0, 2, false),
    ] {
        for kind in [
            PreconditionKind::ExternalSupply,
            PreconditionKind::Ignition,
            PreconditionKind::Engine,
        ] {
            let mut program = recovery_program();
            let flag = Some(Precondition {
                satisfied: Satisfied { lower, upper },
                ..precondition(Some(SVC), Some(SVC))
            });
            match kind {
                PreconditionKind::ExternalSupply => program.preconditions.external_supply = flag,
                PreconditionKind::Ignition => program.preconditions.ignition = flag,
                _ => program.preconditions.engine = flag,
            }
            let expected = if ok {
                Ok(())
            } else {
                Err(ProgramError::FlagRangeOutOfDomain(kind))
            };
            assert_eq!(program.validate(), expected, "{kind:?} {lower}..{upper}");
        }
    }
    // Measured values are not limited.
    let mut program = recovery_program();
    program.preconditions.voltage_mv = Some(Precondition {
        satisfied: Satisfied {
            lower: 11_000,
            upper: 16_000,
        },
        ..precondition(Some(SVC), Some(SVC))
    });
    assert_eq!(program.validate(), Ok(()));
}

#[test]
fn unknown_keys_in_a_program_are_refused() {
    let json = |extra: &str| {
        format!(
            r#"{{"schema_version":2,"code":[],"constants":[],"sections":[],"source_map":[]{extra}}}"#
        )
    };
    assert!(serde_json::from_str::<Program>(&json("")).is_ok());
    assert!(serde_json::from_str::<Program>(&json(r#","preconditions":{}"#)).is_ok());
    // A misspelt key must not be read as "nothing declared".
    assert!(serde_json::from_str::<Program>(&json(r#","precondition":{}"#)).is_err());
    assert!(serde_json::from_str::<Program>(&json(r#","preconditions":{"engin":null}"#)).is_err());
}

/// The layout of `VmState` in schema 1, which had a checkpoint and a resume count at the end.
#[derive(serde::Serialize)]
struct V1State {
    schema_version: u32,
    pc: u32,
    stack: Vec<Value>,
    locals: Vec<Option<Value>>,
    globals: Vec<Option<Value>>,
    call_stack: Vec<Frame>,
    steps: u64,
    checkpoint: Option<V1Checkpoint>,
    resume_count: u16,
}

#[derive(serde::Serialize)]
struct V1Checkpoint {
    pc: u32,
    section: u32,
    vin: Option<String>,
    artifact_digest: Option<String>,
    at: String,
}

/// A journaled schema 1 state decodes (postcard ignores the trailing bytes) but is refused by
/// the schema check, so the removed fields cannot cause a resume from a stale layout.
#[test]
fn a_version_one_state_still_decodes_and_is_refused() {
    let old = V1State {
        schema_version: 1,
        pc: 1,
        stack: vec![Value::I64(7)],
        locals: vec![None],
        globals: Vec::new(),
        call_stack: Vec::new(),
        steps: 4,
        checkpoint: Some(V1Checkpoint {
            pc: 1,
            section: 0,
            vin: Some("VIN".into()),
            artifact_digest: None,
            at: "2026-10-06T00:00:00Z".into(),
        }),
        resume_count: 2,
    };
    let bytes = postcard::to_allocvec(&old).unwrap();
    let (state, rest) = postcard::take_from_bytes::<VmState>(&bytes).unwrap();
    assert!(!rest.is_empty());
    assert!(postcard::from_bytes::<VmState>(&bytes).is_ok());
    assert_eq!(state.schema_version, 1);
    let current = prog(vec![i(1), i(2)], Vec::new());
    assert!(matches!(
        Vm::resume(state).check_state(&current),
        Err(VmError::StateSchemaMismatch { state: 1, .. })
    ));
}
