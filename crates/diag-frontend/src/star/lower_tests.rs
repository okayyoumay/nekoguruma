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
    let entries: Vec<String> = (0..100_000).map(|n| format!("\"k{n}\": {n}")).collect();
    let source = format!(
        "D = {{{}}}\nE = D == D\n\ndef main():\n    if E:\n        diag.fail(\"YES\", \"\")\n",
        entries.join(", ")
    );
    let started = std::time::Instant::now();
    assert_eq!(failed_with(&source), "YES");
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
}

#[test]
fn section_and_idempotency_annotations_are_not_dropped() {
    for (source, line) in [
        ("# @section uninterruptible\ndef main():\n    pass\n", 1),
        ("def main():\n    # @idempotency unsafe\n    x = 1\n", 2),
    ] {
        assert_eq!(
            codes(source),
            vec![(StarCode::NotLowerable, line)],
            "{source}"
        );
    }
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
fn string_and_bytes_literals_are_bounded_too() {
    // A literal of 2^20 bytes counts 2^20 + 1, one past the limit; one byte less fits.
    for (quote, prefix) in [("\"", ""), ("\"", "b")] {
        for (len, over) in [((1 << 20) - 1, false), (1 << 20, true)] {
            let source = format!(
                "T = {prefix}{quote}{}{quote}\n\ndef main():\n    pass\n",
                "x".repeat(len)
            );
            assert_eq!(!codes(&source).is_empty(), over, "{prefix} {len}");
        }
    }
}

#[test]
fn constants_nest_at_most_64_deep() {
    // Each line is shallow, but the globals nest one in the next: folding and dropping them
    // would recurse 100,000 deep.
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
        let found = codes(&chain(100_000, wrap));
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
    assert!(lower("t.star", &checked).is_ok());
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
