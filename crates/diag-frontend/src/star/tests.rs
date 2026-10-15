use super::*;

/// The codes `check` reports for `source`, with their 1-based lines.
fn codes(source: &str) -> Vec<(StarCode, u32)> {
    match check("test.star", source) {
        Ok(_) => Vec::new(),
        Err(errors) => errors.iter().map(|e| (e.code, e.line)).collect(),
    }
}

fn passes(source: &str) -> Checked {
    check("test.star", source).unwrap_or_else(|errors| panic!("{errors:#?}"))
}

#[test]
fn the_example_procedure_passes() {
    let source = include_str!("../../examples/sequence.example.star");
    let checked = passes(source);
    let sections: Vec<_> = checked
        .annotations
        .iter()
        .filter(|a| matches!(a, Annotation::Section { .. }))
        .collect();
    assert_eq!(sections.len(), 2);
}

#[test]
fn a_valid_procedure_passes_with_its_annotations() {
    let checked = passes(
        r#""""A docstring."""
LIMIT = 3
KEYS = {"a": 1}

def helper(x, y):
    """Doc."""
    return x + y

def main():
    total = helper(1, 2)
    res = diag.request(0x22, {"did": 0xF190})
    vin = res["fields"]["vin"]
    raw = res["raw"]
    first = raw[0]
    tail = raw[1:4]
    name = "abc".upper()
    count = len(raw)
    # @maxIterations 10
    for i in range(count):
        if i > LIMIT:
            break
    # @section uninterruptible expected=120000
    # @idempotency checkState
    diag.flash_transfer(1)
    # @endsection
    v = KEYS["a"] if total else -1
"#,
    );
    assert_eq!(
        checked.annotations,
        [
            Annotation::MaxIterations {
                line: 18,
                limit: 10
            },
            Annotation::Section {
                line: 22,
                kind: SectionKind::Uninterruptible,
                expected_millis: Some(120_000)
            },
            Annotation::Idempotency {
                line: 23,
                value: Idempotency::CheckState
            },
            Annotation::EndSection { line: 25 },
        ]
    );
}

#[test]
fn a_file_without_main_is_rejected() {
    assert_eq!(
        codes("X = 1\ndef helper():\n    pass\n"),
        [(StarCode::NoMain, 1)]
    );
    assert_eq!(
        codes("def main(x):\n    pass\n"),
        [(StarCode::NoMain, 1), (StarCode::NoMain, 1)]
    );
}

#[test]
fn a_syntax_error_is_reported_alone() {
    let errors = check("test.star", "def main():\n    x = (1 +\n").unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, StarCode::SyntaxError);
}

#[test]
fn closures_are_rejected() {
    assert_eq!(
        codes("def main():\n    f = lambda x: x\n"),
        [(StarCode::ClosureNotAllowed, 2)]
    );
    assert_eq!(
        codes("def main():\n    def inner():\n        pass\n"),
        [(StarCode::ClosureNotAllowed, 2)]
    );
}

#[test]
fn functions_as_values_are_rejected() {
    let source = "\
def helper():
    pass

def main():
    f = helper
    g = len
    h = diag.request
    helper()
";
    assert_eq!(
        codes(source),
        [
            (StarCode::FunctionValueNotAllowed, 5),
            (StarCode::FunctionValueNotAllowed, 6),
            (StarCode::FunctionValueNotAllowed, 7),
        ]
    );
}

#[test]
fn unsupported_syntax_is_rejected() {
    let source = "\
def helper(a, b = 1, *args, **kwargs):
    pass

def typed(a: int) -> int:
    return a

def main():
    xs = [x for x in [1, 2]]
    d = {k: 1 for k in [1]}
    s = set([1])
    n = None
    a, b = 1, 2
    helper(1, b = 2)
    t: int = 1
";
    let found = codes(source);
    let lines: Vec<u32> = found
        .iter()
        .filter(|(code, _)| *code == StarCode::UnsupportedSyntax)
        .map(|(_, line)| *line)
        .collect();
    // Line 1: default, *args, **kwargs. Line 4: two annotations. Line 12: a tuple target and a
    // tuple value.
    assert_eq!(
        lines,
        [1, 1, 1, 4, 4, 8, 9, 10, 11, 12, 12, 13, 14],
        "{found:?}"
    );
}

#[test]
fn load_is_rejected() {
    assert_eq!(
        codes("load(\"x.star\", \"y\")\ndef main():\n    pass\n"),
        [(StarCode::LoadNotAllowed, 1)]
    );
}

