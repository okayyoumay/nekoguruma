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

#[test]
fn an_f_string_is_reported_beside_the_other_violations() {
    assert_eq!(
        codes("def main():\n    s = f\"{print(1)}\"\n    t = (1, 2)\n"),
        [
            (StarCode::UnsupportedSyntax, 2),
            (StarCode::BuiltinNotAllowed, 2),
            (StarCode::UnsupportedSyntax, 3),
        ]
    );
}

#[test]
fn a_constant_assigned_from_a_dict_constant_is_a_dict() {
    assert_eq!(
        codes(
            "TABLE = {\"a\": 1}\nALIAS = TABLE\nCHAIN = ALIAS\n\ndef main():\n    k = \"a\"\n    \
             x = CHAIN[k]\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 7)]
    );
}

#[test]
fn a_rejected_construct_is_checked_inside_too() {
    // The right-hand side of a rejected top-level target.
    assert_eq!(
        codes("A, B = None, fail(\"x\")\n\ndef main():\n    pass\n"),
        [
            (StarCode::UnsupportedSyntax, 1),
            (StarCode::UnsupportedSyntax, 1),
            (StarCode::UnsupportedSyntax, 1),
            (StarCode::FailNotAllowed, 1),
        ]
    );
    // A lambda's body, a nested def's body, a comprehension's parts and a top-level statement.
    assert_eq!(
        codes(
            "print(1)\n\ndef main():\n    f = lambda: print(1)\n    def inner(x):\n        \
             return dir(x)\n    y = [print(v) for v in range(3) if getattr(v)]\n"
        ),
        [
            (StarCode::ToplevelStatementNotAllowed, 1),
            (StarCode::BuiltinNotAllowed, 1),
            (StarCode::ClosureNotAllowed, 4),
            (StarCode::BuiltinNotAllowed, 4),
            (StarCode::ClosureNotAllowed, 5),
            (StarCode::BuiltinNotAllowed, 6),
            (StarCode::UnsupportedSyntax, 7),
            (StarCode::BuiltinNotAllowed, 7),
            (StarCode::BuiltinNotAllowed, 7),
        ]
    );
}

#[test]
fn a_dict_binding_on_any_path_counts() {
    // A branch that may not run keeps the binding before it.
    assert_eq!(
        codes(
            "def main():\n    d = {\"a\": 1}\n    k = \"a\"\n    if k:\n        d = 0\n    x = d[k]\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 6)]
    );
    // Each branch starts from the bindings before the `if`.
    assert_eq!(
        codes(
            "def main():\n    d = {\"a\": 1}\n    k = \"a\"\n    if k:\n        d = 0\n    \
             else:\n        x = d[k]\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 7)]
    );
    // A loop may run no pass, and a pass sees what the previous one bound.
    assert_eq!(
        codes(
            "def main():\n    d = {\"a\": 1}\n    k = \"a\"\n    # @maxIterations 3\n    for i in \
             range(3):\n        d = 0\n    x = d[k]\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 7)]
    );
    assert_eq!(
        codes(
            "def main():\n    d = 0\n    k = \"a\"\n    # @maxIterations 3\n    for i in \
             range(3):\n        x = d[k]\n        d = {\"a\": 1}\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 6)]
    );
    // A name bound to a non-dict on every path is not one.
    passes("def main():\n    d = {\"a\": 1}\n    k = 0\n    d = [1]\n    x = d[k]\n");
}

#[test]
fn a_loop_iterable_sees_the_binding_before_the_loop() {
    assert_eq!(
        codes(
            "def main():\n    res = diag.request(0x22, {})\n    key = \"n\"\n    \
             # @maxIterations 3\n    for res in range(len(res[key])):\n        pass\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 5)]
    );
}

#[test]
fn only_a_response_fields_is_a_dict() {
    // A field-map entry named `fields` is a value, and so is a plain dict's `"fields"`.
    passes(
        "def main():\n    res = diag.request(0x22, {})\n    raw = res[\"fields\"][\"fields\"]\n    \
         first = raw[0]\n    d = {\"fields\": [1]}\n    second = d[\"fields\"][0]\n",
    );
    // Through an alias and a conditional expression too.
    assert_eq!(
        codes(
            "def main():\n    res = diag.request(0x22, {})\n    alias = res if True else {}\n    \
             f = alias[\"fields\"]\n    k = \"a\"\n    x = f[k]\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 6)]
    );
}

#[test]
fn a_local_hides_the_names_of_the_same_spelling() {
    // A parameter named like a built-in is a value.
    passes("def helper(len):\n    return len\n\ndef main():\n    helper(1)\n");
    // A parameter named like a dict constant is not known to be a dict.
    passes(
        "TABLE = {\"a\": 1}\n\ndef helper(TABLE):\n    return TABLE[0]\n\ndef main():\n    \
         helper([1])\n",
    );
    // A local named like a function is not callable, and neither is a constant.
    assert_eq!(
        codes(
            "LIMIT = 3\n\ndef main():\n    len = 3\n    x = len(\"ab\")\n    y = LIMIT(1)\n    \
             diag = \"a\"\n    z = diag.upper()\n"
        ),
        [
            (StarCode::FunctionValueNotAllowed, 5),
            (StarCode::FunctionValueNotAllowed, 6),
        ]
    );
}

#[test]
fn an_annotation_belongs_to_the_function_its_comment_is_in() {
    // The statement after the comment is in another function.
    assert_eq!(
        codes("def helper():\n    pass  # @idempotency unsafe\n\ndef main():\n    pass\n"),
        [(StarCode::BadAnnotation, 2)]
    );
    assert_eq!(
        codes(
            "def helper():\n    pass\n    # @maxIterations 3\n\ndef main():\n    for i in \
             range(3):\n        pass\n"
        ),
        [(StarCode::BadAnnotation, 3)]
    );
    // A comment at the indentation of `def` is outside the body.
    assert_eq!(
        codes("def main():\n    pass\n# @section interruptible\n    # @endsection\n\nX = 1\n"),
        [(StarCode::BadAnnotation, 3), (StarCode::BadAnnotation, 4)]
    );
    // A comment after the last statement of the body still belongs to it.
    passes(
        "def main():\n    # @section interruptible\n    diag.wait(1)\n    # @endsection\n\nX = 1\n",
    );
}

#[test]
fn a_negative_annotation_value_is_out_of_range() {
    assert_eq!(
        codes(
            "def main():\n    # @maxIterations -1\n    for i in range(3):\n        pass\n    \
             # @section uninterruptible expected=-1\n    pass\n    # @endsection\n"
        ),
        // The rejected `@section` leaves its `@endsection` unpaired.
        [
            (StarCode::ArgumentOutOfRange, 2),
            (StarCode::ArgumentOutOfRange, 5),
            (StarCode::BadAnnotation, 7),
        ]
    );
    assert_eq!(
        codes("def main():\n    # @maxIterations -x\n    for i in range(3):\n        pass\n"),
        [(StarCode::BadAnnotation, 2)]
    );
}

#[test]
fn deeply_nested_loops_are_checked_in_linear_passes() {
    // Each loop body binds `d` after using it, so every level needs a second pass to see it.
    let depth = 40;
    let mut source = String::from("def main():\n    d = 0\n    k = \"a\"\n");
    for level in 0..depth {
        let indent = "    ".repeat(level + 1);
        source.push_str(&format!(
            "{indent}# @maxIterations 2\n{indent}for i{level} in range(2):\n"
        ));
    }
    let indent = "    ".repeat(depth + 1);
    source.push_str(&format!("{indent}x = d[k]\n{indent}d = {{\"a\": 1}}\n"));
    let line = u32::try_from(4 + 2 * depth).unwrap();
    assert_eq!(codes(&source), [(StarCode::DynamicKeyNotAllowed, line)]);
}

#[test]
fn a_binding_carried_by_an_inner_loop_reaches_the_outer_one() {
    // `a` becomes a dict only on the inner loop's second pass, and is used after it.
    assert_eq!(
        codes(
            "def main():\n    a = 0\n    b = 0\n    k = \"a\"\n    # @maxIterations 2\n    for i in \
             range(2):\n        # @maxIterations 2\n        for j in range(2):\n            a = b\n            \
             b = {\"a\": 1}\n        x = a[k]\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 11)]
    );
}
