use diag_ir::{DiagHost, StepOutcome, Vm};

use super::*;
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
    fn log(&mut self, _: u8, _: &str) {}
}

fn lowered(source: &str) -> Result<Program, Vec<StarError>> {
    let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    lower("t.star", &checked)
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
    // NaN is not ordered, so <= is false too.
    assert!(holds(
        "a",
        "0.0",
        "not (a / a <= 1.0) and not (a / a >= 1.0)"
    ));
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
    let result = run("def f(b, i):\n    return b[i]\n\ndef main():\n    f(b\"\\x01\", 5)\n");
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.contains("IndexOutOfRange")),
        "{result:?}"
    );
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
fn what_the_ir_cannot_express_is_not_lowerable() {
    let cases = [
        // Mixed int and float at run time.
        "def f(x):\n    return x + 1.0\n\ndef main():\n    f(1)\n",
        // Float floor division at run time.
        "def f(x):\n    return x // 2.0\n\ndef main():\n    f(1.0)\n",
        // A run-time `<<`.
        "def f(x):\n    return x << 1\n\ndef main():\n    f(1)\n",
        // String concatenation at run time.
        "def f(x):\n    return x + \"a\"\n\ndef main():\n    f(\"b\")\n",
        // A local of two types.
        "def f(x):\n    y = 1\n    if x:\n        y = 1.0\n    return y + y\n\ndef main():\n    f(True)\n",
        // The truth of bytes at run time.
        "def f(x):\n    if x:\n        pass\n\ndef main():\n    f(b\"\")\n",
        // A list that would exist at run time.
        "def main():\n    x = [1, 2]\n",
        // A built-in on a run-time value.
        "def f(x):\n    return abs(x)\n\ndef main():\n    f(1)\n",
        // A loop.
        "def main():\n    for i in range(3):\n        pass\n",
        // A primitive not lowered yet.
        "def main():\n    diag.wait(10)\n",
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
fn diag_functions_without_an_instruction_are_not_available() {
    assert_eq!(
        codes("def main():\n    diag.ecu_info()\n"),
        [(StarCode::ApiNotAvailable, 2)]
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
    let found = codes("def main():\n    diag.wait(1)\n    diag.ecu_info()\n    x = [1]\n");
    assert_eq!(
        found,
        [
            (StarCode::NotLowerable, 2),
            (StarCode::ApiNotAvailable, 3),
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
