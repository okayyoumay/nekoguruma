use diag_ir::declaration::{EcuDocumentT, EcuVariantT, encode_ecu_document, load_ecu_document};
use diag_ir::{DiagHost, StepOutcome, Vm, WaitingOn};

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
    lowered_with_warnings(source).map(|lowered| lowered.program)
}

fn lowered_with_warnings(source: &str) -> Result<Lowered, Vec<StarError>> {
    let checked = check("t.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    lower_for_a_variant(&checked)
}

/// Lowers `checked` for a variant that declares nothing.
fn lower_for_a_variant(checked: &Checked) -> Result<Lowered, Vec<StarError>> {
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("test".to_owned());
    document.source_digest = Some(String::new());
    document.variant = Some(Box::new(EcuVariantT::default()));
    let bytes = encode_ecu_document(&document);
    let variant = load_ecu_document(&bytes)
        .expect("the document loads")
        .variant()
        .expect("a variant");
    lower("t.star", checked, variant)
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
    let entries: Vec<String> = (0..50_000).map(|n| format!("\"k{n}\": {n}")).collect();
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
fn what_the_ir_cannot_express_is_not_lowerable() {
    let cases = [
        // Mixed int and float at run time.
        "def f(x):\n    return x + 1.0\n\ndef main():\n    f(1)\n",
        // Float floor division at run time.
        "def f(x):\n    return x // 2.0\n\ndef main():\n    f(1.0)\n",
        // A run-time `<<`.
        "def f(x):\n    return x << 1\n\ndef main():\n    f(1)\n",
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
        // A primitive not lowered yet.
        "def main():\n    diag.record(\"t\")\n",
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
    let found = codes("def main():\n    diag.record(\"t\")\n    diag.ecu_info()\n    x = [1]\n");
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

/// A host that records the calls procedures make, and reports a wait as done only from its
/// `waits_pending`-th inquiry on.
#[derive(Default)]
struct ScriptedHost {
    calls: Vec<String>,
    waits_pending: u32,
}

impl DiagHost for ScriptedHost {
    type Error = NoHostError;

    fn service_request(&mut self, _: u16, _: &[u8]) -> Result<Vec<u8>, NoHostError> {
        unreachable!("not lowered yet")
    }
    fn read_dtc(&mut self, _: u8) -> Result<Vec<u8>, NoHostError> {
        unreachable!("not lowered yet")
    }
    fn routine_control(&mut self, _: u16, _: u8, _: &[u8]) -> Result<Vec<u8>, NoHostError> {
        unreachable!("not lowered yet")
    }
    fn security_access(&mut self, _: u64, _: u8, _: &[u8]) -> Result<Option<Vec<u8>>, NoHostError> {
        unreachable!("not lowered yet")
    }
    fn flash_transfer(&mut self, _: u32, _: &[u8]) -> Result<(), NoHostError> {
        unreachable!("not lowered yet")
    }
    fn wait(&mut self, _: u64, millis: u32) -> Result<bool, NoHostError> {
        if self.waits_pending > 0 {
            self.waits_pending -= 1;
            return Ok(false);
        }
        self.calls.push(format!("wait {millis}"));
        Ok(true)
    }
    fn hmi_request(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NoHostError> {
        unreachable!("not lowered yet")
    }
    fn record_input(&mut self, _: u64, _: &[u8]) -> Result<Option<Vec<u8>>, NoHostError> {
        unreachable!("not lowered yet")
    }
    fn monitor_capture(&mut self, back_millis: u32) -> Result<(), NoHostError> {
        self.calls.push(format!("capture {back_millis}"));
        Ok(())
    }
    fn log(&mut self, level: u8, message: &str) {
        self.calls.push(format!("log {level} {message}"));
    }
}

/// Runs `source` against `host` to its end, counting the steps that waited.
fn run_on(source: &str, host: &mut ScriptedHost) -> u32 {
    let program = lowered(source).unwrap_or_else(|errors| panic!("{errors:#?}"));
    let mut vm = Vm::new(&program);
    let mut waited = 0;
    for _ in 0..10_000 {
        match vm.step(&program, host) {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => return waited,
            Ok(StepOutcome::Waiting(WaitingOn::Timer)) => waited += 1,
            other => panic!("{other:?}"),
        }
    }
    panic!("the procedure did not end")
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
            "def f(n):\n    diag.wait(n)\n\ndef main():\n    f(1)\n",
            NonConstantArgument,
            2,
        ),
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
        (
            "def f(s):\n    diag.log(1, s)\n\ndef main():\n    f(\"x\")\n",
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
        // The wrong type or number of arguments.
        ("def main():\n    diag.wait(1.5)\n", UnsupportedSyntax, 2),
        (
            "def main():\n    diag.log(1, b\"x\")\n",
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
    let mut lowerer = Lowerer::new(&checked);
    lowerer.annotations();
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
    for body in calls {
        let source = format!("L = [{items}]\n\ndef main():\n{body}");
        let checked = check("t.star", &source).unwrap_or_else(|errors| panic!("{errors:#?}"));
        let mut lowerer = Lowerer::new(&checked);
        lowerer.annotations();
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
        diag.record(\"t\")

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