#[test]
fn while_and_recursion_are_rejected() {
    assert_eq!(
        codes("def main():\n    while True:\n        pass\n"),
        [(StarCode::UnboundedNotAllowed, 2)]
    );
    let source = "\
def a():
    b()

def b():
    a()

def c():
    c()

def main():
    a()
";
    assert_eq!(
        codes(source),
        [
            (StarCode::UnboundedNotAllowed, 2),
            (StarCode::UnboundedNotAllowed, 5),
            (StarCode::UnboundedNotAllowed, 8),
        ]
    );
}

#[test]
fn top_level_statements_are_rejected() {
    let source = "\
X = diag.request(0x22, {})
for i in [1]:
    pass
diag.log(1, \"x\")

def main():
    pass
";
    assert_eq!(
        codes(source),
        [
            (StarCode::ToplevelStatementNotAllowed, 1),
            (StarCode::ToplevelStatementNotAllowed, 2),
            (StarCode::ToplevelStatementNotAllowed, 4),
        ]
    );
}

#[test]
fn computed_dict_keys_are_rejected() {
    let source = "\
TABLE = {\"a\": 1}

def main(): 
    k = \"a\"
    res = diag.request(0x22, {})
    x = res[k]
    y = res[\"fields\"][k]
    z = TABLE[k]
    raw = res[\"raw\"]
    w = raw[k]
";
    assert_eq!(
        codes(source),
        [
            (StarCode::DynamicKeyNotAllowed, 6),
            (StarCode::DynamicKeyNotAllowed, 7),
            (StarCode::DynamicKeyNotAllowed, 8),
        ]
    );
}

#[test]
fn fail_and_other_builtins_are_rejected() {
    let source = "\
def main():
    fail(\"x\")
    print(\"x\")
    getattr(1, \"a\")
    sorted([1])
    \"x\".split(\",\")
    diag.reboot()
";
    assert_eq!(
        codes(source),
        [
            (StarCode::FailNotAllowed, 2),
            (StarCode::BuiltinNotAllowed, 3),
            (StarCode::BuiltinNotAllowed, 4),
            (StarCode::BuiltinNotAllowed, 5),
            (StarCode::BuiltinNotAllowed, 6),
            (StarCode::BuiltinNotAllowed, 7),
        ]
    );
}

#[test]
fn bad_annotations_are_rejected() {
    let source = "\
def main():
    # @maxIterations ten
    for i in range(3):
        pass
    # @maxIterations 5
    diag.log(1, \"not a loop\")
    # @section uninteruptible
    # @section interruptible expected=5 extra=1
    # @idempotency maybe
    # @frobnicate
    # @endsection
    x = \"# @not an annotation\"
";
    let found = codes(source);
    assert!(
        found
            .iter()
            .all(|(code, _)| *code == StarCode::BadAnnotation),
        "{found:?}"
    );
    let lines: Vec<u32> = found.iter().map(|(_, line)| *line).collect();
    assert_eq!(lines, [2, 5, 7, 8, 9, 10, 11]);
}

#[test]
fn sections_pair_within_one_function_at_one_indentation() {
    let source = "\
def helper():
    # @section uninterruptible
    pass

def main():
    if True:
        # @section interruptible
        pass
    # @endsection
    # @endsection
";
    let found = codes(source);
    assert_eq!(
        found,
        [
            (StarCode::BadAnnotation, 2),
            (StarCode::BadAnnotation, 7),
            (StarCode::BadAnnotation, 9),
            (StarCode::BadAnnotation, 10),
        ]
    );
}

#[test]
fn annotation_values_out_of_range_are_rejected() {
    let source = "\
def main():
    # @maxIterations 0
    for i in range(3):
        pass
    # @section uninterruptible expected=4294967296
    diag.flash_transfer(1)
";
    assert_eq!(
        codes(source),
        [
            (StarCode::ArgumentOutOfRange, 2),
            (StarCode::ArgumentOutOfRange, 5),
        ]
    );
}

#[test]
fn errors_carry_their_position_and_source_line() {
    let errors = check("proc.star", "def main():\n    f = lambda x: x + 1\n").unwrap_err();
    let error = &errors[0];
    assert_eq!(
        (
            error.file.as_str(),
            error.line,
            error.column,
            error.snippet.as_str()
        ),
        ("proc.star", 2, 9, "    f = lambda x: x + 1")
    );
    let json = serde_json::to_value(error).expect("serializes");
    assert_eq!(json["code"], "STAR_CLOSURE_NOT_ALLOWED");
}
