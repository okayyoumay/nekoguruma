use diag_ir::declaration::{EcuDocumentT, EcuVariantT, encode_ecu_document, load_ecu_document};
use diag_ir::hmi::{self, HmiForm, HmiValue, decode_form};
use diag_ir::{DiagHost, RuntimeInput, StepError, StepOutcome, Value, Vm, VmError, WaitingOn};

use super::*;
use crate::profile::{Intent, Profile};
use crate::star::check;

/// A host for procedures that call no primitive: every call is a test failure.
struct NoHost;

#[derive(Debug, thiserror::Error)]
#[error("no host")]
struct NoHostError;

impl DiagHost for NoHost {
    type Error = NoHostError;

    fn service_request(&mut self, _: u16, _: &[u8]) -> Result<Vec<u8>, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn send_receive(&mut self, _: u32, _: &[u8]) -> Result<Vec<u8>, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn read_dtc(&mut self, _: u8) -> Result<Vec<u8>, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn routine_control(&mut self, _: u16, _: u8, _: &[u8]) -> Result<Vec<u8>, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn security_access(&mut self, _: u64, _: u8, _: &[u8]) -> Result<Option<Vec<u8>>, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn flash_transfer(&mut self, _: u32, _: &[u8]) -> Result<(), NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn wait(&mut self, _: u64, _: u32) -> Result<bool, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn hmi_request(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn record_input(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn monitor_capture(&mut self, _: u32) -> Result<(), NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn artifact_bytes(&mut self, _: u64, _: u32) -> Result<Vec<u8>, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn runtime_input(&mut self, _: RuntimeInput) -> Result<i64, NoHostError> {
        unreachable!("no primitive is lowered yet")
    }
    fn log(&mut self, _: u8, _: &str) {}
}

fn lowered(source: &str) -> Result<Program, Vec<StarError>> {
    lowered_with_warnings(source).map(|lowered| lowered.program)
}

fn lowered_with_warnings(source: &str) -> Result<Lowered, Vec<StarError>> {
    let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    lower_for_a_variant(&checked)
}

/// A document whose variant declares nothing.
fn empty_document() -> Vec<u8> {
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("test".to_owned());
    document.source_digest = Some(String::new());
    document.variant = Some(Box::new(EcuVariantT::default()));
    encode_ecu_document(&document)
}

/// Lowers `checked` for a variant that declares nothing.
fn lower_for_a_variant(checked: &Checked) -> Result<Lowered, Vec<StarError>> {
    let bytes = empty_document();
    let variant = load_ecu_document(&bytes)
        .expect("the document loads")
        .variant()
        .expect("a variant");
    lower("t.star", checked, variant, &RequestIndex::default())
}

/// Runs `source` to its end: `Ok(None)` when it finishes, `Ok(Some((code, detail)))` when it
/// fails itself, `Err` for a VM error.
fn run(source: &str) -> Result<Option<(String, String)>, String> {
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let mut vm = Vm::new(&program);
    for _ in 0..100_000 {
        match vm.step(&program, &mut NoHost) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => return Ok(None),
            Ok(StepOutcome::Failed(failure)) => {
                return Ok(Some((
                    failure.code,
                    String::from_utf8_lossy(&failure.detail).into_owned(),
                )));
            }
            Ok(StepOutcome::Waiting(_)) => unreachable!("nothing waits"),
            Err(error) => return Err(format!("{error:?}")),
        }
    }
    panic!("the procedure did not end")
}

fn failed_with(source: &str) -> String {
    match run(source) {
        Ok(Some((code, _))) => code,
        other => panic!("expected a failure: {other:?}"),
    }
}

fn codes(source: &str) -> Vec<(StarCode, u32)> {
    match lowered(source) {
        Ok(_) => Vec::new(),
        Err(errors) => errors.iter().map(|e| (e.code, e.line)).collect(),
    }
}

/// `main` checks `expr` (written in terms of `f`'s parameters, so nothing folds) and fails with
/// `YES` or `NO`.
fn holds(params: &str, args: &str, expr: &str) -> bool {
    let source = format!(
        "def f({params}):\n    return {expr}\n\ndef main():\n    if f({args}):\n        diag.fail(\"YES\", \"\")\n    diag.fail(\"NO\", \"\")\n"
    );
    match failed_with(&source).as_str() {
        "YES" => true,
        "NO" => false,
        other => panic!("{other}"),
    }
}

#[test]
fn the_test_helper_tells_true_from_false() {
    assert!(holds("x", "1", "x == 1"));
    assert!(!holds("x", "1", "x == 2"));
}

#[test]
fn an_empty_main_finishes() {
    assert_eq!(run("def main():\n    pass\n"), Ok(None));
}

#[test]
fn diag_fail_ends_the_procedure_with_its_code_and_detail() {
    assert_eq!(
        run(
            "def main():\n    diag.fail(\"NO_ECU\", \"no answer\")\n    diag.fail(\"NEVER\", \"\")\n"
        ),
        Ok(Some(("NO_ECU".into(), "no answer".into())))
    );
}

#[test]
fn calls_pass_arguments_and_return_values() {
    let source = "\
def add(a, b):
    return a + b

def twice(x):
    y = add(x, x)
    return y

def main():
    if twice(add(2, 3)) == 10:
        diag.fail(\"OK\", \"\")
";
    assert_eq!(failed_with(source), "OK");
}

#[test]
fn integer_floor_division_and_remainder_follow_starlark() {
    for (a, b, q, r) in [
        (7, 2, 3, 1),
        (-7, 2, -4, 1),
        (7, -2, -4, -1),
        (-7, -2, 3, -1),
        (-4, 2, -2, 0),
    ] {
        assert!(
            holds("a, b", &format!("{a}, {b}"), &format!("a // b == {q}")),
            "{a} // {b}"
        );
        assert!(
            holds("a, b", &format!("{a}, {b}"), &format!("a % b == {r}")),
            "{a} % {b}"
        );
    }
}

#[test]
fn a_remainder_by_minus_one_is_zero_even_for_the_smallest_integer() {
    let min = "-9223372036854775807 - 1";
    assert!(holds("x, y", &format!("{min}, -1"), "x % y == 0"));
    assert!(holds("x, y", "7, -1", "x % y == 0"));
    // The quotient is out of range, at run time and folded.
    let result = run(&format!(
        "def f(x, y):\n    return x // y\n\ndef main():\n    f({min}, -1)\n"
    ));
    assert!(
        result.as_ref().is_err_and(|e| e.contains("Overflow")),
        "{result:?}"
    );
    assert_eq!(
        fold(BinOp::Percent, &Const::Int(i64::MIN), &Const::Int(-1)),
        Ok(Const::Int(0))
    );
    assert!(fold(BinOp::FloorDivide, &Const::Int(i64::MIN), &Const::Int(-1)).is_err());
}

#[test]
fn a_folded_float_remainder_is_exact_and_takes_the_divisors_sign() {
    let rem = |x: f64, y: f64| match fold(BinOp::Percent, &Const::Float(x), &Const::Float(y)) {
        Ok(Const::Float(r)) => r,
        other => panic!("{other:?}"),
    };
    assert_eq!(rem(0.5, 0.1), 0.5 % 0.1);
    let r = rem(1e308, 1e-10);
    assert!((0.0..1e-10).contains(&r), "{r}");
    assert_eq!(rem(-7.0, 2.0), 1.0);
    assert_eq!(rem(7.0, -2.0), -1.0);
    assert_eq!(rem(-4.0, 2.0), 0.0);
}

#[test]
fn a_folded_shift_count_outside_0_to_63_is_an_error() {
    for (op, n) in [
        (BinOp::RightShift, 64),
        (BinOp::RightShift, -1),
        (BinOp::LeftShift, 64),
        (BinOp::LeftShift, -1),
    ] {
        assert!(fold(op, &Const::Int(1), &Const::Int(n)).is_err(), "{n}");
    }
    assert_eq!(
        fold(BinOp::RightShift, &Const::Int(-8), &Const::Int(63)),
        Ok(Const::Int(-1))
    );
}

#[test]
fn min_and_max_keep_the_first_of_equal_values() {
    let args = [Const::Int(1), Const::Float(1.0)];
    assert_eq!(fold_builtin("max", &args), Ok(Const::Int(1)));
    assert_eq!(fold_builtin("min", &args), Ok(Const::Int(1)));
    assert_eq!(
        fold_builtin("max", &[Const::Int(1), Const::Int(3), Const::Int(2)]),
        Ok(Const::Int(3))
    );
    // The kept int can index a list.
    assert_eq!(
        failed_with("def main():\n    diag.fail([\"A\", \"B\"][max(1, 1.0)], \"\")\n"),
        "B"
    );
}

#[test]
fn constants_that_double_are_stopped_at_the_size_limit() {
    // Each line doubles the one before. Without a limit on the folded size the last lines
    // would need gigabytes; with it, lowering stops building on the first constant too large.
    // A string of 2^20 bytes is on line 21; a list of 2^19 lists of one int, line 20.
    for (first, double, line) in [("\"x\"", "A{p} + A{p}", 21), ("[1]", "[A{p}, A{p}]", 20)] {
        let mut source = format!("A0 = {first}\n");
        for i in 1..40 {
            let line = double.replace("{p}", &(i - 1).to_string());
            source.push_str(&format!("A{i} = {line}\n"));
        }
        source.push_str("\ndef main():\n    pass\n");
        let started = std::time::Instant::now();
        let found = codes(&source);
        assert!(!found.is_empty(), "{first}");
        assert!(
            found
                .iter()
                .all(|(code, _)| *code == StarCode::NotLowerable)
        );
        // The first constant past the limit, over 2^20 values and bytes, is reported.
        assert_eq!(found[0].1, line, "{first}: {found:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
    }
}

#[test]
fn a_global_named_often_is_pooled_once() {
    let mut source = format!(
        "T = \"{}\"\n\ndef same(x, y):\n    return x == y\n\ndef main():\n",
        "x".repeat(1000)
    );
    for _ in 0..50 {
        source.push_str("    if same(T, T):\n        pass\n");
    }
    let program = lowered(&source).unwrap();
    assert_eq!(program.constants.len(), 1);
}

#[test]
fn a_dict_literal_must_not_repeat_a_key() {
    let found = codes("D = {\"k\": 1, \"k\": 2}\n\ndef main():\n    pass\n");
    assert_eq!(found, vec![(StarCode::NotLowerable, 1)]);
}

#[test]
fn large_dicts_compare_in_linear_time() {
    let entries: Vec<String> = (0..50_000).map(|n| format!("\"k{n}\": {n}")).collect();
    let source = format!(
        "D = {{{}}}\nE = D == D\n\ndef main():\n    if E:\n        diag.fail(\"YES\", \"\")\n",
        entries.join(", ")
    );
    let started = std::time::Instant::now();
    assert_eq!(failed_with(&source), "YES");
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
}

/// The instructions of `program` that log `message`.
fn logs_of(program: &Program, message: &str) -> Vec<u32> {
    let index = program
        .constants
        .iter()
        .position(|c| c == message.as_bytes())
        .expect("the message is a constant") as u32;
    (0..program.code.len() as u32)
        .filter(
            |&pc| matches!(program.code[pc as usize], Op::Log { message, .. } if message == index),
        )
        .collect()
}

/// Every jump stays in the copy it is in: its target is in the same sections of the copies that
/// run in a section function (a primitive outside every section has one of its own that is
/// `Yes`, which is left out).
fn jumps_stay_in_their_copies(program: &Program) {
    let covering = |pc: u32| -> Vec<(String, u32)> {
        in_a_section(program, pc)
            .iter()
            .filter(|s| !matches!(s.interruptible, Interruptible::Yes))
            .map(|s| (format!("{:?}", s.interruptible), s.expected_millis))
            .collect()
    };
    let mut jumps = 0;
    for (pc, op) in program.code.iter().enumerate() {
        if let Op::Jump(target) | Op::JumpIfFalse(target) = op {
            assert_eq!(covering(pc as u32), covering(*target), "jump at {pc}");
            jumps += 1;
        }
    }
    assert!(jumps > 0, "the program has a jump to check");
}

/// The calls to the copies of the function that logs `message`: whether each call is in a
/// section, and whether the copy it reaches is.
fn calls_to(program: &Program, message: &str) -> Vec<(bool, bool)> {
    let logs = logs_of(program, message);
    let mut found: Vec<(bool, bool)> = program
        .code
        .iter()
        .enumerate()
        .filter_map(|(pc, op)| match op {
            // The copy entered at `target` holds a log before its first `Ret`.
            Op::Call(target) => {
                let end = (*target..program.code.len() as u32)
                    .find(|&at| matches!(program.code[at as usize], Op::Ret))?;
                logs.iter()
                    .any(|&log| (*target..end).contains(&log))
                    .then(|| {
                        (
                            !in_a_section(program, pc as u32).is_empty(),
                            !in_a_section(program, *target).is_empty(),
                        )
                    })
            }
            _ => None,
        })
        .collect();
    found.sort();
    found
}

fn in_a_section(program: &Program, pc: u32) -> Vec<&Section> {
    program
        .sections
        .iter()
        .filter(|s| (s.start_pc..s.end_pc).contains(&pc))
        .collect()
}

/// The invariant of ADR-332 for a lowered program: the sections are sorted and never overlap,
/// every diagnostic primitive has a section over it alone, a section that covers no primitive
/// is a `CheckState` piece, and pieces share their neighbours' interruptibility and duration, so
/// the copy of a section function they cut is covered without a gap by one set of attributes.
fn assert_sections_split(program: &Program) {
    let attributes = |s: &Section| (format!("{:?}", s.interruptible), s.expected_millis);
    for pair in program.sections.windows(2) {
        assert!(pair[0].start_pc < pair[1].start_pc, "sorted: {pair:?}");
        assert!(pair[0].end_pc <= pair[1].start_pc, "disjoint: {pair:?}");
    }
    let primitive = |pc: u32| program.code[pc as usize].is_diagnostic_primitive();
    let mut pieces = Vec::new();
    for section in &program.sections {
        let covered = (section.start_pc..section.end_pc).filter(|&pc| primitive(pc));
        match covered.count() {
            0 => {
                assert!(
                    matches!(section.idempotency, Idempotency::CheckState),
                    "a piece says nothing: {section:?}"
                );
                pieces.push(section);
            }
            1 => assert_eq!(section.end_pc, section.start_pc + 1, "{section:?}"),
            _ => panic!("one primitive at most: {section:?}"),
        }
    }
    for pc in (0..program.code.len() as u32).filter(|&pc| primitive(pc)) {
        let [section] = in_a_section(program, pc)[..] else {
            panic!("exactly one section covers the primitive at {pc}");
        };
        assert_eq!((section.start_pc, section.end_pc), (pc, pc + 1));
    }
    for pair in program.sections.windows(2) {
        let adjacent = pair[0].end_pc == pair[1].start_pc;
        let piece = |s: &Section| pieces.iter().any(|p| std::ptr::eq(*p, s));
        if adjacent && (piece(&pair[0]) || piece(&pair[1])) {
            assert_eq!(attributes(&pair[0]), attributes(&pair[1]), "{pair:?}");
        }
    }
}

/// The primitives of `program` in pc order, each with the idempotency of its section.
fn primitives_of(program: &Program) -> Vec<String> {
    (0..program.code.len() as u32)
        .filter(|&pc| program.code[pc as usize].is_diagnostic_primitive())
        .map(|pc| {
            let [section] = in_a_section(program, pc)[..] else {
                panic!("one section covers the primitive at {pc}");
            };
            format!("{:?}", section.idempotency)
        })
        .collect()
}

#[test]
fn a_call_has_its_default_idempotency_and_an_annotation_replaces_it() {
    let source = "\
def main():
    diag.wait(1)
    # @idempotency unsafe
    diag.wait(2)
    # @idempotency checkState
    diag.log(1, \"x\")
    diag.log(1, \"y\")
";
    let program = lowered(source).expect("lowers");
    program.validate().expect("valid");
    assert_sections_split(&program);
    assert_eq!(
        primitives_of(&program),
        ["Safe", "Unsafe", "CheckState", "Safe"]
    );
    // Outside every section each primitive's section is `Yes` with an expected duration of 0.
    assert_eq!(program.sections.len(), 4);
    for section in &program.sections {
        assert_eq!(section.end_pc, section.start_pc + 1);
        assert!(matches!(section.interruptible, Interruptible::Yes));
        assert_eq!(section.expected_millis, 0);
    }
}

#[test]
fn an_annotation_reaches_nested_statements_and_the_inner_one_wins() {
    let source = "\
def helper():
    diag.wait(5)

def pick(n):
    # @idempotency unsafe
    if n > 1:
        diag.wait(1)
    else:
        diag.log(1, \"e\")

def main():
    # @idempotency unsafe
    for i in range(3):
        diag.wait(1)
        # @idempotency checkState
        if i > 1:
            diag.wait(2)
        diag.log(1, \"a\")
        helper()
    pick(2)
";
    let program = lowered(source).expect("lowers");
    program.validate().expect("valid");
    assert_sections_split(&program);
    // `main`'s copy comes first: the loop's `wait` and `log` are `Unsafe` and the `if`'s
    // `wait` `CheckState`. `helper`, which the loop calls, keeps its own default, and `pick`
    // gives both branches its annotation.
    let found = primitives_of(&program);
    assert_eq!(found.len(), 6);
    assert_eq!(found[..3], ["Unsafe", "CheckState", "Unsafe"]);
    let mut rest = found[3..].to_vec();
    rest.sort();
    assert_eq!(rest, ["Safe", "Unsafe", "Unsafe"]);
}

#[test]
fn an_annotation_applies_to_the_first_statement_of_its_line_only() {
    let source = "def main():\n    # @idempotency unsafe\n    diag.wait(1); diag.wait(2)\n";
    let program = lowered(source).expect("lowers");
    assert_eq!(primitives_of(&program), ["Unsafe", "Safe"]);
}

#[test]
fn an_annotation_reaches_every_iteration_of_an_unrolled_loop() {
    let source = "\
L = [1, 2, 3]

def main():
    # @idempotency unsafe
    for n in L:
        diag.wait(n)
";
    let program = lowered(source).expect("lowers");
    assert_sections_split(&program);
    assert_eq!(primitives_of(&program), ["Unsafe", "Unsafe", "Unsafe"]);
}

#[test]
fn an_annotated_statement_without_a_primitive_is_refused() {
    let found = |source: &str| codes(source);
    let bad = StarCode::BadAnnotation;
    assert_eq!(
        found("def main():\n    # @idempotency safe\n    x = 1\n"),
        [(bad, 3)]
    );
    // The functions a statement calls keep their own annotations and defaults.
    assert_eq!(
        found(
            "def helper():\n    diag.wait(1)\n\ndef main():\n    # @idempotency safe\n    helper()\n"
        ),
        [(bad, 6)]
    );
    assert_eq!(
        found("def main():\n    # @idempotency safe\n    if 1 < 2:\n        x = 1\n"),
        [(bad, 3)]
    );
    assert_eq!(
        found("def main():\n    # @idempotency safe\n    \"\"\"A docstring.\"\"\"\n    pass\n"),
        [(bad, 3)]
    );
    // An unrolled body is lowered once per item, and reports the problem once.
    assert_eq!(
        found(
            "L = [1, 2, 3]\n\ndef main():\n    for n in L:\n        # @idempotency safe\n        x = n\n"
        ),
        [(bad, 6)]
    );
    // A statement whose primitive is in a branch that never runs is still lowered, so it has
    // one.
    assert_eq!(
        found("def main():\n    # @idempotency safe\n    if 1 > 2:\n        diag.wait(1)\n"),
        []
    );
}

#[test]
fn an_error_in_an_annotated_unrolled_body_is_reported_alone() {
    // The error repeats in each iteration, and is recorded once; the annotation is not
    // reported on top of it.
    let source = "\
L = [1, 2]

def main():
    for n in L:
        # @idempotency safe
        x = n.upper()
";
    let found = codes(source);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_ne!(found[0].0, StarCode::BadAnnotation);
}

#[test]
fn a_call_that_is_not_in_the_defaults_table_is_not_safe_by_default() {
    let op = Op::Ret;
    let default = |call| format!("{:?}", default_idempotency(call, &op, false));
    for call in [
        "read_dtc",
        "ecu_info",
        "precondition",
        "wait",
        "capture",
        "log",
        "hmi",
        "record",
    ] {
        assert_eq!(default(call), "Safe", "{call}");
    }
    for call in ["security_access", "routine", "flash_transfer"] {
        assert_eq!(default(call), "CheckState", "{call}");
    }
    assert_eq!(default("request"), "Unsafe");
    assert_eq!(default("anything_new"), "Unsafe");
    let of = |service, in_plan| {
        format!(
            "{:?}",
            default_idempotency("request", &Op::ServiceRequest { service }, in_plan)
        )
    };
    // A read, a session control and a tester present are safe everywhere (ADR-336 item 8).
    for service in [0x22, 0x19, 0x10, 0x3E] {
        assert_eq!(
            (of(service, false), of(service, true)),
            ("Safe".into(), "Safe".into())
        );
    }
    // Any other service is unsafe, except in a flash plan's expansion.
    for service in [0x11, 0x2E, 0x31] {
        assert_eq!(
            (of(service, false), of(service, true)),
            ("Unsafe".into(), "CheckState".into())
        );
    }
}

#[test]
fn a_section_copy_is_cut_around_its_primitives() {
    let source = "\
# @section uninterruptible expected=700
def write():
    x = 1
    diag.wait(1)
    y = x + 2
    # @idempotency unsafe
    diag.wait(y)
    z = 3

def main():
    diag.log(1, \"before\")
    write()
";
    let program = lowered(source).expect("lowers");
    program.validate().expect("valid");
    assert_sections_split(&program);
    // The copy of `write` is the call target that a section covers.
    let entry = program
        .code
        .iter()
        .find_map(|op| match op {
            Op::Call(at)
                if in_a_section(&program, *at)
                    .iter()
                    .any(|s| matches!(s.interruptible, Interruptible::No)) =>
            {
                Some(*at)
            }
            _ => None,
        })
        .expect("a call into the section");
    let copy: Vec<&Section> = program
        .sections
        .iter()
        .filter(|s| matches!(s.interruptible, Interruptible::No))
        .collect();
    // Contiguous from the call's target to the copy's `Ret`, and every piece has the section
    // function's attributes.
    assert_eq!(copy[0].start_pc, entry);
    for pair in copy.windows(2) {
        assert_eq!(pair[0].end_pc, pair[1].start_pc);
    }
    let end = copy.last().unwrap().end_pc;
    assert!(matches!(program.code[end as usize - 1], Op::Ret));
    for section in &copy {
        assert_eq!(section.expected_millis, 700);
    }
    let kinds: Vec<String> = copy
        .iter()
        .map(|s| format!("{:?}", s.idempotency))
        .collect();
    assert_eq!(
        kinds,
        ["CheckState", "Safe", "CheckState", "Unsafe", "CheckState"]
    );
    // The two primitives of the copy are one instruction each.
    let single: Vec<u32> = copy
        .iter()
        .filter(|s| {
            s.end_pc == s.start_pc + 1
                && program.code[s.start_pc as usize].is_diagnostic_primitive()
        })
        .map(|s| s.start_pc)
        .collect();
    assert_eq!(single.len(), 2);
    // The `log` before the call is outside the section: `Yes`, 0.
    let before = (0..entry)
        .find(|&pc| program.code[pc as usize].is_diagnostic_primitive())
        .expect("the log");
    let [outside] = in_a_section(&program, before)[..] else {
        panic!("one section");
    };
    assert!(matches!(outside.interruptible, Interruptible::Yes));
    assert_eq!(outside.expected_millis, 0);
}

#[test]
fn a_helper_called_in_and_outside_a_section_is_tagged_in_each_copy() {
    let source = "\
def helper():
    # @idempotency unsafe
    diag.wait(1)
    diag.log(1, \"h\")

# @section recoveryRequired expected=9
def write():
    helper()

def main():
    helper()
    write()
";
    let program = lowered(source).expect("lowers");
    program.validate().expect("valid");
    assert_sections_split(&program);
    // Two copies of `helper`: each has the annotation on its wait and the default on its log.
    let found: Vec<(String, String, u32)> = (0..program.code.len() as u32)
        .filter(|&pc| program.code[pc as usize].is_diagnostic_primitive())
        .map(|pc| {
            let [section] = in_a_section(&program, pc)[..] else {
                panic!("one section");
            };
            (
                format!("{:?}", section.idempotency),
                format!("{:?}", section.interruptible),
                section.expected_millis,
            )
        })
        .collect();
    assert_eq!(found.len(), 4);
    let outside = ("Yes".to_owned(), 0);
    let inside = ("RecoveryRequired".to_owned(), 9);
    let mut waits = Vec::new();
    let mut logs = Vec::new();
    for (idempotency, interruptible, millis) in found {
        match idempotency.as_str() {
            "Unsafe" => waits.push((interruptible, millis)),
            "Safe" => logs.push((interruptible, millis)),
            other => panic!("{other}"),
        }
    }
    waits.sort();
    logs.sort();
    // Sorted by name, `RecoveryRequired` comes before `Yes`.
    let expected = vec![inside, outside];
    assert_eq!(waits, expected);
    assert_eq!(logs, expected);
}

#[test]
fn a_section_function_and_what_it_calls_are_covered_by_its_section() {
    let source = "\
def helper(n):
    if n > 1:
        diag.log(1, \"twice\")
    diag.log(1, \"helper\")

def inner():
    helper(2)

# @section recoveryRequired expected=500
def write():
    inner()
    diag.log(1, \"write\")

def main():
    helper(1)
    write()
    write()
";
    let program = lowered(source).expect("lowers");
    program.validate().expect("valid");
    jumps_stay_in_their_copies(&program);
    // Each copy of `helper` branches on its parameter.
    let branches = program
        .code
        .iter()
        .filter(|op| matches!(op, Op::JumpIfFalse(_)))
        .count();
    assert_eq!(branches, 2);
    // `main` calls the outside copy of `helper`, and the section's `inner` its own copy.
    assert_eq!(calls_to(&program, "helper"), [(false, false), (true, true)]);
    assert_sections_split(&program);
    // `main`'s copy of `helper` has a `Yes` section for each of its two logs, and nothing else
    // is outside the section: the copies of `write`, `inner` and `helper` are covered by pieces
    // and one-instruction sections that all have the section's attributes.
    let (outside, inside): (Vec<&Section>, Vec<&Section>) = program
        .sections
        .iter()
        .partition(|s| matches!(s.interruptible, Interruptible::Yes));
    assert_eq!(outside.len(), 2);
    for section in outside {
        assert_eq!(section.end_pc, section.start_pc + 1);
        assert!(matches!(section.idempotency, Idempotency::Safe));
        assert_eq!(section.expected_millis, 0);
    }
    assert!(!inside.is_empty());
    for section in inside {
        assert!(matches!(
            section.interruptible,
            Interruptible::RecoveryRequired
        ));
        assert_eq!(section.expected_millis, 500);
    }
    // `helper` runs twice: from `main`, outside every section, and from the section's copy.
    let helpers = logs_of(&program, "helper");
    assert_eq!(helpers.len(), 2);
    let mut kinds: Vec<bool> = helpers
        .iter()
        .map(|&pc| {
            let [section] = in_a_section(&program, pc)[..] else {
                panic!("one section covers each log");
            };
            matches!(section.interruptible, Interruptible::Yes)
        })
        .collect();
    kinds.sort();
    assert_eq!(kinds, [false, true]);
    let [write] = logs_of(&program, "write")[..] else {
        panic!("one copy of `write`");
    };
    assert_eq!(in_a_section(&program, write).len(), 1);
    // `main` calls the one copy of `write` twice: the calls from outside every section that
    // enter a section.
    let in_section = |pc: u32| {
        in_a_section(&program, pc)
            .iter()
            .any(|s| matches!(s.interruptible, Interruptible::RecoveryRequired))
    };
    let calls = program
        .code
        .iter()
        .enumerate()
        .filter(|(pc, op)| matches!(op, Op::Call(at) if in_section(*at) && !in_section(*pc as u32)))
        .count();
    assert_eq!(calls, 2);
    assert_eq!(run(source), Ok(None));
}

#[test]
fn each_section_has_its_own_copies_and_its_own_attributes() {
    let source = "\
def helper():
    diag.log(1, \"helper\")

# @section uninterruptible expected=10
def first():
    helper()

# @section interruptible
def second():
    helper()

def main():
    first()
    second()
";
    let program = lowered(source).expect("lowers");
    program.validate().expect("valid");
    let helpers = logs_of(&program, "helper");
    assert_eq!(helpers.len(), 2);
    let attributes: Vec<(bool, u32)> = helpers
        .iter()
        .map(|&pc| {
            let [section] = in_a_section(&program, pc)[..] else {
                panic!("one section covers each copy");
            };
            (
                matches!(section.interruptible, Interruptible::No),
                section.expected_millis,
            )
        })
        .collect();
    let mut sorted = attributes.clone();
    sorted.sort();
    assert_eq!(sorted, [(false, 0), (true, 10)]);
}

#[test]
fn main_can_be_a_section_function() {
    let source = "# @section uninterruptible\ndef main():\n    diag.log(1, \"m\")\n";
    let program = lowered(source).expect("lowers");
    program.validate().expect("valid");
    let [log] = logs_of(&program, "m")[..] else {
        panic!("one log");
    };
    assert_eq!(in_a_section(&program, log).len(), 1);
}

#[test]
fn sections_need_at_most_a_thousand_copies() {
    // Section `a` reaches `first` helpers and `b` reaches `second`: 2 + first + second copies.
    let source = |first: usize, second: usize| {
        let mut source = String::new();
        for n in 0..first.max(second) {
            source.push_str(&format!("def h{n}():\n    pass\n\n"));
        }
        let calls =
            |count: usize| -> String { (0..count).map(|n| format!("    h{n}()\n")).collect() };
        source.push_str(&format!(
            "# @section interruptible\ndef a():\n{}\n",
            calls(first)
        ));
        source.push_str(&format!(
            "# @section interruptible\ndef b():\n{}\n",
            calls(second)
        ));
        source.push_str("def main():\n    a()\n    b()\n");
        source
    };
    let program = lowered(&source(500, 498)).expect("1,000 copies are within the budget");
    assert_eq!(program.sections.len(), 1000);

    // The 1,001st copy is `b`'s copy of `h498`, at its call in `b`.
    let text = source(500, 499);
    let found = codes(&text);
    let call_line = text
        .lines()
        .enumerate()
        .filter(|(_, line)| *line == "    h498()")
        .last()
        .map(|(at, _)| at as u32 + 1)
        .unwrap();
    assert_eq!(found, vec![(StarCode::SpecializationLimit, call_line)]);
}

#[test]
fn the_copies_of_a_large_function_are_bounded_in_instructions() {
    // About 270,000 instructions in `big`, and copies for 100 sections.
    let items: Vec<String> = (0..300).map(|n| n.to_string()).collect();
    let mut source = format!(
        "A = [{}]\n\ndef big():\n    for a in A:\n        for b in A:\n            diag.log(1, \"x\")\n\n",
        items.join(", ")
    );
    for n in 0..100 {
        source.push_str(&format!(
            "# @section interruptible\ndef s{n}():\n    big()\n\n"
        ));
    }
    let calls: String = (0..100).map(|n| format!("    s{n}()\n")).collect();
    source.push_str(&format!("def main():\n{calls}"));
    let started = std::time::Instant::now();
    let found = codes(&source);
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, StarCode::SpecializationLimit);
}

#[test]
fn ints_and_floats_compare_by_exact_value() {
    use std::cmp::Ordering::*;
    let order = |a: Const, b: Const| number_order(&a, &b).unwrap();
    // 2^53 + 1 is not a float; the float nearest it is 2^53.
    let (above, float) = (
        Const::Int((1 << 53) + 1),
        Const::Float(9_007_199_254_740_992.0),
    );
    assert_eq!(order(above.clone(), float.clone()), Greater);
    assert_eq!(order(float.clone(), above.clone()), Less);
    assert!(!equal(&above, &float));
    assert_eq!(
        fold_builtin("max", &[float.clone(), above.clone()]),
        Ok(above)
    );
    assert_eq!(
        order(Const::Int(i64::MAX), Const::Float(2f64.powi(63))),
        Less
    );
    assert_eq!(
        order(Const::Int(i64::MIN), Const::Float(-2f64.powi(63))),
        Equal
    );
    assert_eq!(order(Const::Int(i64::MIN), Const::Float(-1e19)), Greater);
    assert_eq!(order(Const::Int(1), Const::Float(1.5)), Less);
    assert_eq!(order(Const::Int(-1), Const::Float(-1.5)), Greater);
    assert_eq!(order(Const::Int(2), Const::Float(f64::NAN)), Less);
    assert!(equal(&Const::Int(3), &Const::Float(3.0)));
}

#[test]
fn int_of_a_float_accepts_every_value_in_range() {
    assert_eq!(
        fold_builtin("int", &[Const::Float(-2f64.powi(63))]),
        Ok(Const::Int(i64::MIN))
    );
    assert_eq!(
        fold_builtin("int", &[Const::Float(9.21e18)]),
        Ok(Const::Int(9_210_000_000_000_000_000))
    );
    assert_eq!(
        fold_builtin("int", &[Const::Float(-2.7)]),
        Ok(Const::Int(-2))
    );
    for f in [2f64.powi(63), -9.3e18, f64::NAN, f64::INFINITY] {
        assert!(fold_builtin("int", &[Const::Float(f)]).is_err(), "{f}");
    }
}

#[test]
fn string_and_bytes_literals_stay_within_the_constant_limit() {
    // A literal of 2^20 bytes would count 2^20 + 1, one past the constant limit, but no file
    // within the source limit (2.8) holds one; the lowering bounds literals all the same.
    for prefix in ["", "b"] {
        let fits = format!(
            "T = {prefix}\"{}\"\n\ndef main():\n    pass\n",
            "x".repeat((1 << 20) - 64)
        );
        assert!(codes(&fits).is_empty(), "{prefix}");
        let over = format!(
            "T = {prefix}\"{}\"\n\ndef main():\n    pass\n",
            "x".repeat(1 << 20)
        );
        let errors = check("t.star", &over).unwrap_err();
        assert_eq!(errors[0].code, StarCode::SourceTooLarge, "{prefix}");
    }
}

#[test]
fn constants_nest_at_most_64_deep() {
    // Each line is shallow, but the globals nest one in the next: folding and dropping them
    // would recurse 40,000 deep.
    let chain = |lines: usize, wrap: &str| {
        let mut source = String::from("A0 = 0\n");
        for i in 1..=lines {
            let inner = format!("A{}", i - 1);
            source.push_str(&format!("A{i} = {}\n", wrap.replace("{}", &inner)));
        }
        source.push_str(&format!(
            "X = A{lines} == A{lines}\n\ndef main():\n    pass\n"
        ));
        source
    };
    for wrap in ["[{}]", "{\"k\": {}}"] {
        assert!(codes(&chain(64, wrap)).is_empty(), "{wrap}");
        // A65 is the first too deep; it is on line 66.
        assert_eq!(
            codes(&chain(65, wrap)),
            vec![(StarCode::NotLowerable, 66)],
            "{wrap}"
        );
        let found = codes(&chain(40_000, wrap));
        assert!(!found.is_empty() && found.iter().all(|(c, _)| *c == StarCode::NotLowerable));
    }
}

#[test]
fn the_smallest_integer_can_be_written_as_a_literal() {
    for literal in ["-9223372036854775808", "-0x8000000000000000"] {
        let source = format!(
            "MIN = {literal}\n\ndef main():\n    if MIN == -9223372036854775807 - 1:\n        diag.fail(\"YES\", \"\")\n"
        );
        assert_eq!(failed_with(&source), "YES", "{literal}");
    }
    // One less is still out of range.
    assert_eq!(
        codes("X = -9223372036854775809\n\ndef main():\n    pass\n"),
        vec![(StarCode::NotLowerable, 1)]
    );
}

#[test]
fn a_fail_detail_must_be_a_string() {
    let found = codes("def main():\n    diag.fail(\"E\", b\"\\xff\")\n");
    assert_eq!(found, vec![(StarCode::NotLowerable, 2)]);
}

#[test]
fn a_path_that_fails_needs_no_return_value() {
    let source = "\
def f(x):
    if x > 0:
        return x
    else:
        diag.fail(\"NEG\", \"\")

def main():
    if f(2) == 2:
        f(-1)
";
    assert_eq!(failed_with(source), "NEG");
}

#[test]
fn an_int_in_bytes_must_be_a_byte_value() {
    for (needle, ok) in [("-1", false), ("256", false), ("0", true), ("255", true)] {
        for op in ["in", "not in"] {
            let source = format!("X = {needle} {op} b\"abc\"\n\ndef main():\n    pass\n");
            assert_eq!(codes(&source).is_empty(), ok, "{needle} {op}");
        }
    }
}

#[test]
fn a_bytes_search_is_linear_and_correct() {
    let found = |h: &[u8], n: &[u8]| contains_bytes(h, n);
    assert!(found(b"abc", b""));
    assert!(found(b"aabaabaaab", b"aaab"));
    assert!(found(b"abcabd", b"abd"));
    assert!(!found(b"abcab", b"abd"));
    assert!(!found(b"ab", b"abc"));
    // An all-`A` haystack and a needle of `A`s with a `B` in the middle: a naive search
    // compares a long prefix at every position.
    let haystack = vec![b'A'; 1 << 19];
    let mut needle = vec![b'A'; (1 << 18) + 1];
    needle[1 << 17] = b'B';
    let started = std::time::Instant::now();
    assert!(!found(&haystack, &needle));
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn the_constants_of_a_procedure_are_bounded_together() {
    // One constant of 2^19 bytes, then many concatenations, each within the limit for one
    // constant but together past the total.
    let mut source = String::from("B0 = b\"x\"\n");
    for i in 1..=19 {
        source.push_str(&format!("B{i} = B{p} + B{p}\n", p = i - 1));
    }
    for i in 0..64 {
        source.push_str(&format!("C{i} = B19 + b\"y\"\n"));
    }
    source.push_str("\ndef main():\n    pass\n");
    let found = codes(&source);
    assert!(!found.is_empty() && found.iter().all(|(c, _)| *c == StarCode::NotLowerable));
    // B0 to B19 hold just over 2^20, and each C line just over 2^19 (its literal counts too),
    // so the 30th C (line 50) passes 2^24.
    assert_eq!(found[0].1, 50, "{found:?}");
}

#[test]
fn constants_built_in_function_bodies_are_bounded_together() {
    // A global of 2^19 bytes, then a body that builds 64 concatenations at once: as the
    // arguments of one call, and as the items of one list. Every inference pass and the
    // emission stop building them once the total passes 2^24.
    let mut doubling = String::from("B0 = b\"x\"\n");
    for i in 1..=19 {
        doubling.push_str(&format!("B{i} = B{p} + B{p}\n", p = i - 1));
    }
    let items = vec!["B19 + b\"y\""; 64].join(", ");
    for body in [format!("max({items})"), format!("[{items}]")] {
        let source = format!("{doubling}\ndef main():\n    x = {body}\n");
        let found = codes(&source);
        assert!(
            found
                .iter()
                .any(|(c, line)| *c == StarCode::NotLowerable && *line == 23),
            "{found:?}"
        );
    }
}

#[test]
fn running_out_of_constants_where_nothing_reports_it_still_rejects() {
    // `h` runs out of the constant budget only inside an operand that is never evaluated, so
    // nothing reports it there. Inference visits `h` before `f` and emission `f` before `h`,
    // so the placeholder could reach `f`'s inferred type; the procedure must be rejected.
    let mut source = String::from("B0 = b\"x\"\n");
    for i in 1..=19 {
        source.push_str(&format!("B{i} = B{p} + B{p}\n", p = i - 1));
    }
    let items = vec!["B19 + b\"y\""; 64].join(", ");
    source.push_str(&format!(
        "\ndef main():\n    h()\n    if f() == \"s\":\n        diag.fail(\"YES\", \"\")\n\ndef h():\n    x = True or [{items}]\n\ndef f():\n    return \"s\"\n"
    ));
    let found = codes(&source);
    assert_eq!(found, vec![(StarCode::NotLowerable, 28)], "{found:?}");
}

#[test]
fn many_parameters_and_locals_are_numbered_in_linear_time() {
    let n = 32_767;
    let params: Vec<String> = (0..n).map(|i| format!("p{i}")).collect();
    let mut source = format!("def unused({}):\n", params.join(", "));
    for i in 0..n {
        source.push_str(&format!("    l{i} = 0\n"));
    }
    // A parameter assigned to is still one slot.
    source.push_str("    p0 = 1\n\ndef main():\n    pass\n");
    let checked = check("t.star", &source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let started = std::time::Instant::now();
    assert!(lower_for_a_variant(&checked).is_ok());
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

#[test]
fn repeated_folds_over_a_shared_constant_are_bounded_together() {
    // B19 is 2^19 bytes of `A`; each `N in B19` reads it all but builds nothing, so only the
    // folding budget (2^30) stops the searches: past about 2,048 of them.
    let mut source = String::from("B0 = b\"A\"\n");
    for i in 1..=19 {
        source.push_str(&format!("B{i} = B{p} + B{p}\n", p = i - 1));
    }
    source.push_str("N = b\"B\"\n");
    for i in 0..2_100 {
        source.push_str(&format!("R{i} = N in B19\n"));
    }
    source.push_str("\ndef main():\n    pass\n");
    let found = codes(&source);
    assert!(!found.is_empty() && found.iter().all(|(c, _)| *c == StarCode::NotLowerable));
    // Lines 1 to 21 build B0 to B19 and N, whose folds read just over 2^20; each search on
    // line 22 onward reads just over 2^19, so the 2,046th (line 2,067) passes 2^30.
    assert_eq!(found[0].1, 2_067, "{:?}", &found[..1]);
}

#[test]
fn list_searches_and_slices_are_charged_for_what_they_read() {
    let mut doubling = String::from("B0 = b\"A\"\n");
    for i in 1..=19 {
        doubling.push_str(&format!("B{i} = B{p} + B{p}\n", p = i - 1));
    }
    // A list search reads its needle once per item: 2^19 + 1 bytes times 4,096 items is past
    // 2^30 at once, though both constants are within their limits.
    let items = vec!["0"; 4_096].join(", ");
    let search = format!("{doubling}L = [{items}]\nX = B19 in L\n\ndef main():\n    pass\n");
    assert_eq!(codes(&search), vec![(StarCode::NotLowerable, 22)]);
    // Each slice reads the whole constant before it copies, even once the constant budget is
    // spent: 2,100 slices of 2^19 bytes pass 2^30.
    let mut slices = doubling.clone();
    for i in 0..2_100 {
        slices.push_str(&format!("S{i} = B19[1:-1]\n"));
    }
    slices.push_str("\ndef main():\n    pass\n");
    let errors = lowered(&slices).unwrap_err();
    let folding = errors
        .iter()
        .find(|e| e.hint.contains("folding"))
        .expect("the folding budget is reported");
    // B0 to B19 read just over 2^20; each slice line from 21 on reads just over 2^19.
    assert!((2_060..=2_070).contains(&folding.line), "{}", folding.line);
}

#[test]
fn a_deeply_nested_procedure_lowers_on_the_parser_stack() {
    // A chain of 400 `+`, which nests one level per link, near the expression bound (subset
    // 2.8): walking it recursively needs more than a test thread's stack, so the lowering runs
    // on the parser's.
    let depth = 400;
    let expr = format!("x{}", " + x".repeat(depth));
    let source = format!(
        "def f(x):\n    return {expr}\n\ndef main():\n    if f(1) == {}:\n        diag.fail(\"YES\", \"\")\n",
        depth + 1
    );
    // Called from a thread with a small stack: only the parser's thread can take the walk.
    let lowered = std::thread::Builder::new()
        .stack_size(128 << 10)
        .spawn(move || lowered(&source).is_ok())
        .unwrap()
        .join()
        .unwrap();
    assert!(lowered);
}

#[test]
fn a_left_shift_at_run_time_keeps_every_bit_or_stops_the_run() {
    assert!(holds("x, n", "3, 4", "x << n == 48"));
    assert!(holds("x, n", "-3, 4", "x << n == -48"));
    assert!(holds("x, n", "0, 63", "x << n == 0"));
    assert!(holds(
        "x, n",
        "-1, 63",
        "x << n == -4611686018427387904 * 2"
    ));
    assert!(holds("x", "1", "x << 62 == 4611686018427387904"));
    // A bit lost, or a count outside 0 to 63, stops the run where Starlark would grow or fail.
    for (args, expr, error) in [
        ("1, 63", "x << n", "Overflow"),
        ("3, 62", "x << n", "Overflow"),
        ("1, 64", "x << n", "ShiftOutOfRange(64)"),
        ("1, -1", "x << n", "ShiftOutOfRange(-1)"),
    ] {
        let source = format!(
            "def f(x, n):\n    return {expr}\n\ndef main():\n    if f({args}) == 0:\n        diag.fail(\"ZERO\", \"\")\n"
        );
        let found = run(&source).expect_err(&source);
        assert!(found.contains(error), "{source}: {found}");
    }
    let augmented = "def f(x):\n    x <<= 2\n    return x\n\ndef main():\n    if f(5) != 20:\n        diag.fail(\"NO\", \"\")\n";
    assert_eq!(run(augmented), Ok(None));
}

#[test]
fn a_left_shift_at_run_time_matches_the_folded_one() {
    // The literal i64::MIN is out of range in Starlark source, so it is written as a sum.
    let literal = |v: i64| {
        if v == i64::MIN {
            format!("({} - 1)", v + 1)
        } else {
            format!("({v})")
        }
    };
    for (x, n) in [
        (0, 63),
        (-1, 63),
        (i64::MIN, 0),
        (i64::MIN, 1),
        (1, 63),
        (-2, 63),
        (3, 62),
        (0, 64),
        (1, -1),
        (i64::MAX >> 1, 1),
    ] {
        let source = format!(
            "def f(x, n):\n    return x << n\n\ndef main():\n    f({}, {})\n",
            literal(x),
            literal(n)
        );
        match fold(BinOp::LeftShift, &Const::Int(x), &Const::Int(n)) {
            Ok(Const::Int(v)) => assert!(
                holds(
                    "x, n",
                    &format!("{}, {}", literal(x), literal(n)),
                    &format!("x << n == {}", literal(v))
                ),
                "{x} << {n}"
            ),
            Ok(other) => panic!("{other:?}"),
            Err(_) => assert!(run(&source).is_err(), "{x} << {n}"),
        }
    }
}

#[test]
fn a_right_shift_is_arithmetic() {
    assert!(holds("x", "-8", "x >> 1 == -4"));
    assert!(holds("x", "-1", "x >> 63 == -1"));
    assert!(holds("x", "8", "x >> 2 == 2"));
}

#[test]
fn and_and_or_give_the_deciding_value() {
    assert!(holds("x", "0", "(x or 5) == 5"));
    assert!(holds("x", "3", "(x or 5) == 3"));
    assert!(holds("x", "0", "(x and 5) == 0"));
    assert!(holds("x", "3", "(x and 5) == 5"));
    assert!(holds("x", "3", "not (x == 4)"));
}

#[test]
fn comparisons_cover_both_numeric_types() {
    assert!(holds(
        "a, b",
        "1, 2",
        "a < b and a <= b and not (a > b) and not (a >= b) and a != b"
    ));
    assert!(holds(
        "a, b",
        "2.5, 2.5",
        "a <= b and a >= b and not (a < b)"
    ));
}

/// `n` is NaN at run time: infinity minus infinity.
const NAN: &str = "1e308 * 10.0 - 1e308 * 10.0";

#[test]
fn floats_are_totally_ordered_with_nan_greatest() {
    // NaN equals NaN and is greater than every other float, at run time and when folded.
    for (params, args) in [
        ("n, x", format!("{NAN}, 1.0")),
        ("n, x", format!("{NAN}, 1e308 * 10.0")),
    ] {
        assert!(holds(
            params,
            &args,
            "n == n and not (n != n) and n <= n and n >= n"
        ));
        assert!(holds(
            params,
            &args,
            "n > x and n >= x and not (n < x) and not (n <= x)"
        ));
        assert!(holds(
            params,
            &args,
            "x < n and x <= n and not (x > n) and x != n"
        ));
    }
    assert!(holds(
        "x, y",
        "1.0, 2.0",
        "x != y and not (x == y) and x < y"
    ));
    let folded = format!(
        "N = {NAN}\n\ndef main():\n    if N == N and N > 1.0 and max(1.0, N) == N and min(N, 1.0) == 1.0:\n        diag.fail(\"YES\", \"\")\n"
    );
    assert_eq!(failed_with(&folded), "YES");
}

#[test]
fn a_float_division_by_zero_is_an_error() {
    let result = run(
        "def r(a, b):\n    return a / b\n\ndef main():\n    if r(1.0, -0.0) > 1.0:\n        diag.fail(\"YES\", \"\")\n",
    );
    assert!(
        result.as_ref().is_err_and(|e| e.contains("DivisionByZero")),
        "{result:?}"
    );
    assert!(holds("a, b", "1.0, 4.0", "a / b == 0.25"));
}

#[test]
fn values_of_different_types_are_unequal() {
    assert!(holds("a, b", "1, \"1\"", "a != b"));
    assert!(holds("a, b", "True, 1", "not (a == b)"));
}

#[test]
fn if_elif_else_and_conditional_expressions() {
    let source = "\
def grade(n):
    if n > 10:
        return \"BIG\"
    elif n > 5:
        return \"MID\"
    else:
        return \"SMALL\" if n > 0 else \"NONE\"

def main():
    diag.fail(\"G\", grade(7))
";
    assert_eq!(run(source), Ok(Some(("G".into(), "MID".into()))));
}

#[test]
fn unary_operators_and_augmented_assignment() {
    assert!(holds("x", "5", "-x == -5 and ~x == -6 and +x == 5"));
    let source = "\
def f(x):
    y = x
    y += 3
    y *= 2
    y //= 3
    return y

def main():
    if f(4) == 4:
        diag.fail(\"OK\", \"\")
";
    assert_eq!(failed_with(source), "OK");
}

#[test]
fn bytes_are_indexed_at_run_time() {
    assert!(holds("b, i", "b\"\\x01\\x02\\x03\", 2", "b[i] == 3"));
    // An index past the end is an error at run time.
    // So is a negative index known only at run time (subset 4.8).
    for index in ["5", "-1"] {
        let result = run(&format!(
            "def f(b, i):\n    return b[i]\n\ndef main():\n    f(b\"\\x01\", {index})\n"
        ));
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("IndexOutOfRange")),
            "{index}: {result:?}"
        );
    }
}

/// `read_uint` of the bytes `0x11 0x22 ... 0x77` at `offset`, in a function whose operands are
/// known only at run time.
fn read_uint_holds(offset: i64, bits: u32, expect: i64) -> bool {
    holds(
        "b, o",
        &format!("b\"\\x11\\x22\\x33\\x44\\x55\\x66\\x77\", {offset}"),
        &format!("read_uint(b, o, {bits}) == {expect}"),
    )
}

#[test]
fn read_uint_reads_big_endian_at_run_time() {
    for (bits, expect) in [
        (8, 0x11),
        (16, 0x1122),
        (24, 0x112233),
        (32, 0x11223344),
        (40, 0x1122334455),
        (48, 0x112233445566),
        (56, 0x11223344556677),
    ] {
        assert!(read_uint_holds(0, bits, expect), "{bits} bits");
    }
    assert!(read_uint_holds(2, 16, 0x3344));
    // The last byte, and the last field that fits.
    assert!(read_uint_holds(6, 8, 0x77));
    assert!(read_uint_holds(5, 16, 0x6677));
    // A high bit stays a non-negative value.
    assert!(holds(
        "b, o",
        "b\"\\xff\\xff\\xff\\xff\\xff\\xff\\xff\", 0",
        "read_uint(b, o, 56) == 72057594037927935"
    ));
}

#[test]
fn read_uint_past_the_end_or_before_the_start_is_an_error() {
    for (offset, bits) in [
        (7, 8),
        (6, 16),
        (1000, 8),
        (-1, 8),
        (-3, 16),
        (i64::MAX, 16),
    ] {
        let result = run(&format!(
            "def f(b, o):\n    return read_uint(b, o, {bits})\n\ndef main():\n    f(b\"\\x11\\x22\\x33\\x44\\x55\\x66\\x77\", {offset})\n"
        ));
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("IndexOutOfRange")),
            "{offset}, {bits}: {result:?}"
        );
    }
}

#[test]
fn read_uint_takes_its_operands_from_any_expression() {
    // Bytes built at run time, and a computed offset.
    assert!(holds(
        "a, b",
        "b\"\\x01\\x02\", b\"\\x03\\x04\"",
        "read_uint(a + b, len(a), 16) == 0x0304"
    ));
}

#[test]
fn write_uint_writes_big_endian_at_run_time() {
    for bits in [8, 16, 24, 32, 40, 48, 56] {
        let width = bits / 8;
        let value: u64 = (1..=width).fold(0, |acc, i| (acc << 8) | (0xa0 + i));
        // Eight bytes, the field at offset 1 and zeros after it.
        let mut expect = String::from("\\x00");
        for i in 0..7 {
            expect.push_str(&format!("\\x{:02x}", if i < width { 0xa1 + i } else { 0 }));
        }
        assert!(
            holds(
                "b, o, v",
                &format!("b\"\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\", 1, {value}"),
                &format!("write_uint(b, o, {bits}, v) == b\"{expect}\"")
            ),
            "{bits} bits"
        );
    }
    // The bytes around the field are kept, the field ends at the last byte, and the operand's
    // value is not changed.
    assert!(holds(
        "b, o",
        "b\"\\x11\\x22\\x33\\x44\", 2",
        "write_uint(b, o, 16, 0xbeef) == b\"\\x11\\x22\\xbe\\xef\""
    ));
    assert!(holds(
        "b, o",
        "b\"\\x11\\x22\\x33\\x44\", 0",
        "write_uint(b, o, 8, 0) == b\"\\x00\\x22\\x33\\x44\""
    ));
    assert!(holds(
        "b, o",
        "b\"\\x11\\x22\", 0",
        "read_uint(write_uint(b, o, 16, 0x0102), 0, 16) == 0x0102 and b == b\"\\x11\\x22\""
    ));
    // The widest value of a width.
    assert!(holds(
        "b, v",
        "b\"\\x00\\x00\", 65535",
        "write_uint(b, 0, 16, v) == b\"\\xff\\xff\""
    ));
}

#[test]
fn write_uint_refuses_a_value_that_does_not_fit_and_an_offset_past_the_end() {
    for value in [
        "65536",
        "-1",
        "9223372036854775807",
        "-9223372036854775807 - 1",
    ] {
        let result = run(&format!(
            "def f(b, o, v):\n    return write_uint(b, o, 16, v)\n\ndef main():\n    f(b\"\\x00\\x00\", 0, {value})\n"
        ));
        let Ok(Some((code, detail))) = &result else {
            panic!("{value}: {result:?}")
        };
        assert_eq!(code, "UINT_OUT_OF_RANGE", "{value}");
        assert!(detail.contains("16 bits"), "{detail}");
    }
    for (offset, bits) in [(1, 16), (2, 8), (-1, 8)] {
        let result = run(&format!(
            "def f(b, o):\n    return write_uint(b, o, {bits}, 1)\n\ndef main():\n    f(b\"\\x00\\x00\", {offset})\n"
        ));
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("IndexOutOfRange")),
            "{offset}, {bits}: {result:?}"
        );
    }
}

#[test]
fn read_uint_and_write_uint_fold_on_constants() {
    let source = "\
B = b\"\\x01\\x02\\x03\"
R = read_uint(B, 1, 16)
W = write_uint(B, 1, 16, 0xaabb)

def main():
    if R == 0x0203 and W == b\"\\x01\\xaa\\xbb\":
        diag.fail(\"OK\", \"\")
";
    assert_eq!(failed_with(source), "OK");
    // A constant past the end is reported at ingestion.
    for call in ["read_uint(B, 2, 16)", "write_uint(B, 0, 8, 256)"] {
        let found = codes(&format!(
            "B = b\"\\x01\\x02\\x03\"\nX = {call}\n\ndef main():\n    pass\n"
        ));
        assert_eq!(found, vec![(StarCode::NotLowerable, 2)], "{call}");
    }
}

#[test]
fn the_width_of_read_uint_and_write_uint_is_a_constant_multiple_of_8_up_to_56() {
    for bits in ["0", "7", "12", "64", "-8", "True"] {
        let found = codes(&format!(
            "def f(b):\n    return read_uint(b, 0, {bits})\n\ndef main():\n    f(b\"\\x01\")\n"
        ));
        assert_eq!(found, vec![(StarCode::ArgumentOutOfRange, 2)], "{bits}");
    }
    let found = codes(
        "def f(b, n):\n    return write_uint(b, 0, n, 1)\n\ndef main():\n    f(b\"\\x01\", 8)\n",
    );
    assert_eq!(found, vec![(StarCode::NonConstantArgument, 2)]);
    let found = codes("def f(b):\n    return read_uint(b, 0)\n\ndef main():\n    f(b\"\\x01\")\n");
    assert_eq!(found, vec![(StarCode::UnsupportedSyntax, 2)]);
    // The operands have their types.
    let found = codes("def f(b):\n    return read_uint(b, 0, 8)\n\ndef main():\n    f(1)\n");
    assert_eq!(found, vec![(StarCode::NotLowerable, 2)]);
}

#[test]
fn len_of_bytes_known_at_run_time_lowers() {
    assert!(holds("b", "b\"\\x01\\x02\\x03\"", "len(b) == 3"));
    assert!(holds("a, b", "b\"\\x01\", b\"\\x02\"", "len(a + b) == 2"));
}

#[test]
fn a_built_in_that_does_not_lower_names_the_instruction_it_waits_for() {
    let cases = [
        (
            "def f(x):\n    return abs(x)\n",
            "1",
            "absolute-value instruction",
        ),
        (
            "def f(x):\n    return min(x, 2)\n",
            "1",
            "minimum and maximum instruction",
        ),
        (
            "def f(x):\n    return max(x, 2)\n",
            "1",
            "minimum and maximum instruction",
        ),
        (
            "def f(x):\n    return str(x)\n",
            "1",
            "integer-to-text conversion instruction",
        ),
        (
            "def f(x):\n    return bool(x)\n",
            "1",
            "truth-conversion instruction",
        ),
        (
            "def f(x):\n    return to_hex(x)\n",
            "b\"a\"",
            "bytes-to-hex-text instruction",
        ),
        (
            "def f(x):\n    return from_hex(x)\n",
            "\"61\"",
            "hex-text-to-bytes instruction",
        ),
        (
            "def f(x):\n    return len(x)\n",
            "\"a\"",
            "character-count instruction",
        ),
        (
            "def f(x):\n    return x.find(\"a\")\n",
            "\"a\"",
            "string-search instruction",
        ),
        (
            "def f(x):\n    return x.upper()\n",
            "\"a\"",
            "case-mapping instruction",
        ),
        (
            "def f(x):\n    return x.strip()\n",
            "\"a\"",
            "trimming instruction",
        ),
        (
            "def f(x):\n    return x.startswith(\"a\")\n",
            "\"a\"",
            "prefix and suffix comparison",
        ),
        (
            "def f(x):\n    return x.format(1)\n",
            "\"a\"",
            "text-formatting instruction",
        ),
        (
            "def f(x):\n    return x.join([\"a\"])\n",
            "\"a\"",
            "text-join instruction",
        ),
    ];
    for (function, arg, waits) in cases {
        let source = format!("{function}\ndef main():\n    f({arg})\n");
        let checked = check("t.star", &source).unwrap_or_else(|e| panic!("{e:#?}"));
        let errors = lower_for_a_variant(&checked).expect_err(&source);
        assert!(
            errors
                .iter()
                .all(|e| e.code == StarCode::NotLowerable && e.hint.contains(waits)),
            "{source}: {errors:#?}"
        );
        assert!(!errors.is_empty(), "{source}");
    }
}

#[test]
fn constants_are_folded_at_ingestion() {
    let source = "\
LIMIT = 1 << 4
NAMES = [\"a\", \"b\", \"c\"]
TABLE = {\"x\": 7 // 2, \"y\": -7 % 3}
CODE = \"C\" + \"ODE\"

def main():
    if LIMIT == 16 and NAMES[-1] == \"c\" and TABLE[\"x\"] == 3 and TABLE[\"y\"] == 2 and len(NAMES) == 3:
        diag.fail(CODE, \"folded\")
";
    let program = lowered(source).unwrap();
    // Everything folds: the condition is known, so no comparison is left in the code.
    assert!(
        !program
            .code
            .iter()
            .any(|op| matches!(op, Op::CmpEq | Op::JumpIfFalse(_)))
    );
    assert_eq!(run(source), Ok(Some(("CODE".into(), "folded".into()))));
}

#[test]
fn a_source_map_entry_starts_each_run_of_one_position() {
    let program = lowered("def main():\n    x = 1\n    diag.fail(\"E\", \"d\")\n").unwrap();
    assert_eq!(program.source_map.first().map(|s| s.pc), Some(0));
    assert!(program.source_map.windows(2).all(|w| w[0].pc < w[1].pc));
    assert!(program.source_map.iter().any(|s| s.line == 3));
}

#[test]
fn bytes_and_strings_join_at_run_time() {
    assert!(holds(
        "a, b",
        "b\"\\x01\", b\"\\x02\\x03\"",
        "a + b == b\"\\x01\\x02\\x03\""
    ));
    assert!(holds("a, b", "\"x\", \"yz\"", "a + b == \"xyz\""));
    assert!(holds("a", "b\"\"", "a + a == b\"\""));
    // A constant joins a run-time value as well.
    assert!(holds("a", "\"y\"", "\"x\" + a == \"xy\""));
}

#[test]
fn bytes_slice_at_run_time_as_at_ingestion() {
    // Each slice of a run-time value gives what the same slice of the constant gives.
    let b = "b\"\\x0a\\x0b\\x0c\\x0d\\x0e\"";
    for slice in [
        "[1:3]", "[:2]", "[3:]", "[:]", "[-2:]", "[1:-1]", "[-100:2]", "[3:100]", "[4:2]",
    ] {
        assert!(holds("x", b, &format!("x{slice} == {b}{slice}")), "{slice}");
    }
    // Bounds known only at run time.
    assert!(holds(
        "x, i, j",
        &format!("{b}, 1, -1"),
        &format!("x[i:j] == {b}[1:-1]")
    ));
    assert!(holds("i", "2", &format!("{b}[i:] == {b}[2:]")));
}

#[test]
fn ints_and_floats_mix_at_run_time() {
    // An int with a float is computed in floats, either side.
    assert!(holds("a, b", "1, 0.5", "a + b == 1.5"));
    assert!(holds("a, b", "1, 0.5", "b - a == -0.5"));
    assert!(holds("a, b", "3, 0.5", "a * b == 1.5"));
    // `/` divides in floats, also for two ints.
    assert!(holds("a, b", "7, 2", "a / b == 3.5"));
    assert!(holds("a, b", "1, 0.5", "a / b == 2.0"));
    assert!(holds("a, b", "1.0, 4", "a / b == 0.25"));
    let divide_by_zero = "def f(a, b):\n    return a / b\n\ndef main():\n    f(1, 0)\n";
    assert!(run(divide_by_zero).is_err_and(|e| e.contains("DivisionByZero")));
    // `int` and `float` of numbers known only at run time.
    assert!(holds("x", "2.9", "int(x) == 2"));
    assert!(holds("x", "-2.9", "int(x) == -2"));
    assert!(holds("x", "3", "float(x) == 3.0"));
    assert!(holds("x", "3", "int(x) == 3"));
    assert!(holds("x", "0.5", "float(x) == 0.5"));
    let infinite = "def f(x):\n    return int(x * x)\n\ndef main():\n    f(1e200)\n";
    assert!(run(infinite).is_err_and(|e| e.contains("FloatNotConvertible")));
}

#[test]
fn what_the_ir_cannot_express_is_not_lowerable() {
    let cases = [
        // An int compared with a float at run time.
        "def f(x):\n    return x == 1.0\n\ndef main():\n    f(1)\n",
        // Float floor division at run time.
        "def f(x):\n    return x // 2.0\n\ndef main():\n    f(1.0)\n",
        // `int` of a string at run time.
        "def f(x):\n    return int(x)\n\ndef main():\n    f(\"1\")\n",
        // Bytes and a string joined at run time.
        "def f(x):\n    return x + \"a\"\n\ndef main():\n    f(b\"b\")\n",
        // A local of two types.
        "def f(x):\n    y = 1\n    if x:\n        y = 1.0\n    return y + y\n\ndef main():\n    f(True)\n",
        // The truth of bytes at run time.
        "def f(x):\n    if x:\n        pass\n\ndef main():\n    f(b\"\")\n",
        // A list that would exist at run time.
        "def main():\n    x = [1, 2]\n",
        // A built-in on a run-time value.
        "def f(x):\n    return abs(x)\n\ndef main():\n    f(1)\n",
        // A string sliced at run time, and a slice with a step.
        "def f(x):\n    return x[1:]\n\ndef main():\n    f(\"ab\")\n",
        "def f(x):\n    return x[::2]\n\ndef main():\n    f(b\"ab\")\n",
        // A loop over bytes known only at run time.
        "def f(b):\n    for x in b:\n        pass\n\ndef main():\n    f(b\"\")\n",
    ];
    for source in cases {
        let found = codes(source);
        assert!(
            !found.is_empty()
                && found
                    .iter()
                    .all(|(code, _)| *code == StarCode::NotLowerable),
            "{source}: {found:?}"
        );
    }
}

#[test]
fn a_protocol_neutral_call_needs_its_profile_to_define_it() {
    let checked = |source: &str| check("t.star", source).unwrap_or_else(|e| panic!("{e:#?}"));
    let bytes = empty_document();
    let variant = load_ecu_document(&bytes)
        .expect("the document loads")
        .variant()
        .expect("a variant");
    let codes_over = |profile: &'static Profile, source: &str| -> Vec<(StarCode, u32)> {
        let requests = RequestIndex {
            services: Vec::new(),
            profile,
            ..RequestIndex::default()
        };
        match lower("t.star", &checked(source), variant, &requests) {
            Ok(_) => Vec::new(),
            Err(errors) => errors.iter().map(|e| (e.code, e.line)).collect(),
        }
    };
    // A profile that defines no call, as one for another application layer might leave some out.
    static NONE: Profile = Profile {
        layer: "TEST",
        calls: &[],
        negative: crate::profile::NEGATIVE_7F,
        pending: &[0x78],
    };
    let source = "def main():\n    diag.read_dtc(0xFF)\n    diag.routine(0x0203, 1, {})\n    \
                  diag.security_access(1)\n";
    assert_eq!(
        codes_over(&NONE, source),
        [
            (StarCode::NotInProfile, 2),
            (StarCode::NotInProfile, 3),
            (StarCode::NotInProfile, 4)
        ]
    );
    // A layer other than UDS sends a routine with the generic send-receive, which only UDS's
    // `RoutineControl` lowering covers yet (ADR-309 item 4).
    static SEND_RECEIVE_ROUTINE: Profile = Profile {
        layer: "TEST",
        calls: &[crate::profile::Call {
            intent: Intent::Routine,
            steps: &[crate::profile::Step {
                kind: crate::profile::StepKind::SendReceive,
                request: crate::profile::Request {
                    sid: 0x31,
                    parts: &[
                        crate::profile::Part::Select {
                            bits: 8,
                            arg: crate::profile::Source::Sub,
                        },
                        crate::profile::Part::Select {
                            bits: 16,
                            arg: crate::profile::Source::Rid,
                        },
                        crate::profile::Part::Params,
                    ],
                },
                declared: true,
                result: crate::profile::ResultMap::Declared,
            }],
        }],
        negative: crate::profile::NEGATIVE_7F,
        pending: &[0x78],
    };
    assert_eq!(SEND_RECEIVE_ROUTINE.validate(), Ok(()));
    assert_eq!(
        codes_over(
            &SEND_RECEIVE_ROUTINE,
            "def main():\n    diag.routine(0x0203, 1, {})\n"
        ),
        [(StarCode::NotLowerable, 2)]
    );
    // UDS defines all three. `diag.read_dtc` and `diag.routine` need a declared service,
    // which this variant lacks; `diag.security_access` needs none.
    assert_eq!(
        codes_over(&crate::profile::UDS_PROFILE, source),
        [(StarCode::UnknownService, 2), (StarCode::UnknownService, 3)]
    );
}

#[test]
fn a_fail_code_is_a_short_string_constant() {
    assert_eq!(
        codes("def f(c):\n    diag.fail(c, \"\")\n\ndef main():\n    f(\"X\")\n"),
        [(StarCode::NonConstantArgument, 2)]
    );
    let long = "C".repeat(65);
    assert_eq!(
        codes(&format!("def main():\n    diag.fail(\"{long}\", \"\")\n")),
        [(StarCode::ArgumentOutOfRange, 2)]
    );
}

#[test]
fn none_cannot_reach_run_time() {
    // A function that returns a value on one path only.
    assert_eq!(
        codes("def f(x):\n    if x:\n        return 1\n\ndef main():\n    f(True)\n"),
        [(StarCode::UnsupportedSyntax, 1)]
    );
    // A call to a function without a value, used as one.
    assert_eq!(
        codes("def g():\n    pass\n\ndef main():\n    x = g()\n"),
        [(StarCode::UnsupportedSyntax, 5)]
    );
}

#[test]
fn every_problem_is_reported() {
    let found = codes("def main():\n    y = [2]\n    diag.ecu_info()\n    x = [1]\n");
    assert_eq!(
        found,
        [
            (StarCode::NotLowerable, 2),
            (StarCode::NotLowerable, 3),
            (StarCode::NotLowerable, 4)
        ]
    );
}

#[test]
fn a_function_main_does_not_reach_is_left_out() {
    // `unused` has no caller, so nothing gives its parameter a type, and it never runs.
    let source = "def unused(x):\n    return x + 1\n\ndef main():\n    pass\n";
    let program = lowered(source).unwrap();
    assert!(!program.code.iter().any(|op| matches!(op, Op::Add)));
    assert_eq!(run(source), Ok(None));
}

#[test]
fn calls_from_an_unreached_function_give_no_types() {
    // `unused` would make `f`'s parameter a string as well as an int.
    let source = "\
def f(x):
    return x + 1

def unused():
    f(\"s\")

def main():
    if f(1) == 2:
        diag.fail(\"OK\", \"\")
";
    assert_eq!(failed_with(source), "OK");
}

/// A host that records the calls procedures make, and reports a wait as done only from its
/// `waits_pending`-th inquiry on. `ServiceRequest`s are answered from `responses` and keys from
/// `keys` in order, a key only from its `keys_pending`-th inquiry on.
#[derive(Default)]
struct ScriptedHost {
    calls: Vec<String>,
    waits_pending: u32,
    responses: std::collections::VecDeque<Vec<u8>>,
    keys: std::collections::VecDeque<Vec<u8>>,
    keys_pending: u32,
    /// The values `Op::ReadInput` reads; an input not listed is not established.
    inputs: Vec<(RuntimeInput, i64)>,
    /// The flash artifact `Op::FlashBlock` reads from.
    artifact: Vec<u8>,
    /// The answers of `hmi_request` and `record_input`, in order, each given from the
    /// `hmi_pending`-th inquiry on: the host answers `Ok(None)` before that.
    answers: std::collections::VecDeque<Vec<u8>>,
    hmi_pending: u32,
    /// Every `hmi_request` (`"hmi"`) and `record_input` (`"record"`) call, polls included:
    /// the inquiry number and the bytes sent.
    sent: Vec<(&'static str, u64, Vec<u8>)>,
}

impl ScriptedHost {
    fn ask(
        &mut self,
        kind: &'static str,
        inquiry: u64,
        bytes: &[u8],
    ) -> Result<Option<Vec<u8>>, NoHostError> {
        self.sent.push((kind, inquiry, bytes.to_vec()));
        if self.hmi_pending > 0 {
            self.hmi_pending -= 1;
            return Ok(None);
        }
        Ok(Some(self.answers.pop_front().expect("a scripted answer")))
    }

    /// A host that answers the HMI inquiries with `answers`.
    fn answering(answers: &[Vec<u8>]) -> Self {
        ScriptedHost {
            answers: answers.iter().cloned().collect(),
            ..ScriptedHost::default()
        }
    }

    /// The forms sent, decoded, polls included.
    fn forms(&self) -> Vec<(&'static str, HmiForm)> {
        self.sent
            .iter()
            .map(|(kind, _, bytes)| (*kind, decode_form(bytes).expect("a form")))
            .collect()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

impl DiagHost for ScriptedHost {
    type Error = NoHostError;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, NoHostError> {
        self.calls
            .push(format!("request {service:02X} {}", hex(payload)));
        Ok(self.responses.pop_front().expect("a scripted response"))
    }
    fn send_receive(&mut self, _: u32, _: &[u8]) -> Result<Vec<u8>, NoHostError> {
        unreachable!("not lowered yet")
    }
    fn read_dtc(&mut self, _: u8) -> Result<Vec<u8>, NoHostError> {
        unreachable!("not lowered yet")
    }
    fn routine_control(&mut self, _: u16, _: u8, _: &[u8]) -> Result<Vec<u8>, NoHostError> {
        unreachable!("not lowered yet")
    }
    fn security_access(
        &mut self,
        _: u64,
        level: u8,
        seed: &[u8],
    ) -> Result<Option<Vec<u8>>, NoHostError> {
        if self.keys_pending > 0 {
            self.keys_pending -= 1;
            return Ok(None);
        }
        self.calls.push(format!("key {level:02X} {}", hex(seed)));
        Ok(Some(self.keys.pop_front().expect("a scripted key")))
    }
    fn flash_transfer(&mut self, block: u32, data: &[u8]) -> Result<(), NoHostError> {
        self.calls.push(format!("transfer {block} {}", hex(data)));
        Ok(())
    }
    fn wait(&mut self, _: u64, millis: u32) -> Result<bool, NoHostError> {
        if self.waits_pending > 0 {
            self.waits_pending -= 1;
            return Ok(false);
        }
        self.calls.push(format!("wait {millis}"));
        Ok(true)
    }
    fn hmi_request(&mut self, inquiry: u64, form: &[u8]) -> Result<Option<Vec<u8>>, NoHostError> {
        self.ask("hmi", inquiry, form)
    }
    fn record_input(
        &mut self,
        inquiry: u64,
        template: &[u8],
    ) -> Result<Option<Vec<u8>>, NoHostError> {
        self.ask("record", inquiry, template)
    }
    fn monitor_capture(&mut self, back_millis: u32) -> Result<(), NoHostError> {
        self.calls.push(format!("capture {back_millis}"));
        Ok(())
    }
    fn artifact_bytes(&mut self, offset: u64, len: u32) -> Result<Vec<u8>, NoHostError> {
        self.calls.push(format!("artifact {offset} {len}"));
        let from = usize::try_from(offset).expect("a small offset");
        Ok(self.artifact[from..from + len as usize].to_vec())
    }
    fn runtime_input(&mut self, input: RuntimeInput) -> Result<i64, NoHostError> {
        self.calls.push(format!("input {input:?}"));
        self.inputs
            .iter()
            .find(|(set, _)| *set == input)
            .map(|&(_, value)| value)
            .ok_or(NoHostError)
    }
    fn log(&mut self, level: u8, message: &str) {
        self.calls.push(format!("log {level} {message}"));
    }
}

/// Runs `source` against `host` to its end, counting the steps that waited, for a timer or a
/// key.
fn run_on(source: &str, host: &mut ScriptedHost) -> u32 {
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let mut vm = Vm::new(&program);
    let mut waited = 0;
    for _ in 0..10_000 {
        match vm.step(&program, host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => return waited,
            Ok(StepOutcome::Waiting(WaitingOn::Timer | WaitingOn::SeedKey | WaitingOn::Hmi)) => {
                waited += 1
            }
            other => panic!("{other:?}"),
        }
    }
    panic!("the procedure did not end")
}

/// Logs what a `diag.security_access(0x01)` result holds.
const SECURITY_ACCESS: &str = "\
def main():
    res = diag.security_access(0x01)
    if res[\"ok\"]:
        diag.log(1, \"unlocked\")
    else:
        diag.log(1, \"refused\")
    if res[\"nrc\"] == 0x35:
        diag.log(1, \"invalid key\")
    if res[\"nrc\"] == 0x22:
        diag.log(1, \"conditions not correct\")
    if res[\"raw\"] == b\"\\x67\\x02\":
        diag.log(1, \"key response\")
";

fn security_access_host(responses: &[&[u8]], keys: &[&[u8]]) -> ScriptedHost {
    ScriptedHost {
        responses: responses.iter().map(|r| r.to_vec()).collect(),
        keys: keys.iter().map(|k| k.to_vec()).collect(),
        ..ScriptedHost::default()
    }
}

#[test]
fn security_access_sends_the_seed_request_the_key_step_and_the_key() {
    let mut host = security_access_host(
        &[&[0x67, 0x01, 0xAA, 0xBB], &[0x67, 0x02]],
        &[&[0x12, 0x34]],
    );
    host.keys_pending = 2;
    // The VM waits on the key until the host has it, then goes on.
    assert_eq!(run_on(SECURITY_ACCESS, &mut host), 2);
    assert_eq!(
        host.calls,
        [
            "request 27 01",
            "key 01 AABB",
            "request 27 021234",
            "log 1 unlocked",
            "log 1 key response"
        ]
    );
    // The highest level: its key is sent with 0x7E.
    let mut host = security_access_host(&[&[0x67, 0x7D, 0x01], &[0x67, 0x7E]], &[&[0x01]]);
    run_on("def main():\n    diag.security_access(0x7D)\n", &mut host);
    assert_eq!(
        host.calls,
        ["request 27 7D", "key 7D 01", "request 27 7E01"]
    );
}

/// Runs `source` against `host` until the procedure fails itself, and returns the failure.
fn run_on_until_failed(source: &str, host: &mut ScriptedHost) -> (String, String) {
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let mut vm = Vm::new(&program);
    for _ in 0..10_000 {
        match vm.step(&program, host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Failed(failure)) => {
                let detail = String::from_utf8_lossy(&failure.detail).into_owned();
                return (failure.code, detail);
            }
            other => panic!("{other:?}"),
        }
    }
    panic!("the procedure did not end")
}

/// A seed response with no seed bytes after the level, or shorter than that, is not a zero seed
/// and not a seed: the procedure ends, naming the empty seed, and no key is asked for or sent
/// (ADR-405 item 3).
#[test]
fn an_empty_seed_ends_the_run_naming_the_empty_seed() {
    for response in [&[0x67_u8, 0x01][..], &[0x67], &[]] {
        let mut host = security_access_host(&[response], &[]);
        let (code, detail) = run_on_until_failed(SECURITY_ACCESS, &mut host);
        assert_eq!(code, "SEED_EMPTY", "{response:02X?}");
        assert!(detail.contains("no seed bytes"), "{detail}");
        assert_eq!(host.calls, ["request 27 01"], "{response:02X?}");
    }
}

/// A seed of zero is the server's answer for a level that is already unlocked (ISO 14229-1
/// (2026) 9.4.1): the call ends there, with the seed response as its value.
#[test]
fn a_zero_seed_ends_security_access_as_unlocked_without_a_key() {
    let source = "\
def main():
    res = diag.security_access(0x01)
    if res[\"ok\"]:
        diag.log(1, \"ok\")
    if res[\"nrc\"] == 0:
        diag.log(1, \"no nrc\")
    if res[\"raw\"][1] == 0x01:
        diag.log(1, \"was unlocked\")
    if res[\"raw\"] == b\"\\x67\\x02\":
        diag.log(1, \"key response\")
";
    // No key is scripted: asking for one would panic.
    for seed in [&[0x67_u8, 0x01, 0, 0, 0, 0][..], &[0x67, 0x01, 0]] {
        let mut host = security_access_host(&[seed], &[]);
        run_on(source, &mut host);
        assert_eq!(
            host.calls,
            [
                "request 27 01",
                "log 1 ok",
                "log 1 no nrc",
                "log 1 was unlocked"
            ],
            "{seed:02X?}"
        );
    }
    // The result is a positive one, so a procedure that tests `ok` takes its unlocked branch.
    let mut host = security_access_host(&[&[0x67, 0x01, 0, 0]], &[]);
    run_on(SECURITY_ACCESS, &mut host);
    assert_eq!(host.calls, ["request 27 01", "log 1 unlocked"]);
    // The last standard seed level just below the supplier-specific range (9.4.2.2).
    let mut host = security_access_host(&[&[0x67, 0x5F, 0]], &[]);
    run_on("def main():\n    diag.security_access(0x5F)\n", &mut host);
    assert_eq!(host.calls, ["request 27 5F"]);
}

/// On the supplier-specific levels (0x61 to 0x7D) the standard defines no seed and key pairing,
/// so the key step runs whatever the seed, a zero or an empty one included (ADR-405).
#[test]
fn the_supplier_specific_levels_send_the_key_whatever_the_seed() {
    for (level, seed, shown) in [
        (0x61_u8, &[0_u8, 0][..], "0000"),
        (0x7D, &[0, 0], "0000"),
        (0x7D, &[], ""),
    ] {
        let mut response = vec![0x67, level];
        response.extend_from_slice(seed);
        let next = [0x67, level + 1];
        let mut host = security_access_host(&[&response, &next], &[&[0x01]]);
        run_on(
            &format!("def main():\n    diag.security_access({level})\n"),
            &mut host,
        );
        assert_eq!(
            host.calls,
            [
                format!("request 27 {level:02X}"),
                format!("key {level:02X} {shown}"),
                format!("request 27 {:02X}01", level + 1)
            ],
            "{level:02X} {seed:02X?}"
        );
    }
}

/// The call's slots hold no seed bytes once it has ended, on the zero path and on the key path
/// (ADR-347, design 8.11): the VM state and its journaled checkpoints would otherwise carry them.
#[test]
fn security_access_leaves_no_seed_or_response_bytes_in_the_vm_state() {
    let source = "def main():\n    diag.security_access(0x01)\n";
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    for (responses, keys) in [
        (vec![vec![0x67, 0x01, 0, 0]], vec![]),
        (
            vec![vec![0x67, 0x01, 0xAA, 0xBB], vec![0x67, 0x02]],
            vec![vec![0x12]],
        ),
    ] {
        let mut host = ScriptedHost {
            responses: responses.into(),
            keys: keys.into(),
            ..ScriptedHost::default()
        };
        let mut vm = Vm::new(&program);
        loop {
            match vm.step(&program, &mut host) {
                Ok(StepOutcome::Continue) => {}
                Ok(StepOutcome::Finished) => break,
                other => panic!("{other:?}"),
            }
        }
        let held: Vec<_> = vm
            .state
            .locals
            .iter()
            .flatten()
            .filter(|value| matches!(value, Value::Bytes(_)))
            .collect();
        assert!(held.is_empty(), "{held:?}");
    }
}

/// Only an all-zero seed is read as unlocked: a non-zero byte anywhere sends the key.
#[test]
fn a_seed_with_any_non_zero_byte_still_sends_the_key() {
    for (seed, shown) in [
        (&[0x67_u8, 0x01, 0, 0, 1][..], "000001"),
        (&[0x67, 0x01, 1, 0], "0100"),
        (&[0x67, 0x01, 0, 1, 0], "000100"),
        (&[0x67, 0x01, 0xFF], "FF"),
    ] {
        let mut host = security_access_host(&[seed, &[0x67, 0x02]], &[&[0x9A]]);
        run_on("def main():\n    diag.security_access(1)\n", &mut host);
        assert_eq!(
            host.calls,
            [
                "request 27 01".to_owned(),
                format!("key 01 {shown}"),
                "request 27 029A".to_owned()
            ],
            "{seed:02X?}"
        );
    }
}

/// A call that finds the level unlocked starts the next pass from a clean state.
#[test]
fn a_repeated_security_access_after_an_unlock_ends_at_the_zero_seed() {
    let mut host = security_access_host(
        &[&[0x67, 0x01, 0x11], &[0x67, 0x02], &[0x67, 0x01, 0, 0]],
        &[&[0xA1]],
    );
    let source = "\
def main():
    for i in range(2):
        res = diag.security_access(0x01)
        diag.log(1, \"done\")
";
    run_on(source, &mut host);
    assert_eq!(
        host.calls,
        [
            "request 27 01",
            "key 01 11",
            "request 27 02A1",
            "log 1 done",
            "request 27 01",
            "log 1 done"
        ]
    );
}

#[test]
fn security_access_in_a_loop_starts_each_call_from_a_new_seed_request() {
    let mut host = security_access_host(
        &[
            &[0x67, 0x01, 0x11],
            &[0x7F, 0x27, 0x35],
            &[0x67, 0x01, 0x22],
            &[0x67, 0x02],
        ],
        &[&[0xA1], &[0xA2]],
    );
    let source = "\
def main():
    for i in range(2):
        res = diag.security_access(0x01)
        if res[\"ok\"]:
            diag.log(1, \"unlocked\")
";
    run_on(source, &mut host);
    assert_eq!(
        host.calls,
        [
            "request 27 01",
            "key 01 11",
            "request 27 02A1",
            "request 27 01",
            "key 01 22",
            "request 27 02A2",
            "log 1 unlocked"
        ]
    );
}

#[test]
fn a_negative_seed_response_ends_security_access_before_the_key_step() {
    let mut host = security_access_host(&[&[0x7F, 0x27, 0x22]], &[]);
    run_on(SECURITY_ACCESS, &mut host);
    assert_eq!(
        host.calls,
        [
            "request 27 01",
            "log 1 refused",
            "log 1 conditions not correct"
        ]
    );
}

#[test]
fn a_negative_key_response_is_the_security_access_result() {
    let mut host = security_access_host(&[&[0x67, 0x01, 0x5A], &[0x7F, 0x27, 0x35]], &[&[0x99]]);
    run_on(SECURITY_ACCESS, &mut host);
    assert_eq!(
        host.calls,
        [
            "request 27 01",
            "key 01 5A",
            "request 27 0299",
            "log 1 refused",
            "log 1 invalid key"
        ]
    );
}

#[test]
fn security_access_arguments_and_results_are_checked() {
    use StarCode::*;
    let cases = [
        (
            "def main():\n    diag.security_access(2)\n",
            ArgumentOutOfRange,
        ),
        (
            "def main():\n    diag.security_access(0)\n",
            ArgumentOutOfRange,
        ),
        (
            "def main():\n    diag.security_access(0xFF)\n",
            ArgumentOutOfRange,
        ),
        (
            "def f(n):\n    diag.security_access(n)\n\ndef main():\n    f(1)\n",
            NonConstantArgument,
        ),
        (
            "def main():\n    diag.security_access(\"1\")\n",
            UnsupportedSyntax,
        ),
        (
            "def main():\n    diag.security_access(1, 2)\n",
            UnsupportedSyntax,
        ),
        (
            "def main():\n    diag.security_access(0x7F)\n",
            ArgumentOutOfRange,
        ),
        (
            "def main():\n    diag.security_access(0x81)\n",
            ArgumentOutOfRange,
        ),
        // The result has no fields (ADR-314 item 4).
        (
            "def main():\n    res = diag.security_access(1)\n    x = res[\"fields\"][\"key\"]\n",
            NotInProfile,
        ),
        // Its `raw` holds the key response, which no message may be built from (ADR-348 item 6),
        // also through a helper.
        (
            "def main():\n    res = diag.security_access(1)\n    diag.log(1, res[\"raw\"])\n",
            SecretInText,
        ),
        (
            "def key(r):\n    return r[\"raw\"][2:]\n\n\
             def main():\n    diag.fail(\"KEY\", key(diag.security_access(1)))\n",
            SecretInText,
        ),
    ];
    for (source, code) in cases {
        let found = codes(source);
        assert!(
            !found.is_empty() && found.iter().all(|(c, _)| *c == code),
            "{source}: {found:?}"
        );
    }
}

#[test]
fn wait_log_and_capture_reach_the_host_in_order() {
    let source = "\
STEP = 250

def main():
    diag.log(2, \"start\")
    diag.wait(STEP * 6)
    diag.capture(STEP)
    diag.wait(0)
    diag.log(0, \"end \" + \"of run\")
";
    let mut host = ScriptedHost::default();
    assert_eq!(run_on(source, &mut host), 0);
    assert_eq!(
        host.calls,
        [
            "log 2 start",
            "wait 1500",
            "capture 250",
            "wait 0",
            "log 0 end of run"
        ]
    );
    // The full ranges of 3.1 are accepted.
    let edges = "def main():\n    diag.wait(4294967295)\n    diag.capture(4294967295)\n    diag.log(255, \"x\")\n";
    let mut host = ScriptedHost::default();
    run_on(edges, &mut host);
    assert_eq!(
        host.calls,
        ["wait 4294967295", "capture 4294967295", "log 255 x"]
    );
}

/// A `diag.wait` duration computed at run time lowers to `WaitFromStack` and reaches the host
/// with its value; a wait not yet over is stepped again with the operand kept.
#[test]
fn a_wait_with_a_run_time_duration_reaches_the_host() {
    let source = "\
def pause(n):
    diag.wait(n * 2 + 1)

def main():
    pause(5)
    diag.log(1, \"between\")
    pause(0)
    diag.wait(7)
";
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    assert!(program.code.contains(&Op::WaitFromStack));
    let mut host = ScriptedHost {
        waits_pending: 2,
        ..ScriptedHost::default()
    };
    assert_eq!(run_on(source, &mut host), 2);
    assert_eq!(host.calls, ["wait 11", "log 1 between", "wait 1", "wait 7"]);
}

/// A run-time duration outside 0 to `u32::MAX` stops the run before the host is called.
#[test]
fn a_run_time_wait_duration_out_of_range_stops_the_run() {
    for (n, expected) in [
        ("-1", -1_i64),
        ("4294967296", 4_294_967_296),
        ("4294967295 + 5", 4_294_967_300),
    ] {
        let source = format!("def pause(n):\n    diag.wait(n)\n\ndef main():\n    pause({n})\n");
        let program = lowered(&source).unwrap_or_else(|errors| panic!("{errors:#?}"));
        let mut host = ScriptedHost::default();
        let mut vm = Vm::new(&program);
        let error = loop {
            match vm.step(&program, &mut host) {
                Ok(StepOutcome::Continue) => {}
                Err(StepError::Vm(error)) => break error,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(error, VmError::WaitOutOfRange(expected), "{n}");
        assert!(host.calls.is_empty(), "{n}: {:?}", host.calls);
    }
}

/// A `diag.log` message built at run time lowers to `LogFromStack` and reaches the host with
/// its text; a constant message still lowers to `Log`.
#[test]
fn a_log_with_a_run_time_message_reaches_the_host() {
    let source = "\
def say(who):
    diag.log(3, \"hello \" + who)

def main():
    say(\"ecu\")
    diag.log(1, \"constant\")
    say(\"\")
";
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    assert!(program.code.contains(&Op::LogFromStack { level: 3 }));
    assert!(
        program
            .code
            .iter()
            .any(|op| matches!(op, Op::Log { level: 1, .. }))
    );
    let mut host = ScriptedHost::default();
    run_on(source, &mut host);
    assert_eq!(
        host.calls,
        ["log 3 hello ecu", "log 1 constant", "log 3 hello "]
    );
}

#[test]
fn a_wait_not_yet_over_is_stepped_again() {
    let mut host = ScriptedHost {
        waits_pending: 2,
        ..ScriptedHost::default()
    };
    let waited = run_on(
        "def main():\n    diag.wait(10)\n    diag.log(1, \"after\")\n",
        &mut host,
    );
    assert_eq!(waited, 2);
    assert_eq!(host.calls, ["wait 10", "log 1 after"]);
}

#[test]
fn wait_log_and_capture_arguments_are_checked() {
    use StarCode::*;
    let cases = [
        // Known only at run time.
        (
            "def f(n):\n    diag.capture(n)\n\ndef main():\n    f(1)\n",
            NonConstantArgument,
            2,
        ),
        (
            "def f(n):\n    diag.log(n, \"x\")\n\ndef main():\n    f(1)\n",
            NonConstantArgument,
            2,
        ),
        // Outside the ranges of 3.1.
        ("def main():\n    diag.wait(-1)\n", ArgumentOutOfRange, 2),
        (
            "def main():\n    diag.wait(4294967296)\n",
            ArgumentOutOfRange,
            2,
        ),
        ("def main():\n    diag.capture(-5)\n", ArgumentOutOfRange, 2),
        (
            "def main():\n    diag.log(256, \"x\")\n",
            ArgumentOutOfRange,
            2,
        ),
        // The wrong type or number of arguments, also for a duration known only at run time.
        (
            "def f(n):\n    diag.wait(n)\n\ndef main():\n    f(1.5)\n",
            UnsupportedSyntax,
            2,
        ),
        ("def main():\n    diag.wait(1.5)\n", UnsupportedSyntax, 2),
        (
            "def main():\n    diag.log(1, b\"x\")\n",
            UnsupportedSyntax,
            2,
        ),
        (
            "def f(b):\n    diag.log(1, b)\n\ndef main():\n    f(b\"x\")\n",
            UnsupportedSyntax,
            2,
        ),
        (
            "def f(n):\n    diag.log(1, n + 1)\n\ndef main():\n    f(1)\n",
            UnsupportedSyntax,
            2,
        ),
        ("def main():\n    diag.wait()\n", UnsupportedSyntax, 2),
        ("def main():\n    diag.log(1)\n", UnsupportedSyntax, 2),
        // They return nothing.
        ("def main():\n    x = diag.wait(1)\n", UnsupportedSyntax, 2),
    ];
    for (source, code, line) in cases {
        assert_eq!(codes(source), vec![(code, line)], "{source}");
    }
}

#[test]
fn a_message_logged_many_times_is_pooled_once() {
    let mut source = format!("M = \"{}\"\n\ndef main():\n", "x".repeat(1000));
    for _ in 0..50 {
        source.push_str("    diag.log(1, M)\n");
    }
    let program = lowered(&source).unwrap();
    assert_eq!(program.constants.len(), 1);
    let mut host = ScriptedHost::default();
    run_on(&source, &mut host);
    assert_eq!(host.calls.len(), 50);
}

/// Runs `source` against a scripted host to its end: the calls it made, and the failure it
/// ended with, if any.
fn run_logged(source: &str) -> (Vec<String>, Option<(String, String)>) {
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let mut host = ScriptedHost::default();
    let mut vm = Vm::new(&program);
    for _ in 0..1_000_000 {
        match vm.step(&program, &mut host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => return (host.calls, None),
            Ok(StepOutcome::Failed(failure)) => {
                let detail = String::from_utf8_lossy(&failure.detail).into_owned();
                return (host.calls, Some((failure.code, detail)));
            }
            other => panic!("{other:?}"),
        }
    }
    panic!("the procedure did not end")
}

#[test]
fn a_loop_over_a_constant_list_or_dict_is_unrolled_with_its_items_folded() {
    // `diag.log`'s level must be a constant, so each iteration's item must fold.
    let source = "\
LEVELS = [1, 2, 3]
NAMES = {\"a\": 1, \"b\": 2}
TABLE = [[1, \"one\"], [2, \"two\"]]

def main():
    for level in LEVELS:
        diag.log(level, \"level\")
    for name in NAMES:
        diag.log(0, name)
    for row in TABLE:
        diag.log(row[0], row[1])
    for level in []:
        diag.log(9, \"never\")
";
    let (calls, failed) = run_logged(source);
    assert_eq!(failed, None);
    assert_eq!(
        calls,
        [
            "log 1 level",
            "log 2 level",
            "log 3 level",
            "log 0 a",
            "log 0 b",
            "log 1 one",
            "log 2 two"
        ]
    );
}

#[test]
fn break_and_continue_work_in_both_kinds_of_loop() {
    let source = "\
def count(n):
    total = 0
    # @maxIterations 100
    for i in range(n):
        if i == 2:
            continue
        if i == 5:
            break
        total += 1
    return total

def main():
    for level in [1, 2, 3, 4]:
        if level == 2:
            continue
        if level == 4:
            break
        diag.log(level, \"unrolled\")
    if count(50) == 4:
        diag.log(9, \"ranged\")
";
    let (calls, failed) = run_logged(source);
    assert_eq!(failed, None);
    assert_eq!(calls, ["log 1 unrolled", "log 3 unrolled", "log 9 ranged"]);
}

#[test]
fn a_loop_past_its_limit_fails_with_loop_limit_exceeded() {
    let program = |n: u32| {
        format!(
            "def spin(n):\n    # @maxIterations 5\n    for i in range(n):\n        diag.wait(1)\n\ndef main():\n    spin({n})\n"
        )
    };
    let (calls, failed) = run_logged(&program(5));
    assert_eq!((calls.len(), failed), (5, None));
    let (calls, failed) = run_logged(&program(6));
    assert_eq!(calls.len(), 5);
    let (code, detail) = failed.expect("the sixth iteration fails");
    assert_eq!(code, "LOOP_LIMIT_EXCEEDED");
    assert!(
        detail.contains("line 3") && detail.contains("5 iterations"),
        "{detail}"
    );
}

#[test]
fn the_default_limit_is_a_warning_and_constant_ranges_have_none() {
    let source =
        "def spin(n):\n    for i in range(n):\n        pass\n\ndef main():\n    spin(2000)\n";
    let lowered = lowered_with_warnings(source).unwrap();
    let warnings: Vec<_> = lowered.warnings.iter().map(|w| (w.code, w.line)).collect();
    assert_eq!(warnings, [(StarCode::LoopLimitDefaulted, 2)]);
    let (_, failed) = run_logged(source);
    assert_eq!(
        failed.map(|(code, _)| code).as_deref(),
        Some("LOOP_LIMIT_EXCEEDED")
    );
    // Constant bounds need no limit, and run past the default.
    let constant = "def main():\n    n = 0\n    for i in range(2000):\n        n += 1\n    if n == 2000:\n        diag.log(1, \"done\")\n";
    assert!(lowered_with_warnings(constant).unwrap().warnings.is_empty());
    assert_eq!(run_logged(constant), (vec!["log 1 done".to_owned()], None));
}

#[test]
fn ranges_follow_their_start_stop_and_step() {
    let holds_sum = |range: &str, sum: i64| {
        let source = format!(
            "def main():\n    total = 0\n    last = -1\n    for i in {range}:\n        total += i\n        last = i\n    if total == {sum}:\n        diag.log(1, \"ok\")\n"
        );
        run_logged(&source).0 == ["log 1 ok"]
    };
    assert!(holds_sum("range(4)", 6));
    assert!(holds_sum("range(2, 5)", 9));
    assert!(holds_sum("range(10, 0, -3)", 22));
    assert!(holds_sum("range(5, 2)", 0));
    assert!(holds_sum("range(0, 10, 4)", 12));
    // Near the end of the int range the loop stops instead of stepping past it.
    let max = i64::MAX;
    let source = format!(
        "def main():\n    n = 0\n    for i in range({}, {max}, 4):\n        n += 1\n    if n == 2:\n        diag.log(1, \"ok\")\n",
        max - 5
    );
    assert_eq!(run_logged(&source).0, ["log 1 ok"]);
    // The variable keeps its last value after the loop, as in Starlark.
    let after = "def main():\n    for i in range(3):\n        pass\n    if i == 2:\n        diag.log(1, \"ok\")\n";
    assert_eq!(run_logged(after).0, ["log 1 ok"]);
}

#[test]
fn loops_that_cannot_be_lowered_are_reported() {
    use StarCode::*;
    let cases = [
        (
            "def main():\n    for i in range(1, 2, 0):\n        pass\n",
            ArgumentOutOfRange,
            2,
        ),
        (
            "def f(s):\n    for i in range(0, 9, s):\n        pass\n\ndef main():\n    f(1)\n",
            NotLowerable,
            2,
        ),
        (
            "def main():\n    for i in range(1, 2, 3, 4):\n        pass\n",
            UnsupportedSyntax,
            2,
        ),
        (
            "def main():\n    for i in range(1.5):\n        pass\n",
            NotLowerable,
            2,
        ),
        (
            "def main():\n    for i in \"abc\":\n        pass\n",
            NotLowerable,
            2,
        ),
        // A list item cannot be assigned at run time.
        (
            "def main():\n    for row in [[1]]:\n        row = 2\n",
            NotLowerable,
            2,
        ),
    ];
    for (source, code, line) in cases {
        let found = codes(source);
        assert!(found.contains(&(code, line)), "{source}: {found:?}");
    }
    // Nested constant loops multiply: 1,100 x 1,100 iterations pass the unrolling bound.
    let items = (0..1100)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        "L = [{items}]\n\ndef main():\n    for a in L:\n        for b in L:\n            pass\n"
    );
    let found = codes(&source);
    assert!(
        !found.is_empty() && found.iter().all(|(c, _)| *c == NotLowerable),
        "{found:?}"
    );
}

#[test]
fn a_variable_over_lists_or_dicts_cannot_be_used_outside_its_loop() {
    // No slot holds a list, so after the loop the variable would read a stale value.
    let after = "\
def main():
    x = 1
    for x in [[5], [6]]:
        pass
    if x == 1:
        diag.log(1, \"stale\")
";
    assert_eq!(codes(after), vec![(StarCode::NotLowerable, 3)]);
    // Used only inside the loop, it is fine.
    let inside = "\
def main():
    for row in [[5], [6]]:
        diag.log(row[0], \"in\")
";
    assert_eq!(run_logged(inside).0, ["log 5 in", "log 6 in"]);
}

#[test]
fn reassigning_an_outer_loop_variable_does_not_leave_a_binding_behind() {
    // The inner loop over T[x] assigns `y` only when `x` is 1; with a run-time break after
    // the first iteration, `x` must read 0 after the loop.
    let source = "\
T = [[], [0]]

def f(c):
    for y in [7]:
        for x in [0, 1]:
            for y in T[x]:
                pass
            if c:
                break
        if x == 0:
            diag.log(1, \"x is 0\")
        if x == 1:
            diag.log(1, \"x is 1\")

def main():
    f(True)
";
    assert_eq!(run_logged(source).0, ["log 1 x is 0"]);
}

#[test]
fn unrolling_counts_the_body_as_well_as_the_iterations() {
    // 4,096 iterations of a body of 300 calls: far fewer iterations than the bound, but past
    // it in statements and expressions lowered.
    let items = (0..4096)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let body = "        diag.wait(1)\n".repeat(300);
    let source = format!("L = [{items}]\n\ndef main():\n    for n in L:\n{body}");
    let found = codes(&source);
    assert!(
        !found.is_empty() && found.iter().all(|(c, _)| *c == StarCode::NotLowerable),
        "{found:?}"
    );
}

#[test]
fn unrolling_counts_the_bytes_of_the_names_it_handles() {
    // 100,000 iterations are well within the bound, but each copies, hashes or compares a
    // 64 KiB name, so the names' bytes take them past it: a long loop variable, and a long
    // local read in the body.
    let items = vec!["0"; 100_000].join(", ");
    let long = "v".repeat(1 << 16);
    let variable = format!("L = [{items}]\n\ndef main():\n    for {long} in L:\n        pass\n");
    let local = format!(
        "L = [{items}]\n\ndef main():\n    {long} = 1\n    for n in L:\n        diag.wait({long})\n"
    );
    for source in [variable, local] {
        let found = codes(&source);
        assert!(
            !found.is_empty() && found.iter().all(|(c, _)| *c == StarCode::NotLowerable),
            "{found:?}"
        );
    }
    // Names shorter than a unit's bytes cost nothing extra.
    let short = format!("L = [{items}]\n\ndef main():\n    for n in L:\n        pass\n");
    assert!(codes(&short).is_empty());
}

#[test]
fn unrolling_stops_once_a_budget_is_spent() {
    // Each copy builds a 256 KiB literal, so the constant budget runs out after 64 copies;
    // the remaining copies are not lowered, even while inference mutes the error.
    let items = vec!["0"; 2000].join(", ");
    let text = "s".repeat(1 << 18);
    let source =
        format!("L = [{items}]\n\ndef main():\n    for n in L:\n        diag.log(1, \"{text}\")\n");
    let checked = check("t.star", &source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let bytes = empty_document();
    let document = load_ecu_document(&bytes).expect("the document loads");
    let requests = RequestIndex::default();
    let mut lowerer = Lowerer::new(&checked, document.variant().expect("a variant"), &requests);
    lowerer.constants();
    lowerer.globals_built = lowerer.built;
    lowerer.functions();
    lowerer.infer();
    assert!(lowerer.exhausted.is_some());
    assert!(lowerer.unrolled < 1000, "{}", lowerer.unrolled);
    let found = codes(&source);
    assert!(
        !found.is_empty() && found.iter().all(|(c, _)| *c == StarCode::NotLowerable),
        "{found:?}"
    );
}

#[test]
fn a_huge_integer_literal_is_rejected_without_formatting_it() {
    // A 16,384-bit literal in 10,000 copies: rejecting it must not convert it to decimal in
    // each copy.
    let items = vec!["0"; 10_000].join(", ");
    let huge = format!("0x{}", "f".repeat(4096));
    let source =
        format!("L = [{items}]\n\ndef main():\n    for n in L:\n        diag.wait({huge})\n");
    let found = codes(&source);
    assert!(
        !found.is_empty() && found.iter().all(|(c, _)| *c == StarCode::NotLowerable),
        "{found:?}"
    );
    // 2^63 is still recognized under its minus sign, in any spelling.
    assert_eq!(
        failed_with(
            "def main():\n    if -0x8000000000000000 < 0:\n        diag.fail(\"MIN\", \"\")\n"
        ),
        "MIN"
    );
}

#[test]
fn unrolling_counts_the_arguments_of_a_call() {
    // 2,000 copies of a call with 3,001 arguments, the last of them named and refused: each
    // copy looks at every argument, so the arguments count against the unrolling bound even
    // though none is lowered.
    let items = vec!["0"; 2000].join(", ");
    let args = vec!["0"; 3000].join(", ");
    let calls = [
        format!("    for n in L:\n        for b in range({args}, stop=1):\n            pass\n"),
        format!("    for n in L:\n        diag.wait(min({args}, x=1))\n"),
    ];
    let bytes = empty_document();
    let requests = RequestIndex::default();
    for body in calls {
        let source = format!("L = [{items}]\n\ndef main():\n{body}");
        let checked = check("t.star", &source).unwrap_or_else(|errors| panic!("{errors:#?}"));
        let document = load_ecu_document(&bytes).expect("the document loads");
        let mut lowerer = Lowerer::new(&checked, document.variant().expect("a variant"), &requests);
        lowerer.constants();
        lowerer.globals_built = lowerer.built;
        lowerer.functions();
        lowerer.infer();
        assert!(lowerer.exhausted.is_some(), "{body}");
        assert!(!codes(&source).is_empty(), "{body}");
    }
}

#[test]
fn an_unrolled_body_reports_each_problem_once() {
    let source = "\
def spin(n):
    for k in [1, 2, 3]:
        for i in range(n):
            pass
    for k in [1, 2, 3, 4, 5]:
        z = [k]

def main():
    spin(1)
";
    let found = codes(source);
    assert_eq!(found, vec![(StarCode::NotLowerable, 6)]);
    let warnings = "def spin(n):\n    for k in [1, 2, 3]:\n        for i in range(n):\n            pass\n\ndef main():\n    spin(1)\n";
    let lowered = lowered_with_warnings(warnings).unwrap();
    assert_eq!(lowered.warnings.len(), 1);
}

#[test]
fn a_range_loop_body_does_not_fold_a_name_it_assigns() {
    // `x` is bound to 1 by the unrolled loop, but the range body assigns it and runs again,
    // so every read in the body must see the run-time value: 1 + 3 = 4.
    let source = "\
def main():
    for x in [1]:
        for j in range(3):
            x = x + 1
        if x == 4:
            diag.log(1, \"ok\")
";
    assert_eq!(run_logged(source).0, ["log 1 ok"]);
}

// ---------------------------------------------------------------- Restart declaration (ADR-336)

/// A request index whose restart facts are a VIN source and the voltage and ignition
/// preconditions: the first from a service, the second with a programming-session source.
fn restart_index() -> RequestIndex {
    let precondition = |lower, upper, default, programming| diag_ir::Precondition {
        satisfied: diag_ir::Satisfied { lower, upper },
        default_session: Some(default),
        programming_session: Some(programming),
    };
    let supply = diag_ir::Source::RuntimeInput(diag_ir::RuntimeInput::SupplyVoltageMillivolts);
    let read = diag_ir::Source::EcuService {
        service_id: 4,
        field_id: 9,
    };
    RequestIndex {
        identity: diag_ir::IdentitySources {
            vin: Some(read),
            ..Default::default()
        },
        preconditions: diag_ir::Preconditions {
            voltage_mv: Some(precondition(12_000, 15_000, supply, read)),
            ignition: Some(precondition(
                1,
                1,
                diag_ir::Source::RuntimeInput(diag_ir::RuntimeInput::IgnitionOn),
                diag_ir::Source::RuntimeInput(diag_ir::RuntimeInput::IgnitionOn),
            )),
            ..Default::default()
        },
        ..RequestIndex::default()
    }
}

fn lowered_against(source: &str, requests: &RequestIndex) -> Result<Program, Vec<StarError>> {
    let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let bytes = empty_document();
    let variant = load_ecu_document(&bytes)
        .expect("the document loads")
        .variant()
        .expect("a variant");
    lower("t.star", &checked, variant, requests).map(|lowered| lowered.program)
}

#[test]
fn a_program_carries_the_identity_and_only_the_named_preconditions() {
    let requests = restart_index();
    // Every program of the variant has the identity sources, one without the annotation too.
    let plain = lowered_against("def main():\n    pass\n", &requests).expect("lowers");
    assert_eq!(plain.identity, requests.identity);
    assert_eq!(plain.preconditions, diag_ir::Preconditions::default());
    plain.validate().expect("a plain procedure validates");

    let source = "# @preconditions ignition voltage\ndef main():\n    pass\n";
    let program = lowered_against(source, &requests).expect("lowers");
    assert_eq!(program.identity, requests.identity);
    assert_eq!(
        program.preconditions.voltage_mv,
        requests.preconditions.voltage_mv
    );
    assert_eq!(
        program.preconditions.ignition,
        requests.preconditions.ignition
    );
    assert_eq!(program.preconditions.engine, None);
    program.validate().expect("the declaration validates");

    let source = "# @preconditions ignition\ndef main():\n    pass\n";
    let program = lowered_against(source, &requests).expect("lowers");
    assert_eq!(program.preconditions.voltage_mv, None);
    assert!(program.preconditions.ignition.is_some());
}

#[test]
fn a_precondition_without_a_row_is_unknown() {
    let source = "\
# @preconditions voltage engine vehicleSpeed externalSupply
def main():
    pass
";
    let errors = lowered_against(source, &restart_index()).expect_err("rejected");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::UnknownPrecondition, 1)]
    );
    // One error names every missing kind, in the order written.
    assert!(
        errors[0]
            .hint
            .contains("engine, vehicle_speed, external_supply"),
        "{}",
        errors[0].hint
    );
    // With no table at all, any name is unknown.
    let errors = lowered("# @preconditions ignition\ndef main():\n    pass\n").expect_err("no row");
    assert_eq!(
        errors.iter().map(|e| e.code).collect::<Vec<_>>(),
        [StarCode::UnknownPrecondition]
    );
}

// ---------------------------------------------------------------- `diag.precondition` (ADR-338)

/// Runs `source` lowered against `requests` on `host` to its end.
fn run_against(source: &str, requests: &RequestIndex, host: &mut ScriptedHost) -> Program {
    let program = lowered_against(source, requests).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let mut vm = Vm::new(&program);
    for _ in 0..10_000 {
        match vm.step(&program, host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => return program,
            other => panic!("{other:?}"),
        }
    }
    panic!("the procedure did not end")
}

fn error_codes(source: &str, requests: &RequestIndex) -> Vec<(StarCode, u32)> {
    match lowered_against(source, requests) {
        Ok(_) => Vec::new(),
        Err(errors) => errors.iter().map(|e| (e.code, e.line)).collect(),
    }
}

#[test]
fn a_precondition_from_a_runtime_input_lowers_to_read_input() {
    use diag_ir::RuntimeInput::{IgnitionOn, SupplyVoltageMillivolts};
    // No `@preconditions`: the call reads a value during the run and declares nothing.
    let source = "\
def main():
    if diag.precondition(\"voltage\") > 12000:
        diag.log(1, \"supply high\")
    if diag.precondition(\"ignition\") == 1:
        diag.log(1, \"ignition on\")
";
    let mut host = ScriptedHost {
        inputs: vec![(SupplyVoltageMillivolts, 12_600), (IgnitionOn, 1)],
        ..ScriptedHost::default()
    };
    let program = run_against(source, &restart_index(), &mut host);
    assert_eq!(
        host.calls,
        [
            "input SupplyVoltageMillivolts",
            "log 1 supply high",
            "input IgnitionOn",
            "log 1 ignition on"
        ]
    );
    assert!(program.code.contains(&Op::ReadInput {
        input: SupplyVoltageMillivolts
    }));
    assert_eq!(program.preconditions, diag_ir::Preconditions::default());
    // A read of an input is no diagnostic primitive, so no section covers it.
    let read_at = program
        .code
        .iter()
        .position(|op| matches!(op, Op::ReadInput { .. }))
        .expect("a read");
    assert!(!program.code[read_at].is_diagnostic_primitive());
    assert!(
        program
            .sections
            .iter()
            .all(|s| !(s.start_pc..s.end_pc).contains(&(read_at as u32)))
    );
    program.validate().expect("valid");
}

#[test]
fn a_precondition_the_host_cannot_establish_is_a_host_error() {
    let program = lowered_against(
        "def main():\n    x = diag.precondition(\"voltage\")\n",
        &restart_index(),
    )
    .expect("lowers");
    let mut vm = Vm::new(&program);
    let mut host = ScriptedHost::default();
    let error = loop {
        match vm.step(&program, &mut host) {
            Ok(StepOutcome::Continue) => {}
            Err(StepError::Host(error)) => break error,
            other => panic!("{other:?}"),
        }
    };
    assert!(matches!(error, NoHostError));
}

#[test]
fn a_precondition_name_is_a_known_string_constant() {
    use StarCode::*;
    let requests = restart_index();
    let cases = [
        // Not a kind: the table's spelling is not the call's, as for `@preconditions`.
        (
            "def main():\n    diag.precondition(\"batteryMilliVolt\")\n",
            ArgumentOutOfRange,
        ),
        (
            "def main():\n    diag.precondition(\"vehicle_speed\")\n",
            ArgumentOutOfRange,
        ),
        // Known only at run time.
        (
            "def f(n):\n    diag.precondition(n)\n\ndef main():\n    f(\"voltage\")\n",
            NonConstantArgument,
        ),
        ("def main():\n    diag.precondition(1)\n", UnsupportedSyntax),
        ("def main():\n    diag.precondition()\n", UnsupportedSyntax),
        (
            "def main():\n    diag.precondition(\"voltage\", \"ignition\")\n",
            UnsupportedSyntax,
        ),
        // A kind the variant's table has no row for.
        (
            "def main():\n    diag.precondition(\"engine\")\n",
            UnknownPrecondition,
        ),
    ];
    for (source, code) in cases {
        let found = error_codes(source, &requests);
        assert_eq!(found, [(code, 2)], "{source}");
    }
    // The error names the kind that has no row.
    let errors = lowered_against(cases[6].0, &requests).expect_err("no row");
    assert!(errors[0].hint.contains("engine"), "{}", errors[0].hint);
}

#[test]
fn a_precondition_row_without_a_default_session_source_is_refused() {
    let mut requests = restart_index();
    requests
        .preconditions
        .voltage_mv
        .as_mut()
        .expect("a row")
        .default_session = None;
    let errors = lowered_against(
        "def main():\n    diag.precondition(\"voltage\")\n",
        &requests,
    )
    .expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::UnknownPrecondition, 2)]
    );
    assert!(
        errors[0].hint.contains("default-session"),
        "{}",
        errors[0].hint
    );
}

#[test]
fn a_service_source_the_variant_does_not_declare_is_not_lowerable() {
    // `restart_index` reads the voltage in the programming session from service 4, which the
    // empty document lacks; make that the default source too.
    let mut requests = restart_index();
    let row = requests.preconditions.voltage_mv.as_mut().expect("a row");
    row.default_session = row.programming_session;
    assert_eq!(
        error_codes(
            "def main():\n    diag.precondition(\"voltage\")\n",
            &requests
        ),
        [(StarCode::NotLowerable, 2)]
    );
}

#[test]
fn main_is_at_pc_zero_and_ends_in_a_ret_with_the_other_functions_after_it() {
    // ADR-336 item 5: no entry call to `main`, so a plan can start in `main`'s own frame.
    let source = "\
def helper():
    diag.log(1, \"b\")

def main():
    diag.log(1, \"a\")
    helper()
";
    let program = lowered(source).expect("lowers");
    program.validate().expect("valid");
    // `main`: the wait, the call, the end; then `helper`: the wait, the return.
    let ret = program
        .code
        .iter()
        .position(|op| matches!(op, Op::Ret))
        .expect("a Ret");
    let call = program
        .code
        .iter()
        .position(|op| matches!(op, Op::Call(_)))
        .expect("a call");
    assert!(call < ret, "main's call comes before its Ret");
    assert_eq!(program.code[call], Op::Call(ret as u32 + 1));
    assert!(program.code[..call].iter().any(Op::is_diagnostic_primitive));
    assert!(!program.code.iter().any(|op| matches!(op, Op::Call(0))));
    // The source map starts at pc 0 with `main`'s first statement (line 5).
    assert_eq!(
        (program.source_map[0].pc, program.source_map[0].line),
        (0, 5)
    );
    // The VM runs it in the top-level frame and ends at that `Ret`.
    assert_eq!(run(source), Ok(None));
}

#[test]
fn a_validate_refusal_naming_a_pc_is_plan_invalid_at_its_span() {
    let source = "def main():\n    diag.wait(1)\n    diag.wait(2)\n";
    let checked = check("t.star", source).expect("checks");
    let program = lowered(source).expect("lowers");
    let module = &checked.module;
    let base = module.statement().span.begin().get();
    let second = source.find("diag.wait(2)").expect("the second call") as u32;
    let spans = vec![
        Span::new(
            starlark_syntax::codemap::Pos::new(base),
            starlark_syntax::codemap::Pos::new(base),
        ),
        Span::new(
            starlark_syntax::codemap::Pos::new(base + second),
            starlark_syntax::codemap::Pos::new(base + second),
        ),
    ];
    let error = ProgramError::DownloadOutsidePlan { pc: 1 };
    let found = resolve(
        "t.star",
        module,
        vec![invalid_program(&checked, &program, &spans, &[], &error)],
    );
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].code, StarCode::PlanInvalid);
    assert_eq!((found[0].line, found[0].column), (3, 5));
    assert!(
        found[0].hint.contains("download instruction at 1"),
        "{}",
        found[0].hint
    );
}

#[test]
fn a_validate_refusal_naming_no_pc_is_not_lowerable() {
    let source = "def main():\n    diag.wait(1)\n";
    let checked = check("t.star", source).expect("checks");
    let program = lowered(source).expect("lowers");
    let spans = vec![checked.module.statement().span];
    for error in [
        ProgramError::BoundaryOutOfOrder { flash_session: 1 },
        // A pc past the code names no instruction either.
        ProgramError::RecoveryRequiredOutOfRange {
            flash_session: 1,
            pc: 99,
        },
    ] {
        let found = resolve(
            "t.star",
            &checked.module,
            vec![invalid_program(&checked, &program, &spans, &[], &error)],
        );
        assert_eq!(found[0].code, StarCode::NotLowerable);
        assert_eq!((found[0].line, found[0].column), (1, 1));
    }
}

// ---------------------------------------------------------------- `diag.ecu_info` (ADR-339)

#[test]
fn ecu_info_is_read_by_a_literal_key_only() {
    use StarCode::*;
    let requests = restart_index();
    let cases = [
        // The call has no value of its own: a statement, a variable, an argument.
        ("def main():\n    diag.ecu_info()\n", NotLowerable),
        ("def main():\n    x = diag.ecu_info()\n", NotLowerable),
        (
            "def main():\n    diag.log(1, diag.ecu_info())\n",
            NotLowerable,
        ),
        // Wrong arguments, and a key that is no literal or not a key.
        (
            "def main():\n    diag.ecu_info(1)[\"vin\"]\n",
            UnsupportedSyntax,
        ),
        (
            "def main():\n    diag.ecu_info()[\"serial\"]\n",
            UnsupportedSyntax,
        ),
        (
            "def f(k):\n    diag.ecu_info()[k]\n\ndef main():\n    f(\"vin\")\n",
            DynamicKeyNotAllowed,
        ),
        // A key whose row the index lacks.
        (
            "def main():\n    diag.ecu_info()[\"part_number\"]\n",
            UnknownIdentity,
        ),
        // A row whose service the variant does not declare.
        ("def main():\n    diag.ecu_info()[\"vin\"]\n", NotLowerable),
    ];
    for (source, code) in cases {
        let found = error_codes(source, &requests);
        assert!(
            !found.is_empty() && found.iter().all(|&(c, line)| c == code && line >= 1),
            "{source}: {found:?}"
        );
    }
}

/// A document whose variant is named `name`.
fn named_variant_document(name: &str) -> Vec<u8> {
    let mut variant = EcuVariantT::default();
    variant.short_name = Some(name.to_owned());
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("test".to_owned());
    document.source_digest = Some(String::new());
    document.variant = Some(Box::new(variant));
    encode_ecu_document(&document)
}

#[test]
fn ecu_info_variant_is_the_variants_short_name_as_a_constant() {
    let source = "\
def main():
    if diag.ecu_info()[\"variant\"] == \"bench\":
        diag.log(1, \"bench\")
    else:
        diag.log(1, \"other\")
";
    let checked = check("t.star", source).expect("checks");
    for (name, logged) in [("bench", "log 1 bench"), ("road", "log 1 other")] {
        let bytes = named_variant_document(name);
        let variant = load_ecu_document(&bytes)
            .expect("the document loads")
            .variant()
            .expect("a variant");
        let program = lower("t.star", &checked, variant, &RequestIndex::default())
            .expect("lowers")
            .program;
        // A constant: no read, and the comparison folds to the branch taken.
        assert!(
            !program
                .code
                .iter()
                .any(|op| matches!(op, Op::SendReceive { .. }))
        );
        let mut vm = Vm::new(&program);
        let mut host = ScriptedHost::default();
        loop {
            match vm.step(&program, &mut host) {
                Ok(StepOutcome::Continue) => {}
                Ok(StepOutcome::Finished) => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(host.calls, [logged]);
    }
    // A variant with no short name has no name to give.
    assert_eq!(
        codes("def main():\n    diag.ecu_info()[\"variant\"]\n"),
        [(StarCode::NotLowerable, 2)]
    );
}

// ---------------------------------------------------------------- Flash plans (4.9, ADR-336)

/// A plan for session 1 whose steps have no erase: its transfer is the last statement.
const PLAN_NO_ERASE: &str = "\
# @section uninterruptible expected=1000
# @flashPlan session=1
def write():
    diag.wait(1)
    res = diag.flash_transfer(1)

# @preconditions voltage externalSupply ignition
def main():
    write()
";

/// The same with an erase, whose `diag.routine` the empty variants of these tests cannot select.
const PLAN_ERASE: &str = "\
# @section uninterruptible expected=1000
# @flashPlan session=1
def write():
    # @erase
    diag.routine(0xFF00, 1, {})
    res = diag.flash_transfer(1)

# @preconditions voltage externalSupply ignition
def main():
    write()
";

/// A request index with the three mandatory preconditions and `identity` sources.
fn flash_index(identity: &[&str]) -> RequestIndex {
    let row = |input| diag_ir::Precondition {
        satisfied: diag_ir::Satisfied { lower: 1, upper: 1 },
        default_session: Some(diag_ir::Source::RuntimeInput(input)),
        programming_session: Some(diag_ir::Source::RuntimeInput(input)),
    };
    let read = Some(diag_ir::Source::EcuService {
        service_id: 4,
        field_id: 9,
    });
    let has = |name: &str| identity.contains(&name).then_some(read).flatten();
    RequestIndex {
        identity: diag_ir::IdentitySources {
            vin: has("vin"),
            hardware_part_number: has("hardware_part_number"),
            software_version: has("software_version"),
        },
        preconditions: diag_ir::Preconditions {
            voltage_mv: Some(row(diag_ir::RuntimeInput::SupplyVoltageMillivolts)),
            external_supply: Some(row(diag_ir::RuntimeInput::ExternalSupplyConnected)),
            ignition: Some(row(diag_ir::RuntimeInput::IgnitionOn)),
            ..Default::default()
        },
        flash: vec![FlashFacts {
            session: 1,
            timing: Some(TIMING),
            version_read_retries: Some(2),
            no_application_nrc: Some(0x22),
        }],
        ..RequestIndex::default()
    }
}

/// The timings `flash_index` gives flash session 1.
const TIMING: diag_ir::RecoveryTiming = diag_ir::RecoveryTiming {
    session_timeout_millis: 5000,
    teardown_margin_millis: 1000,
    ecu_startup_millis: 2000,
    confirmation_window_millis: 3000,
};

/// A segment of 0x25 bytes of the artifact in blocks of 0x10, which the format 0x44 describes.
fn plan_segment() -> diag_ir::declaration::FlashSegmentT {
    let mut segment = diag_ir::declaration::FlashSegmentT::default();
    segment.start_address = 0x1000;
    segment.length = 0x25;
    segment.artifact_offset = 4;
    segment.block_size = 0x10;
    segment
}

/// A document whose variant declares flash session 1 with `steps` and one segment, or no
/// session for `None`.
fn flash_document(steps: Option<&[diag_ir::declaration::FlashStep]>) -> Vec<u8> {
    flash_document_with(steps, vec![plan_segment()])
}

/// [`flash_document`] with the given `segments`.
fn flash_document_with(
    steps: Option<&[diag_ir::declaration::FlashStep]>,
    segments: Vec<diag_ir::declaration::FlashSegmentT>,
) -> Vec<u8> {
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("test".to_owned());
    document.source_digest = Some(String::new());
    let mut variant = EcuVariantT::default();
    variant.flash_sessions = steps.map(|steps| {
        let mut session = diag_ir::declaration::FlashSessionT::default();
        session.id = 1;
        session.steps = Some(steps.to_vec());
        session.segments = Some(segments);
        vec![session]
    });
    document.variant = Some(Box::new(variant));
    encode_ecu_document(&document)
}

/// Lowers `source` against a variant whose flash session 1 has `NO_ERASE` steps and `segments`,
/// and the index of `flash_index(&[])`.
fn lower_plan(
    source: &str,
    segments: Vec<diag_ir::declaration::FlashSegmentT>,
) -> Result<Program, Vec<StarError>> {
    let bytes = flash_document_with(Some(NO_ERASE), segments);
    let variant = load_ecu_document(&bytes)
        .expect("the document loads")
        .variant()
        .expect("a variant");
    let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    lower("t.star", &checked, variant, &flash_index(&[])).map(|lowered| lowered.program)
}

/// What lowering `source` for a variant whose flash session 1 has `steps` reports (no session
/// at all when `steps` is `None`), with its error codes and lines.
fn flash_codes(
    source: &str,
    steps: Option<&[diag_ir::declaration::FlashStep]>,
    requests: &RequestIndex,
) -> Vec<(StarCode, u32)> {
    flash_errors(source, steps, requests)
        .iter()
        .map(|e| (e.code, e.line))
        .collect()
}

fn flash_errors(
    source: &str,
    steps: Option<&[diag_ir::declaration::FlashStep]>,
    requests: &RequestIndex,
) -> Vec<StarError> {
    let bytes = flash_document(steps);
    let variant = load_ecu_document(&bytes)
        .expect("the document loads")
        .variant()
        .expect("a variant");
    let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    lower("t.star", &checked, variant, requests)
        .err()
        .unwrap_or_default()
}

const NO_ERASE: &[diag_ir::declaration::FlashStep] = &[
    diag_ir::declaration::FlashStep::EnterProgrammingSession,
    diag_ir::declaration::FlashStep::TransferData,
];
const WITH_ERASE: &[diag_ir::declaration::FlashStep] = &[
    diag_ir::declaration::FlashStep::EnterProgrammingSession,
    diag_ir::declaration::FlashStep::EraseMemory,
    diag_ir::declaration::FlashStep::TransferData,
];

#[test]
fn a_flash_plan_lowers_with_its_recovery_plan_and_a_transfer_outside_one_is_refused() {
    let requests = flash_index(&[]);
    // The plan of a session without an erase step lowers and validates; the erase's
    // `diag.routine` finds no service in this empty variant, which is its own refusal.
    assert_eq!(flash_codes(PLAN_NO_ERASE, Some(NO_ERASE), &requests), []);
    assert_eq!(
        flash_codes(PLAN_ERASE, Some(WITH_ERASE), &requests),
        [(StarCode::UnknownService, 5)]
    );
    // A transfer outside a plan function is a plan shape error, in `main` or in another function
    // (4.9).
    for source in [
        "def main():\n    diag.flash_transfer(1)\n",
        "def helper():\n    diag.flash_transfer(1)\n\ndef main():\n    helper()\n",
    ] {
        assert_eq!(
            flash_codes(source, Some(NO_ERASE), &requests),
            [(StarCode::PlanShape, 2)],
            "{source}"
        );
    }
}

/// The index of the first instruction that satisfies `is` at or after `from`.
fn position(program: &Program, from: usize, is: impl Fn(&Op) -> bool) -> u32 {
    (from..program.code.len())
        .find(|&pc| is(&program.code[pc]))
        .expect("the instruction") as u32
}

#[test]
fn a_plan_is_laid_out_at_its_call_with_boundaries_derived_from_the_code() {
    let program = lower_plan(PLAN_NO_ERASE, vec![plan_segment()]).expect("lowers");
    program.validate().expect("valid");
    // `main` is at pc 0 and holds the plan in place: no call, and one closing return.
    assert!(!program.code.iter().any(|op| matches!(op, Op::Call(_))));
    assert_eq!(program.code.last(), Some(&Op::Ret));
    assert_eq!(
        program.code.iter().filter(|op| **op == Op::Ret).count(),
        1,
        "{:?}",
        program.code
    );
    let [plan] = &program.flash[..] else {
        panic!("one plan: {:?}", program.flash);
    };
    let download = position(&program, 0, |op| {
        matches!(op, Op::ServiceRequest { service: 0x34 })
    });
    let exit = position(&program, 0, |op| {
        matches!(op, Op::ServiceRequest { service: 0x37 })
    });
    // The entry is the plan's first instruction (its `diag.wait`), the erase point the
    // RequestDownload of a session with no erase step, and the end the closing return.
    assert_eq!(program.code[0], Op::Wait { millis: 1 });
    assert_eq!(
        plan.boundaries,
        diag_ir::RecoveryBoundaries {
            entry_pc: 0,
            erase_pc: download,
            transfer_exit_pc: exit,
            post_transfer_end_pc: program.code.len() as u32 - 1,
        }
    );
    // The default recovery point is the erase point; the rest comes from the session's row.
    assert_eq!(
        (plan.flash_session, plan.stage, plan.max_resumes),
        (1, 1, 3)
    );
    assert_eq!(
        plan.recovery_required,
        diag_ir::RecoveryRequired::FromPc(download)
    );
    assert_eq!(plan.timing, TIMING);
    assert_eq!(plan.version_read_retries, 2);
    assert_eq!(plan.no_application, Some(diag_ir::NoApplication::Nrc(0x22)));
    // The plan function's section covers the whole expansion, pieces cut around each primitive.
    let covering: Vec<_> = program
        .sections
        .iter()
        .filter(|s| {
            s.start_pc >= plan.boundaries.entry_pc
                && s.end_pc <= plan.boundaries.post_transfer_end_pc
        })
        .collect();
    assert!(
        covering
            .iter()
            .all(|s| matches!(s.interruptible, diag_ir::Interruptible::No)
                && s.expected_millis == 1000),
        "{covering:?}"
    );
    let mut next = plan.boundaries.entry_pc;
    for s in &covering {
        assert_eq!(s.start_pc, next, "no gap");
        next = s.end_pc;
    }
    assert_eq!(next, plan.boundaries.post_transfer_end_pc);
}

#[test]
fn the_markers_and_the_section_move_the_recovery_point() {
    let requests = flash_index(&["vin", "hardware_part_number", "software_version"]);
    let lowered = |source: &str| {
        let bytes = flash_document(Some(NO_ERASE));
        let variant = load_ecu_document(&bytes)
            .expect("loads")
            .variant()
            .expect("a variant");
        let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
        let program = lower("t.star", &checked, variant, &requests)
            .unwrap_or_else(|errors| panic!("{errors:#?}"))
            .program;
        program.validate().expect("valid");
        program
    };
    // `recovery=never`.
    let never = lowered(&PLAN_NO_ERASE.replace("session=1", "session=1 recovery=never"));
    assert_eq!(
        never.flash[0].recovery_required,
        diag_ir::RecoveryRequired::Never
    );
    assert!(never.flash[0].allows_restart());
    // A marker after the transfer: the first instruction of its statement.
    let marked = lowered(&PLAN_NO_ERASE.replace(
        "    res = diag.flash_transfer(1)",
        "    res = diag.flash_transfer(1)\n    # @recoveryRequired\n    diag.wait(2)",
    ));
    let plan = &marked.flash[0];
    let wait = position(&marked, 1, |op| matches!(op, Op::Wait { millis: 2 }));
    assert_eq!(
        plan.recovery_required,
        diag_ir::RecoveryRequired::FromPc(wait)
    );
    assert!(wait > plan.boundaries.transfer_exit_pc);
    assert!(plan.allows_restart());
    // A section that requires on-site intervention requires it from the entry.
    let section = lowered(&PLAN_NO_ERASE.replace("uninterruptible", "recoveryRequired"));
    let plan = &section.flash[0];
    assert_eq!(
        plan.recovery_required,
        diag_ir::RecoveryRequired::FromPc(plan.boundaries.entry_pc)
    );
    assert!(!plan.allows_restart());
}

#[test]
fn a_plan_after_other_statements_of_main_begins_after_them() {
    let source = PLAN_NO_ERASE.replace(
        "    write()\n",
        "    diag.wait(7)\n    write()\n    diag.wait(8)\n",
    );
    let program = lower_plan(&source, vec![plan_segment()]).expect("lowers");
    program.validate().expect("valid");
    let plan = &program.flash[0];
    assert_eq!(program.code[0], Op::Wait { millis: 7 });
    assert_eq!(plan.boundaries.entry_pc, 1);
    assert_eq!(program.code[1], Op::Wait { millis: 1 });
    // The statement after the plan is past its end.
    let after = position(&program, 0, |op| matches!(op, Op::Wait { millis: 8 }));
    assert_eq!(plan.boundaries.post_transfer_end_pc, after);
    // The primitives outside the plan are `Yes`, the plan's `No`.
    let section_of = |pc: u32| {
        program
            .sections
            .iter()
            .find(|s| (s.start_pc..s.end_pc).contains(&pc))
            .expect("a section")
    };
    assert!(matches!(
        section_of(0).interruptible,
        diag_ir::Interruptible::Yes
    ));
    assert!(matches!(
        section_of(1).interruptible,
        diag_ir::Interruptible::No
    ));
    assert!(matches!(
        section_of(after).interruptible,
        diag_ir::Interruptible::Yes
    ));
}

#[test]
fn a_plan_in_an_if_is_entered_at_its_first_instruction_only() {
    let source = PLAN_NO_ERASE.replace(
        "    write()\n",
        "    if diag.precondition(\"voltage\") > 0:\n        write()\n",
    );
    // The empty index reads the precondition from an input, which `lower` accepts.
    let program = lower_plan(&source, vec![plan_segment()]).expect("lowers");
    program.validate().expect("valid");
    let plan = &program.flash[0];
    // The jump that skips the plan lands at its end, which is not inside it.
    let skip = program
        .code
        .iter()
        .find_map(|op| match op {
            Op::JumpIfFalse(target) => Some(*target),
            _ => None,
        })
        .expect("the if's jump");
    assert_eq!(skip, plan.boundaries.post_transfer_end_pc);
}

/// The plan `body` (after the `def write():` line, indented) for session 1 without an erase step,
/// with `helpers` before it, called by `main`.
fn plan_calling(helpers: &str, body: &str) -> String {
    format!(
        "{helpers}\n# @section uninterruptible expected=1000\n# @flashPlan session=1\n\
         def write():\n{body}\n# @preconditions voltage externalSupply ignition\n\
         def main():\n    write()\n"
    )
}

/// Lowers `source` with `lower_plan`, validates it and runs it against a host that serves the
/// plan's download, transfer and artifact; the host's log of the calls.
fn run_plan(source: &str) -> (Program, Vec<String>) {
    let bytes = flash_document_with(Some(NO_ERASE), vec![plan_segment()]);
    let document = load_ecu_document(&bytes).expect("the document loads");
    let variant = document.variant().expect("a variant");
    let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let program = lower("t.star", &checked, variant, &flash_index(&[]))
        .unwrap_or_else(|errors| panic!("{errors:#?}"))
        .program;
    program.validate().expect("valid");
    let mut host = ScriptedHost {
        // The RequestDownload's and the RequestTransferExit's responses.
        responses: [vec![0x74, 0x20, 0x01, 0x00], vec![0x77]].into(),
        artifact: vec![0; 64],
        ..ScriptedHost::default()
    };
    let mut vm = Vm::new(&program);
    for _ in 0..10_000 {
        match vm.step_with_declarations(&program, Some(&variant), &mut host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => return (program, host.calls),
            other => panic!("{other:?}"),
        }
    }
    panic!("the procedure did not end")
}

/// The pcs of the plan's range `[entry, end)` of the only plan of `program`.
fn plan_range(program: &Program) -> std::ops::Range<usize> {
    let boundaries = &program.flash[0].boundaries;
    boundaries.entry_pc as usize..boundaries.post_transfer_end_pc as usize
}

#[test]
fn a_plan_expands_the_plain_functions_it_calls_in_place() {
    let helpers = "\
def scale(a, b):
    c = a * b
    return c + 1
";
    let body = r#"
    x = scale(3, 4)
    if x == 13:
        diag.log(1, "scaled")
    y = scale(x, 2)
    if y == 27:
        diag.log(1, "twice")
    res = diag.flash_transfer(1)
"#;
    let (program, calls) = run_plan(&plan_calling(helpers, body));
    // The return values reached the plan: 3 * 4 + 1, then 13 * 2 + 1.
    let logged: Vec<_> = calls
        .iter()
        .filter(|call| call.starts_with("log"))
        .collect();
    assert_eq!(logged, ["log 1 scaled", "log 1 twice"], "{calls:?}");
    // No call or return stands in the plan, and `main`'s closing return is the program's only one.
    assert!(
        program.code[plan_range(&program)]
            .iter()
            .all(|op| !matches!(op, Op::Call(_) | Op::Ret))
    );
    assert!(!program.code.iter().any(|op| matches!(op, Op::Call(_))));
    assert_eq!(program.code.iter().filter(|op| **op == Op::Ret).count(), 1);
    // The function called twice has a set of slots for each call: three for each (a, b, c), and
    // the plan's x, y, res and the transfer's counter, none shared.
    let stored: std::collections::HashSet<u16> = program
        .code
        .iter()
        .filter_map(|op| match op {
            Op::StoreLocal(slot) => Some(*slot),
            _ => None,
        })
        .collect();
    assert_eq!(stored.len(), 10, "{:?}", program.code);
    // The boundaries are the plan's own: its first instruction, the download as the erase point
    // of a session with no erase step, the exit, and the end before `main`'s return.
    let boundaries = &program.flash[0].boundaries;
    let download = position(&program, 0, |op| {
        matches!(op, Op::ServiceRequest { service: 0x34 })
    });
    assert_eq!(boundaries.entry_pc, 0);
    assert_eq!(boundaries.erase_pc, download);
    assert_eq!(
        boundaries.transfer_exit_pc,
        position(&program, 0, |op| matches!(
            op,
            Op::ServiceRequest { service: 0x37 }
        ))
    );
    assert_eq!(
        boundaries.post_transfer_end_pc,
        program.code.len() as u32 - 1
    );
    assert_eq!(
        program.flash[0].recovery_required,
        diag_ir::RecoveryRequired::FromPc(download)
    );
    // The blocks were transferred.
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("transfer")).count(),
        3
    );
}

#[test]
fn nested_calls_and_a_return_in_a_loop_are_expanded_with_their_jumps_inside_the_plan() {
    let helpers = "\
def find(limit):
    for i in range(10):
        if i * i > limit:
            return i
    return 99

def leaf(a):
    return a + 1

def inner(a):
    b = leaf(a)
    return find(leaf(b)) * 2

def outer(a):
    c = inner(a)
    return c + leaf(0)
";
    // Before the transfer and after it, so that the jumps of both stages are checked.
    let body = r#"
    x = outer(1)
    if x == 5:
        diag.log(1, "before")
    res = diag.flash_transfer(1)
    y = find(5)
    if y == 3:
        diag.log(1, "after")
"#;
    let (program, calls) = run_plan(&plan_calling(helpers, body));
    // outer(1) = inner(1) + leaf(0): b = leaf(1) = 2, find(leaf(2)) is the first i with
    // i * i > 3, which is 2, doubled is 4, and 4 + 1 = 5.
    assert!(calls.contains(&"log 1 before".to_owned()), "{calls:?}");
    // find(5): the first i with i * i > 5 is 3.
    assert!(calls.contains(&"log 1 after".to_owned()), "{calls:?}");
    assert!(
        program.code[plan_range(&program)]
            .iter()
            .all(|op| !matches!(op, Op::Call(_) | Op::Ret))
    );
}

#[test]
fn a_plan_expands_a_section_function_with_its_pieces_inside_the_plans() {
    let helpers = "\
def tail():
    diag.log(1, \"tail\")

# @section interruptible expected=500
def hold(n):
    diag.wait(7)
    tail()
    return n + 1
";
    let body = r#"
    x = hold(1)
    if x == 2:
        diag.log(1, "held")
    res = diag.flash_transfer(1)
"#;
    let (program, calls) = run_plan(&plan_calling(helpers, body));
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.starts_with("wait") || c.starts_with("log"))
            .collect::<Vec<_>>(),
        ["wait 7", "log 1 tail", "log 1 held"],
        "{calls:?}"
    );
    assert!(
        program.code[plan_range(&program)]
            .iter()
            .all(|op| !matches!(op, Op::Call(_) | Op::Ret))
    );
    // The function's pieces keep its attributes, cover its expansion without a gap from its
    // first instruction (the store of its parameter, entered by fallthrough), and lie inside the
    // plan.
    let mut held: Vec<_> = program
        .sections
        .iter()
        .filter(|s| s.expected_millis == 500)
        .collect();
    held.sort_by_key(|s| s.start_pc);
    assert!(
        held.iter()
            .all(|s| matches!(s.interruptible, diag_ir::Interruptible::Yes)),
        "{held:?}"
    );
    assert!(matches!(
        program.code[held[0].start_pc as usize],
        Op::StoreLocal(_)
    ));
    for pair in held.windows(2) {
        assert_eq!(pair[0].end_pc, pair[1].start_pc, "no gap: {held:?}");
    }
    let range = plan_range(&program);
    assert!(range.contains(&(held[0].start_pc as usize)));
    assert!(range.contains(&(held.last().expect("pieces").end_pc as usize - 1)));
    // The wait and the log inside each have a one-instruction section of the function, and one
    // of the plan: the plan stays uninterruptible over the whole expansion.
    for pc in 0..program.code.len() as u32 {
        if !range.contains(&(pc as usize)) {
            continue;
        }
        let covering: Vec<_> = program
            .sections
            .iter()
            .filter(|s| (s.start_pc..s.end_pc).contains(&pc))
            .collect();
        assert!(
            covering
                .iter()
                .any(|s| matches!(s.interruptible, diag_ir::Interruptible::No)
                    && s.expected_millis == 1000),
            "pc {pc}: {covering:?}"
        );
        if program.code[pc as usize].is_diagnostic_primitive()
            && held.iter().any(|s| s.start_pc <= pc && pc < s.end_pc)
        {
            assert!(
                covering
                    .iter()
                    .any(|s| s.expected_millis == 500 && s.end_pc - s.start_pc == 1),
                "pc {pc}: {covering:?}"
            );
        }
    }
}

#[test]
fn a_section_function_that_requires_intervention_cannot_stand_before_the_recovery_point() {
    // `Program::validate` refuses a `recoveryRequired` section over the part of the plan before
    // the recovery point (here, the erase point), as it refuses a plan whose own section is one
    // and which takes a marker; after the point it validates.
    let helpers = "\
# @section recoveryRequired
def hold():
    diag.wait(7)
";
    let before = plan_calling(helpers, "    hold()\n    res = diag.flash_transfer(1)\n");
    let errors = lower_plan(&before, vec![plan_segment()]).expect_err("refused");
    // The error is at the call that expanded the section function, not at the file's start.
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, line_of(&before, "    hold()"))]
    );
    assert!(errors[0].hint.contains("not valid"), "{}", errors[0].hint);
    let after = plan_calling(helpers, "    res = diag.flash_transfer(1)\n    hold()\n");
    lower_plan(&after, vec![plan_segment()])
        .expect("lowers")
        .validate()
        .expect("valid");
}

#[test]
fn what_a_plan_call_is_refused_for() {
    let check_codes = |source: &str| -> Vec<(StarCode, u32)> {
        match check("t.star", source) {
            Ok(_) => Vec::new(),
            Err(errors) => errors.iter().map(|e| (e.code, e.line)).collect(),
        }
    };
    // Recursion a plan reaches is the checker's, as anywhere (4.1).
    let recursive = plan_calling(
        "def a():\n    b()\n\ndef b():\n    a()\n",
        "    a()\n    res = diag.flash_transfer(1)\n",
    );
    assert!(
        check_codes(&recursive)
            .iter()
            .all(|(code, _)| *code == StarCode::UnboundedNotAllowed)
            && !check_codes(&recursive).is_empty()
    );
    // A transfer in a function the plan calls twice, an `@erase` in one, and a plan called from a
    // function the plan calls.
    let twice = plan_calling(
        "def again():\n    diag.flash_transfer(1)\n",
        "    again()\n    again()\n    res = diag.flash_transfer(1)\n",
    );
    assert!(
        check_codes(&twice)
            .iter()
            .any(|(code, _)| *code == StarCode::PlanShape),
        "{:?}",
        check_codes(&twice)
    );
    let erase = plan_calling(
        "def erasing():\n    # @erase\n    diag.routine(0xFF00, 1, {})\n",
        "    erasing()\n    res = diag.flash_transfer(1)\n",
    );
    assert!(
        check_codes(&erase)
            .iter()
            .any(|(code, _)| *code == StarCode::BadAnnotation),
        "{:?}",
        check_codes(&erase)
    );
    // An `@idempotency` does not reach the function a statement calls, which has its own.
    let annotated = plan_calling(
        "def quiet():\n    diag.wait(1)\n",
        "    # @idempotency unsafe\n    quiet()\n    res = diag.flash_transfer(1)\n",
    );
    let errors = lower_plan(&annotated, vec![plan_segment()]).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::BadAnnotation, 8)]
    );
    // A function whose calls double at each level passes the copy limits (2.6).
    let mut helpers = String::new();
    for level in 0..11 {
        helpers += &format!(
            "def f{level}():\n    f{}()\n    f{}()\n\n",
            level + 1,
            level + 1
        );
    }
    helpers += "def f11():\n    diag.wait(1)\n";
    let doubling = plan_calling(&helpers, "    f0()\n    res = diag.flash_transfer(1)\n");
    let errors = lower_plan(&doubling, vec![plan_segment()]).expect_err("refused");
    assert!(
        !errors.is_empty()
            && errors
                .iter()
                .all(|e| e.code == StarCode::SpecializationLimit),
        "{errors:#?}"
    );
}

/// The line (1-based) of the first line of `source` that holds `needle`.
fn line_of(source: &str, needle: &str) -> u32 {
    source
        .lines()
        .position(|line| line.contains(needle))
        .expect("the line") as u32
        + 1
}

/// The log calls of `calls`, as `log <level> <text>`.
fn logs(calls: &[String]) -> Vec<&str> {
    calls
        .iter()
        .filter(|c| c.starts_with("log"))
        .map(String::as_str)
        .collect()
}

#[test]
fn a_callee_that_may_read_an_unset_local_is_not_expanded_in_a_loop() {
    let helpers = "def f(c):\n    if c:\n        x = 1\n    return x\n";
    let tail = "    res = diag.flash_transfer(1)\n";
    let looped = plan_calling(
        helpers,
        &format!("    r = 0\n    for i in range(2):\n        r = f(i == 0)\n{tail}"),
    );
    let errors = lower_plan(&looped, vec![plan_segment()]).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, line_of(&looped, "r = f(i"))]
    );
    assert!(errors[0].hint.contains("reads `x`"), "{}", errors[0].hint);
    // The same call outside a loop lowers: the expansion runs once.
    let once = plan_calling(helpers, &format!("    r = f(True)\n{tail}"));
    lower_plan(&once, vec![plan_segment()]).expect("lowers");
    // A constant loop unrolls into an expansion for each pass, so it is no loop here.
    let unrolled = plan_calling(
        helpers,
        &format!("    for k in [1, 2]:\n        r = f(k == 1)\n{tail}"),
    );
    lower_plan(&unrolled, vec![plan_segment()]).expect("lowers");
    // The loop variable of a loop that may not run is unset after it.
    let zero = plan_calling(
        "def k(n):\n    for j in range(n):\n        pass\n    return j\n",
        &format!("    for i in range(2):\n        r = k(i)\n{tail}"),
    );
    let errors = lower_plan(&zero, vec![plan_segment()]).expect_err("refused");
    assert_eq!(errors[0].code, StarCode::NotLowerable);
    assert!(errors[0].hint.contains("reads `j`"), "{}", errors[0].hint);
    // A callee that calls one, and one called in a loop of the callee's own caller, are refused
    // at the call that would run again.
    let nested = plan_calling(
        &format!("{helpers}\ndef g(c):\n    return f(c)\n"),
        &format!("    for i in range(2):\n        r = g(i == 0)\n{tail}"),
    );
    let errors = lower_plan(&nested, vec![plan_segment()]).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, line_of(&nested, "return f(c)"))]
    );
    let inner_loop = plan_calling(
        helpers,
        &format!(
            "    r = 0\n    for i in range(2):\n        for j in range(2):\n            r = f(j == 0)\n{tail}"
        ),
    );
    lower_plan(&inner_loop, vec![plan_segment()]).expect_err("refused");
}

/// Lowers the plan whose body calls `call` in a `range` loop, with `helpers` before it.
fn lower_in_loop(helpers: &str, call: &str) -> Result<Program, Vec<StarError>> {
    let source = plan_calling(
        helpers,
        &format!(
            "    r = 0\n    for i in range(2):\n        r = {call}\n    res = diag.flash_transfer(1)\n"
        ),
    );
    lower_plan(&source, vec![plan_segment()])
}

/// The hint of the one error of a refused call in a loop, which must be about `name`.
fn assert_refused_reading(result: Result<Program, Vec<StarError>>, name: &str) {
    let errors = result.expect_err("refused");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].code, StarCode::NotLowerable);
    assert!(
        errors[0].hint.contains(&format!("reads `{name}`")),
        "{}",
        errors[0].hint
    );
}

#[test]
fn an_augmented_assignment_reads_its_target_in_the_unset_analysis() {
    // `x += 1` reads `x`, which the `if` leaves unset on one path.
    let helpers = "def f(c):\n    if c:\n        x = 1\n    x += 1\n    return x\n";
    assert_refused_reading(lower_in_loop(helpers, "f(i == 0)"), "x");
    // Assigned on every path first, the same update is fine.
    let fine = "def f(c):\n    x = 0\n    if c:\n        x = 1\n    x += 1\n    return x\n";
    lower_in_loop(fine, "f(i == 0)").expect("lowers");
}

#[test]
fn a_path_ended_by_diag_fail_does_not_count_against_the_join() {
    let helpers = "def f(c):\n    if c:\n        x = 1\n    else:\n        diag.fail(\"E\", \"d\")\n    return x\n";
    lower_in_loop(helpers, "f(i == 0)").expect("lowers");
}

#[test]
fn a_path_ended_by_continue_does_not_count_against_the_join() {
    // The read after the branch is reached only when `x` was assigned.
    let lowers = "\
def f(n):
    s = 0
    for j in range(n):
        if j == 1:
            continue
        x = j
        s = s + x
    return s
";
    lower_in_loop(lowers, "f(3)").expect("lowers");
    // Here a path reaches the read with `x` unassigned: only the `continue` path is dropped.
    let refused = "\
def f(n):
    s = 0
    for j in range(n):
        if j == 1:
            continue
        if j == 2:
            x = j
        s = s + x
    return s
";
    assert_refused_reading(lower_in_loop(refused, "f(3)"), "x");
    // A `continue` belongs to its own loop: in an inner loop it skips the inner assignment,
    // which the outer loop's code after the inner loop must not count.
    let nested = "\
def f():
    for a in range(1):
        for u in range(2):
            if u >= 0:
                continue
            z = u
        r = z
    return 0
";
    assert_refused_reading(lower_in_loop(nested, "f()"), "z");
}

#[test]
fn a_constant_range_loop_that_runs_assigns_for_the_code_after_it() {
    let tail = "    return y\n";
    for over in [
        "range(3)",
        "range(1, 4)",
        "range(0, 3, 1)",
        "range(5, 0, -2)",
        "range(N)",
        "range(M - 1)",
    ] {
        let helpers =
            format!("N = 3\nM = N + 1\n\ndef f():\n    for t in {over}:\n        y = t\n{tail}");
        lower_in_loop(&helpers, "f()").unwrap_or_else(|e| panic!("{over}: {e:#?}"));
    }
    // The assignment past a `continue` on some iteration is not certain for the last one.
    let skipped = "def f():\n    for t in range(3):\n        if t == 2:\n            continue\n        y = t\n    return y\n";
    assert_refused_reading(lower_in_loop(skipped, "f()"), "y");
    // A loop that may not run, a bound known at run time, a hidden constant, and a `break`
    // keep the rule that the body assigns nothing after the loop.
    for helpers in [
        "def f():\n    for t in range(0):\n        y = t\n    return y\n",
        "def f():\n    for t in range(3, 1):\n        y = t\n    return y\n",
        "def f():\n    for t in range(0, 3, -1):\n        y = t\n    return y\n",
        "def f():\n    for t in range(3, 0, 1):\n        y = t\n    return y\n",
        "def f(n):\n    for t in range(n):\n        y = t\n    return y\n",
        "N = 3\n\ndef f(N):\n    for t in range(N):\n        y = t\n    return y\n",
        "def f():\n    for t in range(3):\n        y = t\n        if t == 2:\n            break\n    return y\n",
    ] {
        let call = if helpers.contains("f(n)") || helpers.contains("f(N)") {
            "f(3)"
        } else {
            "f()"
        };
        assert_refused_reading(lower_in_loop(helpers, call), "y");
    }
}

#[test]
fn a_section_error_lands_on_the_call_that_produced_the_section() {
    // `outer` requires recovery before the recovery point; `inner`, called inside it, is
    // interruptible. The refusal is at the plan's call of `outer`, not at the call of `inner`.
    let helpers = "\
# @section interruptible expected=5
def inner():
    diag.wait(7)

# @section recoveryRequired expected=9
def outer():
    inner()
    diag.wait(8)
";
    let source = plan_calling(helpers, "    outer()\n    res = diag.flash_transfer(1)\n");
    let errors = lower_plan(&source, vec![plan_segment()]).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, line_of(&source, "    outer()"))],
        "{errors:#?}"
    );
    // The hint names the function, not an index of the lowered program.
    assert!(errors[0].hint.contains("`outer`"), "{}", errors[0].hint);
    assert!(!errors[0].hint.contains("section 0"), "{}", errors[0].hint);
}

#[test]
fn a_callee_that_assigns_before_it_reads_runs_right_in_a_loop() {
    // `g` assigns on both branches, `h` reads its loop variable inside the loop only, and `p`
    // reads a parameter: none may read a stale value on the second pass.
    let helpers = "\
def g(c):
    if c:
        x = 1
    else:
        x = 2
    return x

def h(n):
    s = 0
    for j in range(n):
        s = s + j
    return s

def p(a):
    if a > 5:
        return 0
    y = a
    return y + 10
";
    let body = r#"
    r = 0
    for i in range(2):
        r = r + g(i == 0) + h(3) + p(i)
    if r == 33:
        diag.log(1, "right")
    res = diag.flash_transfer(1)
"#;
    // Pass 0: 1 + 3 + 10 = 14; pass 1: 2 + 3 + 11 = 16; together 30.
    let (_, calls) = run_plan(&plan_calling(helpers, &body.replace("r == 33", "r == 30")));
    assert_eq!(logs(&calls), ["log 1 right"], "{calls:?}");
}

/// Lowers `source` as `lower_plan` does, with `max_copied` as the instruction budget.
fn lower_plan_limited(source: &str, max_copied: usize) -> Result<Program, Vec<StarError>> {
    let bytes = flash_document_with(Some(NO_ERASE), vec![plan_segment()]);
    let variant = load_ecu_document(&bytes)
        .expect("the document loads")
        .variant()
        .expect("a variant");
    let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let requests = flash_index(&[]);
    crate::star::on_parser_stack(|| lower_here("t.star", &checked, variant, &requests, max_copied))
        .map(|lowered| lowered.program)
}

#[test]
fn the_instruction_budget_counts_each_expansion_once() {
    let wait = "    diag.wait(1)\n";
    let tail = "    res = diag.flash_transfer(1)\n";
    // A chain of 300 functions over a leaf of 200 waits holds about 400 instructions in all.
    // Counted again at each level, as the whole body of each caller, it would pass 100,000.
    let mut helpers = String::new();
    for level in 0..300 {
        helpers += &format!("def c{level}():\n    c{}()\n\n", level + 1);
    }
    helpers += &format!("def c300():\n{}", wait.repeat(200));
    let chain = plan_calling(&helpers, &format!("    c0()\n{tail}"));
    let program = lower_plan_limited(&chain, 5000).expect("lowers");
    program.validate().expect("valid");
    assert!(program.code.len() < 1000, "{}", program.code.len());
    // The last expansion is charged too: one call of a body past the budget is refused, at the
    // call, and the same call passes a larger budget.
    let big = plan_calling(
        &format!("def leaf():\n{}", wait.repeat(100)),
        &format!("    leaf()\n{tail}"),
    );
    let errors = lower_plan_limited(&big, 50).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::SpecializationLimit, line_of(&big, "    leaf()"))]
    );
    lower_plan_limited(&big, 100_000).expect("lowers");
    // Many calls of a modest body pass the budget as well.
    let many = plan_calling(
        &format!("def leaf():\n{}", wait.repeat(50)),
        &format!("{}{tail}", "    leaf()\n".repeat(20)),
    );
    let errors = lower_plan_limited(&many, 900).expect_err("refused");
    assert!(
        errors
            .iter()
            .all(|e| e.code == StarCode::SpecializationLimit),
        "{errors:#?}"
    );
    lower_plan_limited(&many, 100_000).expect("lowers");
}

/// The idempotency of the single-instruction sections at `pc`, one for each section covering it
/// with a piece of its own.
fn pieces_at(program: &Program, pc: usize) -> Vec<String> {
    program
        .sections
        .iter()
        .filter(|s| s.start_pc as usize == pc && s.end_pc as usize == pc + 1)
        .map(|s| format!("{:?}", s.idempotency))
        .collect()
}

#[test]
fn an_annotation_in_a_callee_applies_to_its_primitive() {
    // The `CheckState` default of a write request in a callee is checked against the example
    // ECU's services in `tests/plan_callees.rs`.
    let tail = "    res = diag.flash_transfer(1)\n";
    // The annotation reaches the callee's first wait only: not its second, nor the caller's.
    let helpers =
        "def quiet():\n    # @idempotency checkState\n    diag.wait(1)\n    diag.wait(2)\n";
    let program = lower_plan(
        &plan_calling(helpers, &format!("    quiet()\n    diag.wait(3)\n{tail}")),
        vec![plan_segment()],
    )
    .expect("lowers");
    let waits: Vec<usize> = (0..program.code.len())
        .filter(|&pc| matches!(program.code[pc], Op::Wait { .. }))
        .collect();
    let found: Vec<_> = waits.iter().map(|&pc| pieces_at(&program, pc)).collect();
    assert_eq!(
        found,
        [["CheckState"], ["Safe"], ["Safe"]],
        "{:?}",
        program.code
    );
}

#[test]
fn break_and_continue_do_not_cross_an_expansion() {
    let tail = "    res = diag.flash_transfer(1)\n";
    // A callee's own loop keeps its `break`; the caller's loop is not reachable from it.
    let own = plan_calling(
        "def first(n):\n    i = 0\n    for i in range(n):\n        if i == 2:\n            break\n    return i\n",
        &format!("    for k in range(2):\n        r = first(5)\n{tail}"),
    );
    lower_plan(&own, vec![plan_segment()]).expect("lowers");
    let outside = plan_calling(
        "def stop():\n    break\n",
        &format!("    for k in range(2):\n        stop()\n{tail}"),
    );
    // Either the checker or the lowering refuses it; it never jumps out of the expansion.
    match check("t.star", &outside) {
        Err(errors) => assert!(!errors.is_empty()),
        Ok(_) => {
            let errors = lower_plan(&outside, vec![plan_segment()]).expect_err("refused");
            assert_eq!(errors[0].code, StarCode::NotLowerable);
            assert!(
                errors[0].hint.contains("belong in a loop"),
                "{}",
                errors[0].hint
            );
        }
    }
}

#[test]
fn a_section_function_in_another_section_function_keeps_both_sets_of_pieces() {
    let helpers = "\
# @section recoveryRequired expected=700
def inner():
    diag.wait(7)

# @section interruptible expected=500
def outer():
    diag.wait(6)
    inner()
    diag.wait(5)
";
    let body = "    res = diag.flash_transfer(1)\n    outer()\n";
    let (program, calls) = run_plan(&plan_calling(helpers, body));
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.starts_with("wait"))
            .collect::<Vec<_>>(),
        ["wait 6", "wait 7", "wait 5"],
        "{calls:?}"
    );
    let covering = |pc: u32| -> Vec<(u32, String)> {
        let mut found: Vec<_> = program
            .sections
            .iter()
            .filter(|s| (s.start_pc..s.end_pc).contains(&pc))
            .map(|s| (s.expected_millis, format!("{:?}", s.interruptible)))
            .collect();
        found.sort();
        found
    };
    let waits: Vec<u32> = (0..program.code.len() as u32)
        .filter(|&pc| matches!(program.code[pc as usize], Op::Wait { .. }))
        .collect();
    // The waits of `outer` lie in the plan's and in `outer`'s sections, the inner one also in
    // `inner`'s.
    let plan = (1000, "No".to_owned());
    let outer = (500, "Yes".to_owned());
    let inner = (700, "RecoveryRequired".to_owned());
    let (first, middle, last) = (
        waits[waits.len() - 3],
        waits[waits.len() - 2],
        waits[waits.len() - 1],
    );
    assert_eq!(covering(first), [outer.clone(), plan.clone()]);
    assert_eq!(covering(middle), [outer.clone(), inner, plan.clone()]);
    assert_eq!(covering(last), [outer, plan]);
}

#[test]
fn main_locals_keep_their_values_across_a_plan_with_callees() {
    let helpers = "def twice(a):\n    b = a * 2\n    return b\n";
    let source = format!(
        "{helpers}\n# @section uninterruptible expected=1000\n# @flashPlan session=1\n\
         def write():\n    c = twice(4)\n    d = twice(c)\n    res = diag.flash_transfer(1)\n\n\
         # @preconditions voltage externalSupply ignition\n\
         def main():\n    a = 5\n    b = 7\n    write()\n    if a == 5 and b == 7:\n        \
         diag.log(1, \"kept\")\n    a = twice(a)\n    if a == 10:\n        diag.log(1, \"after\")\n"
    );
    let (_, calls) = run_plan(&source);
    assert_eq!(logs(&calls), ["log 1 kept", "log 1 after"], "{calls:?}");
}

#[test]
fn a_flash_plan_with_parameters_is_not_lowered_yet() {
    let source = PLAN_NO_ERASE
        .replace("def write():", "def write(x):")
        .replace("    write()\n", "    write(1)\n");
    let errors = lower_plan(&source, vec![plan_segment()]).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, 9)]
    );
}

#[test]
fn the_session_of_the_transfer_is_evaluated_and_compared_with_the_annotation() {
    // A constant expression the checker does not evaluate is evaluated by the expansion: the
    // same session lowers, another one is a plan shape error at the argument.
    let with_session = |expression: &str| {
        format!(
            "SESSION = 1\n{}",
            PLAN_NO_ERASE.replace(
                "flash_transfer(1)",
                &format!("flash_transfer({expression})")
            )
        )
    };
    let same = with_session("SESSION * 1 + len([1, 2]) - 2");
    lower_plan(&same, vec![plan_segment()])
        .expect("lowers")
        .validate()
        .expect("valid");
    let other = with_session("SESSION * 2");
    let errors = lower_plan(&other, vec![plan_segment()]).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::PlanShape, 6)]
    );
    assert!(
        errors[0].hint.contains("names session 2"),
        "{}",
        errors[0].hint
    );
    // A session known only at run time is no constant.
    let run_time = PLAN_NO_ERASE.replace(
        "    res = diag.flash_transfer(1)",
        "    n = diag.precondition(\"voltage\")\n    res = diag.flash_transfer(n)",
    );
    let errors = lower_plan(&run_time, vec![plan_segment()]).expect_err("refused");
    assert_eq!(errors[0].code, StarCode::NonConstantArgument);
}

fn segment(
    length: u64,
    block_size: u32,
    format: u8,
    start: u64,
) -> diag_ir::declaration::FlashSegmentT {
    let mut segment = plan_segment();
    segment.length = length;
    segment.block_size = block_size;
    segment.address_and_length_format = format;
    segment.start_address = start;
    segment
}

#[test]
fn the_download_request_is_built_from_the_segment_and_its_format() {
    let operand_of = |segment| {
        let program = lower_plan(PLAN_NO_ERASE, vec![segment]).expect("lowers");
        let download = position(&program, 0, |op| {
            matches!(op, Op::ServiceRequest { service: 0x34 })
        });
        let Op::PushBytes(index) = program.code[download as usize - 1] else {
            panic!("the operand is pushed before the request");
        };
        program.constants[index as usize].clone()
    };
    // The default format 0x44: four bytes of address, four of length, after the data format.
    assert_eq!(
        operand_of(plan_segment()),
        [0x00, 0x44, 0, 0, 0x10, 0, 0, 0, 0, 0x25]
    );
    // Another data format and widths: two bytes of address, three of length.
    let mut other = segment(0x25, 0x10, 0x32, 0xBEEF);
    other.data_format = 0x11;
    assert_eq!(operand_of(other), [0x11, 0x32, 0xBE, 0xEF, 0, 0, 0x25]);
    // The widest fields.
    assert_eq!(
        operand_of(segment(1, 1, 0x88, u64::MAX)),
        [&[0x00, 0x88][..], &[0xFF; 8], &[0, 0, 0, 0, 0, 0, 0, 1]].concat()
    );
}

#[test]
fn a_value_that_does_not_fit_the_format_widths_is_not_lowerable() {
    for (segment, name) in [
        // An address of three bytes in a two-byte field.
        (segment(0x25, 0x10, 0x42, 0x1_0000), "address"),
        // A length of two bytes in a one-byte field.
        (segment(0x100, 0x10, 0x14, 0x10), "length"),
        // A zero-width length cannot hold the length.
        (segment(0x25, 0x10, 0x04, 0x10), "zero length width"),
        // Nor can a zero-width address field, even for the address 0.
        (segment(0x25, 0x10, 0x40, 0), "zero address width"),
    ] {
        let errors = lower_plan(PLAN_NO_ERASE, vec![segment]).expect_err(name);
        assert_eq!(
            errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
            [(StarCode::NotLowerable, 5)],
            "{name}"
        );
        assert!(
            errors[0].hint.contains("does not fit"),
            "{name}: {}",
            errors[0].hint
        );
    }
    // The largest value of each width fits.
    lower_plan(PLAN_NO_ERASE, vec![segment(0xFF, 0x10, 0x11, 0xFF)]).expect("fits");
}

#[test]
fn a_block_size_above_what_a_transfer_takes_is_not_lowerable() {
    let most = diag_ir::MAX_BYTES_LEN as u32 - 2;
    // The largest block size lowers (one block of that size); one more does not.
    lower_plan(PLAN_NO_ERASE, vec![segment(u64::from(most), most, 0x44, 0)]).expect("lowers");
    // Only the longest block that exists counts: a block size above the length is one block.
    lower_plan(PLAN_NO_ERASE, vec![segment(0x25, u32::MAX, 0x44, 0)]).expect("lowers");
    let errors = lower_plan(
        PLAN_NO_ERASE,
        vec![segment(u64::from(most) + 1, most + 1, 0x44, 0)],
    )
    .expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, 5)]
    );
    assert!(errors[0].hint.contains("block_size"), "{}", errors[0].hint);
}

#[test]
fn more_blocks_than_the_journal_numbers_are_not_lowerable() {
    // 2^32 - 1 blocks of one byte lower; 2^32 do not (the length fits six bytes).
    let blocks = u64::from(u32::MAX);
    let program = lower_plan(PLAN_NO_ERASE, vec![segment(blocks, 1, 0x64, 0)]).expect("lowers");
    assert!(program.code.contains(&Op::PushI64(blocks as i64)));
    let errors =
        lower_plan(PLAN_NO_ERASE, vec![segment(blocks + 1, 1, 0x64, 0)]).expect_err("refused");
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(StarCode::NotLowerable, 5)]
    );
    assert!(errors[0].hint.contains("blocks"), "{}", errors[0].hint);
}

#[test]
fn a_session_with_several_segments_or_none_is_not_lowerable() {
    for segments in [Vec::new(), vec![plan_segment(), plan_segment()]] {
        let count = segments.len();
        let errors = lower_plan(PLAN_NO_ERASE, segments).expect_err("refused");
        assert_eq!(
            errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
            [(StarCode::NotLowerable, 5)],
            "{count} segments"
        );
    }
}

#[test]
fn a_lowered_plan_transfers_the_artifact_block_by_block() {
    let program = lower_plan(PLAN_NO_ERASE, vec![plan_segment()]).expect("lowers");
    let bytes = flash_document(Some(NO_ERASE));
    let variant = load_ecu_document(&bytes)
        .expect("the document loads")
        .variant()
        .expect("a variant");
    // The segment is 0x25 bytes from offset 4 in blocks of 0x10: 16, 16 and 5 bytes.
    let artifact: Vec<u8> = (0..64u8).map(|byte| byte.wrapping_mul(3)).collect();
    let mut host = ScriptedHost {
        // The RequestDownload's answer, which is dropped, and the RequestTransferExit's, which
        // the plan's `res` holds.
        responses: [vec![0x74, 0x20, 0x01, 0x00], vec![0x77]].into(),
        artifact: artifact.clone(),
        ..ScriptedHost::default()
    };
    let mut vm = Vm::new(&program);
    for _ in 0..1000 {
        match vm.step_with_declarations(&program, Some(&variant), &mut host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(host.responses.is_empty(), "both requests were sent");
    let block = |from: usize, to: usize| hex(&artifact[from..to]);
    assert_eq!(
        host.calls,
        [
            "wait 1".to_owned(),
            // The format bytes, then the address and the length in four bytes each.
            "request 34 00440000100000000025".to_owned(),
            "artifact 4 16".to_owned(),
            format!("transfer 0 {}", block(4, 20)),
            "artifact 20 16".to_owned(),
            format!("transfer 0 {}", block(20, 36)),
            "artifact 36 5".to_owned(),
            format!("transfer 0 {}", block(36, 41)),
            // The transfer exit is the service id alone.
            "request 37 ".to_owned(),
        ]
    );
}

#[test]
fn a_flash_plan_names_a_session_of_the_variant() {
    let requests = flash_index(&[]);
    assert_eq!(
        flash_codes(PLAN_NO_ERASE, None, &requests),
        [(StarCode::UnknownFlashSession, 2)]
    );
    let other = PLAN_NO_ERASE
        .replace("session=1", "session=2")
        .replace("flash_transfer(1)", "flash_transfer(2)");
    assert_eq!(
        flash_codes(&other, Some(NO_ERASE), &requests),
        [(StarCode::UnknownFlashSession, 2)]
    );
}

#[test]
fn erase_is_required_exactly_when_the_session_erases() {
    let requests = flash_index(&[]);
    // A session with an erase step needs the `@erase`; one without it refuses the `@erase`.
    assert_eq!(
        flash_codes(PLAN_NO_ERASE, Some(WITH_ERASE), &requests),
        [(StarCode::PlanShape, 2)]
    );
    assert_eq!(
        flash_codes(PLAN_ERASE, Some(NO_ERASE), &requests),
        [(StarCode::PlanShape, 4)]
    );
}

#[test]
fn a_restartable_flash_plan_needs_the_identity_rows() {
    let never = PLAN_NO_ERASE.replace("session=1", "session=1 recovery=never");
    // A marker on a statement after the transfer, which a no-erase session's erase point
    // (the RequestDownload) precedes.
    let marked = PLAN_NO_ERASE.replace(
        "    res = diag.flash_transfer(1)",
        "    res = diag.flash_transfer(1)\n    # @recoveryRequired\n    diag.wait(2)",
    );
    let all = ["vin", "hardware_part_number", "software_version"];
    for source in [&never, &marked] {
        // One error, naming the rows that are missing.
        let errors = flash_errors(source, Some(NO_ERASE), &flash_index(&["vin"]));
        assert_eq!(
            errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
            [(StarCode::PlanShape, 2)]
        );
        assert!(
            errors[0]
                .hint
                .contains("missing: hardware_part_number, software_version"),
            "{}",
            errors[0].hint
        );
        // With every row it lowers.
        assert_eq!(flash_codes(source, Some(NO_ERASE), &flash_index(&all)), []);
    }
    // A marker that is not after the erase point leaves the default: no restart.
    let wait_marked = |source: &str, before: &str| {
        source.replace(before, &format!("    # @recoveryRequired\n{before}"))
    };
    let requests = flash_index(&[]);
    for (source, steps) in [
        // On the transfer's statement of a session without an erase step.
        (
            wait_marked(PLAN_NO_ERASE, "    res = diag.flash_transfer(1)"),
            NO_ERASE,
        ),
        // Before the erase, and on the `@erase` statement itself.
        (
            PLAN_ERASE.replace(
                "    # @erase",
                "    # @recoveryRequired\n    diag.wait(0)\n    # @erase",
            ),
            WITH_ERASE,
        ),
        (
            PLAN_ERASE.replace("    # @erase\n", "    # @erase\n    # @recoveryRequired\n"),
            WITH_ERASE,
        ),
    ] {
        let codes = flash_codes(&source, Some(steps), &requests);
        assert!(!codes.contains(&(StarCode::PlanShape, 2)), "{codes:?}");
    }
    // After the erase and before the transfer it is restartable.
    let between = wait_marked(PLAN_ERASE, "    res = diag.flash_transfer(1)");
    assert_eq!(
        flash_codes(&between, Some(WITH_ERASE), &requests),
        [(StarCode::PlanShape, 2)]
    );
    // The default point, at the erase, and a section that requires on-site intervention from
    // its entry allow no restart: the identity rows are not needed.
    let requests = flash_index(&[]);
    assert_eq!(flash_codes(PLAN_NO_ERASE, Some(NO_ERASE), &requests), []);
    assert_eq!(
        flash_codes(
            &PLAN_NO_ERASE.replace("uninterruptible", "recoveryRequired"),
            Some(NO_ERASE),
            &requests
        ),
        []
    );
}

#[test]
fn a_flash_plan_is_for_uds_only() {
    static OTHER: Profile = Profile {
        layer: "TEST",
        calls: &[],
        negative: crate::profile::NEGATIVE_7F,
        pending: &[0x78],
    };
    let bytes = flash_document(Some(NO_ERASE));
    let variant = load_ecu_document(&bytes)
        .expect("loads")
        .variant()
        .expect("a variant");
    let over = |source: &str| -> Vec<(StarCode, u32)> {
        let requests = RequestIndex {
            profile: &OTHER,
            ..flash_index(&[])
        };
        let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
        match lower("t.star", &checked, variant, &requests) {
            Ok(_) => Vec::new(),
            Err(errors) => errors.iter().map(|e| (e.code, e.line)).collect(),
        }
    };
    // The annotation is refused, and so is the call, wherever it is.
    assert_eq!(over(PLAN_NO_ERASE), [(StarCode::NotInProfile, 2)]);
    assert_eq!(
        over("def main():\n    diag.flash_transfer(1)\n"),
        [(StarCode::NotInProfile, 2)]
    );
}

#[test]
fn the_documented_flash_plan_passes_every_check() {
    let source = "\
# @section uninterruptible expected=180000
# @flashPlan session=1 maxResumes=3 recovery=never
def write_application():
    diag.request(0x10, {\"session\": 0x02})
    diag.security_access(0x11)
    # @erase
    diag.routine(0xFF00, 0x01, {})
    res = diag.flash_transfer(1)
    if not res[\"ok\"]:
        diag.fail(\"TRANSFER_EXIT_REFUSED\", \"\")
    diag.routine(0xFF01, 0x01, {})
    diag.request(0x11, {\"reset\": 0x01})

# @preconditions voltage externalSupply ignition engine vehicleSpeed
def main():
    write_application()
";
    check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let all = ["vin", "hardware_part_number", "software_version"];
    // The index has rows for the three mandatory preconditions only.
    let source = &source.replace(" engine vehicleSpeed", "");
    let codes = flash_codes(source, Some(WITH_ERASE), &flash_index(&all));
    // Nothing of the plan's own checks and nothing at the transfer; the empty variant's services
    // remain.
    assert!(
        codes.iter().all(|(code, _)| matches!(
            code,
            StarCode::UnknownService | StarCode::NotLowerable | StarCode::UnsupportedSyntax
        )),
        "{codes:?}"
    );
    assert!(codes.iter().all(|&(_, line)| line != 8), "{codes:?}");
}

#[test]
fn a_recovery_marker_on_a_statement_that_emits_nothing_is_refused() {
    // A docstring and a `pass` emit no instruction, so the point would fall back to the erase.
    let docstring = PLAN_NO_ERASE.replace(
        "def write():\n",
        "def write():\n    # @recoveryRequired\n    \"The plan.\"\n",
    );
    let trailing = PLAN_NO_ERASE.replace(
        "    res = diag.flash_transfer(1)\n",
        "    res = diag.flash_transfer(1)\n    # @recoveryRequired\n    pass\n",
    );
    let requests = flash_index(&["vin", "hardware_part_number", "software_version"]);
    for (source, line) in [(docstring, 5), (trailing, 7)] {
        let found = flash_errors(&source, Some(NO_ERASE), &requests);
        assert_eq!(
            found.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
            [(StarCode::BadAnnotation, line)],
            "{source}"
        );
    }
}

#[test]
fn a_local_of_main_live_across_a_plan_keeps_its_value() {
    // `main` holds `x` across the plan, which has a local of the same name: the plan's frame is
    // shifted past `main`'s, so the plan's `x` is another slot.
    let source = "\
# @section uninterruptible expected=1000
# @flashPlan session=1
def write():
    x = diag.precondition(\"ignition\")
    diag.log(1, \"plan x=\" + (\"one\" if x == 1 else \"other\"))
    res = diag.flash_transfer(1)

# @preconditions voltage externalSupply ignition
def main():
    x = diag.precondition(\"voltage\")
    write()
    if x == 5:
        diag.log(1, \"main kept x\")
    else:
        diag.log(1, \"main lost x\")
";
    let program = lower_plan(source, vec![plan_segment()]).expect("lowers");
    program.validate().expect("valid");
    let bytes = flash_document(Some(NO_ERASE));
    let variant = load_ecu_document(&bytes)
        .expect("loads")
        .variant()
        .expect("a variant");
    let mut host = ScriptedHost {
        responses: [vec![0x74, 0x20, 0x01, 0x00], vec![0x77]].into(),
        artifact: vec![0; 64],
        inputs: vec![
            (RuntimeInput::SupplyVoltageMillivolts, 5),
            (RuntimeInput::IgnitionOn, 1),
        ],
        ..ScriptedHost::default()
    };
    let mut vm = Vm::new(&program);
    for _ in 0..1000 {
        match vm.step_with_declarations(&program, Some(&variant), &mut host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => break,
            other => panic!("{other:?}"),
        }
    }
    let logged: Vec<_> = host.calls.iter().filter(|c| c.starts_with("log")).collect();
    assert_eq!(logged, ["log 1 plan x=one", "log 1 main kept x"]);
}

// ---------------------------------------------------------------- diag.hmi and diag.record
// (subset 3.1, 3.2, 3.4, ADR-340)

fn form(id: &str, params: &[(&str, HmiValue)]) -> HmiForm {
    HmiForm {
        id: id.to_owned(),
        params: params
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    }
}

fn text(value: &str) -> HmiValue {
    HmiValue::Text(value.to_owned())
}

#[test]
fn a_form_with_constant_params_lowers_to_hmi_request_and_reaches_the_host_decoded() {
    let source = "\
def main():
    ans = diag.hmi(\"confirm\", {\"title\": \"Ignition off\", \"count\": 2, \"strict\": True})
    if ans[\"ok\"]:
        diag.log(1, \"ok \" + ans[\"value\"])
    if ans[\"timed_out\"]:
        diag.log(1, \"timed out\")
";
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    assert!(
        program
            .code
            .iter()
            .any(|op| matches!(op, Op::HmiRequest { .. }))
    );
    assert!(
        !program
            .code
            .iter()
            .any(|op| matches!(op, Op::HmiRequestFromStack { .. }))
    );
    let mut host = ScriptedHost::answering(&[hmi::answered("yes")]);
    run_on(source, &mut host);
    assert_eq!(
        host.forms(),
        [(
            "hmi",
            form(
                "confirm",
                &[
                    ("title", text("Ignition off")),
                    ("count", HmiValue::Int(2)),
                    ("strict", HmiValue::Bool(true)),
                ]
            )
        )]
    );
    assert_eq!(host.calls, ["log 1 ok yes"]);
}

#[test]
fn hmi_and_record_are_safe_primitives_with_a_section_each() {
    let source = "def main():\n    diag.hmi(\"f\", {})\n    diag.record(\"t\")\n";
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    program.validate().expect("valid");
    assert_sections_split(&program);
    assert_eq!(primitives_of(&program), ["Safe", "Safe"]);
    let position = |wanted: fn(&Op) -> bool| program.code.iter().position(wanted);
    assert!(
        position(|op| matches!(op, Op::HmiRequest { .. }))
            < position(|op| matches!(op, Op::RecordInput { .. }))
    );
}

#[test]
fn constant_and_run_time_params_are_filled_in_source_order() {
    let source = "\
def ask(n, who, on):
    ans = diag.hmi(\"mixed\", {\"a\": 1, \"n\": n, \"b\": \"x\", \"who\": who, \"on\": on, \"c\": False})
    diag.log(1, \"value \" + ans[\"value\"])

def main():
    ask(7, \"ecu\", True)
    ask(-3, \"\", False)
";
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    assert!(
        program
            .code
            .iter()
            .any(|op| matches!(op, Op::HmiRequestFromStack { .. }))
    );
    let mut host = ScriptedHost::answering(&[hmi::answered("one"), hmi::answered("two")]);
    run_on(source, &mut host);
    let filled = |n, who: &str, on| {
        (
            "hmi",
            form(
                "mixed",
                &[
                    ("a", HmiValue::Int(1)),
                    ("n", HmiValue::Int(n)),
                    ("b", text("x")),
                    ("who", text(who)),
                    ("on", HmiValue::Bool(on)),
                    ("c", HmiValue::Bool(false)),
                ],
            ),
        )
    };
    assert_eq!(
        host.forms(),
        [filled(7, "ecu", true), filled(-3, "", false)]
    );
    assert_eq!(host.calls, ["log 1 value one", "log 1 value two"]);
}

#[test]
fn a_run_time_param_is_computed_before_the_instruction_in_source_order() {
    let source = "\
def ask(n):
    diag.hmi(\"f\", {\"first\": n + 1, \"second\": n * 2})

def main():
    ask(5)
";
    let mut host = ScriptedHost::answering(&[hmi::answered("")]);
    run_on(source, &mut host);
    assert_eq!(
        host.forms(),
        [(
            "hmi",
            form(
                "f",
                &[("first", HmiValue::Int(6)), ("second", HmiValue::Int(10))]
            )
        )]
    );
}

#[test]
fn a_record_lowers_to_record_input_and_its_value_is_the_judgment() {
    let source = "\
def main():
    ans = diag.record(\"final_check\")
    if ans[\"ok\"] and ans[\"value\"] == \"ok\":
        diag.log(1, \"passed\")
    else:
        diag.log(1, \"not passed\")
";
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    assert!(
        program
            .code
            .iter()
            .any(|op| matches!(op, Op::RecordInput { .. }))
    );
    let mut host = ScriptedHost::answering(&[hmi::answered("ok")]);
    run_on(source, &mut host);
    assert_eq!(host.forms(), [("record", form("final_check", &[]))]);
    assert_eq!(host.calls, ["log 1 passed"]);
    let mut host = ScriptedHost::answering(&[hmi::answered("ng")]);
    run_on(source, &mut host);
    assert_eq!(host.calls, ["log 1 not passed"]);
}

/// Logs what an answer reads as.
const READ_ANSWER: &str = "\
def main():
    ans = diag.hmi(\"f\", {})
    diag.log(1, \"ok \" + (\"yes\" if ans[\"ok\"] else \"no\"))
    diag.log(1, \"timed_out \" + (\"yes\" if ans[\"timed_out\"] else \"no\"))
    diag.log(1, \"value [\" + ans[\"value\"] + \"]\")
";

fn answer_reads(answer: Vec<u8>) -> Vec<String> {
    let mut host = ScriptedHost::answering(&[answer]);
    run_on(READ_ANSWER, &mut host);
    host.calls
}

#[test]
fn a_decline_reads_as_not_ok_with_an_empty_value() {
    assert_eq!(
        answer_reads(hmi::declined()),
        ["log 1 ok no", "log 1 timed_out no", "log 1 value []"]
    );
}

#[test]
fn a_timed_out_answer_is_read_through_timed_out() {
    assert_eq!(
        answer_reads(hmi::timed_out()),
        ["log 1 ok no", "log 1 timed_out yes", "log 1 value []"]
    );
}

#[test]
fn an_operator_answer_reads_as_ok_with_its_text() {
    assert_eq!(
        answer_reads(hmi::answered("12.5")),
        ["log 1 ok yes", "log 1 timed_out no", "log 1 value [12.5]"]
    );
    assert_eq!(
        answer_reads(hmi::answered("")),
        ["log 1 ok yes", "log 1 timed_out no", "log 1 value []"]
    );
}

#[test]
fn the_answer_reads_are_total_over_bytes_that_break_the_contract() {
    // An empty answer and one with an unknown status are not ok; `value` is what follows the
    // first byte (ADR-340 item 3).
    assert_eq!(
        answer_reads(Vec::new()),
        ["log 1 ok no", "log 1 timed_out no", "log 1 value []"]
    );
    assert_eq!(
        answer_reads(b"\x07rest".to_vec()),
        ["log 1 ok no", "log 1 timed_out no", "log 1 value [rest]"]
    );
}

#[test]
fn a_form_that_is_not_answered_yet_is_polled_with_the_same_bytes_and_inquiry() {
    let source = "\
def ask(n):
    ans = diag.hmi(\"f\", {\"fixed\": 1, \"n\": n})
    diag.log(1, \"value \" + ans[\"value\"])

def main():
    ask(4)
";
    let mut host = ScriptedHost {
        hmi_pending: 2,
        ..ScriptedHost::answering(&[hmi::answered("done")])
    };
    let waited = run_on(source, &mut host);
    assert_eq!(waited, 2);
    assert_eq!(host.sent.len(), 3);
    assert!(host.sent.iter().all(|sent| *sent == host.sent[0]));
    assert_eq!(host.calls, ["log 1 value done"]);
    // The same holds for a record.
    let mut host = ScriptedHost {
        hmi_pending: 1,
        ..ScriptedHost::answering(&[hmi::answered("")])
    };
    run_on("def main():\n    diag.record(\"t\")\n", &mut host);
    assert_eq!(host.sent.len(), 2);
    assert_eq!(host.sent[0], host.sent[1]);
}

#[test]
fn an_answer_can_be_kept_and_read_later_and_a_form_id_may_be_128_bytes() {
    let id = "f".repeat(128);
    let source = format!(
        "def main():\n    ans = diag.hmi(\"{id}\", {{\"_k1\": \"\"}})\n    diag.wait(1)\n    if ans[\"ok\"]:\n        diag.log(1, \"kept\")\n"
    );
    let mut host = ScriptedHost::answering(&[hmi::answered("")]);
    run_on(&source, &mut host);
    assert_eq!(host.forms(), [("hmi", form(&id, &[("_k1", text(""))]))]);
    assert_eq!(host.calls, ["wait 1", "log 1 kept"]);
}

#[test]
fn hmi_and_record_take_exactly_their_arguments() {
    for source in [
        "def main():\n    diag.hmi(\"f\")\n",
        "def main():\n    diag.hmi()\n",
        "def main():\n    diag.hmi(\"f\", {}, 1)\n",
        "def main():\n    diag.record()\n",
        "def main():\n    diag.record(\"t\", {})\n",
        // The params are a dict literal, not a name.
        "P = {\"a\": 1}\n\ndef main():\n    diag.hmi(\"f\", P)\n",
        "def main():\n    diag.hmi(\"f\", 1)\n",
    ] {
        assert_eq!(
            codes(source),
            [(
                StarCode::UnsupportedSyntax,
                if source.starts_with("P") { 4 } else { 2 }
            )],
            "{source}"
        );
    }
}

#[test]
fn the_id_of_hmi_and_record_is_a_constant_string_of_1_to_128_bytes() {
    let too_long = "x".repeat(129);
    for (source, expected) in [
        (
            "def f(x):\n    diag.hmi(x, {})\n\ndef main():\n    f(\"a\")\n".to_owned(),
            StarCode::NonConstantArgument,
        ),
        (
            "def f(x):\n    diag.record(x)\n\ndef main():\n    f(\"a\")\n".to_owned(),
            StarCode::NonConstantArgument,
        ),
        (
            "def main():\n    diag.hmi(1, {})\n".to_owned(),
            StarCode::UnsupportedSyntax,
        ),
        (
            "def main():\n    diag.record(b\"t\")\n".to_owned(),
            StarCode::UnsupportedSyntax,
        ),
        (
            "def main():\n    diag.hmi(\"\", {})\n".to_owned(),
            StarCode::ArgumentOutOfRange,
        ),
        (
            "def main():\n    diag.record(\"\")\n".to_owned(),
            StarCode::ArgumentOutOfRange,
        ),
        (
            format!("def main():\n    diag.hmi(\"{too_long}\", {{}})\n"),
            StarCode::ArgumentOutOfRange,
        ),
    ] {
        assert_eq!(codes(&source), [(expected, 2)], "{source}");
    }
}

#[test]
fn the_param_names_of_a_form_are_unique_identifiers_of_at_most_64_bytes() {
    let long = "k".repeat(65);
    let many = (0..33)
        .map(|i| format!("\"k{i}\": {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let at_most = (0..32)
        .map(|i| format!("\"k{i}\": {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    for (params, expected) in [
        ("{\"a b\": 1}".to_owned(), Some(StarCode::UnsupportedSyntax)),
        ("{\"1a\": 1}".to_owned(), Some(StarCode::UnsupportedSyntax)),
        ("{\"\": 1}".to_owned(), Some(StarCode::UnsupportedSyntax)),
        ("{\"a-b\": 1}".to_owned(), Some(StarCode::UnsupportedSyntax)),
        (
            "{\"a\" + \"b\": 1}".to_owned(),
            Some(StarCode::DynamicKeyNotAllowed),
        ),
        ("{1: 1}".to_owned(), Some(StarCode::DynamicKeyNotAllowed)),
        (
            format!("{{\"{long}\": 1}}"),
            Some(StarCode::ArgumentOutOfRange),
        ),
        (format!("{{\"{}\": 1}}", "k".repeat(64)), None),
        (
            "{\"a\": 1, \"a\": 2}".to_owned(),
            Some(StarCode::ArgumentOutOfRange),
        ),
        (format!("{{{many}}}"), Some(StarCode::ArgumentOutOfRange)),
        (format!("{{{at_most}}}"), None),
    ] {
        let source = format!("def main():\n    diag.hmi(\"f\", {params})\n");
        let found = match check("t.star", &source) {
            Ok(checked) => match lower_for_a_variant(&checked) {
                Ok(_) => None,
                Err(errors) => {
                    assert_eq!(errors.len(), 1, "{source}: {errors:?}");
                    Some(errors[0].code)
                }
            },
            // The checker refuses a computed key before the lowering sees it.
            Err(errors) => Some(errors[0].code),
        };
        assert_eq!(found, expected, "{params}");
    }
}

#[test]
fn a_param_value_is_an_int_a_bool_or_a_str() {
    for value in ["1.5", "b\"x\"", "[1]", "{}"] {
        let source = format!("def main():\n    diag.hmi(\"f\", {{\"a\": {value}}})\n");
        assert_eq!(
            codes(&source),
            [(StarCode::UnsupportedSyntax, 2)],
            "{value}"
        );
    }
    // The same at run time, and an answer or its parts.
    for (param, argument) in [("x", "1.5"), ("x", "b\"x\"")] {
        let source = format!(
            "def f({param}):\n    diag.hmi(\"f\", {{\"a\": x}})\n\ndef main():\n    f({argument})\n"
        );
        assert_eq!(
            codes(&source),
            [(StarCode::UnsupportedSyntax, 2)],
            "{argument}"
        );
    }
    assert_eq!(
        codes("def main():\n    a = diag.hmi(\"f\", {})\n    diag.hmi(\"g\", {\"a\": a})\n"),
        [(StarCode::UnsupportedSyntax, 3)]
    );
    // A value that can be two types is not one slot.
    assert_eq!(
        codes(
            "def f(x):\n    diag.hmi(\"f\", {\"a\": x})\n\ndef main():\n    f(1)\n    f(\"s\")\n"
        ),
        [(StarCode::NotLowerable, 2)]
    );
}

#[test]
fn a_form_that_encodes_to_more_than_16_kib_is_refused() {
    let big = "x".repeat(4000);
    let params = (0..5)
        .map(|i| format!("\"k{i}\": \"{big}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!("def main():\n    diag.hmi(\"f\", {{{params}}})\n");
    assert_eq!(codes(&source), [(StarCode::ArgumentOutOfRange, 2)]);
}

/// The 16 KiB bound is on the encoding: a form of exactly 16 KiB lowers, one byte more does not.
#[test]
fn a_form_of_exactly_16_kib_lowers() {
    let encoded = |n: usize| {
        hmi::encode_template(&hmi::HmiTemplate {
            id: "f".to_owned(),
            params: vec![(
                "k".to_owned(),
                hmi::HmiParam::Const(HmiValue::Text("x".repeat(n))),
            )],
        })
        .map(|bytes| bytes.len())
    };
    let n = (0..hmi::MAX_HMI_FORM_LEN)
        .find(|&n| encoded(n) == Ok(hmi::MAX_HMI_FORM_LEN))
        .expect("a text length that encodes to exactly 16 KiB");
    let source = |n: usize| {
        format!(
            "def main():\n    diag.hmi(\"f\", {{\"k\": \"{}\"}})\n",
            "x".repeat(n)
        )
    };
    assert_eq!(codes(&source(n)), []);
    assert_eq!(codes(&source(n + 1)), [(StarCode::ArgumentOutOfRange, 2)]);
}

/// The id bound counts bytes of UTF-8, for a record template as for a form.
#[test]
fn a_record_template_id_is_1_to_128_bytes_of_utf8() {
    for (id, expected) in [
        ("t".repeat(128), vec![]),
        ("t".repeat(129), vec![(StarCode::ArgumentOutOfRange, 2)]),
        ("\u{e9}".repeat(64), vec![]),
        (
            format!("{}t", "\u{e9}".repeat(64)),
            vec![(StarCode::ArgumentOutOfRange, 2)],
        ),
    ] {
        let source = format!("def main():\n    diag.record(\"{id}\")\n");
        assert_eq!(codes(&source), expected, "{id}");
    }
}

#[test]
fn an_answer_is_read_only_by_its_three_literal_keys() {
    assert_eq!(
        codes("def main():\n    ans = diag.hmi(\"f\", {})\n    x = ans[\"number\"]\n"),
        [(StarCode::UnsupportedSyntax, 3)]
    );
    assert_eq!(
        codes("def main():\n    x = diag.record(\"t\")[\"fields\"]\n"),
        [(StarCode::UnsupportedSyntax, 2)]
    );
    // A computed key is refused by the checker where it sees the answer, and by the lowering
    // where the answer came through a function.
    let errors = check(
        "t.star",
        "def main():\n    k = \"ok\"\n    ans = diag.hmi(\"f\", {})\n    x = ans[k]\n",
    )
    .expect_err("a computed key");
    assert!(
        errors
            .iter()
            .any(|e| e.code == StarCode::DynamicKeyNotAllowed)
    );
    assert_eq!(
        codes(
            "def read(a, k):\n    return a[k]\n\ndef main():\n    read(diag.hmi(\"f\", {}), \"ok\")\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 2)]
    );
}

#[test]
fn an_answer_cannot_be_used_as_a_value() {
    for body in [
        "if ans:\n        pass",
        "x = not ans",
        "x = ans == 1",
        "x = ans + \"a\"",
        "x = ans and True",
        "x = 1 if ans else 2",
    ] {
        let source = format!("def main():\n    ans = diag.hmi(\"f\", {{}})\n    {body}\n");
        let found = codes(&source);
        assert!(
            !found.is_empty()
                && found
                    .iter()
                    .all(|(code, _)| *code == StarCode::NotLowerable),
            "{body}: {found:?}"
        );
    }
}
